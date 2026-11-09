//! `nexus lock`: a session lock (ext-session-lock) covering every monitor.
//! The compositor keeps the session locked even if this process dies.
//! Each monitor shows a capture of itself that blurs in, under one of the
//! compositions of [`theme::LOCK_STYLES`]: the time, who is locked out, the
//! password field, the battery and keyboard, and the power actions.
//!
//! Every visible control sits in a `gtk::Overlay` overlay, which does not count
//! towards the window's size: the surface must match the monitor exactly, so no
//! composition may ever ask for more than a monitor has.
pub mod pam;
use super::{clear, hbox, label, screenshot::screencopy::Capturer, vbox};
use gtk::{gdk, gio, glib, pango, prelude::*};
use gtk4_session_lock::Instance;
use nexus_control::theme;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::Write,
    rc::Rc,
    time::{Duration, Instant},
};

/// Matches the leaving animations in the theme.
const LEAVE: Duration = Duration::from_millis(150);
/// Locking never waits longer than this for the captures.
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Blur radius in pixels of the quarter-size copy the blurred layer is made from.
const BLUR: f32 = 9.;
/// How often the battery is read again.
const BATTERY_EVERY: u32 = 30;
/// Under either of these (logical pixels), compositions drop what is optional.
const COMPACT: (i32, i32) = (1100, 760);
/// Operation, label, icon and, unless harmless, what to ask first.
const POWER: [(&str, &str, &str, Option<&str>); 3] = [
    ("sleep", "Sleep", "weather-clear-night-symbolic", None),
    (
        "restart",
        "Restart",
        "view-refresh-symbolic",
        Some("Restart now? Unsaved work will be lost."),
    ),
    (
        "shutdown",
        "Shut down",
        "system-shutdown-symbolic",
        Some("Shut down now? Unsaved work will be lost."),
    ),
];

/// The arrangements of `theme::LOCK_STYLES`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Composition {
    Panel,
    Clock,
    Bar,
}
impl Composition {
    /// A missing or unknown `lock_style` must still give a lock screen.
    fn of(id: &str) -> Self {
        match id {
            "clock" => Self::Clock,
            "bar" => Self::Bar,
            _ => Self::Panel,
        }
    }
}

/// `--preview` shows the lock in a plain fullscreen window, without locking
/// anything: Escape closes it, no password is checked, and the power buttons send
/// nothing. `NEXUS_LOCK_PREVIEW_STATE` (checking, wrong, locked, caps, confirm)
/// shows that state in preview without going through PAM, whose failures count.
/// `NEXUS_LOCK_TEST=N` locks for real but unlocks on its own after N seconds, to
/// try the lock without risking a stuck session.
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let preview = args.iter().any(|a| a == "--preview");
    log("start");
    // A preview holds nothing, or a real lock would take it for one already up.
    let _guard = if preview {
        None
    } else {
        match nexus_control::instance_lock("lock") {
            Ok(guard) => Some(guard),
            // Already locked: binding the shortcut and an idle daemon to it is fine.
            Err(e) if lock_is_held(&e) => return Ok(()),
            // Anything else says nothing about the screen: lock it all the same.
            Err(e) => {
                log(&format!("no instance lock: {e:#}"));
                None
            }
        }
    };
    let test = if cfg!(any(test, feature = "lab")) {
        std::env::var("NEXUS_LOCK_TEST")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
    } else {
        None
    };
    glib::set_prgname(Some("io.github.vitrixxl.Nexus.Lock"));
    let app = gtk::Application::builder()
        .application_id("io.github.vitrixxl.Nexus.Lock")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let failed = Rc::new(Cell::new(false));
    let failure = failed.clone();
    app.connect_activate(move |app| {
        let (app, failure) = (app.clone(), failure.clone());
        // The lock lives exactly as long as this task.
        let hold = app.hold();
        glib::spawn_future_local(async move {
            failure.set(!start(&app, preview, test).await);
            drop(hold);
        });
    });
    app.run_with_args::<&str>(&[]);
    if failed.get() {
        anyhow::bail!("The compositor did not lock the session");
    }
    Ok(())
}

/// Whether `instance_lock` failed because another process has the lock.
fn lock_is_held(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
}

/// Never `eprintln!` in here: it panics when stderr can't be written (full disk,
/// dead tty), and a panic in a GTK callback aborts, leaving the session locked for
/// good.
fn log(text: &str) {
    thread_local!(static START: Instant = Instant::now());
    let elapsed = START.with(|s| s.elapsed().as_secs_f64());
    let _ = writeln!(std::io::stderr(), "[lock {elapsed:>7.3}s] {text}");
}

/// Each monitor's pixels by connector name, taken before anything covers them.
async fn captures(display: &gdk::Display, capturer: &Capturer) -> HashMap<String, gdk::Texture> {
    let list = display.monitors();
    let names: Vec<String> = (0..list.n_items())
        .filter_map(|i| list.item(i).and_downcast::<gdk::Monitor>())
        .filter_map(|m| m.connector().map(|c| c.to_string()))
        .collect();
    let frames = glib::future_with_timeout(CAPTURE_TIMEOUT, capturer.capture(names.clone()))
        .await
        .unwrap_or_else(|_| Err("timed out".into()));
    match frames {
        Ok(frames) => names
            .into_iter()
            .zip(frames)
            .map(|(name, frame)| (name, frame.texture().upcast()))
            .collect(),
        Err(e) => {
            log(&format!("capture failed: {e}"));
            HashMap::new()
        }
    }
}

/// Who is locked out, as far as the system says; pixels are RGBA, 256 square.
fn read_person() -> (String, Option<Vec<u8>>) {
    let user = glib::user_name();
    let home = glib::home_dir();
    // ponytail: the default path; AccountsService's D-Bus IconFile if a custom one matters.
    let avatar = [
        std::path::Path::new("/var/lib/AccountsService/icons").join(&user),
        home.join(".face"),
        home.join(".face.icon"),
    ]
    .iter()
    .find_map(|path| read_avatar(path));
    let name = glib::real_name().to_string_lossy().into_owned();
    // GLib's word for an empty GECOS field.
    let name = if name.is_empty() || name == "Unknown" {
        user.to_string_lossy().into_owned()
    } else {
        name
    };
    (name, avatar)
}
/// None for anything missing or undecodable. The image crate instead of GTK's
/// loaders: a bad file in the home folder must never take the lock down.
fn read_avatar(path: &std::path::Path) -> Option<Vec<u8>> {
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let image = image.resize_to_fill(256, 256, image::imageops::FilterType::Triangle);
    Some(image.to_rgba8().into_raw())
}
/// The Adwaita icon for a battery at `level` percent.
fn battery_icon(level: u8, charging: bool) -> String {
    let level = level.min(100) / 10 * 10;
    let state = match (charging, level) {
        (false, _) => "",
        (true, 100) => "-charged",
        (true, _) => "-charging",
    };
    format!("battery-level-{level}{state}-symbolic")
}
/// PAM's words for an account that refuses even the right password for now.
fn locked_out(message: &str) -> bool {
    let message = message.to_lowercase();
    ["locked", "too many", "verrouill"]
        .iter()
        .any(|word| message.contains(word))
}

struct Person {
    name: String,
    avatar: Option<gdk::Texture>,
}
struct State {
    /// None in preview, where nothing is locked.
    instance: Option<Instance>,
    busy: Cell<bool>,
    composition: Composition,
    /// Arrive after the lock is up, and reach every view, present and to come.
    person: RefCell<Option<Person>>,
    battery: Cell<Option<(u8, bool)>>,
    views: RefCell<Vec<Rc<View>>>,
    /// Ends the lock: true once unlocked, false if locking failed.
    done: async_channel::Sender<bool>,
}
impl State {
    fn unlock(&self) {
        match &self.instance {
            Some(instance) => instance.unlock(),
            None => {
                for view in self.views.take() {
                    view.window.close();
                }
                let _ = self.done.try_send(true);
            }
        }
    }
    fn add(
        self: &Rc<Self>,
        app: &gtk::Application,
        shot: Option<&gdk::Texture>,
        monitor: Option<&gdk::Monitor>,
    ) -> Rc<View> {
        let size = monitor
            .map(|m| m.geometry())
            .filter(|g| g.width() > 0 && g.height() > 0)
            .map_or((1280, 800), |g| (g.width(), g.height()));
        let view = View::new(self, shot, size);
        if let Some(person) = &*self.person.borrow() {
            view.show_person(person);
        }
        view.show_battery(self.battery.get());
        view.window.set_application(Some(app));
        self.views.borrow_mut().push(view.clone());
        view
    }
}

/// Locks until the session is unlocked; false if it could not be locked.
async fn start(app: &gtk::Application, preview: bool, test: Option<u32>) -> bool {
    // Connecting for the captures overlaps with loading the theme.
    let capturer = Capturer::start();
    // Started now so as to be ready soon, and never waited for.
    let person = gio::spawn_blocking(read_person);
    if !preview && !gtk4_session_lock::is_supported() {
        log("This compositor does not support ext-session-lock");
        return false;
    }
    let Some(display) = gdk::Display::default() else {
        log("no display");
        return false;
    };
    let settings = theme::load();
    super::install_theme(&display, &settings);
    // A preview opens from Settings, which must not end up as its backdrop.
    let wallpaper = settings
        .wallpaper
        .as_ref()
        .filter(|_| preview)
        .and_then(|path| gdk::Texture::from_filename(path).ok());
    let shots = Rc::new(if wallpaper.is_some() {
        HashMap::new()
    } else {
        captures(&display, &capturer).await
    });
    log(&format!("captured {} monitor(s)", shots.len()));
    let (done, finished) = async_channel::bounded(1);
    // Handlers hold the state weakly; this task owns it until the end.
    let state = Rc::new(State {
        instance: (!preview).then(Instance::new),
        busy: Cell::new(false),
        composition: Composition::of(&settings.lock_style),
        person: RefCell::default(),
        battery: Cell::new(None),
        views: RefCell::default(),
        done,
    });
    let weak = Rc::downgrade(&state);
    glib::spawn_future_local(async move {
        let Ok((name, avatar)) = person.await else {
            return;
        };
        let Some(state) = weak.upgrade() else { return };
        let avatar = avatar.map(|pixels| {
            gdk::MemoryTexture::new(
                256,
                256,
                gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(pixels),
                256 * 4,
            )
            .upcast()
        });
        let person = Person { name, avatar };
        for view in state.views.borrow().iter() {
            view.show_person(&person);
        }
        state.person.replace(Some(person));
    });
    let weak = Rc::downgrade(&state);
    glib::spawn_future_local(async move {
        loop {
            // Reading `capacity` can take 100 ms through ACPI.
            let reading = gio::spawn_blocking(nexus_control::power::read_battery)
                .await
                .ok()
                .flatten();
            let Some(state) = weak.upgrade() else { return };
            state.battery.set(reading);
            for view in state.views.borrow().iter() {
                view.show_battery(reading);
            }
            drop(state);
            glib::timeout_future_seconds(BATTERY_EVERY).await;
        }
    });
    let weak = Rc::downgrade(&state);
    glib::timeout_add_seconds_local(1, move || {
        let Some(state) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        for view in state.views.borrow().iter() {
            view.tick();
        }
        glib::ControlFlow::Continue
    });
    match &state.instance {
        None => {
            let monitor = display.monitors().item(0).and_downcast::<gdk::Monitor>();
            let shot = wallpaper.or_else(|| {
                monitor
                    .as_ref()
                    .and_then(|m| m.connector())
                    .and_then(|c| shots.get(c.as_str()).cloned())
            });
            let view = state.add(app, shot.as_ref(), monitor.as_ref());
            view.preview_state();
            let done = state.done.clone();
            view.window.connect_destroy(move |_| {
                let _ = done.try_send(true);
            });
            view.window.fullscreen();
            view.window.present();
        }
        Some(instance) => {
            let (weak, app) = (Rc::downgrade(&state), app.clone());
            instance.connect_monitor(move |instance, monitor| {
                let Some(state) = weak.upgrade() else { return };
                let name = monitor.connector().unwrap_or_default();
                let view = state.add(&app, shots.get(name.as_str()), Some(monitor));
                instance.assign_window_to_monitor(&view.window, monitor);
            });
            let done = state.done.clone();
            instance.connect_failed(move |_| {
                log("Could not lock the session; is another locker running?");
                let _ = done.try_send(false);
            });
            instance.connect_locked(|_| log("locked"));
            let weak = Rc::downgrade(&state);
            instance.connect_unlocked(move |_| {
                log("unlocked");
                if let Some(state) = weak.upgrade() {
                    drop(state.views.take());
                    let _ = state.done.try_send(true);
                }
            });
            if let Some(seconds) = test {
                let weak = Rc::downgrade(&state);
                glib::timeout_add_seconds_local_once(seconds, move || {
                    log("test time is up, unlocking");
                    if let Some(state) = weak.upgrade() {
                        state.unlock();
                    }
                });
            }
            log("locking");
            instance.lock();
        }
    }
    finished.recv().await.unwrap_or(false)
}

/// What each composition arranges in its own way.
struct Parts {
    time: gtk::Label,
    date: gtk::Label,
    avatar: gtk::Box,
    name: gtk::Label,
    field: gtk::Box,
    notes: gtk::Box,
    status: gtk::Box,
    power: gtk::Box,
    confirm: gtk::Box,
}
fn identity(parts: &Parts) -> gtk::Box {
    let row = hbox(12);
    row.add_css_class("lock-identity");
    row.append(&parts.avatar);
    row.append(&parts.name);
    row
}
/// Everything in one solid column along the left edge, read top to bottom.
fn panel(parts: &Parts) -> gtk::Widget {
    let root = vbox(0);
    root.add_css_class("lock-column");
    root.set_halign(gtk::Align::Start);
    root.append(&parts.time);
    root.append(&parts.date);
    let middle = vbox(0);
    middle.set_vexpand(true);
    middle.set_valign(gtk::Align::Center);
    middle.append(&identity(parts));
    middle.append(&parts.field);
    middle.append(&parts.notes);
    root.append(&middle);
    root.append(&parts.status);
    root.append(&parts.power);
    root.append(&parts.confirm);
    root.upcast()
}
/// Hours over minutes as the screen's one image, the rest in a narrow right column.
fn clock(parts: &Parts, (width, height): (i32, i32)) -> gtk::Widget {
    // Sized from the monitor so that two lines always fit it.
    let px = (height * 22 / 100).min(width / 4);
    let attrs = pango::AttrList::new();
    attrs.insert(pango::AttrSize::new_size_absolute(px * pango::SCALE));
    attrs.insert(pango::AttrInt::new_letter_spacing(
        -(px * pango::SCALE) / 25,
    ));
    parts.time.set_attributes(Some(&attrs));
    parts.time.add_css_class("poster-time");
    // Where the digits' side bearing puts their ink.
    parts.date.set_margin_start(px / 25);
    let root = hbox(0);
    root.add_css_class("lock-poster");
    let left = vbox(0);
    left.set_hexpand(true);
    left.set_valign(gtk::Align::Center);
    left.append(&parts.time);
    left.append(&parts.date);
    root.append(&left);
    let right = vbox(0);
    right.add_css_class("poster-side");
    parts.status.set_halign(gtk::Align::End);
    right.append(&parts.status);
    let form = vbox(0);
    form.append(&identity(parts));
    form.append(&parts.field);
    form.append(&parts.notes);
    form.append(&parts.power);
    form.append(&parts.confirm);
    form.set_vexpand(true);
    form.set_valign(gtk::Align::End);
    right.append(&form);
    root.append(&right);
    root.upcast()
}
/// The time low on the left, on one full-width strip holding all the rest.
fn bar(parts: &Parts) -> gtk::Widget {
    let root = vbox(0);
    root.set_valign(gtk::Align::End);
    let clock = vbox(0);
    clock.add_css_class("bar-above");
    clock.append(&parts.time);
    clock.append(&parts.date);
    root.append(&clock);
    let strip = gtk::CenterBox::new();
    strip.add_css_class("lock-strip");
    // Both sides line up with the field, not with the field and its notes.
    let who = identity(parts);
    who.set_valign(gtk::Align::Start);
    strip.set_start_widget(Some(&who));
    let form = vbox(0);
    form.append(&parts.field);
    form.append(&parts.notes);
    strip.set_center_widget(Some(&form));
    let end = hbox(20);
    end.set_valign(gtk::Align::Start);
    end.set_margin_top(5);
    parts.status.set_valign(gtk::Align::Center);
    end.append(&parts.status);
    end.append(&parts.power);
    parts.confirm.set_orientation(gtk::Orientation::Horizontal);
    end.append(&parts.confirm);
    strip.set_end_widget(Some(&end));
    root.append(&strip);
    root.upcast()
}

/// The lock surface of one monitor. Each has its own password field; the
/// compositor gives keyboard focus to one of them.
struct View {
    window: gtk::Window,
    shot: gtk::Overlay,
    /// What fades out on leaving, over the backdrop.
    ui: [gtk::Widget; 2],
    time: gtk::Label,
    date: gtk::Label,
    /// strftime for `time`: one line, or hours over minutes.
    time_format: &'static str,
    avatar: gtk::Box,
    avatar_size: i32,
    name: gtk::Label,
    field: gtk::Box,
    entry: gtk::PasswordEntry,
    submit: gtk::Button,
    submit_icon: gtk::Image,
    spinner: gtk::Spinner,
    message: gtk::Label,
    caps: gtk::Box,
    battery: (gtk::Box, gtk::Image, gtk::Label),
    layout: (gtk::Box, gtk::Label),
    power: gtk::Box,
    confirm: gtk::Box,
    question: gtk::Label,
    accept: gtk::Button,
    /// The operation `accept` runs.
    pending: Cell<&'static str>,
}
impl View {
    fn new(state: &Rc<State>, shot: Option<&gdk::Texture>, size: (i32, i32)) -> Rc<Self> {
        let compact = size.0 < COMPACT.0 || size.1 < COMPACT.1;
        let window = gtk::Window::builder().title("Nexus lock").build();
        window.add_css_class("nexus");
        window.add_css_class("lock-screen");
        let overlay = gtk::Overlay::new();
        // The capture, and above it a blurred copy that fades in: blurring
        // once is cheap, blurring the whole screen on every frame is not.
        let picture = |paintable: Option<&gdk::Texture>| {
            let p = gtk::Picture::new();
            p.set_content_fit(gtk::ContentFit::Cover);
            p.set_can_shrink(true);
            p.set_paintable(paintable);
            p
        };
        let layers = gtk::Overlay::new();
        layers.add_css_class("lock-shot");
        let sharp = picture(shot);
        layers.set_child(Some(&sharp));
        let blurred = picture(None);
        layers.add_overlay(&blurred);
        overlay.set_child(Some(&layers));
        // Blurred once the window shows, so that blurring does not delay it;
        // the class starts its fade only then.
        if let Some(shot) = shot.cloned() {
            window.connect_map(move |window| {
                let (window, blurred, sharp, shot) =
                    (window.clone(), blurred.clone(), sharp.clone(), shot.clone());
                glib::idle_add_local_once(move || {
                    match window.renderer().and_then(|r| blur(&r, &shot)) {
                        Some(texture) => {
                            blurred.set_paintable(Some(&texture));
                            blurred.add_css_class("lock-blurred");
                            log("blurred");
                        }
                        // Never leave the sharp desktop on show.
                        None => sharp.set_paintable(None::<&gdk::Texture>),
                    }
                });
            });
        }
        let scrim = vbox(0);
        scrim.add_css_class("lock-scrim");
        scrim.add_css_class("lock-ui");
        overlay.add_overlay(&scrim);

        let time = label("", "lock-time");
        time.set_wrap(false);
        let date = label("", "lock-date");
        date.set_wrap(false);
        let avatar = hbox(0);
        avatar.add_css_class("lock-avatar");
        avatar.set_overflow(gtk::Overflow::Hidden);
        avatar.set_valign(gtk::Align::Center);
        avatar.set_hexpand(false);
        let avatar_size = match state.composition {
            Composition::Panel => 64,
            Composition::Clock | Composition::Bar => 44,
        };
        avatar.set_size_request(avatar_size, avatar_size);
        let name = label("", "lock-name");
        name.set_wrap(false);
        name.set_ellipsize(pango::EllipsizeMode::End);
        // A real password entry: keyboard layouts, compose sequences, the
        // secure buffer and the Caps Lock indicator all work as usual.
        let entry = gtk::PasswordEntry::new();
        entry.add_css_class("lock-password");
        entry.set_placeholder_text(Some("Password"));
        entry.set_hexpand(true);
        entry.update_property(&[gtk::accessible::Property::Label("Password")]);
        let go = gtk::Button::new();
        go.add_css_class("lock-submit");
        go.set_focus_on_click(false);
        go.set_sensitive(false);
        go.set_valign(gtk::Align::Center);
        go.update_property(&[gtk::accessible::Property::Label("Unlock")]);
        let submit_icon = gtk::Image::from_icon_name("go-next-symbolic");
        go.set_child(Some(&submit_icon));
        let field = hbox(0);
        field.add_css_class("lock-field");
        // Its entry fills it, without the field then asking its parent for all the room.
        field.set_hexpand(false);
        field.append(&entry);
        field.append(&go);
        let message = label("", "lock-message");
        message.set_halign(gtk::Align::Start);
        // PAM's text wraps instead of widening the composition around it.
        message.set_max_width_chars(44);
        message.set_visible(false);
        let caps = hbox(6);
        caps.add_css_class("lock-caps");
        caps.set_halign(gtk::Align::Start);
        caps.append(&gtk::Image::from_icon_name("caps-lock-symbolic"));
        caps.append(&gtk::Label::new(Some("Caps Lock is on")));
        caps.set_visible(false);
        // Holds its place so that a message does not move the field above it.
        let notes = vbox(6);
        notes.add_css_class("lock-notes");
        notes.append(&caps);
        notes.append(&message);

        let status = hbox(16);
        status.add_css_class("lock-status");
        let battery_box = hbox(6);
        let battery_icon = gtk::Image::new();
        let battery_label = gtk::Label::new(None);
        battery_box.append(&battery_icon);
        battery_box.append(&battery_label);
        battery_box.set_visible(false);
        status.append(&battery_box);
        let layout_box = hbox(6);
        let layout_label = gtk::Label::new(None);
        layout_box.append(&gtk::Image::from_icon_name("input-keyboard-symbolic"));
        layout_box.append(&layout_label);
        layout_box.set_visible(false);
        status.append(&layout_box);

        let power = hbox(8);
        power.add_css_class("lock-power-row");
        power.set_homogeneous(state.composition != Composition::Bar);
        let confirm = vbox(10);
        confirm.add_css_class("lock-confirm-row");
        confirm.set_visible(false);
        let question = label("", "lock-question");
        question.set_max_width_chars(44);
        // One line on the strip, unless the monitor is too narrow for it.
        question.set_wrap(compact || state.composition != Composition::Bar);
        confirm.append(&question);
        let answers = hbox(8);
        answers.set_homogeneous(true);
        answers.set_valign(gtk::Align::Center);
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::new();
        accept.add_css_class("destructive-action");
        for b in [&cancel, &accept] {
            b.add_css_class("lock-answer");
            b.set_focus_on_click(false);
            answers.append(b);
        }
        confirm.append(&answers);

        let parts = Parts {
            time: time.clone(),
            date: date.clone(),
            avatar: avatar.clone(),
            name: name.clone(),
            field: field.clone(),
            notes,
            status,
            power: power.clone(),
            confirm: confirm.clone(),
        };
        let (root, time_format, class): (gtk::Widget, _, _) = match state.composition {
            Composition::Panel => (panel(&parts), "%H:%M", "panel"),
            Composition::Clock => (clock(&parts, size), "%H\n%M", "clock"),
            Composition::Bar => (bar(&parts), "%H:%M", "bar"),
        };
        window.add_css_class(class);
        if compact {
            window.add_css_class("compact");
        }
        // Too narrow for the name beside everything else.
        name.set_visible(!compact || state.composition != Composition::Bar);
        root.add_css_class("lock-ui");
        overlay.add_overlay(&root);
        if state.instance.is_none() {
            let notice = label("Preview · Esc to close", "lock-preview");
            notice.set_halign(gtk::Align::Center);
            notice.set_valign(gtk::Align::Start);
            notice.set_can_target(false);
            overlay.add_overlay(&notice);
        }
        window.set_child(Some(&overlay));

        let view = Rc::new(Self {
            window,
            shot: layers,
            ui: [scrim.upcast(), root],
            time,
            date,
            time_format,
            avatar,
            avatar_size,
            name,
            field,
            entry,
            submit: go,
            submit_icon,
            spinner: gtk::Spinner::new(),
            message,
            caps,
            battery: (battery_box, battery_icon, battery_label),
            layout: (layout_box, layout_label),
            power,
            confirm,
            question,
            accept,
            pending: Cell::new(""),
        });
        for (op, title, icon, ask) in POWER {
            let b = gtk::Button::new();
            b.add_css_class("lock-power");
            b.set_focus_on_click(false);
            b.update_property(&[gtk::accessible::Property::Label(title)]);
            let content = hbox(8);
            content.set_halign(gtk::Align::Center);
            content.append(&gtk::Image::from_icon_name(icon));
            if !compact {
                content.append(&gtk::Label::new(Some(title)));
            }
            b.set_child(Some(&content));
            let (weak, s) = (Rc::downgrade(&view), Rc::downgrade(state));
            b.connect_clicked(move |_| {
                let (Some(view), Some(state)) = (weak.upgrade(), s.upgrade()) else {
                    return;
                };
                match ask {
                    Some(question) => view.ask(op, title, question),
                    None => perform_power(&state, &view, op),
                }
            });
            view.power.append(&b);
        }
        let weak = Rc::downgrade(&view);
        cancel.connect_clicked(move |_| {
            if let Some(view) = weak.upgrade() {
                view.settle();
            }
        });
        let (weak, s) = (Rc::downgrade(&view), Rc::downgrade(state));
        view.accept.connect_clicked(move |_| {
            if let (Some(view), Some(state)) = (weak.upgrade(), s.upgrade()) {
                perform_power(&state, &view, view.pending.get());
            }
        });
        let (weak, s) = (Rc::downgrade(&view), Rc::downgrade(state));
        view.submit.connect_clicked(move |_| {
            if let (Some(view), Some(state)) = (weak.upgrade(), s.upgrade()) {
                submit(&state, &view);
            }
        });
        let weak = Rc::downgrade(&view);
        view.entry.connect_changed(move |_| {
            if let Some(view) = weak.upgrade() {
                if !view.field.has_css_class("checking") {
                    view.say("", "");
                }
                view.field.remove_css_class("error");
                view.update_submit();
            }
        });
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (weak, s) = (Rc::downgrade(&view), Rc::downgrade(state));
        // Enter and Escape are handled here, and typing reaches the field,
        // even when the compositor's focus did not land on it. A button reached
        // with Tab keeps its own keys.
        key.connect_key_pressed(move |controller, key, _, _| {
            let (Some(view), Some(state)) = (weak.upgrade(), s.upgrade()) else {
                return glib::Propagation::Proceed;
            };
            let on_button =
                GtkWindowExt::focus(&view.window).is_some_and(|w| w.is::<gtk::Button>());
            let typing = key
                .to_unicode()
                .is_some_and(|c| !c.is_control() && c != ' ');
            match key {
                gdk::Key::Escape => {
                    if view.confirm.is_visible() {
                        view.settle();
                    } else if state.instance.is_none() {
                        state.unlock();
                    } else if !state.busy.get() {
                        view.entry.set_text("");
                        view.entry.grab_focus();
                    }
                }
                _ if on_button && !typing => return glib::Propagation::Proceed,
                gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => {
                    submit(&state, &view)
                }
                // The focus sits on the text inside the entry, not the entry.
                _ if view.entry.focus_child().is_some() => return glib::Propagation::Proceed,
                _ => {
                    view.entry.grab_focus();
                    if let Some(text) = view.entry.delegate() {
                        controller.forward(&text);
                    }
                }
            }
            glib::Propagation::Stop
        });
        view.window.add_controller(key);
        let weak = Rc::downgrade(&view);
        view.window.connect_map(move |_| {
            if let Some(view) = weak.upgrade() {
                let entry = view.entry.clone();
                glib::idle_add_local_once(move || {
                    entry.grab_focus();
                });
            }
        });
        if let Some(device) = view.keyboard() {
            let weak = Rc::downgrade(&view);
            device.connect_caps_lock_state_notify(move |device| {
                if let Some(view) = weak.upgrade() {
                    view.caps.set_visible(device.is_caps_locked());
                }
            });
            view.caps.set_visible(device.is_caps_locked());
            let weak = Rc::downgrade(&view);
            device.connect_notify_local(Some("active-layout-index"), move |_, _| {
                if let Some(view) = weak.upgrade() {
                    view.show_layout();
                }
            });
        }
        view.tick();
        view
    }
    fn keyboard(&self) -> Option<gdk::Device> {
        WidgetExt::display(&self.window).default_seat()?.keyboard()
    }
    /// The layout's name as the keymap gives it; the keymap can come late, so
    /// [`Self::tick`] asks again.
    fn show_layout(&self) {
        // By property: GTK older than 4.18 has none, and then neither has the lock.
        let name = self.keyboard().and_then(|device| {
            device.find_property("layout-names")?;
            device.find_property("active-layout-index")?;
            let index = device.property::<i32>("active-layout-index");
            let names = device.property::<Vec<String>>("layout-names");
            names.into_iter().nth(usize::try_from(index).ok()?)
        });
        self.layout.0.set_visible(name.is_some());
        self.layout.1.set_text(name.as_deref().unwrap_or_default());
    }
    fn show_person(&self, person: &Person) {
        self.name.set_text(&person.name);
        clear(&self.avatar);
        match &person.avatar {
            Some(texture) => {
                let image = gtk::Image::from_paintable(Some(texture));
                image.set_pixel_size(self.avatar_size);
                self.avatar.append(&image);
            }
            None => {
                let initial: String = person
                    .name
                    .chars()
                    .take(1)
                    .flat_map(char::to_uppercase)
                    .collect();
                let letter = gtk::Label::new(Some(&initial));
                letter.set_hexpand(true);
                self.avatar.append(&letter);
            }
        }
    }
    fn show_battery(&self, reading: Option<(u8, bool)>) {
        self.battery.0.set_visible(reading.is_some());
        if let Some((level, charging)) = reading {
            self.battery
                .1
                .set_icon_name(Some(&battery_icon(level, charging)));
            self.battery.2.set_text(&format!("{level}%"));
        }
    }
    /// What only a preview can show, since nothing here goes through PAM.
    fn preview_state(&self) {
        if !cfg!(any(test, feature = "lab")) {
            return;
        }
        match std::env::var("NEXUS_LOCK_PREVIEW_STATE").as_deref() {
            Ok("checking") => self.checking(true),
            Ok("wrong") => self.refused("Wrong password".into()),
            Ok("locked") => self.refused(
                "The account is locked due to 3 failed logins.\n(9 minutes 41 seconds left to unlock)".into(),
            ),
            Ok("caps") => self.caps.set_visible(true),
            Ok("confirm") => {
                if let (op, title, _, Some(question)) = POWER[2] {
                    self.ask(op, title, question);
                }
            }
            _ => {}
        }
    }
    /// Asks before a power action, in place of the buttons.
    fn ask(&self, op: &'static str, title: &str, question: &str) {
        self.pending.set(op);
        self.question.set_text(question);
        self.accept.set_label(title);
        self.power.set_visible(false);
        self.confirm.set_visible(true);
    }
    /// Back to the buttons, with typing going to the field.
    fn settle(&self) {
        self.confirm.set_visible(false);
        self.power.set_visible(true);
        self.entry.grab_focus();
    }
    fn update_submit(&self) {
        self.submit
            .set_sensitive(!self.field.has_css_class("checking") && !self.entry.text().is_empty());
    }
    fn tick(&self) {
        let Ok(now) = glib::DateTime::now_local() else {
            return;
        };
        self.time
            .set_text(&now.format(self.time_format).unwrap_or_default());
        self.date
            .set_text(&now.format("%A %-d %B").unwrap_or_default());
        self.show_layout();
    }
    /// `kind` is the message's class: nothing, "error" or "warning".
    fn say(&self, text: &str, kind: &str) {
        self.message.set_text(text);
        self.message.set_visible(!text.is_empty());
        for class in ["error", "warning"] {
            if class == kind {
                self.message.add_css_class(class);
            } else {
                self.message.remove_css_class(class);
            }
        }
    }
    fn checking(&self, on: bool) {
        self.entry.set_editable(!on);
        if on {
            self.field.add_css_class("checking");
            self.spinner.set_spinning(true);
            self.submit.set_child(Some(&self.spinner));
            self.say("Checking…", "");
        } else {
            self.field.remove_css_class("checking");
            self.spinner.set_spinning(false);
            self.submit.set_child(Some(&self.submit_icon));
            self.say("", "");
        }
        self.update_submit();
    }
    /// The field stays usable when locked out: nothing says when that ends, and
    /// the right password must work as soon as it does.
    fn refused(&self, error: String) {
        self.checking(false);
        // GTK replays an animation only when its name changes.
        let (add, remove) = if self.field.has_css_class("shake-a") {
            ("shake-b", "shake-a")
        } else {
            ("shake-a", "shake-b")
        };
        self.field.remove_css_class(remove);
        self.field.add_css_class(add);
        self.entry.set_text("");
        self.field.add_css_class("error");
        if locked_out(&error) {
            self.say(&format!("Locked out\n{error}"), "warning");
        } else {
            self.say(&error, "error");
        }
    }
    fn leave(&self) {
        for widget in &self.ui {
            widget.add_css_class("leaving");
        }
        self.shot.add_css_class("leaving");
    }
}
/// Through the daemon's login1 call, off the main thread. Unlocks nothing: the
/// lock stays until a password opens it, across a sleep too.
fn perform_power(state: &Rc<State>, view: &Rc<View>, op: &'static str) {
    view.settle();
    if state.instance.is_none() {
        view.say("Preview: nothing was sent", "");
        return;
    }
    let view = Rc::downgrade(view);
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            let _ = nexus_control::ensure_daemon();
            super::perform(&super::action(op, "", ""))
        })
        .await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e,
            Err(_) => "The power action failed".into(),
        };
        if let Some(view) = view.upgrade() {
            view.say(&error, "error");
        }
    });
}
/// A blurred, quarter-size copy of `texture`.
fn blur(renderer: &gtk::gsk::Renderer, texture: &gdk::Texture) -> Option<gdk::Texture> {
    let (w, h) = (texture.width() as f32 / 4., texture.height() as f32 / 4.);
    let bounds = gtk::graphene::Rect::new(0., 0., w, h);
    let snapshot = gtk::Snapshot::new();
    snapshot.push_clip(&bounds);
    snapshot.push_blur(BLUR.into());
    // Stretched past the edges so the blur does not pull in transparency.
    let margin = BLUR * 3.;
    snapshot.append_texture(
        texture,
        &gtk::graphene::Rect::new(-margin, -margin, w + 2. * margin, h + 2. * margin),
    );
    snapshot.pop();
    snapshot.pop();
    Some(renderer.render_texture(snapshot.to_node()?, Some(&bounds)))
}
fn submit(state: &Rc<State>, view: &Rc<View>) {
    if state.busy.get() || view.entry.text().is_empty() {
        return;
    }
    // Wrong tries here would count towards the real lockout.
    if state.instance.is_none() {
        view.entry.set_text("");
        view.say("Preview: passwords are not checked", "");
        return;
    }
    state.busy.set(true);
    view.checking(true);
    let password = view.entry.text().to_string();
    let (tx, rx) = async_channel::bounded(1);
    // `thread::spawn` panics if it can't start one. Failing here drops `tx`, which
    // the task below takes for a failed attempt.
    let _ = std::thread::Builder::new().spawn(move || {
        let _ = tx.send_blocking(pam::authenticate(password));
    });
    let (state, view) = (state.clone(), view.clone());
    glib::spawn_future_local(async move {
        let result = rx
            .recv()
            .await
            .unwrap_or_else(|_| Err("Authentication failed".into()));
        match result {
            Ok(()) => {
                view.checking(false);
                for v in state.views.borrow().iter() {
                    v.leave();
                }
                glib::timeout_add_local_once(LEAVE, move || state.unlock());
            }
            Err(e) => {
                state.busy.set(false);
                view.refused(e);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(ms: u64) {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(4));
        }
    }
    #[test]
    fn unknown_lock_styles_fall_back() {
        assert_eq!(Composition::of(""), Composition::Panel);
        assert_eq!(Composition::of("from-the-future"), Composition::Panel);
        // The default, and the three the chooser offers.
        assert_eq!(
            Composition::of(&theme::Settings::default().lock_style),
            Composition::Panel
        );
        let all: Vec<_> = theme::LOCK_STYLES
            .iter()
            .map(|l| Composition::of(l.0))
            .collect();
        assert_eq!(
            all,
            [Composition::Panel, Composition::Clock, Composition::Bar]
        );
    }
    #[test]
    fn only_a_held_lock_stops_a_lock() {
        let name = format!("lock-test-{}", std::process::id());
        let held = nexus_control::instance_lock(&name).unwrap();
        assert!(lock_is_held(
            &nexus_control::instance_lock(&name).unwrap_err()
        ));
        drop(held);
        std::fs::remove_file(
            nexus_control::runtime()
                .unwrap()
                .join(format!("{name}.lock")),
        )
        .unwrap();
        // What a missing or unsafe runtime directory gives.
        assert!(!lock_is_held(&anyhow::anyhow!(
            "Unsafe Nexus runtime directory"
        )));
        assert!(!lock_is_held(
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()
        ));
    }
    #[test]
    fn lockouts_are_told_from_wrong_passwords() {
        assert!(!locked_out("Wrong password"));
        assert!(!locked_out("Authentication failed (9)"));
        assert!(locked_out("Too many attempts"));
        assert!(locked_out("The account is locked due to 3 failed logins."));
    }
    #[test]
    fn bad_avatars_are_skipped() {
        let dir = std::env::temp_dir().join(format!("nexus-avatar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (bad, good) = (dir.join("bad"), dir.join("good"));
        std::fs::write(&bad, b"not an image").unwrap();
        image::RgbaImage::new(40, 10)
            .save_with_format(&good, image::ImageFormat::Png)
            .unwrap();
        assert!(read_avatar(&dir.join("missing")).is_none());
        assert!(read_avatar(&bad).is_none());
        // Cropped to the square the texture expects, whatever its shape.
        assert_eq!(read_avatar(&good).unwrap().len(), 256 * 256 * 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    #[ignore = "requires an isolated display: run it inside the lab"]
    fn lock_view_follows_typing_and_results() {
        gtk::init().unwrap();
        for composition in [Composition::Panel, Composition::Clock, Composition::Bar] {
            // Never locks anything: there is no instance.
            let state = Rc::new(State {
                instance: None,
                busy: Cell::new(false),
                composition,
                person: RefCell::default(),
                battery: Cell::new(None),
                views: RefCell::default(),
                done: async_channel::bounded(1).0,
            });
            let view = View::new(&state, None, (1280, 800));
            view.window.present();
            wait(300);
            view.entry.set_text("hunter2");
            view.checking(true);
            assert!(!view.entry.is_editable());
            assert!(!view.submit.is_sensitive());
            view.refused("Wrong password".into());
            assert!(view.entry.is_editable());
            assert!(view.entry.text().is_empty());
            assert_eq!(view.message.text(), "Wrong password");
            assert!(view.field.has_css_class("error"));
            // Typing again dismisses the error.
            view.entry.set_text("x");
            assert_eq!(view.message.text(), "");
            assert!(!view.message.is_visible());
            assert!(!view.field.has_css_class("error"));
            assert!(view.submit.is_sensitive());
            // Locked out, the field stays usable.
            view.refused("Too many attempts".into());
            assert!(view.entry.is_editable());
            assert!(view.message.has_css_class("warning"));
            // Preview: the power path stops before sending anything.
            perform_power(&state, &view, "shutdown");
            assert_eq!(view.message.text(), "Preview: nothing was sent");
            // Preview: a password never reaches PAM.
            view.entry.set_text("hunter2");
            submit(&state, &view);
            assert!(!state.busy.get());
            assert!(view.entry.text().is_empty());
            assert_eq!(view.message.text(), "Preview: passwords are not checked");
            view.window.close();
        }
    }
}
