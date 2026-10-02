//! `nexus lock`: a session lock (ext-session-lock) covering every monitor.
//! The compositor keeps the session locked even if this process dies.
mod pam;
use super::{label, vbox};
use gtk::{gio, glib, prelude::*};
use gtk4_session_lock::Instance;
use nexus_control::{runtime, theme};
use std::{
    cell::{Cell, RefCell},
    f64::consts::{PI, TAU},
    rc::Rc,
    time::Duration,
};

/// Shapes shown at most; longer passwords keep counting without growing the field.
const MAX_SHAPES: usize = 32;
const SHAPE_KINDS: u32 = 6;
/// Matches the leaving animations in the theme.
const SHAPE_OUT: Duration = Duration::from_millis(170);
const LEAVE: Duration = Duration::from_millis(420);

pub fn run() -> anyhow::Result<()> {
    use fs2::FileExt;
    let guard = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(runtime()?.join("lock.lock"))?;
    // Already locked: binding the shortcut and an idle daemon to it is fine.
    if guard.try_lock_exclusive().is_err() {
        return Ok(());
    }
    glib::set_prgname(Some("io.github.vitrixxl.Nexus.Lock"));
    let app = gtk::Application::builder()
        .application_id("io.github.vitrixxl.Nexus.Lock")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let failed = Rc::new(Cell::new(false));
    let failure = failed.clone();
    app.connect_activate(move |app| start(app, failure.clone()));
    app.run_with_args::<&str>(&[]);
    if failed.get() {
        anyhow::bail!("The compositor did not lock the session");
    }
    Ok(())
}

struct State {
    instance: Instance,
    busy: Cell<bool>,
    views: RefCell<Vec<Rc<View>>>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
}
fn start(app: &gtk::Application, failed: Rc<Cell<bool>>) {
    if !gtk4_session_lock::is_supported() {
        eprintln!("This compositor does not support ext-session-lock");
        failed.set(true);
        return;
    }
    let settings = theme::load();
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&theme::css(&settings));
    let display = gtk::gdk::Display::default().expect("no display");
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    if let Some(gtk_settings) = gtk::Settings::default() {
        gtk_settings.set_gtk_application_prefer_dark_theme(settings.dark());
    }
    let state = Rc::new(State {
        instance: Instance::new(),
        busy: Cell::new(false),
        views: RefCell::default(),
        hold: RefCell::new(Some(app.hold())),
    });
    let wallpaper = settings.wallpaper.clone();
    let (weak, app2) = (Rc::downgrade(&state), app.clone());
    state.instance.connect_monitor(move |instance, monitor| {
        let Some(state) = weak.upgrade() else { return };
        let view = View::new(&state, wallpaper.as_deref());
        view.window.set_application(Some(&app2));
        instance.assign_window_to_monitor(&view.window, monitor);
        state.views.borrow_mut().push(view);
    });
    let weak = Rc::downgrade(&state);
    state.instance.connect_failed(move |_| {
        eprintln!("Could not lock the session; is another locker running?");
        failed.set(true);
        if let Some(state) = weak.upgrade() {
            state.hold.take();
        }
    });
    let weak = Rc::downgrade(&state);
    state.instance.connect_unlocked(move |_| {
        if let Some(state) = weak.upgrade() {
            state.views.borrow_mut().clear();
            state.hold.take();
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
    // Watch Caps Lock, which makes a refused password likely.
    if let Some(keyboard) = display.default_seat().and_then(|s| s.keyboard()) {
        let weak = Rc::downgrade(&state);
        keyboard.connect_caps_lock_state_notify(move |_| {
            if let Some(state) = weak.upgrade() {
                for view in state.views.borrow().iter() {
                    view.show_message();
                }
            }
        });
    }
    state.instance.lock();
}

/// The lock surface of one monitor. Each has its own hidden password field;
/// the compositor gives keyboard focus to one of them.
struct View {
    window: gtk::Window,
    content: gtk::Box,
    badge: gtk::Box,
    padlock: gtk::Image,
    clock: gtk::Label,
    date: gtk::Label,
    text: gtk::Text,
    revealer: gtk::Revealer,
    field: gtk::Box,
    message: gtk::Label,
    shapes: RefCell<Vec<gtk::DrawingArea>>,
    last_kind: Cell<u32>,
    error: RefCell<Option<String>>,
    shake: Cell<bool>,
    /// Bumped on each keystroke so that an older pending hide does nothing.
    generation: Cell<u64>,
}
impl View {
    fn new(state: &Rc<State>, wallpaper: Option<&str>) -> Rc<Self> {
        let window = gtk::Window::builder().title("Nexus lock").build();
        window.add_css_class("nexus");
        window.add_css_class("lock-screen");
        let overlay = gtk::Overlay::new();
        let background = gtk::Picture::new();
        background.set_content_fit(gtk::ContentFit::Cover);
        background.set_can_shrink(true);
        if let Some(path) = wallpaper.filter(|p| std::path::Path::new(p).is_file()) {
            background.set_filename(Some(path));
        }
        overlay.set_child(Some(&background));
        let dim = vbox(0);
        dim.add_css_class("lock-dim");
        overlay.add_overlay(&dim);

        let content = vbox(0);
        content.add_css_class("lock-content");
        content.set_halign(gtk::Align::Center);
        content.set_valign(gtk::Align::Center);
        let badge = vbox(0);
        badge.add_css_class("lock-badge");
        badge.set_halign(gtk::Align::Center);
        let padlock = gtk::Image::from_icon_name("changes-prevent-symbolic");
        padlock.set_pixel_size(22);
        padlock.set_vexpand(true);
        badge.append(&padlock);
        content.append(&badge);
        let user = glib::real_name().to_string_lossy().into_owned();
        let user = if user.is_empty() || user == "Unknown" {
            glib::user_name().to_string_lossy().into_owned()
        } else {
            user
        };
        let user = label(&user, "lock-user");
        user.set_xalign(0.5);
        content.append(&user);
        let clock = label("", "lock-clock");
        clock.set_xalign(0.5);
        content.append(&clock);
        let date = label("", "lock-date");
        date.set_xalign(0.5);
        content.append(&date);

        // The password lives in a hidden text field so that keyboard layouts,
        // compose sequences and the secure password buffer all work as usual.
        let text = gtk::Text::with_buffer(&gtk::PasswordEntryBuffer::new());
        text.set_visibility(false);
        text.set_opacity(0.);
        text.set_can_target(false);
        text.set_size_request(1, 1);
        text.set_halign(gtk::Align::Center);
        let field = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        field.add_css_class("lock-field");
        field.set_halign(gtk::Align::Center);
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::Crossfade);
        revealer.set_transition_duration(220);
        revealer.set_child(Some(&field));
        // Room for the field is kept so the clock does not move when it appears.
        let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        slot.add_css_class("lock-slot");
        slot.set_valign(gtk::Align::Center);
        slot.append(&revealer);
        slot.append(&text);
        content.append(&slot);
        let message = label("", "lock-message");
        message.set_xalign(0.5);
        message.set_justify(gtk::Justification::Center);
        content.append(&message);
        overlay.add_overlay(&content);
        window.set_child(Some(&overlay));

        let view = Rc::new(Self {
            window,
            content,
            badge,
            padlock,
            clock,
            date,
            text,
            revealer,
            field,
            message,
            shapes: RefCell::default(),
            last_kind: Cell::new(SHAPE_KINDS),
            error: RefCell::default(),
            shake: Cell::new(false),
            generation: Cell::new(0),
        });
        let weak = Rc::downgrade(&view);
        view.text.connect_changed(move |_| {
            if let Some(view) = weak.upgrade() {
                view.sync();
            }
        });
        let (weak, s) = (Rc::downgrade(&view), Rc::downgrade(state));
        view.text.connect_activate(move |_| {
            if let (Some(view), Some(state)) = (weak.upgrade(), s.upgrade()) {
                submit(&state, &view);
            }
        });
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (weak, s) = (Rc::downgrade(&view), Rc::downgrade(state));
        key.connect_key_pressed(move |_, key, _, _| {
            if key != gtk::gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            if let (Some(view), Some(state)) = (weak.upgrade(), s.upgrade())
                && !state.busy.get()
            {
                view.text.set_text("");
            }
            glib::Propagation::Stop
        });
        view.window.add_controller(key);
        let weak = Rc::downgrade(&view);
        view.window.connect_map(move |_| {
            if let Some(view) = weak.upgrade() {
                let text = view.text.clone();
                glib::idle_add_local_once(move || {
                    text.grab_focus();
                });
            }
        });
        view.tick();
        view.show_message();
        view
    }
    fn tick(&self) {
        let Ok(now) = glib::DateTime::now_local() else {
            return;
        };
        let format = |f: &str| now.format(f).map(|s| s.to_string()).unwrap_or_default();
        self.clock.set_text(&format("%H:%M"));
        self.date.set_text(&format("%A %-d %B"));
    }
    /// The PAM error until the next keystroke, else a Caps Lock warning, else a hint.
    fn show_message(&self) {
        let caps = WidgetExt::display(&self.window)
            .default_seat()
            .and_then(|s| s.keyboard())
            .is_some_and(|k| k.is_caps_locked());
        let error = self.error.borrow().clone();
        let typing = self.revealer.reveals_child();
        let (text, class) = match (&error, caps) {
            (Some(e), _) => (e.as_str(), Some("error")),
            (None, true) => ("Caps Lock is on", Some("warning")),
            (None, false) if typing => ("", None),
            (None, false) => ("Type your password to unlock", None),
        };
        self.message.set_text(text);
        for c in ["error", "warning"] {
            if Some(c) == class {
                self.message.add_css_class(c);
            } else {
                self.message.remove_css_class(c);
            }
        }
    }
    /// One shape per typed character: new ones pop in, erased ones shrink away.
    fn sync(self: &Rc<Self>) {
        let count = (self.text.buffer().length() as usize).min(MAX_SHAPES);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        if self.error.borrow_mut().take().is_some() {
            self.field.remove_css_class("error");
        }
        let mut shapes = self.shapes.borrow_mut();
        while shapes.len() < count {
            let mut kind = glib::random_int_range(0, SHAPE_KINDS as i32) as u32;
            if kind == self.last_kind.get() {
                kind = (kind + 1) % SHAPE_KINDS;
            }
            self.last_kind.set(kind);
            let area = shape(kind);
            self.field.append(&area);
            shapes.push(area);
        }
        while shapes.len() > count {
            let area = shapes.pop().unwrap();
            area.add_css_class("leaving");
            let field = self.field.clone();
            glib::timeout_add_local_once(SHAPE_OUT, move || {
                if area.parent().is_some() {
                    field.remove(&area);
                }
            });
        }
        drop(shapes);
        if count > 0 {
            self.revealer.set_reveal_child(true);
        } else {
            // The field lingers a moment after the last character is erased.
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(Duration::from_millis(1400), move || {
                if let Some(view) = weak.upgrade()
                    && view.generation.get() == generation
                {
                    view.revealer.set_reveal_child(false);
                    view.show_message();
                }
            });
        }
        self.show_message();
    }
    fn checking(&self, on: bool) {
        self.text.set_editable(!on);
        if on {
            self.field.add_css_class("checking");
        } else {
            self.field.remove_css_class("checking");
        }
    }
    fn refused(self: &Rc<Self>, error: String) {
        self.checking(false);
        // GTK replays an animation only when its name changes.
        let (add, remove) = if self.shake.replace(!self.shake.get()) {
            ("shake-b", "shake-a")
        } else {
            ("shake-a", "shake-b")
        };
        self.field.remove_css_class(remove);
        self.field.add_css_class(add);
        self.field.add_css_class("error");
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(520), move || {
            if let Some(view) = weak.upgrade() {
                view.text.set_text("");
                // The message alone remains, until the user types again.
                view.revealer.set_reveal_child(false);
                *view.error.borrow_mut() = Some(error);
                view.show_message();
            }
        });
    }
    fn opened(&self) {
        self.checking(false);
        self.padlock.set_icon_name(Some("changes-allow-symbolic"));
        self.badge.add_css_class("unlocked");
    }
}
fn submit(state: &Rc<State>, view: &Rc<View>) {
    if state.busy.get() || view.text.buffer().length() == 0 {
        return;
    }
    state.busy.set(true);
    view.checking(true);
    let password = view.text.text().to_string();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(pam::authenticate(password));
    });
    let (state, view) = (state.clone(), view.clone());
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(())) => {
                view.opened();
                for v in state.views.borrow().iter() {
                    v.content.add_css_class("leaving");
                }
                glib::timeout_add_local_once(LEAVE, move || state.instance.unlock());
            }
            result => {
                state.busy.set(false);
                view.refused(match result {
                    Ok(Err(e)) => e,
                    _ => "Authentication failed".into(),
                });
            }
        }
    });
}

/// Draws a filled regular polygon of `sides` around the centre, squashed
/// horizontally by `squash`, with alternating `inner` points for stars.
fn polygon(
    cr: &gtk::cairo::Context,
    (cx, cy, r): (f64, f64, f64),
    points: u32,
    inner: f64,
    squash: f64,
) {
    for i in 0..points {
        let angle = -PI / 2. + TAU * f64::from(i) / f64::from(points);
        let radius = if i % 2 == 1 { r * inner } else { r };
        let (x, y) = (
            cx + radius * angle.cos() * squash,
            cy + radius * angle.sin(),
        );
        if i == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    cr.close_path();
}
/// A small shape standing for one character. Its colour comes from CSS so the
/// field can tint it, for instance red on a refused password.
fn shape(kind: u32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(16);
    area.set_content_height(16);
    area.set_valign(gtk::Align::Center);
    area.add_css_class("lock-shape");
    area.set_draw_func(move |area, cr, w, h| {
        let color = area.color();
        cr.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            f64::from(color.alpha()),
        );
        let (w, h) = (f64::from(w), f64::from(h));
        let centre = (w / 2., h / 2., w.min(h) / 2.);
        match kind {
            0 => cr.arc(centre.0, centre.1, centre.2 * 0.82, 0., TAU),
            1 => {
                let side = centre.2 * 1.35;
                cr.rectangle(centre.0 - side / 2., centre.1 - side / 2., side, side);
            }
            2 => polygon(cr, (centre.0, centre.1 + 1.5, centre.2), 3, 1., 1.),
            3 => polygon(cr, centre, 4, 1., 0.72),
            4 => polygon(cr, centre, 10, 0.45, 1.),
            _ => polygon(cr, centre, 6, 1., 1.),
        }
        let _ = cr.fill();
    });
    area
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait(ms: u64) {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(4));
        }
    }
    /// Saves what the window shows when NEXUS_LOCK_FRAMES names a directory.
    fn frame(window: &gtk::Window, name: &str) {
        let Some(dir) = std::env::var_os("NEXUS_LOCK_FRAMES") else {
            return;
        };
        let paintable = gtk::WidgetPaintable::new(Some(window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(window.width()),
            f64::from(window.height()),
        );
        let node = snapshot.to_node().unwrap();
        let texture = window.renderer().unwrap().render_texture(&node, None);
        texture
            .save_to_png(std::path::Path::new(&dir).join(format!("{name}.png")))
            .unwrap();
    }
    #[test]
    #[ignore = "requires an isolated display: GDK_BACKEND=x11 xvfb-run -a cargo test --bin nexus lock_view -- --ignored"]
    fn lock_view_follows_typing_and_results() {
        gtk::init().unwrap();
        let settings = theme::load();
        let provider = gtk::CssProvider::new();
        provider.load_from_data(&theme::css(&settings));
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().unwrap(),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // Never locks anything: the instance is not asked to lock.
        let state = Rc::new(State {
            instance: Instance::new(),
            busy: Cell::new(false),
            views: RefCell::default(),
            hold: RefCell::default(),
        });
        let view = View::new(&state, settings.wallpaper.as_deref());
        view.window.set_default_size(1280, 800);
        view.window.present();
        wait(700);
        assert!(!view.revealer.reveals_child());
        assert_eq!(view.message.text(), "Type your password to unlock");
        frame(&view.window, "1-idle");

        let typed = "correct horse";
        // One character at a time, as a keyboard inserts them.
        let buffer = view.text.buffer();
        for (i, c) in typed.chars().enumerate() {
            buffer.insert_text(i as u16, c.to_string());
            wait(45);
        }
        frame(&view.window, "2-typing");
        wait(400);
        assert_eq!(view.shapes.borrow().len(), typed.len());
        assert!(view.revealer.reveals_child());
        assert_eq!(view.message.text(), "");
        frame(&view.window, "3-typed");
        for _ in 0..3 {
            buffer.delete_text(buffer.length() - 1, Some(1));
            wait(30);
        }
        frame(&view.window, "4-erasing");
        assert_eq!(view.shapes.borrow().len(), 10);

        view.checking(true);
        assert!(!view.text.is_editable());
        wait(380);
        frame(&view.window, "5-checking");
        view.refused("Wrong password".into());
        wait(140);
        frame(&view.window, "6-shake");
        wait(600);
        assert!(view.text.is_editable());
        assert_eq!(view.text.buffer().length(), 0);
        assert!(view.shapes.borrow().is_empty());
        assert_eq!(view.message.text(), "Wrong password");
        assert!(!view.revealer.reveals_child());
        assert!(view.message.has_css_class("error"));
        frame(&view.window, "7-refused");

        // Typing again dismisses the error; erasing everything hides the field.
        view.text.set_text("x");
        wait(50);
        assert!(!view.message.has_css_class("error"));
        assert!(!view.field.has_css_class("error"));
        view.text.set_text("");
        wait(1700);
        assert!(!view.revealer.reveals_child());
        assert_eq!(view.message.text(), "Type your password to unlock");

        view.text.set_text("open");
        wait(400);
        view.opened();
        view.content.add_css_class("leaving");
        wait(160);
        frame(&view.window, "8-opening");
        assert!(view.badge.has_css_class("unlocked"));
    }
}
