//! Keeps a JMAP EventSource open to the mail server. On each Email/Mailbox state change it
//! re-reads the Inbox unread count and toasts mail that is new since the last look. The same
//! stream carries the server's calendar/task reminders (CalendarAlert), shown via calendar.rs.
//! Depends only on JMAP (RFC 8620/8621), not on Bulwark.

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use jmap_client::{
    client::{Client, Credentials},
    core::{
        query::{Filter, QueryResponse},
        response::{EmailGetResponse, MailboxGetResponse},
    },
    email, mailbox,
    event_source::PushNotification,
    DataType,
};
use tauri::{AppHandle, Manager, Url};
use tokio::sync::Notify;

use crate::{auth, calendar, config, notify, tray, AppState};

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    NotConfigured,
    Connecting,
    Online { unread: usize },
    Offline(String),
    AuthFailed,
}

/// Re-arms the push stream this often even when the server is quiet, so a connection that died
/// silently (sleep, network switch) is replaced. The server's pings are not surfaced by jmap-client.
const STREAM_REARM: Duration = Duration::from_secs(300);
/// Only mail received this recently is toasted, so mail moved into the Inbox from an old
/// folder does not pop up.
const NOTIFY_MAX_AGE_SECS: i64 = 24 * 3600;
const MAX_TOASTS_PER_BURST: usize = 3;

pub fn spawn(app: AppHandle, restart: Arc<Notify>) {
    tauri::async_runtime::spawn(run(app, restart));
}

/// Why a session ended without an error.
enum Ended {
    /// The access token is about to expire: reconnect at once with a fresh one.
    Renew,
}

async fn run(app: AppHandle, restart: Arc<Notify>) {
    let mut backoff = Duration::from_secs(5);
    // Kept across reconnects, so mail that came in while the connection was down is still toasted.
    let mut known: Option<HashSet<String>> = None;
    loop {
        let cfg = app.state::<AppState>().config.lock().unwrap().clone();
        if !cfg.is_complete() {
            tray::set_status(&app, &Status::NotConfigured);
            known = None;
            restart.notified().await;
            continue;
        }

        let token = match auth::access_token(&app).await {
            Ok(t) => t,
            Err(auth::AuthError::SignInNeeded(e)) => {
                crate::log(&format!("watcher: sign-in needed ({e})"));
                tray::set_status(&app, &Status::AuthFailed);
                restart.notified().await; // until the user signs in again in Settings
                continue;
            }
            Err(auth::AuthError::Temporary(e)) => {
                tray::set_status(&app, &Status::Offline(e.clone()));
                crate::log(&format!("watcher: {e}; retrying in {}s", backoff.as_secs()));
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => { backoff = (backoff * 2).min(Duration::from_secs(300)); }
                    _ = restart.notified() => { backoff = Duration::from_secs(5); }
                }
                continue;
            }
        };

        tray::set_status(&app, &Status::Connecting);
        let outcome = tokio::select! {
            r = session(&app, &cfg, &token, &mut known) => r,
            _ = restart.notified() => { backoff = Duration::from_secs(5); known = None; continue; }
        };
        let err = match outcome {
            Ok(Ended::Renew) => {
                backoff = Duration::from_secs(5);
                continue;
            }
            Err(e) => e,
        };
        if is_auth_error(&err) {
            // Revoked or expired early: drop the cached token; the next turn refreshes or asks to sign in.
            crate::log(&format!("watcher: {err}; renewing the token"));
            *app.state::<AppState>().token.lock().await = None;
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        crate::log(&format!("watcher: {err}; retrying in {}s", backoff.as_secs()));
        tray::set_status(&app, &Status::Offline(err));
        tokio::select! {
            _ = tokio::time::sleep(backoff) => { backoff = (backoff * 2).min(Duration::from_secs(300)); }
            _ = restart.notified() => { backoff = Duration::from_secs(5); known = None; }
        }
    }
}

fn is_auth_error(err: &str) -> bool {
    err.contains("401") || err.to_ascii_lowercase().contains("unauthorized")
}

pub async fn connect(cfg: &config::Config, access_token: &str) -> Result<Client, String> {
    let base = cfg.jmap_url.trim().trim_end_matches('/').to_string();
    let host = Url::parse(&base).map_err(|e| format!("JMAP URL: {e}"))?.host_str().unwrap_or_default().to_string();
    Client::new()
        .credentials(Credentials::bearer(access_token))
        .timeout(Duration::from_secs(30))
        .follow_redirects([host])
        .connect(&base)
        .await
        .map_err(|e| e.to_string())
}

pub async fn inbox_id(client: &Client) -> Result<String, String> {
    let mut req = client.build();
    req.query_mailbox().filter(mailbox::query::Filter::role(mailbox::Role::Inbox));
    let mut resp = req.send_single::<QueryResponse>().await.map_err(|e| e.to_string())?;
    resp.take_ids().into_iter().next().ok_or_else(|| "the account has no Inbox".into())
}

pub async fn inbox_unread(client: &Client, inbox: &str) -> Result<usize, String> {
    let mut req = client.build();
    req.get_mailbox().ids([inbox]).properties([mailbox::Property::UnreadEmails]);
    let mut resp = req.send_single::<MailboxGetResponse>().await.map_err(|e| e.to_string())?;
    Ok(resp.take_list().first().map(|m| m.unread_emails()).unwrap_or(0))
}

async fn session(
    app: &AppHandle,
    cfg: &config::Config,
    token: &auth::AccessToken,
    known: &mut Option<HashSet<String>>,
) -> Result<Ended, String> {
    let client = connect(cfg, &token.token).await?;
    let has_calendars = client.session().capabilities().any(|c| c == "urn:ietf:params:jmap:calendars");
    crate::log(&format!("connected; calendars capability: {has_calendars}"));
    let inbox = inbox_id(&client).await?;
    let seed = known.is_none();
    let known = known.get_or_insert_with(HashSet::new);
    refresh(app, cfg, &client, &inbox, known, seed).await?;

    loop {
        let mut stream = client
            .event_source(Some([DataType::Email, DataType::Mailbox, DataType::CalendarAlert]), false, Some(60), None)
            .await
            .map_err(|e| e.to_string())?;
        crate::log("push stream armed (Email, Mailbox, CalendarAlert)");
        // Anything that changed while the stream was down.
        refresh(app, cfg, &client, &inbox, known, false).await?;
        loop {
            let next = tokio::select! {
                n = tokio::time::timeout(STREAM_REARM, stream.next()) => n,
                _ = tokio::time::sleep_until(token.renew_at.into()) => return Ok(Ended::Renew),
            };
            match next {
                Err(_) => {
                    crate::log("push stream quiet for 5 min: re-arming");
                    break;
                }
                Ok(None) => return Err("push stream closed".into()),
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(PushNotification::StateChange(changes)))) => {
                    let types: Vec<String> = changes
                        .changed_accounts()
                        .cloned()
                        .collect::<Vec<_>>()
                        .iter()
                        .flat_map(|a| changes.changes(a).into_iter().flatten().map(|(t, _)| t.to_string()).collect::<Vec<_>>())
                        .collect();
                    crate::log(&format!("state change: {}", types.join(", ")));
                    // Let a burst of changes (a delivery touches Email and Mailbox) settle.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    refresh(app, cfg, &client, &inbox, known, false).await?;
                }
                Ok(Some(Ok(PushNotification::CalendarAlert(alert)))) => {
                    crate::log(&format!(
                        "calendar alert: event {} alert {} recurrence {:?}",
                        alert.calendar_event_id, alert.alert_id, alert.recurrence_id
                    ));
                    if cfg.calendar_alerts {
                        match calendar::fetch(&client, &alert).await {
                            Ok(item) => {
                                crate::log(&format!(
                                    "  -> {} '{}' {}",
                                    if item.is_task { "task" } else { "event" }, item.title, item.when
                                ));
                                notify::calendar_alert(app, item, alert.calendar_event_id.clone());
                            }
                            Err(e) => crate::log(&format!("  -> fetch failed: {e}")),
                        }
                    }
                }
            }
        }
    }
}

async fn refresh(
    app: &AppHandle,
    cfg: &config::Config,
    client: &Client,
    inbox: &str,
    known: &mut HashSet<String>,
    seed: bool,
) -> Result<(), String> {
    let unread = inbox_unread(client, inbox).await?;
    tray::set_status(app, &Status::Online { unread });

    let mut req = client.build();
    req.query_email()
        .filter(Filter::and([
            email::query::Filter::in_mailbox(inbox),
            email::query::Filter::not_keyword("$seen"),
        ]))
        .sort([email::query::Comparator::received_at().descending()])
        .limit(20);
    let ids = req.send_single::<QueryResponse>().await.map_err(|e| e.to_string())?.take_ids();

    let fresh: Vec<String> = ids.iter().filter(|id| !known.contains(*id)).cloned().collect();
    if known.len() > 1000 {
        known.clear();
    }
    known.extend(ids);
    if seed || fresh.is_empty() || !cfg.notifications {
        return Ok(());
    }

    let mut req = client.build();
    req.get_email().ids(fresh.iter().map(String::as_str)).properties([
        email::Property::Id,
        email::Property::From,
        email::Property::Subject,
        email::Property::Preview,
        email::Property::ReceivedAt,
    ]);
    let mut emails = req.send_single::<EmailGetResponse>().await.map_err(|e| e.to_string())?.take_list();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    emails.retain(|e| e.received_at().map(|t| now - t <= NOTIFY_MAX_AGE_SECS).unwrap_or(true));
    emails.sort_by_key(|e| std::cmp::Reverse(e.received_at().unwrap_or(0)));

    for e in emails.iter().take(MAX_TOASTS_PER_BURST) {
        let from = e
            .from()
            .and_then(|f| f.first())
            .map(|a| a.name().filter(|n| !n.is_empty()).unwrap_or(a.email()).to_string())
            .unwrap_or_default();
        notify::new_mail(
            app,
            notify::NewMail {
                id: e.id().unwrap_or_default().to_string(),
                from,
                subject: e.subject().unwrap_or_default().to_string(),
                preview: e.preview().unwrap_or_default().chars().take(140).collect(),
            },
        );
    }
    if emails.len() > MAX_TOASTS_PER_BURST {
        notify::summary(app, emails.len() - MAX_TOASTS_PER_BURST);
    }
    Ok(())
}
