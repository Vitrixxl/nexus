//! Messages: the paired phone's SMS conversations, in a window of its own
//! (`nexus messages`). Conversations on the left, the open one on the right,
//! and a full-window state while the phone is away or refuses. Everything goes
//! through the daemon, like the Phone page. Files and voice messages go out as
//! MMS: the daemon hands them to the phone, which sends them.
use super::super::{button, caption, clear, hbox, install_theme, label, scrolled, spinner, vbox};
use super::phone::{ago, call, clear_list, spawn_sibling};
use gtk::{gdk, gio, glib, prelude::*};
use nexus_control::theme;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

const APP_ID: &str = "io.github.vitrixxl.NexusMessages";
/// What one message may carry, as the daemon checks too.
const MAX_FILES: usize = 10;
const MAX_FILE: u64 = 20 << 20;
/// A voice message stops by itself there: an MMS is small.
const MAX_VOICE: u64 = 120;

pub fn run() -> anyhow::Result<()> {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            return window.present();
        }
        if let Some(display) = gtk::gdk::Display::default() {
            install_theme(&display, &theme::load());
        }
        Messages::new(app).window.present();
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

/// Up to two initials, or nothing for a bare number.
fn initials(name: &str) -> String {
    if name.chars().any(|c| c.is_ascii_digit()) {
        return String::new();
    }
    name.split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}
fn avatar(name: &str, size: i32) -> gtk::Widget {
    let text = initials(name);
    let w: gtk::Widget = if text.is_empty() {
        let i = gtk::Image::from_icon_name("avatar-default-symbolic");
        i.set_pixel_size(size / 2);
        i.upcast()
    } else {
        gtk::Label::new(Some(&text)).upcast()
    };
    w.add_css_class("avatar");
    w.set_size_request(size, size);
    w.set_valign(gtk::Align::Center);
    w
}
/// A day and time for a separator between messages far apart.
fn stamp(ms: i64) -> String {
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|d| d.format("%a %-d %b · %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}
/// What a failed request means here, and what to do: a title and a line.
fn trouble(error: &str) -> (&'static str, String) {
    if error.contains("No phone is connected") {
        (
            "Phone not connected",
            "Messages are read from your phone. Connect it in Settings → Phone; this window follows by itself.".into(),
        )
    } else if error.contains("SMS") || error.contains("ermission") {
        (
            "Messages need permission",
            "Open Nexus on the phone and allow reading and sending SMS.".into(),
        )
    } else {
        (
            "Can't read messages",
            format!("{error}. This retries every 10 seconds."),
        )
    }
}
/// How many messages of a conversation stay on disk.
const KEPT: usize = 500;
fn cache_load(name: &str) -> Value {
    std::fs::read(glib::user_cache_dir().join("nexus/messages").join(name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
/// Private to this user, and replaced whole. Losing it only costs a reload.
fn cache_save(name: &str, value: &Value) {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let dir = glib::user_cache_dir().join("nexus/messages");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = std::fs::create_dir_all(&dir)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
        })
        .and_then(|mut f| f.write_all(&serde_json::to_vec(value).unwrap_or_default()))
        .and_then(|()| std::fs::rename(&tmp, dir.join(name)));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}
fn date_of(m: &Value) -> i64 {
    m["date"].as_i64().unwrap_or_default()
}
fn is_chat(m: &Value) -> bool {
    m["id"].as_str().is_some_and(|id| id.starts_with('c'))
}
/// From when the phone has something to tell: the newest message kept, or
/// the oldest still going out, whose state will change.
fn since(all: &[Value]) -> i64 {
    let newest = all.last().map_or(0, date_of);
    all.iter()
        .filter(|m| m["status"] == "sending")
        .map(date_of)
        .fold(newest, i64::min)
}
/// Adds `new` to `kept` by id, in date order. Returns how many were not yet
/// known from the phone's message stores, which `count` is about.
fn merge(kept: &mut Vec<Value>, new: Vec<Value>) -> i64 {
    let mut fresh = 0;
    for m in new {
        if let Some(k) = kept.iter_mut().rev().find(|k| k["id"] == m["id"]) {
            *k = m;
        } else {
            fresh += i64::from(!is_chat(&m));
            kept.push(m);
        }
    }
    kept.sort_by_key(date_of);
    // A chat kept from a notification may reach the SMS store just after.
    let stored: Vec<_> = kept.iter().filter(|m| !is_chat(m)).cloned().collect();
    kept.retain(|m| {
        !is_chat(m)
            || !stored.iter().any(|s| {
                s["body"] == m["body"]
                    && s["body"] != ""
                    && (date_of(s) - date_of(m)).abs() < 90_000
            })
    });
    let extra = kept.len().saturating_sub(KEPT);
    kept.drain(..extra);
    fresh
}
/// Applies the phone's answer. `Some(changed)`, or `None` when the counts
/// don't add up (messages were deleted on the phone): it has to be read again.
fn apply(
    all: &mut Vec<Value>,
    known: i64,
    since: i64,
    new: Vec<Value>,
    count: Option<i64>,
) -> Option<bool> {
    // An app from before the cache sends everything, without ids.
    let Some(count) = count else {
        let changed = *all != new;
        *all = new;
        return Some(changed);
    };
    let mut next = if since > 0 { all.clone() } else { Vec::new() };
    let fresh = merge(&mut next, new);
    if since > 0 && count != known + fresh {
        return None;
    }
    let changed = *all != next;
    *all = next;
    Some(changed)
}
fn mime(path: &Path) -> String {
    let guess = gio::content_type_guess(Some(path), None).0;
    gio::content_type_get_mime_type(&guess).map_or_else(String::new, |m| m.to_string())
}
pub(super) fn minutes(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}
/// An Ogg Opus file's length, from its last page, without decoding it: what
/// voice messages from here are. Others show theirs once played.
fn ogg_duration(path: &Path) -> Option<u64> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 64];
    f.read_exact(&mut head).ok()?;
    let at = head.windows(8).position(|w| w == b"OpusHead")?;
    let skip = u16::from_le_bytes(head.get(at + 10..at + 12)?.try_into().ok()?);
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(64 << 10))).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    let at = tail.windows(4).rposition(|w| w == b"OggS")?;
    let granule = u64::from_le_bytes(tail.get(at + 6..at + 14)?.try_into().ok()?);
    Some((granule.saturating_sub(skip.into()) + 24_000) / 48_000)
}
fn open(path: &Path) {
    gtk::FileLauncher::new(Some(&gio::File::for_path(path))).launch(
        None::<&gtk::Window>,
        None::<&gio::Cancellable>,
        |_| {},
    );
}
fn icon_button(icon: &str, class: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class(class);
    b.set_tooltip_text(Some(tip));
    b.set_valign(gtk::Align::End);
    b
}
/// A picture of `path` fitting `side` pixels, decoded at that size.
// ponytail: decoded at 1x, a little soft on a scaled screen; a paintable
// reporting half its pixels if that shows.
/// In a Fixed, so that it keeps its own size whatever width its column has.
fn picture(path: &Path, side: i32, class: &str) -> Option<gtk::Fixed> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, side, side, true).ok()?;
    let p = gtk::Picture::for_paintable(&gdk::Texture::for_pixbuf(&pixbuf));
    p.set_can_shrink(false);
    p.set_content_fit(gtk::ContentFit::Cover);
    p.set_overflow(gtk::Overflow::Hidden);
    p.add_css_class(class);
    let fixed = gtk::Fixed::new();
    fixed.put(&p, 0., 0.);
    Some(fixed)
}
/// Any file: its icon, name and size, opening in its own application.
fn file_row(path: &Path, out: bool) -> gtk::Widget {
    let b = gtk::Button::new();
    b.add_css_class("attachment-file");
    b.add_css_class(if out { "out" } else { "in" });
    let row = hbox(10);
    let icon = gtk::Image::from_gicon(&gio::content_type_get_symbolic_icon(
        &gio::content_type_guess(Some(path), None).0,
    ));
    icon.set_pixel_size(20);
    row.append(&icon);
    let text = vbox(0);
    let name = label(
        &path.file_name().unwrap_or_default().to_string_lossy(),
        "row-title",
    );
    name.set_wrap(false);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    name.set_max_width_chars(32);
    text.append(&name);
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    text.append(&caption(&glib::format_size(size)));
    row.append(&text);
    b.set_child(Some(&row));
    b.set_tooltip_text(Some("Open"));
    let path = path.to_owned();
    b.connect_clicked(move |_| open(&path));
    b.upcast()
}
/// A voice message: play or pause, how far, how long.
fn voice(path: &Path, out: bool) -> gtk::Widget {
    let row = hbox(10);
    row.add_css_class("voice");
    row.add_css_class(if out { "out" } else { "in" });
    let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
    play.add_css_class("voice-play");
    play.set_tooltip_text(Some("Play"));
    play.set_valign(gtk::Align::Center);
    row.append(&play);
    let bar = gtk::ProgressBar::new();
    bar.add_css_class("voice-bar");
    bar.set_hexpand(true);
    bar.set_valign(gtk::Align::Center);
    row.append(&bar);
    let time = label(
        &ogg_duration(path).map_or_else(|| "Voice".into(), minutes),
        "voice-time",
    );
    time.set_wrap(false);
    row.append(&time);
    // Made on the first play: a stream each holds a decoder.
    let media: Rc<RefCell<Option<gtk::MediaFile>>> = Rc::default();
    let path = path.to_owned();
    play.connect_clicked(move |play| {
        if let Some(m) = &*media.borrow() {
            if m.error().is_some() {
                return open(&path);
            }
            if m.is_ended() {
                m.seek(0);
            }
            return m.set_playing(!m.is_playing());
        }
        let m = gtk::MediaFile::for_filename(&path);
        let (b, t) = (bar.clone(), time.clone());
        m.connect_timestamp_notify(move |m| {
            if m.duration() > 0 {
                b.set_fraction(m.timestamp() as f64 / m.duration() as f64);
            }
            t.set_text(&minutes((m.timestamp() / 1_000_000) as u64));
        });
        let (p, t) = (play.clone(), time.clone());
        m.connect_playing_notify(move |m| {
            p.set_icon_name(if m.is_playing() {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
            p.set_tooltip_text(Some(if m.is_playing() { "Pause" } else { "Play" }));
            if !m.is_playing() && m.is_ended() && m.duration() > 0 {
                t.set_text(&minutes((m.duration() / 1_000_000) as u64));
            }
        });
        let (p, t) = (play.clone(), time.clone());
        m.connect_error_notify(move |_| {
            p.set_icon_name("document-open-symbolic");
            p.set_tooltip_text(Some("Open in another app"));
            t.set_text("Can't play here");
        });
        m.set_playing(true);
        *media.borrow_mut() = Some(m);
    });
    row.upcast()
}
/// What a message carries, by its type.
fn attachment(path: &Path, ct: &str, out: bool) -> gtk::Widget {
    let w = if ct.starts_with("image/") {
        picture(path, 280, "attachment-image").map(|p| {
            p.set_cursor_from_name(Some("pointer"));
            p.set_tooltip_text(Some("Open"));
            let click = gtk::GestureClick::new();
            let path = path.to_owned();
            click.connect_released(move |_, _, _, _| open(&path));
            p.add_controller(click);
            p.upcast()
        })
    } else if ct.starts_with("audio/") {
        Some(voice(path, out))
    } else {
        None
    };
    let w = w.unwrap_or_else(|| file_row(path, out));
    w.set_halign(if out {
        gtk::Align::End
    } else {
        gtk::Align::Start
    });
    w
}

/// A voice message being recorded: one pw-record writes it, another feeds
/// the level meter.
struct Recording {
    encoder: Child,
    meter: Child,
    path: PathBuf,
    started: Instant,
    level: Arc<AtomicU32>,
}
#[derive(Clone, PartialEq)]
enum Sending {
    Now,
    Done,
    Failed(String),
}
/// A message sent from here, shown until the phone's store has it.
struct Pending {
    id: u64,
    to: String,
    body: String,
    files: Vec<PathBuf>,
    state: Sending,
}

struct Messages {
    window: gtk::ApplicationWindow,
    /// "main" once conversations came; "state" before, or while they can't.
    stack: gtk::Stack,
    state_title: gtk::Label,
    state_detail: gtk::Label,
    state_busy: gtk::Spinner,
    settings: gtk::Button,
    threads: gtk::ListBox,
    /// The threads as last drawn.
    listed: RefCell<Value>,
    thread_ids: RefCell<Vec<(i64, String, String)>>,
    /// "empty" until a conversation opens, then "open".
    pane: gtk::Stack,
    conversation: gtk::Box,
    messages: gtk::Box,
    messages_scroll: gtk::ScrolledWindow,
    /// The open conversation's messages, as the phone last gave them.
    all: RefCell<Vec<Value>>,
    /// Messages in the phone's stores at the last look; -1 to read them all.
    count: Cell<i64>,
    /// Showing what is kept because the phone can't be reached.
    stale: Cell<bool>,
    call_button: gtk::Button,
    pending: RefCell<Vec<Pending>>,
    next_pending: Cell<u64>,
    head: gtk::Box,
    to: gtk::Entry,
    text: gtk::TextView,
    placeholder: gtk::Label,
    chips: gtk::Box,
    files: RefCell<Vec<PathBuf>>,
    /// The left button and the field: "edit", or "record" while recording.
    left: gtk::Stack,
    middle: gtk::Stack,
    action: gtk::Button,
    timer: gtk::Label,
    level: gtk::LevelBar,
    recording: RefCell<Option<Recording>>,
    /// When the press that started the recording began: held long, its
    /// release sends.
    pressed: Cell<Option<Instant>>,
    status: gtk::Label,
    /// The open conversation: its thread, number and name.
    current: RefCell<Option<(i64, String, String)>>,
}
impl Messages {
    fn new(app: &gtk::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Messages")
            .default_width(1100)
            .default_height(760)
            .build();
        window.add_css_class("nexus");
        window.add_css_class("messages");
        window.maximize();
        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(150);
        window.set_child(Some(&stack));

        // Away, refused, or loading: one message across the window.
        let state = vbox(10);
        state.add_css_class("messages-state");
        state.set_valign(gtk::Align::Center);
        state.set_halign(gtk::Align::Center);
        let glyph = gtk::Image::from_icon_name("phone-symbolic");
        glyph.set_pixel_size(40);
        glyph.add_css_class("muted");
        state.append(&glyph);
        let state_busy = spinner();
        state.append(&state_busy);
        let state_title = label("Loading conversations…", "section-title");
        state_title.set_xalign(0.5);
        state.append(&state_title);
        let state_detail = label("", "muted");
        state_detail.set_xalign(0.5);
        state_detail.set_justify(gtk::Justification::Center);
        state_detail.set_max_width_chars(48);
        state.append(&state_detail);
        let settings = button("Open Phone settings");
        settings.set_halign(gtk::Align::Center);
        settings.set_margin_top(8);
        settings.set_visible(false);
        state.append(&settings);
        stack.add_named(&state, Some("state"));

        let panes = hbox(0);
        // The conversations.
        let side = vbox(0);
        side.add_css_class("messages-side");
        side.set_size_request(340, -1);
        side.set_hexpand(false);
        let top = hbox(8);
        top.add_css_class("messages-top");
        let heading = label("Messages", "title");
        heading.set_hexpand(true);
        top.append(&heading);
        let new = gtk::Button::from_icon_name("document-edit-symbolic");
        new.add_css_class("icon");
        new.set_tooltip_text(Some("New message"));
        new.set_valign(gtk::Align::Center);
        top.append(&new);
        side.append(&top);
        let threads = gtk::ListBox::new();
        threads.add_css_class("threads");
        threads.set_selection_mode(gtk::SelectionMode::Single);
        threads.set_activate_on_single_click(true);
        side.append(&scrolled(&threads));
        panes.append(&side);

        // The open conversation.
        let pane = gtk::Stack::new();
        pane.set_hexpand(true);
        let empty = vbox(6);
        empty.set_valign(gtk::Align::Center);
        let pick = label("Pick a conversation", "section-title");
        pick.set_xalign(0.5);
        empty.append(&pick);
        let or = caption("or start a new one with the pencil");
        or.set_xalign(0.5);
        empty.append(&or);
        pane.add_named(&empty, Some("empty"));
        let conversation = vbox(0);
        conversation.add_css_class("conversation");
        let head = hbox(12);
        head.add_css_class("conversation-head");
        conversation.append(&head);
        let call_button = icon_button(
            "call-start-symbolic",
            "compose-round",
            "Call from your phone",
        );
        call_button.set_hexpand(true);
        call_button.set_halign(gtk::Align::End);
        call_button.set_valign(gtk::Align::Center);
        let to = gtk::Entry::new();
        to.set_placeholder_text(Some("To: phone number"));
        to.set_input_purpose(gtk::InputPurpose::Phone);
        to.add_css_class("conversation-to");
        to.set_visible(false);
        conversation.append(&to);
        let messages = vbox(0);
        messages.add_css_class("bubbles");
        messages.set_valign(gtk::Align::End);
        let messages_scroll = scrolled(&messages);
        messages_scroll.add_css_class("bubbles-scroll");
        conversation.append(&messages_scroll);
        let status = label("", "error");
        status.add_css_class("caption");
        status.add_css_class("compose-status");
        status.set_visible(false);
        conversation.append(&status);

        // The composer: attach, the field, then record or send.
        let compose = hbox(8);
        compose.add_css_class("composer");
        let left = gtk::Stack::new();
        left.set_valign(gtk::Align::End);
        let attach = icon_button("mail-attachment-symbolic", "compose-round", "Attach files");
        left.add_named(&attach, Some("edit"));
        let cancel = icon_button(
            "user-trash-symbolic",
            "compose-round",
            "Discard recording (Esc)",
        );
        left.add_named(&cancel, Some("record"));
        compose.append(&left);
        let middle = gtk::Stack::new();
        middle.set_hexpand(true);
        middle.set_vhomogeneous(false);
        middle.set_interpolate_size(true);
        let field = vbox(0);
        field.add_css_class("compose-field");
        field.set_valign(gtk::Align::End);
        let chips = hbox(8);
        chips.add_css_class("compose-chips");
        chips.set_visible(false);
        field.append(&chips);
        let text = gtk::TextView::new();
        text.add_css_class("compose-text");
        text.set_wrap_mode(gtk::WrapMode::WordChar);
        text.set_accepts_tab(false);
        text.set_top_margin(10);
        text.set_bottom_margin(10);
        text.set_left_margin(14);
        text.set_right_margin(14);
        let text_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(168)
            .child(&text)
            .build();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&text_scroll));
        let placeholder = label("Message", "compose-placeholder");
        placeholder.set_wrap(false);
        placeholder.set_can_target(false);
        placeholder.set_valign(gtk::Align::Center);
        placeholder.set_halign(gtk::Align::Start);
        placeholder.set_margin_start(14);
        overlay.add_overlay(&placeholder);
        field.append(&overlay);
        middle.add_named(&field, Some("edit"));
        let rec = hbox(10);
        rec.add_css_class("compose-recording");
        rec.set_valign(gtk::Align::End);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("rec-dot");
        dot.set_valign(gtk::Align::Center);
        rec.append(&dot);
        let timer = label("0:00", "rec-time");
        timer.set_wrap(false);
        rec.append(&timer);
        let level = gtk::LevelBar::new();
        level.add_css_class("rec-level");
        level.set_size_request(200, -1);
        level.set_valign(gtk::Align::Center);
        level.set_mode(gtk::LevelBarMode::Discrete);
        level.set_max_value(32.);
        rec.append(&level);
        let hint = caption("Release or click send");
        hint.set_wrap(false);
        hint.set_hexpand(true);
        hint.set_xalign(1.);
        rec.append(&hint);
        middle.add_named(&rec, Some("record"));
        compose.append(&middle);
        let action = icon_button(
            "audio-input-microphone-symbolic",
            "compose-round",
            "Record a voice message",
        );
        compose.append(&action);
        conversation.append(&compose);
        pane.add_named(&conversation, Some("open"));
        panes.append(&pane);
        stack.add_named(&panes, Some("main"));
        stack.set_visible_child_name("state");

        let m = Rc::new(Self {
            window,
            stack,
            state_title,
            state_detail,
            state_busy,
            settings,
            threads,
            listed: RefCell::default(),
            thread_ids: RefCell::default(),
            pane,
            conversation,
            messages,
            messages_scroll,
            all: RefCell::default(),
            count: Cell::new(-1),
            stale: Cell::new(false),
            call_button,
            pending: RefCell::default(),
            next_pending: Cell::new(0),
            head,
            to,
            text,
            placeholder,
            chips,
            files: RefCell::default(),
            left,
            middle,
            action,
            timer,
            level,
            recording: RefCell::default(),
            pressed: Cell::new(None),
            status,
            current: RefCell::default(),
        });

        let p = m.clone();
        m.call_button.connect_clicked(move |_| p.dial());
        m.settings.connect_clicked(|_| {
            let _ = spawn_sibling("nexus", &["phone"]);
        });
        let p = m.clone();
        m.threads.connect_row_activated(move |_, row| {
            let Some(t) = p.thread_ids.borrow().get(row.index() as usize).cloned() else {
                return;
            };
            p.to.set_visible(false);
            p.set_head(&t.2, &t.1);
            // What is kept shows at once; only what's new comes from the phone.
            let mut kept = cache_load(&format!("t{}.json", t.0));
            p.count.set(kept["count"].as_i64().unwrap_or(-1));
            *p.all.borrow_mut() = match kept["messages"].take() {
                Value::Array(all) => all,
                _ => Vec::new(),
            };
            *p.current.borrow_mut() = Some(t);
            p.draw();
            p.report(Ok(()));
            p.pane.set_visible_child_name("open");
            p.load_messages();
            p.text.grab_focus();
        });
        let p = m.clone();
        new.connect_clicked(move |_| {
            *p.current.borrow_mut() = None;
            p.threads.unselect_all();
            p.set_head("New message", "");
            p.all.borrow_mut().clear();
            p.count.set(-1);
            p.report(Ok(()));
            p.to.set_text("");
            p.to.set_visible(true);
            p.draw();
            p.pane.set_visible_child_name("open");
            p.to.grab_focus();
        });
        let p = m.clone();
        m.to.connect_activate(move |_| {
            p.text.grab_focus();
        });
        let p = m.clone();
        m.to.connect_changed(move |_| p.draw());

        // Enter sends, Shift+Enter starts a new line.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let p = m.clone();
        keys.connect_key_pressed(move |_, key, _, mods| {
            if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter)
                && !mods.contains(gdk::ModifierType::SHIFT_MASK)
            {
                p.send();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        m.text.add_controller(keys);
        let p = m.clone();
        m.text.buffer().connect_changed(move |_| p.update_action());
        // Esc throws a recording away.
        let keys = gtk::EventControllerKey::new();
        let p = m.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape && p.recording.borrow().is_some() {
                p.stop_recording(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        m.window.add_controller(keys);

        let p = m.clone();
        attach.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Attach files")
                .modal(true)
                .build();
            let p = p.clone();
            dialog.open_multiple(
                Some(&p.window.clone()),
                None::<&gio::Cancellable>,
                move |r| {
                    if let Ok(list) = r {
                        let files = (0..list.n_items())
                            .filter_map(|i| list.item(i).and_downcast::<gio::File>()?.path());
                        p.add_files(files.collect());
                    }
                },
            );
        });
        let p = m.clone();
        cancel.connect_clicked(move |_| p.stop_recording(false));
        // Held, the mic records until released; clicked, until clicked again.
        let press = gtk::GestureClick::new();
        press.set_propagation_phase(gtk::PropagationPhase::Capture);
        let p = m.clone();
        press.connect_pressed(move |_, _, _, _| {
            if p.recording.borrow().is_none() && !p.has_content() && p.start_recording() {
                p.pressed.set(Some(Instant::now()));
            }
        });
        m.action.add_controller(press);
        let p = m.clone();
        m.action.connect_clicked(move |_| match p.pressed.take() {
            Some(at) if at.elapsed() < Duration::from_millis(400) => {}
            Some(_) => p.stop_recording(true),
            None if p.recording.borrow().is_some() => p.stop_recording(true),
            None if p.has_content() => p.send(),
            None => {
                p.start_recording();
            }
        });

        // Files dropped on the conversation are attached.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let c = m.conversation.clone();
        drop.connect_enter(move |_, _, _| {
            c.add_css_class("drop");
            gdk::DragAction::COPY
        });
        let c = m.conversation.clone();
        drop.connect_leave(move |_| c.remove_css_class("drop"));
        let p = m.clone();
        drop.connect_drop(move |_, value, _, _| {
            p.conversation.remove_css_class("drop");
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            p.add_files(list.files().iter().filter_map(|f| f.path()).collect());
            true
        });
        m.conversation.add_controller(drop);
        m.update_action();
        // At the bottom, the view stays there as pictures load and notes change.
        let adj = m.messages_scroll.vadjustment();
        let stick = Rc::new(Cell::new(true));
        let s = stick.clone();
        adj.connect_value_changed(move |a| s.set(a.value() + a.page_size() >= a.upper() - 4.));
        adj.connect_changed(move |a| {
            if stick.get() {
                a.set_value(a.upper());
            }
        });

        // Conversations kept from last time, while the phone is asked.
        let kept = cache_load("threads.json");
        if !kept.is_null() {
            m.show_threads(kept);
        }
        // New messages arrive on the phone: look again now and then.
        m.refresh();
        let w = Rc::downgrade(&m);
        glib::timeout_add_seconds_local(10, move || match w.upgrade() {
            Some(p) => {
                p.refresh();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        m
    }
    fn set_head(&self, name: &str, address: &str) {
        clear(&self.head);
        let title = if name.is_empty() { address } else { name };
        self.head.append(&avatar(title, 36));
        let text = vbox(1);
        text.set_valign(gtk::Align::Center);
        text.append(&label(title, "row-title"));
        if !address.is_empty() && address != title {
            text.append(&caption(address));
        }
        self.head.append(&text);
        self.head.append(&self.call_button);
        self.call_button
            .set_visible(!address.is_empty() || name == "New message");
    }
    /// Calls the open conversation's number, in the call window.
    fn dial(&self) {
        let opened = self
            .recipient()
            .and_then(|to| spawn_sibling("nexus", &["call", "dial", &to]));
        self.report(opened);
    }
    fn refresh(self: &Rc<Self>) {
        self.load_threads();
        self.load_messages();
    }
    /// A failed send, under the composer.
    fn report(&self, r: Result<(), String>) {
        let text = r.err().unwrap_or_default();
        self.status.set_text(&text);
        self.status.set_visible(!text.is_empty());
    }
    /// The whole window says why there is nothing to show.
    fn trouble(&self, error: &str) {
        let (title, detail) = trouble(error);
        self.state_busy.set_visible(false);
        self.state_title.set_text(title);
        self.state_detail.set_text(&detail);
        self.state_detail.set_visible(true);
        self.settings.set_visible(title == "Phone not connected");
        self.stack.set_visible_child_name("state");
    }

    /// Where what is written goes: the open conversation's number, or the one typed.
    fn recipient(&self) -> Result<String, String> {
        match &*self.current.borrow() {
            Some((_, address, _)) if !address.is_empty() => Ok(address.clone()),
            // Chats kept from notifications come without a number.
            Some(_) => Err("This chat has no number: answer it from the phone".into()),
            None => match self.to.text().trim() {
                "" => Err("Enter a phone number first".into()),
                to => Ok(to.to_owned()),
            },
        }
    }
    fn body(&self) -> String {
        let b = self.text.buffer();
        b.text(&b.start_iter(), &b.end_iter(), false).to_string()
    }
    fn has_content(&self) -> bool {
        !self.body().trim().is_empty() || !self.files.borrow().is_empty()
    }
    /// The right button: the mic while there is nothing to send, else send.
    fn update_action(&self) {
        let recording = self.recording.borrow().is_some();
        let send = recording || self.has_content();
        self.placeholder
            .set_visible(self.text.buffer().char_count() == 0);
        let a = &self.action;
        a.set_icon_name(if send {
            "go-up-symbolic"
        } else {
            "audio-input-microphone-symbolic"
        });
        a.set_tooltip_text(Some(match (recording, send) {
            (true, _) => "Send voice message",
            (_, true) => "Send (Enter)",
            _ => "Record a voice message: click, or hold and release",
        }));
        if send {
            a.add_css_class("send");
        } else {
            a.remove_css_class("send");
        }
        let mode = if recording { "record" } else { "edit" };
        self.left.set_visible_child_name(mode);
        self.middle.set_visible_child_name(mode);
    }
    fn add_files(self: &Rc<Self>, paths: Vec<PathBuf>) {
        let mut files = self.files.borrow_mut();
        let mut problem = None;
        for path in paths.into_iter().filter(|p| p.is_file()) {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if files.len() >= MAX_FILES {
                problem = Some(format!("Up to {MAX_FILES} attachments per message"));
                break;
            }
            if std::fs::metadata(&path).map_or(0, |m| m.len()) > MAX_FILE {
                problem = Some(format!("{name} is over {} MB", MAX_FILE >> 20));
                continue;
            }
            if !files.contains(&path) {
                files.push(path);
            }
        }
        drop(files);
        self.report(problem.map_or(Ok(()), Err));
        self.draw_chips();
        self.text.grab_focus();
    }
    /// What is attached, above the text: a thumbnail, or the name and size.
    fn draw_chips(self: &Rc<Self>) {
        clear(&self.chips);
        let files = self.files.borrow();
        self.chips.set_visible(!files.is_empty());
        for (i, path) in files.iter().enumerate() {
            let chip = gtk::Overlay::new();
            let image = mime(path)
                .starts_with("image/")
                .then(|| picture(path, 64, "chip-image"))
                .flatten();
            match image {
                Some(p) => {
                    if let Some(c) = p.first_child() {
                        c.set_size_request(64, 64);
                    }
                    chip.set_child(Some(&p));
                }
                None => {
                    let row = file_row(path, false);
                    row.add_css_class("chip-file");
                    row.set_can_target(false);
                    row.set_valign(gtk::Align::Center);
                    chip.set_child(Some(&row));
                }
            }
            let remove = gtk::Button::from_icon_name("window-close-symbolic");
            remove.add_css_class("chip-remove");
            remove.set_tooltip_text(Some("Remove"));
            remove.set_halign(gtk::Align::End);
            remove.set_valign(gtk::Align::Start);
            let p = self.clone();
            remove.connect_clicked(move |_| {
                p.files.borrow_mut().remove(i);
                p.draw_chips();
            });
            chip.add_overlay(&remove);
            self.chips.append(&chip);
        }
        drop(files);
        self.update_action();
    }
    fn send(self: &Rc<Self>) {
        if self.recording.borrow().is_some() {
            return self.stop_recording(true);
        }
        let body = self.body().trim_end().to_owned();
        if body.trim().is_empty() && self.files.borrow().is_empty() {
            return;
        }
        let to = match self.recipient() {
            Ok(to) => to,
            Err(e) => return self.report(Err(e)),
        };
        self.text.buffer().set_text("");
        let files = self.files.take();
        self.draw_chips();
        self.queue(to, body, files);
    }
    fn queue(self: &Rc<Self>, to: String, body: String, files: Vec<PathBuf>) {
        let id = self.next_pending.get();
        self.next_pending.set(id + 1);
        self.pending.borrow_mut().push(Pending {
            id,
            to,
            body,
            files,
            state: Sending::Now,
        });
        self.dispatch(id);
    }
    /// Sends the pending message `id`: SMS, or MMS when it carries files.
    fn dispatch(self: &Rc<Self>, id: u64) {
        let (op, args) = {
            let mut pending = self.pending.borrow_mut();
            let Some(m) = pending.iter_mut().find(|m| m.id == id) else {
                return;
            };
            m.state = Sending::Now;
            if m.files.is_empty() {
                ("sms_send", json!({ "to": m.to, "body": m.body }))
            } else {
                (
                    "mms_send",
                    json!({ "to": m.to, "body": m.body, "files": m.files }),
                )
            }
        };
        self.report(Ok(()));
        self.draw();
        let p = self.clone();
        call(op, args, move |r| {
            let ok = r.is_ok();
            if let Some(m) = p.pending.borrow_mut().iter_mut().find(|m| m.id == id) {
                m.state = r.map_or_else(Sending::Failed, |_| Sending::Done);
            }
            p.draw();
            if ok {
                // The phone writes the message to its store as it sends it.
                glib::timeout_add_local_once(Duration::from_millis(800), move || p.refresh());
            }
        });
    }
    fn discard(self: &Rc<Self>, id: u64) {
        let mut pending = self.pending.borrow_mut();
        for m in pending.iter().filter(|m| m.id == id) {
            forget(&m.files);
        }
        pending.retain(|m| m.id != id);
        drop(pending);
        self.draw();
    }

    /// Starts a voice message; says whether it did.
    fn start_recording(self: &Rc<Self>) -> bool {
        if let Err(e) = self.recipient() {
            self.report(Err(e));
            return false;
        }
        let dir = glib::user_cache_dir().join("nexus/messages/voice");
        let path = dir.join(format!(
            "Voice {}.oga",
            glib::DateTime::now_local()
                .and_then(|d| d.format("%Y-%m-%d %H.%M.%S"))
                .map(|s| s.to_string())
                .unwrap_or_default()
        ));
        // What a window that died mid-recording left, a day on.
        for old in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if old
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(86_400))
            {
                let _ = std::fs::remove_file(old.path());
            }
        }
        let pw = |args: &[&str]| {
            use std::os::unix::process::CommandExt;
            let mut command = Command::new("pw-record");
            // The mic is let go if this window dies: SIGINT also ends the file.
            // SAFETY: only an async-signal-safe call, in the child before exec.
            unsafe {
                command.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT);
                    Ok(())
                });
            }
            command
                .args([
                    "--channels",
                    "1",
                    "-P",
                    "{ media.role = \"Communication\" }",
                ])
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
        };
        let started = std::fs::create_dir_all(&dir).and_then(|()| {
            let encoder = pw(&[
                "--rate",
                "16000",
                "--format",
                "opus",
                &path.to_string_lossy(),
            ])?;
            match pw(&["--rate", "8000", "--format", "s16", "--raw", "-"]) {
                Ok(meter) => Ok((encoder, meter)),
                Err(e) => {
                    stop(encoder);
                    Err(e)
                }
            }
        });
        let (encoder, mut meter) = match started {
            Ok(both) => both,
            Err(e) => {
                self.report(Err(format!(
                    "Can't record: {e}. Voice messages need PipeWire's pw-record."
                )));
                return false;
            }
        };
        let level = Arc::new(AtomicU32::new(0));
        if let Some(mut out) = meter.stdout.take() {
            let level = level.clone();
            // Ends with the meter's output.
            std::thread::spawn(move || {
                let mut buf = [0u8; 400];
                while out.read_exact(&mut buf).is_ok() {
                    let peak = buf
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|s| i16::from_le_bytes(*s).unsigned_abs())
                        .max()
                        .unwrap_or(0);
                    level.store(peak.into(), Ordering::Relaxed);
                }
            });
        }
        let started = Instant::now();
        *self.recording.borrow_mut() = Some(Recording {
            encoder,
            meter,
            path,
            started,
            level,
        });
        self.report(Ok(()));
        self.timer.set_text("0:00");
        self.level.set_value(0.);
        self.update_action();
        let w = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(80), move || {
            let Some(p) = w.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let (secs, peak) = match &*p.recording.borrow() {
                Some(r) if r.started == started => (
                    r.started.elapsed().as_secs(),
                    r.level.load(Ordering::Relaxed),
                ),
                _ => return glib::ControlFlow::Break,
            };
            p.timer.set_text(&minutes(secs));
            p.level
                .set_value((f64::from(peak) / 32768.).sqrt() * p.level.max_value());
            if secs >= MAX_VOICE {
                p.stop_recording(true);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        true
    }
    /// Ends the recording, and sends it or throws it away.
    fn stop_recording(self: &Rc<Self>, send: bool) {
        self.pressed.set(None);
        let Some(r) = self.recording.take() else {
            return;
        };
        self.update_action();
        let long_enough = r.started.elapsed() >= Duration::from_millis(700);
        let to = self.recipient();
        let Recording {
            encoder,
            meter,
            path,
            ..
        } = r;
        let p = self.clone();
        glib::spawn_future_local(async move {
            // pw-record closes the file properly on SIGINT.
            let _ = gio::spawn_blocking(move || {
                stop(meter);
                stop(encoder);
            })
            .await;
            match to {
                Ok(to) if send && long_enough && path.is_file() => {
                    p.queue(to, String::new(), vec![path])
                }
                _ => {
                    let _ = std::fs::remove_file(&path);
                    if send && !long_enough {
                        p.report(Err("Hold the mic a little longer to record".into()));
                    }
                }
            }
        });
    }

    fn load_threads(self: &Rc<Self>) {
        let p = self.clone();
        let stamp = self.listed.borrow()["stamp"].clone();
        call("sms_threads", json!({ "stamp": stamp }), move |r| {
            let data = match r {
                Ok(d) => d,
                // What is kept stays readable while the phone is away.
                Err(e) if !p.listed.borrow().is_null() => {
                    p.stale.set(true);
                    return p.report(Err(e));
                }
                Err(e) => return p.trouble(&e),
            };
            if p.stale.take() {
                p.report(Ok(()));
            }
            p.stack.set_visible_child_name("main");
            if data["same"] != true && *p.listed.borrow() != data {
                cache_save("threads.json", &data);
                p.show_threads(data);
            }
        });
    }
    fn show_threads(self: &Rc<Self>, data: Value) {
        {
            let p = self;
            p.stack.set_visible_child_name("main");
            clear_list(&p.threads);
            let mut ids = p.thread_ids.borrow_mut();
            ids.clear();
            let open = p.current.borrow().as_ref().map(|c| c.0);
            let list = data["threads"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                let none = caption("No conversations on the phone yet");
                none.add_css_class("empty-state");
                none.set_xalign(0.5);
                p.threads.set_placeholder(Some(&none));
            }
            for t in &list {
                let text = |k: &str| t[k].as_str().unwrap_or_default().to_owned();
                let (address, name) = (text("address"), text("name"));
                let title = if name.is_empty() { &address } else { &name };
                let r = hbox(12);
                r.add_css_class("thread");
                r.append(&avatar(title, 40));
                let lines = vbox(2);
                lines.set_hexpand(true);
                let top = hbox(8);
                let who = label(title, "row-title");
                who.set_wrap(false);
                who.set_hexpand(true);
                who.set_ellipsize(gtk::pango::EllipsizeMode::End);
                top.append(&who);
                if let Some(date) = t["date"].as_i64().filter(|d| *d > 0) {
                    top.append(&caption(&ago(date)));
                }
                lines.append(&top);
                let snippet = caption(
                    &text("snippet")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                snippet.set_wrap(false);
                snippet.set_ellipsize(gtk::pango::EllipsizeMode::End);
                lines.append(&snippet);
                r.append(&lines);
                p.threads.append(&r);
                let thread = t["thread"].as_i64().unwrap_or_default();
                if Some(thread) == open
                    && let Some(row) = p.threads.row_at_index(ids.len() as i32)
                {
                    p.threads.select_row(Some(&row));
                }
                ids.push((thread, address, name));
            }
            drop(ids);
            p.listed.replace(data);
            // Lab only: a conversation open, for captures without a pointer.
            #[cfg(feature = "lab")]
            if std::env::var_os("NEXUS_MESSAGES_OPEN").is_some()
                && p.current.borrow().is_none()
                && let Some(row) = p.threads.row_at_index(0)
            {
                row.activate();
                p.lab_demo();
            }
        }
    }
    /// Lab only: the composer in a given state, for captures without a keyboard.
    #[cfg(feature = "lab")]
    fn lab_demo(self: &Rc<Self>) {
        let var = |k: &str| std::env::var(k).ok();
        if let Some(t) = var("NEXUS_MESSAGES_TEXT") {
            self.text.buffer().set_text(&t.replace("\\n", "\n"));
        }
        if let Some(f) = var("NEXUS_MESSAGES_ATTACH") {
            self.add_files(f.split(':').map(PathBuf::from).collect());
        }
        if var("NEXUS_MESSAGES_SEND").is_some() {
            self.send();
        }
        if var("NEXUS_MESSAGES_DROP").is_some() {
            self.conversation.add_css_class("drop");
        }
        if var("NEXUS_MESSAGES_RECORD").is_some() && self.start_recording() {
            let p = self.clone();
            glib::timeout_add_local_once(Duration::from_secs(7), move || p.stop_recording(false));
        }
    }
    /// The open conversation, redrawn only when it has new messages, so that
    /// reading back up is not cut short.
    fn load_messages(self: &Rc<Self>) {
        let Some((thread, address, name)) = self.current.borrow().clone() else {
            return;
        };
        let name = if name.is_empty() { address } else { name };
        let from = if self.count.get() < 0 {
            0
        } else {
            since(&self.all.borrow())
        };
        let p = self.clone();
        call(
            "sms_list",
            json!({ "thread": thread, "name": name, "since": from }),
            move |r| {
                let mut d = match r {
                    Ok(d) => d,
                    Err(e) => return p.report(Err(e)),
                };
                // Another conversation was opened meanwhile.
                if p.current.borrow().as_ref().map(|c| c.0) != Some(thread) {
                    return;
                }
                let new = match d["messages"].take() {
                    Value::Array(new) => new,
                    _ => Vec::new(),
                };
                let count = d["count"].as_i64();
                let mut all = p.all.borrow_mut();
                let Some(changed) = apply(&mut all, p.count.get(), from, new, count) else {
                    drop(all);
                    p.count.set(-1);
                    return p.load_messages();
                };
                let count = count.unwrap_or(-1);
                if changed || p.count.replace(count) != count {
                    p.count.set(count);
                    cache_save(
                        &format!("t{thread}.json"),
                        &json!({ "count": count, "messages": *all }),
                    );
                }
                drop(all);
                if !changed {
                    return;
                }
                // What was sent from here is in the phone's store by now.
                let mut pending = p.pending.borrow_mut();
                for m in pending.iter().filter(|m| m.state == Sending::Done) {
                    forget(&m.files);
                }
                pending.retain(|m| m.state != Sending::Done);
                drop(pending);
                p.draw();
            },
        );
    }
    /// The conversation: the phone's messages, then those sent from here.
    fn draw(self: &Rc<Self>) {
        clear(&self.messages);
        let mut last = 0;
        let mut side = None;
        // Messages one after another from the same side stay close.
        let group = |out: bool, side: &mut Option<bool>| {
            let g = vbox(3);
            g.set_margin_top(if *side == Some(out) { 3 } else { 10 });
            *side = Some(out);
            g
        };
        for m in self.all.borrow().iter() {
            // A time over the messages that follow an hour of silence.
            let date = m["date"].as_i64().unwrap_or_default();
            if date - last > 3_600_000 {
                let when = caption(&stamp(date));
                when.set_xalign(0.5);
                when.add_css_class("bubble-time");
                self.messages.append(&when);
                side = None;
            }
            last = date;
            let out = m["out"] == true;
            let g = group(out, &mut side);
            for part in m["parts"].as_array().into_iter().flatten() {
                g.append(&self.part(part, out));
            }
            if let Some(body) = m["body"].as_str().filter(|b| !b.is_empty()) {
                g.append(&bubble(body, out));
            }
            match m["status"].as_str() {
                Some("sending") => g.append(&note("Sending…", out)),
                Some("failed") => {
                    let n = note("Not sent", out);
                    n.add_css_class("error");
                    g.append(&n);
                }
                _ => {}
            }
            self.messages.append(&g);
        }
        let to = self.recipient().ok();
        let pending = self.pending.borrow();
        let here: Vec<_> = pending
            .iter()
            .filter(|m| Some(&m.to) == to.as_ref())
            .collect();
        for m in &here {
            let g = group(true, &mut side);
            for f in &m.files {
                g.append(&attachment(f, &mime(f), true));
            }
            if !m.body.is_empty() {
                g.append(&bubble(&m.body, true));
            }
            match &m.state {
                Sending::Now => g.append(&note("Sending…", true)),
                Sending::Done => g.append(&note("Sent", true)),
                Sending::Failed(e) => {
                    g.add_css_class("failed");
                    let row = hbox(10);
                    row.set_halign(gtk::Align::End);
                    let n = note(&format!("Not sent: {e}"), true);
                    n.add_css_class("error");
                    n.set_wrap(true);
                    n.set_max_width_chars(60);
                    row.append(&n);
                    let retry = button("Retry");
                    retry.add_css_class("note-action");
                    let (p, id) = (self.clone(), m.id);
                    retry.connect_clicked(move |_| p.dispatch(id));
                    row.append(&retry);
                    let discard = button("Discard");
                    discard.add_css_class("note-action");
                    let (p, id) = (self.clone(), m.id);
                    discard.connect_clicked(move |_| p.discard(id));
                    row.append(&discard);
                    g.append(&row);
                }
            }
            self.messages.append(&g);
        }
        if self.all.borrow().is_empty() && here.is_empty() && self.current.borrow().is_some() {
            let none = caption("No messages yet");
            none.set_xalign(0.5);
            self.messages.append(&none);
        }
        // Newest at the bottom, in view.
        let adj = self.messages_scroll.vadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper()));
    }
    /// A received attachment, fetched from the phone through the daemon.
    fn part(&self, part: &Value, out: bool) -> gtk::Widget {
        let slot = vbox(0);
        slot.set_halign(if out {
            gtk::Align::End
        } else {
            gtk::Align::Start
        });
        let ct = part["ct"].as_str().unwrap_or_default().to_owned();
        let name = match part["name"].as_str().filter(|n| !n.is_empty()) {
            Some(n) => n.to_owned(),
            None => format!(
                "attachment.{}",
                ct.split('/')
                    .nth(1)
                    .unwrap_or("bin")
                    .trim_start_matches("x-")
            ),
        };
        let wait = caption("Loading attachment…");
        wait.add_css_class("attachment-loading");
        slot.append(&wait);
        let s = slot.clone();
        call(
            "mms_part",
            json!({ "part": part["id"], "name": name }),
            move |r| {
                clear(&s);
                s.append(&match r {
                    Ok(d) => {
                        attachment(Path::new(d["path"].as_str().unwrap_or_default()), &ct, out)
                    }
                    Err(e) => {
                        let c = caption(&format!("Attachment unavailable: {e}"));
                        c.add_css_class("attachment-loading");
                        c.upcast()
                    }
                });
            },
        );
        slot.upcast()
    }
}
fn bubble(body: &str, out: bool) -> gtk::Widget {
    let bubble = label(body, "bubble");
    bubble.add_css_class(if out { "out" } else { "in" });
    bubble.set_selectable(true);
    bubble.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    bubble.set_max_width_chars(56);
    bubble.set_halign(if out {
        gtk::Align::End
    } else {
        gtk::Align::Start
    });
    bubble.upcast()
}
/// A line under a message: how its sending goes.
fn note(text: &str, out: bool) -> gtk::Label {
    let n = caption(text);
    n.add_css_class("bubble-note");
    n.set_wrap(false);
    n.set_xalign(if out { 1. } else { 0. });
    n.set_halign(if out {
        gtk::Align::End
    } else {
        gtk::Align::Start
    });
    n
}
/// Ends a pw-record: SIGINT lets it finish the file.
fn stop(mut child: Child) {
    // SAFETY: a plain kill(2) on our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    let _ = child.wait();
}
/// Voice messages recorded here are gone once sent or thrown away.
fn forget(files: &[PathBuf]) {
    let voice = glib::user_cache_dir().join("nexus/messages/voice");
    for f in files.iter().filter(|f| f.starts_with(&voice)) {
        let _ = std::fs::remove_file(f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sms(id: &str, date: i64) -> Value {
        json!({ "id": id, "date": date, "body": id })
    }
    #[test]
    fn only_new_messages_are_added() {
        let mut all = vec![sms("s1", 10_000_000), sms("s2", 20_000_000)];
        // The newest comes again; one is new.
        let new = vec![sms("s2", 20_000_000), sms("s3", 30_000_000)];
        assert_eq!(
            apply(&mut all, 2, 20_000_000, new.clone(), Some(3)),
            Some(true)
        );
        assert_eq!(all.len(), 3);
        // Nothing new: nothing to redraw.
        assert_eq!(
            apply(&mut all, 3, 30_000_000, vec![new[1].clone()], Some(3)),
            Some(false)
        );
    }
    #[test]
    fn a_deletion_on_the_phone_asks_for_everything() {
        let mut all = vec![sms("s1", 10_000_000), sms("s2", 20_000_000)];
        assert_eq!(apply(&mut all, 2, 20_000_000, vec![], Some(1)), None);
        // Reading everything replaces what was kept, and always adds up.
        assert_eq!(
            apply(&mut all, -1, 0, vec![sms("s2", 20_000_000)], Some(1)),
            Some(true)
        );
        assert_eq!(all.len(), 1);
    }
    #[test]
    fn a_message_going_out_is_asked_about_again() {
        let mut all = vec![sms("s1", 10), sms("s2", 20), sms("s3", 30)];
        assert_eq!(since(&all), 30);
        all[1]["status"] = json!("sending");
        assert_eq!(since(&all), 20);
        // Its new state replaces the old one.
        assert_eq!(
            apply(&mut all, 3, 20, vec![sms("s2", 20)], Some(3)),
            Some(true)
        );
        assert!(all[1]["status"].is_null());
    }
    #[test]
    fn a_chat_that_reached_the_store_goes_away_for_good() {
        let chat = json!({ "id": "c21", "date": 21, "body": "hi" });
        let store = json!({ "id": "s1", "date": 20, "body": "hi" });
        let mut all = vec![chat.clone()];
        assert_eq!(merge(&mut all, vec![store.clone()]), 1);
        assert_eq!(all, vec![store.clone()]);
        // Sent again by the phone: nothing moves.
        assert_eq!(apply(&mut all, 1, 20, vec![chat], Some(1)), Some(false));
    }
    #[test]
    fn what_is_kept_is_bounded() {
        let mut all = Vec::new();
        merge(
            &mut all,
            (0..KEPT as i64 + 9)
                .map(|i| sms(&format!("s{i}"), i))
                .collect(),
        );
        assert_eq!(all.len(), KEPT);
        assert_eq!(date_of(&all[0]), 9);
    }
    #[test]
    fn an_older_phone_app_still_works() {
        let mut all = vec![json!({ "date": 1, "body": "a" })];
        let new = vec![json!({ "date": 2, "body": "b" })];
        assert_eq!(apply(&mut all, -1, 0, new.clone(), None), Some(true));
        assert_eq!(all, new);
    }
    #[test]
    fn initials_and_trouble() {
        assert_eq!(initials("Ada Lovelace King"), "AL");
        assert_eq!(initials("+33 6 00 00 00 00"), "");
        assert_eq!(trouble("No phone is connected").0, "Phone not connected");
        assert_eq!(
            trouble("Permission Denial: READ_SMS").0,
            "Messages need permission"
        );
        assert_eq!(minutes(75), "1:15");
    }
    #[test]
    fn voice_length_from_the_last_ogg_page() {
        let mut f = b"OggS\0\x02".to_vec();
        f.resize(28, 0);
        f.extend(b"OpusHead\x01\x01\x38\x01");
        f.resize(64, 0);
        f.extend(b"OggS\0\x04");
        f.extend((7 * 48_000u64 + 312).to_le_bytes());
        f.resize(200, 0);
        let path = std::env::temp_dir().join(format!("nexus-voice-{}.oga", std::process::id()));
        std::fs::write(&path, f).unwrap();
        assert_eq!(ogg_duration(&path), Some(7));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(ogg_duration(Path::new("/nonexistent")), None);
    }
}
