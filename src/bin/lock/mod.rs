//! `nexus lock`: a session lock (ext-session-lock) covering every monitor.
//! The compositor keeps the session locked even if this process dies.
//! Each monitor shows a capture of itself that blurs in, under a card rising
//! from the bottom with the time and the password field.
mod pam;
use super::{label, screenshot::screencopy::Capturer, vbox};
use gtk::{gdk, gio, glib, prelude::*};
use gtk4_session_lock::Instance;
use nexus_control::theme;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

/// Matches the leaving animations in the theme.
const LEAVE: Duration = Duration::from_millis(220);
/// Locking never waits longer than this for the captures.
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Blur radius in pixels of the quarter-size copy the blurred layer is made from.
const BLUR: f32 = 9.;

/// `--preview` shows the lock in a plain fullscreen window, without locking
/// anything: Escape or the right password closes it. `NEXUS_LOCK_TEST=N`
/// locks for real but unlocks on its own after N seconds, to try the lock
/// without risking a stuck session.
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let preview = args.iter().any(|a| a == "--preview");
    log("start");
    let guard = nexus_control::instance_lock("lock");
    // Already locked: binding the shortcut and an idle daemon to it is fine.
    if !preview && guard.is_err() {
        return Ok(());
    }
    let test = std::env::var("NEXUS_LOCK_TEST")
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
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

fn log(text: &str) {
    thread_local!(static START: Instant = Instant::now());
    START.with(|s| eprintln!("[lock {:>7.3}s] {text}", s.elapsed().as_secs_f64()));
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

struct State {
    /// None in preview, where nothing is locked.
    instance: Option<Instance>,
    busy: Cell<bool>,
    views: RefCell<Vec<Rc<View>>>,
    /// Ends the lock: true once unlocked, false if locking failed.
    done: async_channel::Sender<bool>,
}
impl State {
    fn unlock(&self) {
        match &self.instance {
            Some(instance) => instance.unlock(),
            None => {
                for view in self.views.borrow_mut().drain(..) {
                    view.window.close();
                }
                let _ = self.done.try_send(true);
            }
        }
    }
    fn add(self: &Rc<Self>, app: &gtk::Application, shot: Option<&gdk::Texture>) -> Rc<View> {
        let view = View::new(self, shot);
        view.window.set_application(Some(app));
        self.views.borrow_mut().push(view.clone());
        view
    }
}

/// Locks until the session is unlocked; false if it could not be locked.
async fn start(app: &gtk::Application, preview: bool, test: Option<u32>) -> bool {
    // Connecting for the captures overlaps with loading the theme.
    let capturer = Capturer::start();
    if !preview && !gtk4_session_lock::is_supported() {
        eprintln!("This compositor does not support ext-session-lock");
        return false;
    }
    let display = gdk::Display::default().expect("no display");
    super::install_theme(&display, &theme::load());
    let shots = Rc::new(captures(&display, &capturer).await);
    log(&format!("captured {} monitor(s)", shots.len()));
    let (done, finished) = async_channel::bounded(1);
    // Handlers hold the state weakly; this task owns it until the end.
    let state = Rc::new(State {
        instance: (!preview).then(Instance::new),
        busy: Cell::new(false),
        views: RefCell::default(),
        done,
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
            let shot = display
                .monitors()
                .item(0)
                .and_downcast::<gdk::Monitor>()
                .and_then(|m| m.connector())
                .and_then(|c| shots.get(c.as_str()).cloned());
            let view = state.add(app, shot.as_ref());
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
                let view = state.add(&app, shots.get(name.as_str()));
                instance.assign_window_to_monitor(&view.window, monitor);
            });
            let done = state.done.clone();
            instance.connect_failed(move |_| {
                eprintln!("Could not lock the session; is another locker running?");
                let _ = done.try_send(false);
            });
            instance.connect_locked(|_| log("locked"));
            let weak = Rc::downgrade(&state);
            instance.connect_unlocked(move |_| {
                log("unlocked");
                if let Some(state) = weak.upgrade() {
                    state.views.borrow_mut().clear();
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

/// The lock surface of one monitor. Each has its own password field; the
/// compositor gives keyboard focus to one of them.
struct View {
    window: gtk::Window,
    shot: gtk::Overlay,
    card: gtk::Box,
    clock: gtk::Label,
    entry: gtk::PasswordEntry,
    message: gtk::Label,
}
impl View {
    fn new(state: &Rc<State>, shot: Option<&gdk::Texture>) -> Rc<Self> {
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
        layers.set_child(Some(&picture(shot)));
        let blurred = picture(None);
        layers.add_overlay(&blurred);
        overlay.set_child(Some(&layers));
        // Blurred once the window shows, so that blurring does not delay it;
        // the class starts its fade only then.
        if let Some(shot) = shot.cloned() {
            window.connect_map(move |window| {
                let (window, blurred, shot) = (window.clone(), blurred.clone(), shot.clone());
                glib::idle_add_local_once(move || {
                    if let Some(renderer) = window.renderer() {
                        blurred.set_paintable(Some(&blur(&renderer, &shot)));
                        blurred.add_css_class("lock-blurred");
                        log("blurred");
                    }
                });
            });
        }

        let card = vbox(0);
        card.add_css_class("lock-card");
        card.set_halign(gtk::Align::Center);
        card.set_valign(gtk::Align::Center);
        let clock = label("", "lock-clock");
        clock.set_xalign(0.5);
        card.append(&clock);
        // A real password entry: keyboard layouts, compose sequences, the
        // secure buffer and the Caps Lock indicator all work as usual.
        let entry = gtk::PasswordEntry::new();
        entry.add_css_class("lock-password");
        entry.set_placeholder_text(Some("Password"));
        card.append(&entry);
        let message = label("", "lock-message");
        message.set_xalign(0.5);
        message.set_justify(gtk::Justification::Center);
        card.append(&message);
        overlay.add_overlay(&card);
        window.set_child(Some(&overlay));

        let view = Rc::new(Self {
            window,
            shot: layers,
            card,
            clock,
            entry,
            message,
        });
        let weak = Rc::downgrade(&view);
        view.entry.connect_changed(move |_| {
            if let Some(view) = weak.upgrade() {
                if !view.card.has_css_class("checking") {
                    view.say("", false);
                }
                view.entry.remove_css_class("error");
            }
        });
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (weak, s) = (Rc::downgrade(&view), Rc::downgrade(state));
        // Enter and Escape are handled here, and typing reaches the field,
        // even when the compositor's focus did not land on it.
        key.connect_key_pressed(move |controller, key, _, _| {
            let (Some(view), Some(state)) = (weak.upgrade(), s.upgrade()) else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Escape => {
                    if state.instance.is_none() {
                        state.unlock();
                    } else if !state.busy.get() {
                        view.entry.set_text("");
                    }
                }
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
        view.tick();
        view
    }
    fn tick(&self) {
        let Ok(now) = glib::DateTime::now_local() else {
            return;
        };
        self.clock
            .set_text(&now.format("%H:%M").unwrap_or_default());
    }
    fn say(&self, text: &str, error: bool) {
        self.message.set_text(text);
        if error {
            self.message.add_css_class("error");
        } else {
            self.message.remove_css_class("error");
        }
    }
    fn checking(&self, on: bool) {
        self.entry.set_editable(!on);
        if on {
            self.card.add_css_class("checking");
            self.say("Checking…", false);
        } else {
            self.card.remove_css_class("checking");
            self.say("", false);
        }
    }
    fn refused(&self, error: String) {
        self.checking(false);
        // GTK replays an animation only when its name changes.
        let (add, remove) = if self.entry.has_css_class("shake-a") {
            ("shake-b", "shake-a")
        } else {
            ("shake-a", "shake-b")
        };
        self.entry.remove_css_class(remove);
        self.entry.add_css_class(add);
        self.entry.set_text("");
        self.entry.add_css_class("error");
        self.say(&error, true);
    }
    fn leave(&self) {
        self.card.add_css_class("leaving");
        self.shot.add_css_class("leaving");
    }
}
/// A blurred, quarter-size copy of `texture`.
fn blur(renderer: &gtk::gsk::Renderer, texture: &gdk::Texture) -> gdk::Texture {
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
    match snapshot.to_node() {
        Some(node) => renderer.render_texture(node, Some(&bounds)),
        None => texture.clone(),
    }
}
fn submit(state: &Rc<State>, view: &Rc<View>) {
    if state.busy.get() || view.entry.text().is_empty() {
        return;
    }
    state.busy.set(true);
    view.checking(true);
    let password = view.entry.text().to_string();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
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
    #[ignore = "requires an isolated display: GDK_BACKEND=x11 xvfb-run -a cargo test --bin nexus lock_view -- --ignored"]
    fn lock_view_follows_typing_and_results() {
        gtk::init().unwrap();
        // Never locks anything: there is no instance.
        let state = Rc::new(State {
            instance: None,
            busy: Cell::new(false),
            views: RefCell::default(),
            done: async_channel::bounded(1).0,
        });
        let view = View::new(&state, None);
        view.window.present();
        wait(300);
        view.entry.set_text("hunter2");
        view.checking(true);
        assert!(!view.entry.is_editable());
        view.refused("Wrong password".into());
        assert!(view.entry.is_editable());
        assert!(view.entry.text().is_empty());
        assert_eq!(view.message.text(), "Wrong password");
        assert!(view.entry.has_css_class("error"));
        // Typing again dismisses the error.
        view.entry.set_text("x");
        assert_eq!(view.message.text(), "");
        assert!(!view.entry.has_css_class("error"));
    }
}
