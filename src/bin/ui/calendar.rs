//! The month and the day's events in the notification center. A grid of its
//! own rather than `gtk::Calendar`, whose cells take whatever shape the grid
//! leaves them and cannot carry events. See [`nexus_control::calendar`].
use super::{
    super::{caption, hbox, label, vbox},
    phone::call,
};
use gtk::{glib, prelude::*};
use nexus_control::{
    calendar::{self, Day, Event, LOCAL, parse_time},
    phone::CalendarSync,
};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

const CELLS: usize = 42;
const AGENDA_HEIGHT: i32 = 176;

type Date = (i32, i32, i32);

fn local(d: Date) -> Option<glib::DateTime> {
    glib::DateTime::from_local(d.0, d.1, d.2, 0, 0, 0.).ok()
}
fn date_of(t: &glib::DateTime) -> Date {
    (t.year(), t.month(), t.day_of_month())
}
fn ms(t: &glib::DateTime) -> i64 {
    t.to_unix() * 1000
}
fn shift(d: Date, days: i32) -> Date {
    local(d)
        .and_then(|t| t.add_days(days).ok())
        .map_or(d, |t| date_of(&t))
}
fn utc_ms(d: Date) -> i64 {
    glib::DateTime::from_utc(d.0, d.1, d.2, 0, 0, 0.).map_or(0, |t| ms(&t))
}
fn today() -> Date {
    glib::DateTime::now_local().map_or((2000, 1, 1), |t| date_of(&t))
}
fn day_of(d: Date) -> Day {
    Day {
        start: local(d).map_or(0, |t| ms(&t)),
        end: local(shift(d, 1)).map_or(0, |t| ms(&t)),
        utc: utc_ms(d),
    }
}
fn format(d: Date, pattern: &str) -> String {
    local(d)
        .and_then(|t| t.format(pattern).ok())
        .map_or_else(String::new, |s| s.to_string())
}
fn clock(at: i64) -> String {
    glib::DateTime::from_unix_local(at / 1000)
        .and_then(|t| t.format("%H:%M"))
        .map_or_else(|_| String::new(), |s| s.to_string())
}
fn date_at(at: i64) -> Date {
    glib::DateTime::from_unix_local(at / 1000).map_or((2000, 1, 1), |t| date_of(&t))
}
/// The UTC date of an all-day event's midnight.
fn utc_date(at: i64) -> Date {
    glib::DateTime::from_unix_utc(at / 1000).map_or((2000, 1, 1), |t| date_of(&t))
}
/// The day the locale starts its weeks on: 0 is Sunday, as glibc counts here.
fn week_start() -> i32 {
    std::process::Command::new("locale")
        .args(["-k", "first_weekday"])
        .output()
        .ok()
        .map_or(0, |o| first_weekday(&String::from_utf8_lossy(&o.stdout)))
}
fn first_weekday(locale: &str) -> i32 {
    locale
        .trim()
        .strip_prefix("first_weekday=")
        .and_then(|n| n.parse::<i32>().ok())
        .map_or(0, |n| (n - 1).rem_euclid(7))
}
fn rgb(color: i64) -> (f64, f64, f64) {
    let c = |shift: i64| ((color >> shift) & 0xff) as f64 / 255.;
    (c(16), c(8), c(0))
}
/// A row of up to three coloured dots, centred.
fn dots(colors: &Rc<RefCell<Vec<i64>>>, size: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_height(size);
    area.set_can_target(false);
    let colors = colors.clone();
    area.set_draw_func(move |_, cr, width, height| {
        const GAP: f64 = 2.;
        let colors = colors.borrow();
        let d = f64::from(height);
        let total = colors.len() as f64 * (d + GAP) - GAP;
        let mut x = (f64::from(width) - total) / 2.;
        for color in colors.iter() {
            let (r, g, b) = rgb(*color);
            cr.set_source_rgb(r, g, b);
            cr.arc(x + d / 2., d / 2., d / 2., 0., std::f64::consts::TAU);
            let _ = cr.fill();
            x += d + GAP;
        }
    });
    area
}

struct DayCell {
    button: gtk::Button,
    number: gtk::Label,
    colors: Rc<RefCell<Vec<i64>>>,
    dots: gtk::DrawingArea,
}

/// The fields of the event being written; the date is the one selected above.
struct Form {
    title: gtk::Entry,
    all_day: gtk::Switch,
    times: gtk::Box,
    start: gtk::Entry,
    end: gtk::Entry,
    days_row: gtk::Box,
    days: gtk::SpinButton,
    calendar_row: gtk::Box,
    calendar: gtk::DropDown,
    delete: gtk::Button,
    cancel: gtk::Button,
    save: gtk::Button,
    note: gtk::Label,
}

pub struct Month {
    pub widget: gtk::Box,
    title: gtk::Label,
    today_button: gtk::Button,
    cells: Vec<DayCell>,
    /// The first day of the month shown, and the day picked.
    shown: Cell<Date>,
    selected: Cell<Date>,
    first: i32,
    events: RefCell<Vec<Event>>,
    calendars: RefCell<Vec<calendar::Calendar>>,
    day_title: gtk::Label,
    add: gtk::Button,
    /// Out of the form, where the add button was on the other side.
    back: gtk::Button,
    pages: gtk::Stack,
    list: gtk::Box,
    scroll: gtk::ScrolledWindow,
    empty: gtk::Label,
    status: gtk::Label,
    form: Form,
    /// The event open in the form; `None` for a new one.
    editing: RefCell<Option<Event>>,
    sync: RefCell<CalendarSync>,
    /// Answers to an older question are dropped.
    asked: Cell<u64>,
}

impl Month {
    pub fn new() -> Rc<Self> {
        let first = week_start();
        let widget = vbox(0);
        widget.add_css_class("center-calendar");

        let title = label("", "calendar-month");
        title.set_hexpand(true);
        title.set_wrap(false);
        let head = hbox(2);
        head.add_css_class("calendar-head");
        head.append(&title);
        let today_button = gtk::Button::with_label("Today");
        today_button.add_css_class("calendar-today");
        today_button.set_valign(gtk::Align::Center);
        head.append(&today_button);
        let mut steps = vec![];
        for (icon, name) in [
            ("go-previous-symbolic", "Previous month"),
            ("go-next-symbolic", "Next month"),
        ] {
            let step = gtk::Button::from_icon_name(icon);
            step.add_css_class("calendar-step");
            step.set_tooltip_text(Some(name));
            step.update_property(&[gtk::accessible::Property::Label(name)]);
            head.append(&step);
            steps.push(step);
        }
        widget.append(&head);

        let grid = gtk::Grid::new();
        grid.set_column_homogeneous(true);
        grid.add_css_class("calendar-grid");
        // Any week will do: 2023-01-01 was a Sunday.
        for i in 0..7 {
            let name = gtk::Label::new(Some(&format((2023, 1, 1 + (first + i) % 7), "%a")));
            name.add_css_class("calendar-weekday");
            grid.attach(&name, i, 0, 1, 1);
        }
        let mut cells = vec![];
        for i in 0..CELLS as i32 {
            let number = gtk::Label::new(None);
            number.add_css_class("calendar-day");
            number.set_halign(gtk::Align::Center);
            let colors: Rc<RefCell<Vec<i64>>> = Rc::default();
            let dots = dots(&colors, 4);
            let inner = vbox(2);
            inner.append(&number);
            inner.append(&dots);
            let button = gtk::Button::new();
            button.add_css_class("calendar-cell");
            button.set_child(Some(&inner));
            grid.attach(&button, i % 7, 1 + i / 7, 1, 1);
            cells.push(DayCell {
                button,
                number,
                colors,
                dots,
            });
        }
        widget.append(&grid);

        // The day: its events, or the event being written.
        let day_title = label("", "calendar-day-title");
        day_title.set_hexpand(true);
        day_title.set_wrap(false);
        day_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let add = gtk::Button::from_icon_name("list-add-symbolic");
        add.add_css_class("calendar-step");
        add.set_tooltip_text(Some("Add an event"));
        add.update_property(&[gtk::accessible::Property::Label("Add an event")]);
        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        back.add_css_class("calendar-step");
        back.set_tooltip_text(Some("Back"));
        back.update_property(&[gtk::accessible::Property::Label("Back")]);
        back.set_visible(false);
        let day_head = hbox(2);
        day_head.add_css_class("calendar-day-head");
        day_head.append(&back);
        day_head.append(&day_title);
        day_head.append(&add);
        widget.append(&day_head);

        let list = vbox(2);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(AGENDA_HEIGHT)
            .child(&list)
            .build();
        let empty = caption("Nothing on this day");
        empty.add_css_class("calendar-empty");
        let status = caption("");
        status.add_css_class("calendar-status");
        status.set_visible(false);
        let agenda = vbox(0);
        agenda.append(&scroll);
        agenda.append(&empty);
        agenda.append(&status);

        let (form_box, form) = Self::form();
        let pages = gtk::Stack::new();
        pages.set_vhomogeneous(false);
        pages.set_interpolate_size(true);
        pages.set_transition_type(gtk::StackTransitionType::Crossfade);
        pages.set_transition_duration(140);
        pages.add_named(&agenda, Some("agenda"));
        pages.add_named(&form_box, Some("form"));
        widget.append(&pages);

        let now = today();
        let this = Rc::new(Self {
            widget,
            title,
            today_button,
            cells,
            shown: Cell::new((now.0, now.1, 1)),
            selected: Cell::new(now),
            first,
            events: RefCell::default(),
            calendars: RefCell::default(),
            day_title,
            add,
            back,
            pages,
            list,
            scroll,
            empty,
            status,
            form,
            editing: RefCell::default(),
            sync: RefCell::default(),
            asked: Cell::new(0),
        });
        for (i, cell) in this.cells.iter().enumerate() {
            let t = Rc::downgrade(&this);
            cell.button.connect_clicked(move |_| {
                if let Some(t) = t.upgrade() {
                    let day = t.cell_date(i);
                    t.select(day);
                }
            });
        }
        for (step, by) in steps.iter().zip([-1, 1]) {
            let t = Rc::downgrade(&this);
            step.connect_clicked(move |_| {
                if let Some(t) = t.upgrade() {
                    let (y, m, _) = t.shown.get();
                    let m0 = y * 12 + (m - 1) + by;
                    t.show_month((m0.div_euclid(12), m0.rem_euclid(12) + 1, 1));
                }
            });
        }
        let t = Rc::downgrade(&this);
        this.today_button.connect_clicked(move |_| {
            if let Some(t) = t.upgrade() {
                t.select(today());
            }
        });
        let t = Rc::downgrade(&this);
        this.add.connect_clicked(move |_| {
            if let Some(t) = t.upgrade() {
                t.open(None);
            }
        });
        let t = Rc::downgrade(&this);
        this.back.connect_clicked(move |_| {
            if let Some(t) = t.upgrade() {
                t.close_form();
            }
        });
        // Escape leaves the form first, and the panel only after that.
        let keys = gtk::EventControllerKey::new();
        let t = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, _| match t.upgrade() {
            Some(t) if key == gtk::gdk::Key::Escape && t.back.is_visible() => {
                t.close_form();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        this.widget.add_controller(keys);
        this.connect_form();
        this.render();
        this
    }

    fn form() -> (gtk::Box, Form) {
        let row = |name: &str, field: &gtk::Widget| {
            let text = label(name, "calendar-field");
            text.set_hexpand(true);
            text.set_valign(gtk::Align::Center);
            let r = hbox(8);
            r.append(&text);
            r.append(field);
            r
        };
        let title = gtk::Entry::new();
        title.set_placeholder_text(Some("Add a title"));
        title.update_property(&[gtk::accessible::Property::Label("Title")]);
        let all_day = gtk::Switch::new();
        all_day.set_valign(gtk::Align::Center);
        all_day.update_property(&[gtk::accessible::Property::Label("All day")]);
        let time = |name: &str| {
            let e = gtk::Entry::new();
            e.set_width_chars(5);
            e.set_max_width_chars(5);
            e.set_max_length(8);
            gtk::prelude::EditableExt::set_alignment(&e, 0.5);
            e.update_property(&[gtk::accessible::Property::Label(name)]);
            e
        };
        let (start, end) = (time("Starts"), time("Ends"));
        let to = caption("to");
        to.set_valign(gtk::Align::Center);
        let span = hbox(6);
        span.append(&start);
        span.append(&to);
        span.append(&end);
        let times = row("Time", span.upcast_ref());
        let days = gtk::SpinButton::with_range(1., 90., 1.);
        days.set_valign(gtk::Align::Center);
        days.update_property(&[gtk::accessible::Property::Label("Number of days")]);
        let days_row = row("Days", days.upcast_ref());
        let calendar = gtk::DropDown::from_strings(&[]);
        calendar.set_valign(gtk::Align::Center);
        calendar.set_hexpand(false);
        calendar.add_css_class("calendar-picker");
        calendar.update_property(&[gtk::accessible::Property::Label("Calendar")]);
        let calendar_row = row("Calendar", calendar.upcast_ref());
        let note = caption("");
        note.set_visible(false);

        let delete = gtk::Button::with_label("Delete");
        delete.add_css_class("flat");
        delete.add_css_class("calendar-delete");
        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("flat");
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        let gap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        gap.set_hexpand(true);
        let actions = hbox(6);
        actions.add_css_class("calendar-actions");
        actions.append(&delete);
        actions.append(&gap);
        actions.append(&cancel);
        actions.append(&save);

        let body = vbox(8);
        body.add_css_class("calendar-form");
        body.append(&title);
        body.append(&row("All day", all_day.upcast_ref()));
        body.append(&times);
        body.append(&days_row);
        body.append(&calendar_row);
        body.append(&note);
        body.append(&actions);
        (
            body,
            Form {
                title,
                all_day,
                times,
                start,
                end,
                days_row,
                days,
                calendar_row,
                calendar,
                delete,
                cancel,
                save,
                note,
            },
        )
    }

    fn connect_form(self: &Rc<Self>) {
        let f = &self.form;
        let (times, days_row) = (f.times.clone(), f.days_row.clone());
        f.all_day.connect_active_notify(move |s| {
            times.set_visible(!s.is_active());
            days_row.set_visible(s.is_active());
        });
        for entry in [&f.start, &f.end] {
            entry.connect_changed(|e| e.remove_css_class("error"));
        }
        let t = Rc::downgrade(self);
        let save = move || {
            if let Some(t) = t.upgrade() {
                t.save();
            }
        };
        let s = save.clone();
        f.title.connect_activate(move |_| s());
        f.save.connect_clicked(move |_| save());
        let t = Rc::downgrade(self);
        f.cancel.connect_clicked(move |_| {
            if let Some(t) = t.upgrade() {
                t.close_form();
            }
        });
        let t = Rc::downgrade(self);
        f.delete.connect_clicked(move |_| {
            if let Some(t) = t.upgrade() {
                t.delete();
            }
        });
    }

    fn cell_date(&self, i: usize) -> Date {
        let first = self.shown.get();
        let weekday = local(first).map_or(0, |t| t.day_of_week() % 7);
        let lead = (weekday - self.first).rem_euclid(7);
        shift(first, i as i32 - lead)
    }

    fn select(self: &Rc<Self>, day: Date) {
        self.selected.set(day);
        if (day.0, day.1, 1) != self.shown.get() {
            self.show_month((day.0, day.1, 1));
        } else {
            self.render();
        }
    }
    fn show_month(self: &Rc<Self>, first: Date) {
        self.shown.set(first);
        self.render();
        self.load();
    }
    /// Back to today, on opening: it may have stayed up for days.
    pub fn reset(self: &Rc<Self>) {
        self.close_form();
        let now = today();
        self.selected.set(now);
        self.shown.set((now.0, now.1, 1));
        self.render();
        self.load();
    }
    /// The daemon's calendar state, on every poll.
    pub fn update(self: &Rc<Self>, sync: &CalendarSync, open: bool) {
        let moved = self.sync.borrow().revision != sync.revision;
        *self.sync.borrow_mut() = sync.clone();
        if moved && open {
            self.load();
        } else if open {
            self.show_status();
        }
    }

    fn load(self: &Rc<Self>) {
        let (from, to) = (
            day_of(self.cell_date(0)).start - 86_400_000,
            day_of(self.cell_date(CELLS - 1)).end + 86_400_000,
        );
        let asked = self.asked.get() + 1;
        self.asked.set(asked);
        let t = Rc::downgrade(self);
        call(
            "calendar",
            json!({ "from": from, "to": to }),
            move |reply| {
                let Some(t) = t.upgrade() else { return };
                if t.asked.get() != asked {
                    return;
                }
                if let Ok(data) = reply {
                    let parse = |k: &str| data[k].clone();
                    *t.events.borrow_mut() =
                        serde_json::from_value(parse("events")).unwrap_or_default();
                    *t.calendars.borrow_mut() =
                        serde_json::from_value(parse("calendars")).unwrap_or_default();
                    t.render();
                }
            },
        );
    }

    fn on_day(&self, day: Date) -> Vec<Event> {
        let d = day_of(day);
        self.events
            .borrow()
            .iter()
            .filter(|e| e.on(&d))
            .cloned()
            .collect()
    }

    fn render(self: &Rc<Self>) {
        let (shown, selected, now) = (self.shown.get(), self.selected.get(), today());
        self.title.set_text(&format(shown, "%B %Y"));
        self.today_button
            .set_visible(selected != now || (now.0, now.1, 1) != shown);
        for (i, cell) in self.cells.iter().enumerate() {
            let day = self.cell_date(i);
            let events = self.on_day(day);
            cell.number.set_text(&day.2.to_string());
            for (class, on) in [
                ("today", day == now),
                ("selected", day == selected),
                ("outside", (day.0, day.1) != (shown.0, shown.1)),
            ] {
                if on {
                    cell.button.add_css_class(class);
                } else {
                    cell.button.remove_css_class(class);
                }
            }
            let mut colors = vec![];
            for e in &events {
                if !colors.contains(&e.color) {
                    colors.push(e.color);
                }
            }
            colors.truncate(3);
            if *cell.colors.borrow() != colors {
                *cell.colors.borrow_mut() = colors;
                cell.dots.queue_draw();
            }
            let name = format(day, "%A %-d %B");
            cell.button
                .update_property(&[gtk::accessible::Property::Label(&match events.len() {
                    0 => name,
                    1 => format!("{name}, 1 event"),
                    n => format!("{name}, {n} events"),
                })]);
        }
        self.day_title.set_text(&if selected == now {
            format!("Today, {}", format(selected, "%B %-d"))
        } else {
            format(selected, "%A, %B %-d")
        });
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let events = self.on_day(selected);
        for e in &events {
            self.list.append(&self.row(e, selected));
        }
        self.scroll.set_visible(!events.is_empty());
        self.empty.set_visible(events.is_empty());
        self.show_status();
    }

    /// What keeps the list from being the whole truth, if anything.
    fn show_status(&self) {
        let sync = self.sync.borrow();
        let text = if let Some(e) = &sync.error {
            e.clone()
        } else if sync.pending > 0 {
            "Saved here. It reaches your phone when it connects.".into()
        } else if !sync.synced {
            "Connect your phone to see its calendar.".to_owned()
        } else {
            String::new()
        };
        self.status.set_visible(!text.is_empty());
        self.status.set_text(&text);
    }

    fn row(self: &Rc<Self>, e: &Event, day: Date) -> gtk::Widget {
        let colors = Rc::new(RefCell::new(vec![e.color]));
        let dot = dots(&colors, 8);
        dot.set_content_width(8);
        dot.set_valign(gtk::Align::Start);
        dot.add_css_class("calendar-event-dot");
        let title = label(
            if e.title.is_empty() {
                "No title"
            } else {
                &e.title
            },
            "calendar-event-title",
        );
        title.set_wrap(false);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let mut line = when(e, day);
        if !e.location.is_empty() {
            line = format!("{line} · {}", e.location);
        }
        let time = caption(&line);
        time.set_wrap(false);
        time.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let texts = vbox(0);
        texts.set_hexpand(true);
        texts.append(&title);
        texts.append(&time);
        let inner = hbox(10);
        inner.append(&dot);
        inner.append(&texts);
        if !e.writable {
            inner.add_css_class("calendar-event");
            inner.set_tooltip_text(Some("This calendar is read-only"));
            return inner.upcast();
        }
        let button = gtk::Button::new();
        button.add_css_class("calendar-event");
        button.set_child(Some(&inner));
        let t = Rc::downgrade(self);
        let event = e.clone();
        button.connect_clicked(move |_| {
            if let Some(t) = t.upgrade() {
                t.open(Some(event.clone()));
            }
        });
        button.upcast()
    }

    /// Writes `event`, or a new one on the selected day.
    fn open(self: &Rc<Self>, event: Option<Event>) {
        let f = &self.form;
        let calendars: Vec<_> = self
            .calendars
            .borrow()
            .iter()
            .filter(|c| c.writable)
            .cloned()
            .collect();
        let names: Vec<&str> = calendars.iter().map(|c| c.name.as_str()).collect();
        f.calendar.set_model(Some(&gtk::StringList::new(&names)));
        let wanted = event
            .as_ref()
            .map(|e| e.calendar)
            .or_else(|| calendars.iter().find(|c| c.primary).map(|c| c.id));
        f.calendar.set_selected(
            calendars
                .iter()
                .position(|c| Some(c.id) == wanted)
                .unwrap_or_default() as u32,
        );
        // Moving an occurrence out of its series is not offered: it stays a sibling.
        f.calendar_row
            .set_visible(event.is_none() && calendars.len() > 1);
        f.delete.set_visible(event.is_some());
        f.delete.set_label(match event.as_ref().map(|e| e.repeats) {
            Some(true) => "Delete this one",
            _ => "Delete",
        });
        let repeats = event.as_ref().is_some_and(|e| e.repeats);
        f.note.set_visible(repeats);
        f.note
            .set_text("Repeats. Changes apply to this occurrence only.");
        f.title.remove_css_class("error");
        f.start.remove_css_class("error");
        f.end.remove_css_class("error");
        match &event {
            Some(e) => {
                f.title.set_text(&e.title);
                f.all_day.set_active(e.all_day);
                f.start.set_text(&if e.all_day {
                    "09:00".into()
                } else {
                    clock(e.begin)
                });
                f.end.set_text(&if e.all_day {
                    "10:00".into()
                } else {
                    clock(e.end)
                });
                let days = if e.all_day {
                    ((e.end - e.begin) / 86_400_000).max(1)
                } else {
                    1
                };
                f.days.set_value(days as f64);
                self.selected.set(if e.all_day {
                    utc_date(e.begin)
                } else {
                    date_at(e.begin)
                });
            }
            None => {
                f.title.set_text("");
                f.all_day.set_active(false);
                // The next full hour today; the morning any other day.
                let hour = glib::DateTime::now_local()
                    .ok()
                    .filter(|_| self.selected.get() == today())
                    .map_or(9, |t| (t.hour() + 1).min(23));
                f.start.set_text(&format!("{hour:02}:00"));
                f.end.set_text(&format!("{:02}:00", (hour + 1) % 24));
                f.days.set_value(1.);
            }
        }
        f.times.set_visible(!f.all_day.is_active());
        f.days_row.set_visible(f.all_day.is_active());
        *self.editing.borrow_mut() = event;
        self.add.set_visible(false);
        self.back.set_visible(true);
        self.pages.set_visible_child_name("form");
        self.render();
        f.title.grab_focus();
    }

    fn close_form(self: &Rc<Self>) {
        *self.editing.borrow_mut() = None;
        self.add.set_visible(true);
        self.back.set_visible(false);
        self.pages.set_visible_child_name("agenda");
    }

    fn save(self: &Rc<Self>) {
        let f = &self.form;
        let name = f.title.text().trim().to_owned();
        if name.is_empty() {
            f.title.add_css_class("error");
            f.title.grab_focus();
            return;
        }
        let day = self.selected.get();
        let mut event = self.editing.borrow().clone().unwrap_or_else(|| {
            let calendars = self.calendars.borrow();
            let writable: Vec<_> = calendars.iter().filter(|c| c.writable).collect();
            let picked = writable.get(f.calendar.selected() as usize);
            Event {
                id: format!("{LOCAL}{}", glib::real_time()),
                instance: glib::real_time(),
                writable: true,
                calendar: picked.map_or(0, |c| c.id),
                color: picked.map_or(0xff0a84ff, |c| c.color),
                ..Default::default()
            }
        });
        event.title = name;
        event.all_day = f.all_day.is_active();
        if event.all_day {
            event.begin = utc_ms(day);
            event.end = utc_ms(shift(day, f.days.value_as_int()));
        } else {
            let parsed = [&f.start, &f.end].map(|e| {
                let t = parse_time(&e.text());
                if t.is_none() {
                    e.add_css_class("error");
                }
                t
            });
            let [Some(start), Some(end)] = parsed else {
                return;
            };
            let at = |d: Date, (h, m): (u32, u32)| {
                glib::DateTime::from_local(d.0, d.1, d.2, h as i32, m as i32, 0.)
                    .map_or(0, |t| ms(&t))
            };
            // Days a stored event already spans stay; an end before the start
            // is the next morning.
            let span = self
                .editing
                .borrow()
                .as_ref()
                .filter(|e| !e.all_day)
                .map_or(0, |e| {
                    (local(date_at(e.end)).map_or(0, |t| ms(&t))
                        - local(date_at(e.begin)).map_or(0, |t| ms(&t)))
                    .max(0)
                        / 86_400_000
                }) as i32;
            let span = if span == 0 && end <= start { 1 } else { span };
            event.begin = at(day, start);
            event.end = at(shift(day, span), end);
        }
        self.send("calendar_save", event);
    }

    fn delete(self: &Rc<Self>) {
        if let Some(event) = self.editing.borrow().clone() {
            self.send("calendar_delete", event);
        }
    }

    fn send(self: &Rc<Self>, op: &str, event: Event) {
        self.close_form();
        // On screen before the daemon answers.
        let same = |e: &Event| e.id == event.id && e.instance == event.instance;
        self.events.borrow_mut().retain(|e| !same(e));
        if op == "calendar_save" {
            self.events.borrow_mut().push(event.clone());
            calendar::sort(&mut self.events.borrow_mut());
        }
        self.render();
        let t = Rc::downgrade(self);
        call(op, json!({ "event": event }), move |reply| {
            if let (Err(e), Some(t)) = (reply, t.upgrade()) {
                t.status.set_text(&e);
                t.status.set_visible(true);
            }
        });
    }
}

/// The line under an event's title, for the list of `day`.
fn when(e: &Event, day: Date) -> String {
    if e.all_day {
        let days = (e.end - e.begin) / 86_400_000;
        return if days > 1 {
            format!("All day, until {}", format(utc_date(e.end - 1), "%B %-d"))
        } else {
            "All day".into()
        };
    }
    match (date_at(e.begin) == day, date_at(e.end - 1) == day) {
        (true, true) => format!("{} – {}", clock(e.begin), clock(e.end)),
        (true, false) => format!("From {}", clock(e.begin)),
        (false, true) => format!("Until {}", clock(e.end)),
        (false, false) => "All day".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_week_starts_where_the_locale_says() {
        assert_eq!(first_weekday("first_weekday=1\n"), 0);
        assert_eq!(first_weekday("first_weekday=2\n"), 1);
        assert_eq!(first_weekday("nonsense"), 0);
    }

    #[test]
    fn a_month_grid_starts_on_the_week_of_the_first() {
        // November 2026 starts on a Sunday; October's last Monday is the 26th.
        let start = |first: i32| {
            let lead = (local((2026, 11, 1)).unwrap().day_of_week() % 7 - first).rem_euclid(7);
            shift((2026, 11, 1), -lead)
        };
        assert_eq!(start(0), (2026, 11, 1));
        assert_eq!(start(1), (2026, 10, 26));
    }
}
