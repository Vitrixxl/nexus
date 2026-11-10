//! The phone's calendars on the desktop. The phone's calendar provider is the
//! source of truth, Google's and the rest included; the daemon keeps a copy of
//! it for when the phone is away, and the changes made meanwhile, which reach
//! the phone once it connects.
use serde::{Deserialize, Serialize};

/// The start of the ids of events the phone has not been told about yet.
pub const LOCAL: &str = "local-";

/// One occurrence: a repeating event has one per date, under the same `id`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Event {
    /// Empty or starting with [`LOCAL`] until the phone has it.
    #[serde(default)]
    pub id: String,
    /// Milliseconds since the epoch. All day, midnights UTC, as Android has
    /// them, the end excluded.
    pub begin: i64,
    pub end: i64,
    /// When this occurrence was to start: what tells it from its siblings.
    #[serde(default)]
    pub instance: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default)]
    pub location: String,
    /// ARGB.
    #[serde(default)]
    pub color: i64,
    #[serde(default)]
    pub calendar: i64,
    #[serde(default)]
    pub repeats: bool,
    #[serde(default)]
    pub writable: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Calendar {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub color: i64,
    #[serde(default)]
    pub writable: bool,
    #[serde(default)]
    pub primary: bool,
}

/// A date, as the instants that bound it here and its midnight UTC.
#[derive(Clone, Copy, Debug)]
pub struct Day {
    pub start: i64,
    pub end: i64,
    pub utc: i64,
}

impl Event {
    /// Whether it takes place on `day`. A timed event that ends at midnight
    /// does not reach the next day; one with no length is on the day it starts.
    pub fn on(&self, day: &Day) -> bool {
        if self.all_day {
            self.begin <= day.utc && day.utc < self.end.max(self.begin + 1)
        } else {
            self.begin < day.end && (self.end > day.start || self.begin >= day.start)
        }
    }
    fn same(&self, other: &Self) -> bool {
        self.id == other.id && self.instance == other.instance
    }
}

/// All-day events first, then by time.
pub fn sort(events: &mut [Event]) {
    events.sort_by(|a, b| (!a.all_day, a.begin, &a.title).cmp(&(!b.all_day, b.begin, &b.title)));
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Change {
    Save(Event),
    Delete(Event),
}
impl Change {
    fn event(&self) -> &Event {
        match self {
            Change::Save(e) | Change::Delete(e) => e,
        }
    }
}

/// What the daemon keeps, on disk too: the changes are the user's work.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Cache {
    pub events: Vec<Event>,
    pub calendars: Vec<Calendar>,
    #[serde(default)]
    pub pending: Vec<Change>,
    /// The span the events cover.
    #[serde(default)]
    pub from: i64,
    #[serde(default)]
    pub to: i64,
}
impl Cache {
    fn show(&mut self, change: &Change) {
        self.events.retain(|e| !e.same(change.event()));
        if let Change::Save(e) = change {
            self.events.push(e.clone());
        }
    }
    /// Takes a change: shown at once, on its way to the phone. Only the last
    /// change of an event travels, and nothing for one the phone never had.
    pub fn apply(&mut self, change: Change) {
        self.show(&change);
        self.pending.retain(|c| !c.event().same(change.event()));
        if !matches!(&change, Change::Delete(e) if e.id.is_empty() || e.id.starts_with(LOCAL)) {
            self.pending.push(change);
        }
    }
    /// The phone's listing, with the changes it has yet to hear of on top.
    /// Whether that differs from what was shown.
    pub fn replace(&mut self, mut listing: Cache) -> bool {
        listing.pending = self.pending.clone();
        for change in listing.pending.clone() {
            listing.show(&change);
        }
        // The provider's order is not stable from one listing to the next.
        sort(&mut listing.events);
        sort(&mut self.events);
        let changed = *self != listing;
        *self = listing;
        changed
    }
    pub fn between(&self, from: i64, to: i64) -> Vec<Event> {
        let mut events: Vec<Event> = self
            .events
            .iter()
            .filter(|e| e.begin < to && e.end.max(e.begin + 1) > from)
            .cloned()
            .collect();
        sort(&mut events);
        events
    }
}

/// A time as people type it: `14`, `14:30`, `14h30`, `1430`, `2pm`, `2:30 pm`.
pub fn parse_time(text: &str) -> Option<(u32, u32)> {
    let t = text.trim().to_ascii_lowercase();
    let (t, half) = match (t.strip_suffix("am"), t.strip_suffix("pm")) {
        (Some(r), _) => (r.trim_end(), Some(0)),
        (_, Some(r)) => (r.trim_end(), Some(12)),
        _ => (t.as_str(), None),
    };
    if t.is_empty() {
        return None;
    }
    let (h, m) = match t.split_once([':', 'h', '.']) {
        Some((h, "")) => (h.parse().ok()?, 0),
        Some((h, m)) if m.len() == 2 => (h.parse().ok()?, m.parse().ok()?),
        None if t.len() <= 2 => (t.parse().ok()?, 0),
        None if t.len() <= 4 && t.bytes().all(|b| b.is_ascii_digit()) => {
            let (h, m) = t.split_at(t.len() - 2);
            (h.parse().ok()?, m.parse().ok()?)
        }
        _ => return None,
    };
    let (h, m): (u32, u32) = (h, m);
    let h = match half {
        Some(_) if !(1..=12).contains(&h) => return None,
        Some(add) => h % 12 + add,
        None => h,
    };
    (h < 24 && m < 60).then_some((h, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;
    fn event(id: &str, begin: i64) -> Event {
        Event {
            id: id.into(),
            begin,
            end: begin + H,
            instance: begin,
            ..Default::default()
        }
    }
    fn day(n: i64) -> Day {
        Day {
            start: n * 24 * H - 3 * H,
            end: (n + 1) * 24 * H - 3 * H,
            utc: n * 24 * H,
        }
    }

    #[test]
    fn events_land_on_their_days() {
        let timed = event("a", 24 * H);
        assert!(timed.on(&day(1)) && !timed.on(&day(0)) && !timed.on(&day(2)));
        // Ends right at the local midnight: not on the next day.
        let mut late = event("b", 18 * H);
        late.end = 21 * H;
        assert!(late.on(&day(0)) && !late.on(&day(1)));
        // Across midnight: on both.
        late.end = 22 * H;
        assert!(late.on(&day(0)) && late.on(&day(1)));
        // No length: on its day.
        late.end = late.begin;
        assert!(late.on(&day(0)) && !late.on(&day(1)));
        // All day goes by the UTC date, whatever the zone: two days, end excluded.
        let all = Event {
            all_day: true,
            begin: 24 * H,
            end: 72 * H,
            ..Default::default()
        };
        assert!(!all.on(&day(0)) && all.on(&day(1)) && all.on(&day(2)) && !all.on(&day(3)));
    }

    #[test]
    fn only_the_last_change_travels() {
        let mut c = Cache::default();
        let mut made = event("local-1", 0);
        c.apply(Change::Save(made.clone()));
        made.title = "renamed".into();
        c.apply(Change::Save(made.clone()));
        assert_eq!(c.pending, [Change::Save(made.clone())]);
        assert_eq!(c.events, [made.clone()]);
        // Deleted before the phone ever heard of it: nothing to tell.
        c.apply(Change::Delete(made));
        assert!(c.pending.is_empty() && c.events.is_empty());
        // One occurrence of a repeating event leaves its siblings alone.
        c.events = vec![event("7", 0), event("7", 24 * H)];
        c.apply(Change::Delete(event("7", 0)));
        assert_eq!(c.events, [event("7", 24 * H)]);
        assert_eq!(c.pending, [Change::Delete(event("7", 0))]);
    }

    #[test]
    fn a_listing_keeps_what_the_phone_has_not_heard() {
        let mut c = Cache::default();
        c.apply(Change::Save(event("local-1", 5 * H)));
        c.apply(Change::Delete(event("2", 0)));
        let listing = Cache {
            events: vec![event("3", 9 * H), event("2", 0)],
            ..Default::default()
        };
        assert!(c.replace(listing.clone()));
        let ids: Vec<&str> = c.events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["local-1", "3"]);
        assert_eq!(c.pending.len(), 2);
        // The same listing again changes nothing, whatever its order.
        let mut again = listing;
        again.events.reverse();
        assert!(!c.replace(again));
    }

    #[test]
    fn between_catches_what_overlaps() {
        let c = Cache {
            events: vec![event("a", 0), event("b", 10 * H)],
            ..Default::default()
        };
        assert_eq!(c.between(H / 2, 2 * H).len(), 1);
        assert_eq!(c.between(H, 10 * H).len(), 0);
        assert_eq!(c.between(0, 11 * H).len(), 2);
    }

    #[test]
    fn times_parse_as_typed() {
        for (text, want) in [
            ("14", (14, 0)),
            ("14:30", (14, 30)),
            ("14h30", (14, 30)),
            ("14h", (14, 0)),
            ("9.05", (9, 5)),
            ("1430", (14, 30)),
            ("930", (9, 30)),
            ("2pm", (14, 0)),
            ("2:30 PM", (14, 30)),
            ("12am", (0, 0)),
            ("12pm", (12, 0)),
            ("0", (0, 0)),
        ] {
            assert_eq!(parse_time(text), Some(want), "{text}");
        }
        for bad in [
            "", "24", "12:60", "13pm", "0am", "abc", "1:5", "12345", "pm",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }
}
