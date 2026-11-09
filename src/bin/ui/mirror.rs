//! `nexus phone mirror`: the phone's screen in a window, and the mouse and
//! keyboard on it. Live once the phone has agreed to share its screen; until
//! then screenshots, through which that agreement can be given from here.

use super::super::{caption, install_theme, vbox};
use super::phone::call;
use gtk::{gdk, gio, glib, prelude::*};
use nexus_control::{phone::CAST_SOCKET, phone_client::phone, runtime, theme};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    io::Read,
    os::unix::net::UnixListener,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const WAITING: &str = "Slow view. Accept screen sharing on the phone for a live one.";

pub fn run() -> anyhow::Result<()> {
    let app = gtk::Application::builder()
        .application_id("io.github.vitrixxl.NexusMirror")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(|app| {
        if let Some(display) = gdk::Display::default() {
            install_theme(&display, &theme::load());
        }
        Mirror::start(app);
    });
    app.run_with_args(&["nexus"]);
    Ok(())
}

/// Window position to phone pixels, through the letterboxing of `Contain`.
/// None on the bars around the image.
fn to_phone(
    (x, y): (f64, f64),
    widget: (f64, f64),
    image: (f64, f64),
    screen: (f64, f64),
) -> Option<(f64, f64)> {
    let scale = (widget.0 / image.0).min(widget.1 / image.1);
    let fx = (x - (widget.0 - image.0 * scale) / 2.0) / (image.0 * scale);
    let fy = (y - (widget.1 - image.1 * scale) / 2.0) / (image.1 * scale);
    ((0.0..1.0).contains(&fx) && (0.0..1.0).contains(&fy)).then_some((fx * screen.0, fy * screen.1))
}

struct Frame {
    texture: gdk::Texture,
    screen: (f64, f64),
}

struct Mirror {
    picture: gtk::Picture,
    status: gtk::Label,
    /// The latest screen's size in pixels: what taps are expressed in.
    screen: Cell<Option<(f64, f64)>>,
    live: Cell<bool>,
    asked: Cell<bool>,
    /// In order: two keys typed fast must not swap.
    actions: mpsc::Sender<(&'static str, Value)>,
}

impl Mirror {
    fn start(app: &gtk::Application) {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Phone")
            .default_width(400)
            .default_height(820)
            .build();
        window.add_css_class("nexus");
        window.add_css_class("mirror");
        let page = vbox(0);
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .focusable(true)
            .build();
        page.append(&picture);
        let status = caption("Connecting…");
        status.set_wrap(true);
        status.set_xalign(0.5);
        status.set_justify(gtk::Justification::Center);
        status.set_margin_top(6);
        page.append(&status);
        let (actions, errors) = worker();
        let keys = gtk::CenterBox::new();
        let nav = gtk::Box::new(gtk::Orientation::Horizontal, 28);
        nav.set_margin_top(4);
        nav.set_margin_bottom(4);
        for (icon, tip, key) in [
            ("go-previous-symbolic", "Back", "back"),
            ("go-home-symbolic", "Home", "home"),
            ("view-grid-symbolic", "Recent apps", "recents"),
        ] {
            let b = gtk::Button::from_icon_name(icon);
            b.add_css_class("flat");
            b.set_tooltip_text(Some(tip));
            let tx = actions.clone();
            b.connect_clicked(move |_| {
                let _ = tx.send(("screen_key", json!({ "key": key })));
            });
            nav.append(&b);
        }
        keys.set_center_widget(Some(&nav));
        page.append(&keys);
        window.set_child(Some(&page));
        let m = Rc::new(Self {
            picture,
            status,
            screen: Cell::new(None),
            live: Cell::new(false),
            asked: Cell::new(false),
            actions,
        });
        let p = m.clone();
        glib::spawn_future_local(async move {
            while let Ok(e) = errors.recv().await {
                p.say(&e, true);
            }
        });
        m.input(&window);
        m.stream();
        // The first screen, then one for as long as there is no stream.
        let p = m.clone();
        let busy = Rc::new(Cell::new(false));
        let shown = window.downgrade();
        glib::timeout_add_local(Duration::from_millis(350), move || {
            if shown.upgrade().is_none() {
                return glib::ControlFlow::Break;
            }
            if !p.live.get() && !busy.replace(true) {
                let (q, busy) = (p.clone(), busy.clone());
                call("screen_shot", json!({ "max": 900, "q": 60 }), move |r| {
                    busy.set(false);
                    q.shot(r);
                });
            }
            glib::ControlFlow::Continue
        });
        window.present();
        m.picture.grab_focus();
    }

    fn say(&self, text: &str, error: bool) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
        if error {
            self.status.add_css_class("error");
        } else {
            self.status.remove_css_class("error");
        }
    }

    fn shot(self: &Rc<Self>, r: Result<Value, String>) {
        let image = r.and_then(|v| {
            let jpeg = glib::base64_decode(v["b64"].as_str().unwrap_or_default());
            let texture = gdk::Texture::from_bytes(&glib::Bytes::from_owned(jpeg))
                .map_err(|e| e.to_string())?;
            let screen = (
                v["w"].as_f64().unwrap_or(1.0),
                v["h"].as_f64().unwrap_or(1.0),
            );
            Ok(Frame { texture, screen })
        });
        // A stream may have begun while this one travelled.
        if self.live.get() {
            return;
        }
        match image {
            Ok(frame) => {
                self.show(&frame);
                self.say(WAITING, false);
                self.ask_to_stream();
            }
            Err(e) => self.say(&e, true),
        }
    }

    fn show(&self, frame: &Frame) {
        self.picture.set_paintable(Some(&frame.texture));
        self.screen.set(Some(frame.screen));
    }

    /// Frames the phone sends, decoded away from the interface.
    fn stream(self: &Rc<Self>) {
        let bound = runtime().and_then(|d| {
            let path = d.join(CAST_SOCKET);
            let _ = std::fs::remove_file(&path);
            Ok(UnixListener::bind(path)?)
        });
        let listener = match bound {
            Ok(l) => l,
            Err(e) => return self.say(&e.to_string(), true),
        };
        let (tx, rx) = async_channel::bounded::<Option<Frame>>(1);
        thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                while let Some(frame) = next_frame(&mut conn) {
                    if tx.send_blocking(Some(frame)).is_err() {
                        return;
                    }
                }
                if tx.send_blocking(None).is_err() {
                    return;
                }
            }
        });
        let p = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(frame) = rx.recv().await {
                p.live.set(frame.is_some());
                match frame {
                    Some(f) => {
                        p.show(&f);
                        p.say("", false);
                    }
                    None => p.say(WAITING, false),
                }
            }
        });
    }

    /// Once, when the phone answers: it may not be linked yet at launch, and
    /// a refusal on the phone is not to be asked again and again.
    fn ask_to_stream(self: &Rc<Self>) {
        if self.asked.replace(true) {
            return;
        }
        let p = self.clone();
        call("cast_start", json!({}), move |r| {
            if let Err(e) = r {
                p.say(&e, true);
            }
        });
    }

    fn at(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let image = self.picture.paintable()?;
        to_phone(
            (x, y),
            (self.picture.width() as f64, self.picture.height() as f64),
            (
                image.intrinsic_width() as f64,
                image.intrinsic_height() as f64,
            ),
            self.screen.get()?,
        )
    }

    fn act(&self, op: &'static str, args: Value) {
        let _ = self.actions.send((op, args));
    }

    fn input(self: &Rc<Self>, window: &gtk::ApplicationWindow) {
        let drag = gtk::GestureDrag::new();
        let began = Rc::new(Cell::new(Instant::now()));
        let b = began.clone();
        let p = self.clone();
        drag.connect_drag_begin(move |_, _, _| {
            b.set(Instant::now());
            p.picture.grab_focus();
        });
        let p = self.clone();
        drag.connect_drag_end(move |g, dx, dy| {
            let Some((x0, y0)) = g.start_point() else {
                return;
            };
            let Some((x, y)) = p.at(x0, y0) else {
                return;
            };
            let held = began.get().elapsed().as_millis() as u64;
            if dx.hypot(dy) < 8.0 {
                let op = if held > 500 {
                    "screen_hold"
                } else {
                    "screen_tap"
                };
                p.act(op, json!({ "x": x, "y": y }));
            } else if let Some((x2, y2)) = p.at(x0 + dx, y0 + dy) {
                p.act(
                    "screen_swipe",
                    json!({ "x": x, "y": y, "x2": x2, "y2": y2, "ms": held.clamp(80, 2000) }),
                );
            }
        });
        self.picture.add_controller(drag);

        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let turned = RefCell::new(0.0);
        let p = self.clone();
        wheel.connect_scroll(move |_, _, dy| {
            let mut total = turned.borrow_mut();
            *total += dy;
            if total.abs() < 1.0 {
                return glib::Propagation::Stop;
            }
            if let Some((w, h)) = p.screen.get() {
                let step = h / 4.0 * total.signum();
                p.act(
                    "screen_swipe",
                    json!({ "x": w / 2.0, "y": h / 2.0 + step, "x2": w / 2.0, "y2": h / 2.0 - step, "ms": 180 }),
                );
            }
            *total = 0.0;
            glib::Propagation::Stop
        });
        self.picture.add_controller(wheel);

        let keys = gtk::EventControllerKey::new();
        let p = self.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            if state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
                return glib::Propagation::Proceed;
            }
            let named = match key {
                gdk::Key::Escape => Some("back"),
                gdk::Key::BackSpace => Some("backspace"),
                gdk::Key::Return | gdk::Key::KP_Enter => Some("enter"),
                _ => None,
            };
            if let Some(k) = named {
                p.act("screen_key", json!({ "key": k }));
            } else if let Some(c) = key.to_unicode().filter(|c| !c.is_control()) {
                p.act("screen_text", json!({ "text": c.to_string() }));
            } else {
                return glib::Propagation::Proceed;
            }
            glib::Propagation::Stop
        });
        window.add_controller(keys);
    }
}

/// Runs what the window asks for, one at a time; hands back what failed.
fn worker() -> (
    mpsc::Sender<(&'static str, Value)>,
    async_channel::Receiver<String>,
) {
    let (tx, rx) = mpsc::channel::<(&'static str, Value)>();
    let (failed, errors) = async_channel::bounded(8);
    thread::spawn(move || {
        for (op, args) in rx {
            if let Err(e) = phone(op, args) {
                let _ = failed.try_send(e);
            }
        }
    });
    (tx, errors)
}

/// [length, screen width, screen height], big-endian, then the JPEG.
fn next_frame(conn: &mut impl Read) -> Option<Frame> {
    let mut head = [0u8; 12];
    conn.read_exact(&mut head).ok()?;
    let word = |i: usize| u32::from_be_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
    let (len, w, h) = (word(0) as usize, word(4), word(8));
    if len > 16 << 20 {
        return None;
    }
    let mut jpeg = vec![0; len];
    conn.read_exact(&mut jpeg).ok()?;
    let texture = gdk::Texture::from_bytes(&glib::Bytes::from_owned(jpeg)).ok()?;
    Some(Frame {
        texture,
        screen: (w as f64, h as f64),
    })
}

#[cfg(test)]
mod tests {
    use super::to_phone;

    #[test]
    fn positions_skip_the_letterbox_and_scale_to_the_screen() {
        // A 100x200 image in a 200x200 widget: 50 px bars on each side.
        let at = |x, y| to_phone((x, y), (200.0, 200.0), (100.0, 200.0), (1080.0, 2160.0));
        assert_eq!(at(40.0, 100.0), None);
        assert_eq!(at(50.0, 0.0), Some((0.0, 0.0)));
        assert_eq!(at(100.0, 100.0), Some((540.0, 1080.0)));
        assert_eq!(at(150.0, 100.0), None);
    }
}
