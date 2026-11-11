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
use tauri::{AppHandle, Manager};
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

/// Why a session (or a JMAP call) failed.
#[derive(Debug)]
pub struct Failure {
    /// The server refused the access token (HTTP 401): renew it before anything else.
    pub auth: bool,
    pub msg: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl From<String> for Failure {
    fn from(msg: String) -> Self {
        Failure { auth: false, msg }
    }
}

impl From<jmap_client::Error> for Failure {
    fn from(e: jmap_client::Error) -> Self {
        use jmap_client::Error as E;
        let auth = match &e {
            E::Problem(p) => p.status() == Some(401),
            // handle_error() formats the bare status: "401 Unauthorized".
            E::Server(status) => status.split_whitespace().next() == Some("401"),
            E::Transport(t) => t.status().is_some_and(|s| s.as_u16() == 401),
            _ => false,
        };
        Failure { auth, msg: e.to_string() }
    }
}

impl From<auth::AuthError> for Failure {
    fn from(e: auth::AuthError) -> Self {
        let auth = matches!(e, auth::AuthError::SignInNeeded(_));
        Failure { auth, msg: e.to_string() }
    }
}

/// Retry pacing, reset once a session is up.
struct Health {
    backoff: Duration,
    /// 401s in a row: a token the server keeps refusing must not be renewed in a tight loop.
    auth_failures: u32,
}

const FIRST_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// After this many 401s in a row (each with a freshly renewed token), ask the user to sign in.
const MAX_AUTH_FAILURES: u32 = 4;

impl Health {
    fn ok(&mut self) {
        self.backoff = FIRST_BACKOFF;
        self.auth_failures = 0;
    }

    fn next_backoff(&mut self) -> Duration {
        let wait = self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        wait
    }
}

async fn run(app: AppHandle, restart: Arc<Notify>) {
    let mut health = Health { backoff: FIRST_BACKOFF, auth_failures: 0 };
    // Kept across reconnects, so mail that came in while the connection was down is still toasted.
    let mut known: Option<Seen> = None;
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
                health.ok();
                continue;
            }
            Err(auth::AuthError::Temporary(e)) => {
                tray::set_status(&app, &Status::Offline(e.clone()));
                let wait = health.next_backoff();
                crate::log(&format!("watcher: {e}; retrying in {}s", wait.as_secs()));
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = restart.notified() => { health.ok(); }
                }
                continue;
            }
        };

        tray::set_status(&app, &Status::Connecting);
        let err = tokio::select! {
            Err(e) = session(&app, &cfg, token, &mut health, &mut known) => e,
            _ = restart.notified() => { health.ok(); known = None; continue; }
        };
        if err.auth {
            health.auth_failures += 1;
            // Revoked or expired early: drop the cached token; the next turn refreshes it.
            *app.state::<AppState>().token.lock().await = None;
            if health.auth_failures >= MAX_AUTH_FAILURES {
                crate::log(&format!("watcher: {err}; refused {} times in a row, sign-in needed", health.auth_failures));
                tray::set_status(&app, &Status::AuthFailed);
                restart.notified().await;
                health.ok();
                continue;
            }
            // The first one renews at once; repeats wait like any other failure.
            let wait = if health.auth_failures == 1 { Duration::from_secs(2) } else { health.next_backoff() };
            crate::log(&format!("watcher: {err}; renewing the token in {}s", wait.as_secs()));
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = restart.notified() => { health.ok(); known = None; }
            }
            continue;
        }
        let wait = health.next_backoff();
        crate::log(&format!("watcher: {err}; retrying in {}s", wait.as_secs()));
        tray::set_status(&app, &Status::Offline(err.msg));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = restart.notified() => { health.ok(); known = None; }
        }
    }
}

/// Connects to the server's JMAP session (`<server>/.well-known/jmap`). The bearer token only
/// goes over https: to the configured server, and to the API and push URLs its session names.
pub async fn connect(cfg: &config::Config, access_token: &str) -> Result<Client, Failure> {
    let base = auth::server_base(&cfg.jmap_url)?;
    auth::secure_url(base.as_str()).map_err(|e| format!("Mail server URL {e}"))?;
    let host = base.host_str().unwrap_or_default().to_string();
    let client = Client::new()
        .credentials(Credentials::bearer(access_token))
        .timeout(Duration::from_secs(30))
        .follow_redirects([host])
        .connect(base.as_str().trim_end_matches('/'))
        .await?;
    let session = client.session();
    for (what, url) in [("API", session.api_url()), ("push", session.event_source_url())] {
        auth::secure_url(url).map_err(|e| format!("the server's JMAP {what} URL {e}"))?;
    }
    Ok(client)
}

pub async fn inbox_id(client: &Client) -> Result<String, Failure> {
    let mut req = client.build();
    req.query_mailbox().filter(mailbox::query::Filter::role(mailbox::Role::Inbox));
    let mut resp = req.send_single::<QueryResponse>().await?;
    resp.take_ids().into_iter().next().ok_or_else(|| "the account has no Inbox".to_string().into())
}

pub async fn inbox_unread(client: &Client, inbox: &str) -> Result<usize, Failure> {
    let mut req = client.build();
    req.get_mailbox().ids([inbox]).properties([mailbox::Property::UnreadEmails]);
    let mut resp = req.send_single::<MailboxGetResponse>().await?;
    Ok(resp.take_list().first().map(|m| m.unread_emails()).unwrap_or(0))
}

type PushStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = jmap_client::Result<PushNotification>> + Send>>;

/// jmap-client only bounds the connect step, so a server or proxy that accepts the connection
/// and never answers would otherwise hang the watcher here for good.
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

async fn open_stream(client: &Client) -> Result<PushStream, Failure> {
    let open = client.event_source(Some([DataType::Email, DataType::Mailbox, DataType::CalendarAlert]), false, Some(60), None);
    let stream = tokio::time::timeout(STREAM_OPEN_TIMEOUT, open)
        .await
        .map_err(|_| "the push stream did not open within 30 s".to_string())??;
    Ok(Box::pin(stream))
}

/// Runs until something fails (it never ends on its own). A new access token or a quiet
/// stream gets a new stream, opened before the old one is dropped, so nothing is missed in
/// between; a catch-up refresh covers the moment of the swap.
async fn session(
    app: &AppHandle,
    cfg: &config::Config,
    mut token: auth::AccessToken,
    health: &mut Health,
    known: &mut Option<Seen>,
) -> Result<std::convert::Infallible, Failure> {
    let client = connect(cfg, &token.token).await?;
    let has_calendars = client.session().capabilities().any(|c| c == "urn:ietf:params:jmap:calendars");
    crate::log(&format!("connected; calendars capability: {has_calendars}"));
    let inbox = inbox_id(&client).await?;
    // Only a successful first look decides what counts as already seen; a failed one must not
    // leave an empty set behind that would make every unread mail look new next time.
    let mut first = known.clone().unwrap_or_default();
    refresh(app, &client, &inbox, &mut first, known.is_none()).await?;
    let known = known.insert(first);

    let mut client = client;
    let mut stream = open_stream(&client).await?;
    crate::log("push stream armed (Email, Mailbox, CalendarAlert)");
    health.ok();
    // Anything that changed between the first look and the stream.
    refresh(app, &client, &inbox, known, false).await?;

    loop {
        let next = tokio::select! {
            n = tokio::time::timeout(STREAM_REARM, stream.next()) => n,
            _ = tokio::time::sleep_until(token.renew_at.into()) => {
                token = auth::access_token(app).await?;
                let fresh = connect(cfg, &token.token).await?;
                let fresh_stream = open_stream(&fresh).await?;
                stream = fresh_stream;
                client = fresh;
                crate::log("access token renewed: push stream re-armed");
                refresh(app, &client, &inbox, known, false).await?;
                continue;
            }
        };
        match next {
            Err(_) => {
                crate::log("push stream quiet for 5 min: re-arming");
                stream = open_stream(&client).await?;
                refresh(app, &client, &inbox, known, false).await?;
            }
            Ok(None) => return Err("push stream closed".to_string().into()),
            // A comment line or an unknown event the parser can't read: skip it, keep the stream.
            Ok(Some(Err(e @ (jmap_client::Error::Parse(_) | jmap_client::Error::Internal(_))))) => {
                crate::log(&format!("push stream: skipped an unreadable event ({e})"));
            }
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(Some(Ok(PushNotification::StateChange(changes)))) => {
                let types: Vec<String> = changes
                    .changed_accounts()
                    .flat_map(|a| changes.changes(a).into_iter().flatten().map(|(t, _)| t.to_string()))
                    .collect();
                crate::log(&format!("state change: {}", types.join(", ")));
                // Let a burst of changes (a delivery touches Email and Mailbox) settle.
                tokio::time::sleep(Duration::from_millis(300)).await;
                refresh(app, &client, &inbox, known, false).await?;
            }
            Ok(Some(Ok(PushNotification::CalendarAlert(alert)))) => {
                crate::log(&format!(
                    "calendar alert: event {} alert {} recurrence {:?}",
                    alert.calendar_event_id, alert.alert_id, alert.recurrence_id
                ));
                if wants(app).1 {
                    match calendar::fetch(&client, &alert).await {
                        Ok(item) => {
                            crate::log(&format!("  -> {}", if item.is_task { "task" } else { "event" }));
                            notify::calendar_alert(app, item, alert.calendar_event_id.clone());
                        }
                        Err(e) => crate::log(&format!("  -> fetch failed: {e}")),
                    }
                }
            }
        }
    }
}

/// What has been looked at already: the unread ids seen, and the newest arrival time among
/// them. Only mail that arrived at or after that time counts as new, so older unread mail
/// moving into the "newest 20" window (after you read some) is not mistaken for new.
#[derive(Clone, Default)]
pub struct Seen {
    ids: HashSet<String>,
    newest: Option<i64>,
}

impl Seen {
    fn remember(&mut self, ids: Vec<String>) {
        if self.ids.len() > 1000 {
            self.ids.clear(); // `newest` still keeps old mail from looking new
        }
        self.ids.extend(ids);
    }

    fn arrived(&mut self, t: i64) {
        self.newest = Some(self.newest.map_or(t, |n| n.max(t)));
    }
}

/// The two toggles, read when needed, so changing them doesn't restart the connection.
fn wants(app: &AppHandle) -> (bool, bool) {
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    (cfg.notifications, cfg.calendar_alerts)
}

async fn refresh(app: &AppHandle, client: &Client, inbox: &str, seen: &mut Seen, seed: bool) -> Result<(), Failure> {
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
    let ids = req.send_single::<QueryResponse>().await?.take_ids();
    let fresh: Vec<String> = ids.iter().filter(|id| !seen.ids.contains(*id)).cloned().collect();

    if seed {
        // The newest unread mail sets the bar; nothing already there is toasted.
        if let Some(first) = ids.first() {
            let mut req = client.build();
            req.get_email().ids([first.as_str()]).properties([email::Property::Id, email::Property::ReceivedAt]);
            let list = req.send_single::<EmailGetResponse>().await?.take_list();
            if let Some(t) = list.first().and_then(|e| e.received_at()) {
                seen.arrived(t);
            }
        }
        seen.remember(ids);
        return Ok(());
    }
    if fresh.is_empty() || !wants(app).0 {
        seen.remember(ids);
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
    let mut emails = req.send_single::<EmailGetResponse>().await?.take_list();
    // Only now are they "seen": if the lookup above failed, the next refresh tries them again.
    seen.remember(ids);
    let bar = seen.newest;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    emails.retain(|e| match e.received_at() {
        Some(t) => now - t <= NOTIFY_MAX_AGE_SECS && bar.is_none_or(|b| t >= b),
        None => bar.is_none(),
    });
    for t in emails.iter().filter_map(|e| e.received_at()) {
        seen.arrived(t);
    }
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
