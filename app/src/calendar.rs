//! Calendar and task reminders straight from Stalwart.
//!
//! Stalwart fires DISPLAY alarms itself (services/task_manager/tasks/alarm.rs) and pushes a JMAP
//! `CalendarAlert` {accountId, calendarEventId, uid, recurrenceId, alertId} to every push
//! subscriber that asked for the `CalendarAlert` type. This does not depend on a Bulwark tab
//! being open, which is where Bulwark's own reminders run. jmap-client has no CalendarEvent
//! methods yet, so the details are fetched with one plain JMAP request.

use jmap_client::{client::Client, CalendarAlert};
use serde_json::{json, Value};
use std::time::Duration;

pub struct Item {
    pub is_task: bool,
    pub title: String,
    /// "Today 14:30", "Tomorrow, all day", "Due Nov 12 09:00"…
    pub when: String,
    pub location: String,
}

pub async fn fetch(client: &Client, alert: &CalendarAlert) -> Result<Item, String> {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
        "methodCalls": [["CalendarEvent/get", {
            "accountId": alert.account_id,
            "ids": [alert.calendar_event_id],
            "properties": ["@type", "title", "start", "due", "showWithoutTime", "locations"]
        }, "a"]]
    });
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?
        .post(client.session().api_url())
        .headers(client.headers().clone())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("CalendarEvent/get: HTTP {}", response.status()));
    }
    let value: Value = serde_json::from_slice(&response.bytes().await.map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let call = &value["methodResponses"][0];
    if call[0] != "CalendarEvent/get" {
        return Err(format!("CalendarEvent/get failed: {}", call[1]));
    }
    let event = call[1]["list"].get(0).ok_or("the event is gone")?;
    Ok(describe(event, alert.recurrence_id.as_deref(), &today(), &tomorrow()))
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn tomorrow() -> String {
    (chrono::Local::now() + chrono::Duration::days(1)).format("%Y-%m-%d").to_string()
}

/// Builds the toast text from a JSCalendar Event or Task. For a recurring event the instance's
/// recurrenceId is its original start, which is what the reminder is about.
fn describe(event: &Value, recurrence_id: Option<&str>, today: &str, tomorrow: &str) -> Item {
    let is_task = event["@type"] == "Task";
    let at = recurrence_id
        .or_else(|| if is_task { event["due"].as_str().or(event["start"].as_str()) } else { event["start"].as_str() })
        .unwrap_or_default();
    let all_day = event["showWithoutTime"].as_bool().unwrap_or(false);
    let (date, time) = at.split_once('T').unwrap_or((at, ""));
    let day = if date == today {
        "Today".to_string()
    } else if date == tomorrow {
        "Tomorrow".to_string()
    } else {
        chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map(|d| d.format("%a %b %-d").to_string())
            .unwrap_or_else(|_| date.to_string())
    };
    let mut when = if at.is_empty() {
        String::new()
    } else if all_day || time.is_empty() {
        format!("{day}, all day")
    } else {
        format!("{day} {}", &time[..time.len().min(5)])
    };
    if is_task && !when.is_empty() {
        when = format!("Due {when}");
    }
    let location = event["locations"]
        .as_object()
        .and_then(|m| m.values().find_map(|l| l["name"].as_str()))
        .unwrap_or_default()
        .to_string();
    let title = event["title"].as_str().filter(|t| !t.is_empty());
    Item {
        is_task,
        title: title.unwrap_or(if is_task { "(untitled task)" } else { "(untitled event)" }).to_string(),
        when,
        location,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_events_and_tasks() {
        let ev = json!({"@type": "Event", "title": "Standup", "start": "2026-11-11T09:30:00",
            "locations": {"l1": {"@type": "Location", "name": "Room 2"}}});
        let item = describe(&ev, None, "2026-11-11", "2026-11-12");
        assert_eq!((item.title.as_str(), item.when.as_str(), item.location.as_str(), item.is_task),
            ("Standup", "Today 09:30", "Room 2", false));

        let recurring = describe(&ev, Some("2026-11-12T09:30:00"), "2026-11-11", "2026-11-12");
        assert_eq!(recurring.when, "Tomorrow 09:30");

        let task = json!({"@type": "Task", "title": "", "due": "2026-11-20T17:00:00"});
        let item = describe(&task, None, "2026-11-11", "2026-11-12");
        assert_eq!((item.title.as_str(), item.when.as_str(), item.is_task), ("(untitled task)", "Due Fri Nov 20 17:00", true));

        let all_day = json!({"@type": "Event", "title": "Holiday", "start": "2026-11-11T00:00:00", "showWithoutTime": true});
        assert_eq!(describe(&all_day, None, "2026-11-11", "2026-11-12").when, "Today, all day");
    }
}
