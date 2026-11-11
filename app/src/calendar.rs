//! Calendar and task reminders straight from Stalwart.
//!
//! Stalwart fires DISPLAY alarms itself (services/task_manager/tasks/alarm.rs) and pushes a JMAP
//! `CalendarAlert` {accountId, calendarEventId, uid, recurrenceId, alertId} to every push
//! subscriber that asked for the `CalendarAlert` type. This does not depend on a Bulwark tab
//! being open, which is where Bulwark's own reminders run. jmap-client has no CalendarEvent
//! methods yet, so the details are fetched with one plain JMAP request.

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use jmap_client::{client::Client, CalendarAlert};
use serde_json::{json, Value};
use std::{sync::LazyLock, time::Duration};

pub struct Item {
    pub is_task: bool,
    pub title: String,
    /// "Today 14:30", "Tomorrow, all day", "Due Fri Nov 20 09:00"…, in this computer's time zone.
    pub when: String,
    pub location: String,
}

/// One client for every reminder lookup, so the connection is reused.
static HTTP: LazyLock<Result<reqwest::Client, String>> =
    LazyLock::new(|| reqwest::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string()));

pub async fn fetch(client: &Client, alert: &CalendarAlert) -> Result<Item, String> {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
        "methodCalls": [["CalendarEvent/get", {
            "accountId": alert.account_id,
            "ids": [alert.calendar_event_id],
            "properties": ["@type", "title", "start", "due", "timeZone", "showWithoutTime", "locations", "recurrenceOverrides"]
        }, "a"]]
    });
    let response = HTTP
        .clone()?
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
    let now = Local::now();
    Ok(describe(event, alert.recurrence_id.as_deref(), &Local, now.date_naive()))
}

/// A JSCalendar LocalDateTime (`2026-11-11T09:30:00`) in the event's `timeZone`, as wall-clock
/// time in `target`. No time zone means a floating time: the same wall-clock time everywhere.
/// A trailing `Z` (UTC) is accepted too.
fn wall_clock<T: TimeZone>(at: &str, time_zone: Option<&str>, target: &T) -> Option<NaiveDateTime> {
    let (text, utc) = match at.strip_suffix('Z') {
        Some(t) => (t, true),
        None => (at, false),
    };
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    if utc {
        return Some(Utc.from_utc_datetime(&naive).with_timezone(target).naive_local());
    }
    let Some(tz) = time_zone.and_then(|z| z.parse::<chrono_tz::Tz>().ok()) else {
        return Some(naive); // floating, or a zone this build doesn't know: shown as written
    };
    // A time skipped by a DST change doesn't exist: an hour later is what clocks show then.
    let instant = tz
        .from_local_datetime(&naive)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(naive + chrono::Duration::hours(1))).earliest())?;
    Some(instant.with_timezone(target).naive_local())
}

/// Builds the toast text from a JSCalendar Event or Task. For a recurring event the instance's
/// recurrenceId is its original start; an override for that instance may have moved it.
fn describe<T: TimeZone>(event: &Value, recurrence_id: Option<&str>, target: &T, today: NaiveDate) -> Item {
    let is_task = event["@type"] == "Task";
    let base = if is_task { event["due"].as_str().or(event["start"].as_str()) } else { event["start"].as_str() };
    let at = match recurrence_id {
        Some(rid) => event["recurrenceOverrides"][rid]["start"].as_str().unwrap_or(rid),
        None => base.unwrap_or_default(),
    };
    let all_day = event["showWithoutTime"].as_bool().unwrap_or(false);
    let day_name = |date: NaiveDate| {
        if date == today {
            "Today".to_string()
        } else if Some(date) == today.succ_opt() {
            "Tomorrow".to_string()
        } else {
            date.format("%a %b %-d").to_string()
        }
    };
    let mut when = if at.is_empty() {
        String::new()
    } else if all_day || !at.contains('T') {
        // A date, not a moment: no time zone applies.
        let date = at.split('T').next().unwrap_or(at);
        match NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            Ok(d) => format!("{}, all day", day_name(d)),
            Err(_) => date.to_string(),
        }
    } else {
        match wall_clock(at, event["timeZone"].as_str(), target) {
            Some(t) => format!("{} {}", day_name(t.date()), t.format("%H:%M")),
            None => at.chars().take(16).collect(), // unparseable: show what the server sent
        }
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
    use chrono::FixedOffset;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn describes_events_and_tasks() {
        let utc = Utc;
        let ev = json!({"@type": "Event", "title": "Standup", "start": "2026-11-11T09:30:00",
            "locations": {"l1": {"@type": "Location", "name": "Room 2"}}});
        let item = describe(&ev, None, &utc, day("2026-11-11"));
        assert_eq!((item.title.as_str(), item.when.as_str(), item.location.as_str(), item.is_task),
            ("Standup", "Today 09:30", "Room 2", false));

        let recurring = describe(&ev, Some("2026-11-12T09:30:00"), &utc, day("2026-11-11"));
        assert_eq!(recurring.when, "Tomorrow 09:30");

        let task = json!({"@type": "Task", "title": "", "due": "2026-11-20T17:00:00"});
        let item = describe(&task, None, &utc, day("2026-11-11"));
        assert_eq!((item.title.as_str(), item.when.as_str(), item.is_task), ("(untitled task)", "Due Fri Nov 20 17:00", true));

        let all_day = json!({"@type": "Event", "title": "Holiday", "start": "2026-11-11T00:00:00", "showWithoutTime": true});
        assert_eq!(describe(&all_day, None, &utc, day("2026-11-11")).when, "Today, all day");
    }

    #[test]
    fn converts_time_zones_and_overrides() {
        let istanbul = FixedOffset::east_opt(3 * 3600).unwrap();
        // 09:30 in New York (EST, UTC-5) is 17:30 in Istanbul.
        let ev = json!({"@type": "Event", "title": "Call", "start": "2026-11-11T09:30:00", "timeZone": "America/New_York"});
        assert_eq!(describe(&ev, None, &istanbul, day("2026-11-11")).when, "Today 17:30");
        // Late evening in New York is the next day here.
        let late = json!({"@type": "Event", "title": "Late", "start": "2026-11-11T20:00:00", "timeZone": "America/New_York"});
        assert_eq!(describe(&late, None, &istanbul, day("2026-11-11")).when, "Tomorrow 04:00");
        // Floating: as written.
        let floating = json!({"@type": "Event", "title": "Gym", "start": "2026-11-11T07:00:00"});
        assert_eq!(describe(&floating, None, &istanbul, day("2026-11-11")).when, "Today 07:00");
        // A moved instance shows its new time.
        let moved = json!({"@type": "Event", "title": "Weekly", "start": "2026-11-04T10:00:00", "timeZone": "Europe/Istanbul",
            "recurrenceOverrides": {"2026-11-11T10:00:00": {"start": "2026-11-11T11:15:00"}}});
        assert_eq!(describe(&moved, Some("2026-11-11T10:00:00"), &istanbul, day("2026-11-11")).when, "Today 11:15");
        // Odd input never panics.
        let odd = json!({"@type": "Event", "title": "Odd", "start": "2026-11-11T9"});
        assert_eq!(describe(&odd, None, &istanbul, day("2026-11-11")).when, "2026-11-11T9");
    }
}
