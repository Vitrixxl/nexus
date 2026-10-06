//! Screenshots: a picker over every monitor, on a frozen or a live screen, and
//! direct captures of the focused screen or the active window. The picker's
//! windows stay realized between captures and outputs are copied in memory, so
//! a shortcut shows the picker within a frame or two. The same picker chooses
//! what wf-recorder records.
mod screencopy;
use anyhow::{Context, Result, ensure};
use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use image::{
    ExtendedColorType, ImageEncoder, RgbImage,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use nexus_control::notifications::{Image, Notification, Pixels, Server, Urgency};
use screencopy::{Capturer, Frame};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    rc::{Rc, Weak},
    thread,
    time::{Duration, Instant},
};

/// x, y, width, height in logical pixels of one monitor.
type Rect = [f64; 4];
/// Width of the accent ring, drawn just outside the selection.
const RING: f64 = 2.;
/// Seconds for the dimming to come in and, once a selection is made, to go.
const FADE_IN: f64 = 0.12;
const FADE_OUT: f64 = 0.16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Region,
    Screen,
    Window,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    pub mode: Mode,
    /// Pick on the moving screen and capture on release, rather than freezing it.
    pub live: bool,
    /// Keep a file in ~/Pictures/screenshots besides the clipboard copy.
    pub save: bool,
    /// Record the picked area to ~/Videos/recordings, or stop the recording.
    pub record: bool,
}
impl Options {
    pub fn parse(args: &str) -> Result<Self, String> {
        let mut o = Self {
            mode: Mode::Region,
            live: false,
            save: false,
            record: false,
        };
        for arg in args.split_whitespace() {
            match arg {
                "region" => o.mode = Mode::Region,
                "screen" => o.mode = Mode::Screen,
                "window" => o.mode = Mode::Window,
                "--live" => o.live = true,
                "--save" => o.save = true,
                // Picked on the live screen, as that is what gets recorded.
                "--record" => (o.record, o.live) = (true, true),
                _ => return Err(format!("Unknown screenshot option: {arg}")),
            }
        }
        Ok(o)
    }
}

/// A request on Hyprland's command socket, without starting hyprctl.
fn hypr(command: &str) -> Result<Value> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .zip(std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE"))
        .map(|(p, id)| p.join("hypr").join(id).join(".socket.sock"))
        .context("Hyprland is not running")?;
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(format!("j/{command}").as_bytes())?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    Ok(serde_json::from_slice(&reply)?)
}
fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.)
}
fn contains(r: Rect, p: (f64, f64)) -> bool {
    p.0 >= r[0] && p.1 >= r[1] && p.0 < r[0] + r[2] && p.1 < r[1] + r[3]
}
/// The part of a rectangle on a monitor of this size.
fn within(r: Rect, size: (f64, f64)) -> Rect {
    let (x, y) = (r[0].clamp(0., size.0), r[1].clamp(0., size.1));
    [
        x,
        y,
        (r[0] + r[2]).clamp(x, size.0) - x,
        (r[1] + r[3]).clamp(y, size.1) - y,
    ]
}
/// The device pixels covering a logical rectangle, for a frame of `pixels`.
fn device_box(r: Rect, size: (f64, f64), pixels: (u32, u32)) -> (u32, u32, u32, u32) {
    let r = within(r, size);
    let (sx, sy) = (pixels.0 as f64 / size.0, pixels.1 as f64 / size.1);
    // A hair inside each edge, so float noise cannot widen the box by a pixel.
    let left = (r[0] * sx + 1e-6).floor();
    let top = (r[1] * sy + 1e-6).floor();
    let right = ((r[0] + r[2]) * sx - 1e-6)
        .ceil()
        .clamp(left, pixels.0 as f64);
    let bottom = ((r[1] + r[3]) * sy - 1e-6)
        .ceil()
        .clamp(top, pixels.1 as f64);
    (
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    )
}
/// Windows shown on a monitor, relative to it, the topmost first.
fn windows(monitor: &Value, clients: &Value) -> Vec<Rect> {
    let special = &monitor["specialWorkspace"];
    let workspace = if special["name"].as_str().is_some_and(|n| !n.is_empty()) {
        &special["id"]
    } else {
        &monitor["activeWorkspace"]["id"]
    };
    let mut list: Vec<&Value> = clients
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| {
            c["mapped"] == true
                && c["hidden"] != true
                && (&c["workspace"]["id"] == workspace || c["pinned"] == true)
        })
        .collect();
    // Pinned, fullscreen and floating windows lie above tiled ones, then the
    // most recently focused above the others.
    list.sort_by_key(|c| {
        (
            c["pinned"] != true,
            num(&c["fullscreen"]) == 0.,
            c["floating"] != true,
            c["focusHistoryID"].as_i64().unwrap_or(i64::MAX),
        )
    });
    list.iter()
        .map(|c| {
            [
                num(&c["at"][0]) - num(&monitor["x"]),
                num(&c["at"][1]) - num(&monitor["y"]),
                num(&c["size"][0]),
                num(&c["size"][1]),
            ]
        })
        .collect()
}
fn gdk_monitor(name: &str) -> Option<gdk::Monitor> {
    let list = gdk::Display::default()?.monitors();
    (0..list.n_items())
        .filter_map(|i| list.item(i).and_downcast::<gdk::Monitor>())
        .find(|m| m.connector().as_deref() == Some(name))
}
fn logical_size(monitor: &gdk::Monitor) -> (f64, f64) {
    let g = monitor.geometry();
    (g.width() as f64, g.height() as f64)
}

/// The picker on one monitor.
struct View {
    shot: Weak<Screenshot>,
    monitor: gdk::Monitor,
    window: gtk::Window,
    picture: gtk::Picture,
    layer: gtk::Fixed,
    selection: gtk::Box,
    name: RefCell<String>,
    /// Position of the monitor in the layout, in logical pixels.
    origin: Cell<(f64, f64)>,
    size: Cell<(f64, f64)>,
    /// The frozen screen, if the picker is not live.
    frame: RefCell<Option<Frame>>,
    windows: RefCell<Vec<Rect>>,
    current: Cell<Rect>,
    target: Cell<Rect>,
    on_window: Cell<bool>,
    drag: Cell<Option<(f64, f64)>>,
    dragging: Cell<bool>,
    pointer: Cell<Option<(f64, f64)>>,
    opacity: Cell<f64>,
    closing: Cell<bool>,
    ticking: Cell<bool>,
    last_tick: Cell<Option<i64>>,
}
impl View {
    fn new(shot: &Rc<Screenshot>, monitor: &gdk::Monitor) -> Rc<Self> {
        let window = gtk::Window::builder().application(&shot.app).build();
        window.add_css_class("screenshot");
        window.init_layer_shell();
        window.set_namespace(Some("nexus-screenshot"));
        window.set_layer(Layer::Overlay);
        window.set_monitor(Some(monitor));
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        window.set_exclusive_zone(-1);
        window.set_cursor_from_name(Some("crosshair"));
        let overlay = gtk::Overlay::new();
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Fill);
        picture.set_can_shrink(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        overlay.set_child(Some(&picture));
        let layer = gtk::Fixed::new();
        layer.set_can_target(false);
        let selection = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        selection.add_css_class("screenshot-selection");
        layer.put(&selection, 0., 0.);
        overlay.add_overlay(&layer);
        window.set_child(Some(&overlay));
        let view = Rc::new(Self {
            shot: Rc::downgrade(shot),
            monitor: monitor.clone(),
            window,
            picture,
            layer,
            selection,
            name: RefCell::default(),
            origin: Cell::default(),
            size: Cell::new(logical_size(monitor)),
            frame: RefCell::default(),
            windows: RefCell::default(),
            current: Cell::new([0.; 4]),
            target: Cell::new([0.; 4]),
            on_window: Cell::new(false),
            drag: Cell::new(None),
            dragging: Cell::new(false),
            pointer: Cell::new(None),
            opacity: Cell::new(0.),
            closing: Cell::new(false),
            ticking: Cell::new(false),
            last_tick: Cell::new(None),
        });

        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&view);
        let moved = move |x: f64, y: f64| {
            if let Some(v) = weak.upgrade() {
                v.pointer.set(Some((x, y)));
                if let Some(shot) = v.shot.upgrade() {
                    *shot.focused.borrow_mut() = Rc::downgrade(&v);
                }
                if v.drag.get().is_none() {
                    v.hover((x, y));
                }
            }
        };
        let entered = moved.clone();
        motion.connect_enter(move |_, x, y| entered(x, y));
        motion.connect_motion(move |_, x, y| moved(x, y));
        overlay.add_controller(motion);

        let drag = gtk::GestureDrag::new();
        let weak = Rc::downgrade(&view);
        drag.connect_drag_begin(move |_, x, y| {
            if let Some(v) = weak.upgrade() {
                v.hover((x, y));
                v.drag.set(Some((x, y)));
                v.dragging.set(false);
            }
        });
        let weak = Rc::downgrade(&view);
        drag.connect_drag_update(move |_, dx, dy| {
            if let Some(v) = weak.upgrade() {
                v.drag_to(dx, dy);
            }
        });
        let weak = Rc::downgrade(&view);
        drag.connect_drag_end(move |_, dx, dy| {
            if let Some(v) = weak.upgrade()
                && v.drag.get().is_some()
            {
                v.drag_to(dx, dy);
                v.drag.set(None);
                let rect = v.target.get();
                if let Some(shot) = v.shot.upgrade()
                    && rect[2] >= 1.
                    && rect[3] >= 1.
                {
                    shot.finish(&v, rect);
                }
            }
        });
        overlay.add_controller(drag);

        let cancel = gtk::GestureClick::new();
        cancel.set_button(gdk::BUTTON_SECONDARY);
        let weak = Rc::downgrade(shot);
        cancel.connect_pressed(move |_, _, _, _| {
            if let Some(shot) = weak.upgrade() {
                shot.cancel();
            }
        });
        overlay.add_controller(cancel);

        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(shot);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(shot) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Escape => shot.cancel(),
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    let focused = shot.focused.borrow().upgrade();
                    if let Some(v) = focused {
                        shot.finish(&v, v.target.get());
                    }
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        view.window.add_controller(keys);
        view
    }
    /// Prepares a new pick from Hyprland's view of the monitor.
    fn reset(self: &Rc<Self>, monitor: &Value, clients: &Value, cursor: &Value) {
        let size = logical_size(&self.monitor);
        self.size.set(size);
        self.origin.set((num(&monitor["x"]), num(&monitor["y"])));
        *self.windows.borrow_mut() = windows(monitor, clients);
        self.drag.set(None);
        self.dragging.set(false);
        self.closing.set(false);
        self.opacity.set(0.);
        self.layer.set_opacity(0.);
        let p = (
            num(&cursor["x"]) - num(&monitor["x"]),
            num(&cursor["y"]) - num(&monitor["y"]),
        );
        let inside = contains([0., 0., size.0, size.1], p);
        self.pointer.set(inside.then_some(p));
        self.target.set([0., 0., size.0, size.1]);
        self.set_on_window(false);
        self.hover(if inside { p } else { (-1., -1.) });
        // Start on the window under the cursor rather than sliding to it.
        self.current.set(self.target.get());
        self.place();
    }
    fn set_on_window(&self, on: bool) {
        self.on_window.set(on);
        if on {
            self.selection.add_css_class("window");
        } else {
            self.selection.remove_css_class("window");
        }
    }
    /// Selects the window under the pointer, or the whole monitor outside them.
    fn hover(self: &Rc<Self>, p: (f64, f64)) {
        if self.closing.get() {
            return;
        }
        let size = self.size.get();
        let hit = self
            .windows
            .borrow()
            .iter()
            .copied()
            .find(|r| contains(*r, p));
        let (rect, on) = match hit {
            Some(r) => (within(r, size), true),
            None => ([0., 0., size.0, size.1], false),
        };
        if rect != self.target.get() || on != self.on_window.get() {
            self.target.set(rect);
            self.set_on_window(on);
            self.animate();
        }
    }
    fn drag_to(&self, dx: f64, dy: f64) {
        let Some(start) = self.drag.get() else { return };
        if !self.dragging.get() && dx.hypot(dy) < 3. {
            return;
        }
        self.dragging.set(true);
        self.set_on_window(false);
        let p = (start.0 + dx, start.1 + dy);
        let rect = within(
            [
                start.0.min(p.0),
                start.1.min(p.1),
                (p.0 - start.0).abs(),
                (p.1 - start.1).abs(),
            ],
            self.size.get(),
        );
        // The selection follows the pointer exactly while dragging.
        self.target.set(rect);
        self.current.set(rect);
        self.place();
    }
    fn place(&self) {
        let r = self.current.get();
        self.layer.move_(&self.selection, r[0] - RING, r[1] - RING);
        self.selection.set_size_request(
            (r[2] + 2. * RING).round() as i32,
            (r[3] + 2. * RING).round() as i32,
        );
    }
    /// Runs the animation on the frame clock until everything has settled.
    fn animate(self: &Rc<Self>) {
        if self.ticking.replace(true) {
            return;
        }
        self.last_tick.set(None);
        let weak = Rc::downgrade(self);
        self.layer.add_tick_callback(move |_, clock| {
            let Some(v) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let now = clock.frame_time();
            let dt = v
                .last_tick
                .replace(Some(now))
                .map_or(0., |t| ((now - t) as f64 / 1e6).min(0.1));
            if v.step(dt) {
                glib::ControlFlow::Continue
            } else {
                v.ticking.set(false);
                glib::ControlFlow::Break
            }
        });
    }
    /// Advances the animation; whether it goes on.
    fn step(self: &Rc<Self>, dt: f64) -> bool {
        let closing = self.closing.get();
        let opacity = if closing {
            (self.opacity.get() - dt / FADE_OUT).max(0.)
        } else {
            (self.opacity.get() + dt / FADE_IN).min(1.)
        };
        self.opacity.set(opacity);
        self.layer.set_opacity(opacity);
        let mut settled = true;
        if self.drag.get().is_none() || closing {
            let rate = if closing { 32. } else { 18. };
            let f = 1. - (-rate * dt).exp();
            let target = self.target.get();
            let mut current = self.current.get();
            for (a, b) in current.iter_mut().zip(target) {
                *a += (b - *a) * f;
                if (*a - b).abs() < 0.05 {
                    *a = b;
                }
                settled &= *a == b;
            }
            self.current.set(current);
            self.place();
        }
        if closing && opacity == 0. {
            self.hide();
            if let Some(shot) = self.shot.upgrade() {
                shot.closed();
            }
            return false;
        }
        !settled || (!closing && opacity < 1.) || closing
    }
    fn show(self: &Rc<Self>) {
        self.window.set_keyboard_mode(KeyboardMode::Exclusive);
        self.window.present();
        self.animate();
    }
    /// Fades the dimming out while the selection opens onto the whole screen.
    fn close(self: &Rc<Self>) {
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.closing.set(true);
        let size = self.size.get();
        self.target.set([0., 0., size.0, size.1]);
        self.animate();
    }
    fn hide(&self) {
        self.closing.set(true);
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.window.set_visible(false);
        self.picture.set_paintable(None::<&gdk::Paintable>);
        self.frame.borrow_mut().take();
    }
}

/// A screen recording in progress.
struct Recording {
    recorder: Child,
    path: PathBuf,
}
/// Reports when a recording starts, with its start time, and when it stops.
pub type RecordingChanged = Box<dyn Fn(Option<Instant>)>;
pub struct Screenshot {
    app: gtk::Application,
    recording: RefCell<Option<Recording>>,
    on_recording: RefCell<Option<RecordingChanged>>,
    server: Rc<Server>,
    capturer: Capturer,
    views: RefCell<Vec<Rc<View>>>,
    /// Views of the pick in progress.
    active: RefCell<Vec<Rc<View>>>,
    options: Cell<Option<Options>>,
    finishing: Cell<bool>,
    focused: RefCell<Weak<View>>,
    refresh: RefCell<Option<glib::SourceId>>,
    rounding: gtk::CssProvider,
    radius: Cell<f64>,
}
impl Screenshot {
    pub fn new(app: &gtk::Application, server: Rc<Server>) -> Rc<Self> {
        let rounding = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &rounding,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }
        let this = Rc::new(Self {
            app: app.clone(),
            recording: RefCell::default(),
            on_recording: RefCell::default(),
            server,
            capturer: Capturer::start(),
            views: RefCell::default(),
            active: RefCell::default(),
            options: Cell::new(None),
            finishing: Cell::new(false),
            focused: RefCell::default(),
            refresh: RefCell::default(),
            rounding,
            radius: Cell::new(-1.),
        });
        // Realize the windows ahead of the first capture.
        let weak = Rc::downgrade(&this);
        glib::idle_add_local_once(move || {
            if let Some(s) = weak.upgrade() {
                for v in s.sync_views() {
                    WidgetExt::realize(&v.window);
                }
            }
        });
        this
    }
    /// One view per connected monitor, keeping the existing ones.
    fn sync_views(self: &Rc<Self>) -> Vec<Rc<View>> {
        let Some(display) = gdk::Display::default() else {
            return vec![];
        };
        let list = display.monitors();
        let monitors: Vec<gdk::Monitor> = (0..list.n_items())
            .filter_map(|i| list.item(i).and_downcast::<gdk::Monitor>())
            .filter(|m| m.is_valid())
            .collect();
        let mut views = self.views.borrow_mut();
        views.retain(|v| {
            let keep = monitors.contains(&v.monitor);
            if !keep {
                v.window.destroy();
            }
            keep
        });
        for m in &monitors {
            if !views.iter().any(|v| &v.monitor == m) {
                views.push(View::new(self, m));
            }
        }
        views.clone()
    }
    fn set_radius(&self, radius: f64) {
        if self.radius.replace(radius) != radius {
            self.rounding.load_from_data(&format!(
                ".screenshot-selection.window {{ border-radius: {}px; }}",
                radius + RING
            ));
        }
    }
    pub fn connect_recording(&self, changed: impl Fn(Option<Instant>) + 'static) {
        *self.on_recording.borrow_mut() = Some(Box::new(changed));
    }
    fn recording_changed(&self, started: Option<Instant>) {
        if let Some(changed) = self.on_recording.borrow().as_ref() {
            changed(started);
        }
    }
    /// The shortcut again while picking cancels, as it closes the other panels.
    /// While recording, a record request stops the recording.
    pub fn take(self: &Rc<Self>, options: Options) {
        if options.record && self.stop_recording() {
            return;
        }
        if self.options.get().is_some() {
            if !self.active.borrow().is_empty() {
                self.cancel();
            }
            return;
        }
        self.options.set(Some(options));
        self.finishing.set(false);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = match options.mode {
                Mode::Region => this.pick(options).await,
                _ if options.record => this.pick(options).await,
                _ => this.capture_now(options).await,
            };
            if let Err(e) = result {
                this.end();
                this.fail(&format!("{e:#}"));
            }
        });
    }
    async fn capture_now(self: &Rc<Self>, options: Options) -> Result<()> {
        let monitors = hypr("monitors")?;
        let list = monitors.as_array().context("Hyprland lists no monitors")?;
        let (monitor, rect) = if options.mode == Mode::Window {
            let window = hypr("activewindow")?;
            ensure!(window["mapped"] == true, "No window is active");
            let monitor = list
                .iter()
                .find(|m| m["id"] == window["monitor"])
                .context("The active window is on no monitor")?;
            let rect = [
                num(&window["at"][0]) - num(&monitor["x"]),
                num(&window["at"][1]) - num(&monitor["y"]),
                num(&window["size"][0]),
                num(&window["size"][1]),
            ];
            (monitor, Some(rect))
        } else {
            let focused = list.iter().find(|m| m["focused"] == true);
            (focused.or(list.first()).context("No monitor")?, None)
        };
        let name = monitor["name"]
            .as_str()
            .context("Unnamed monitor")?
            .to_string();
        let size = logical_size(&gdk_monitor(&name).context("Unknown monitor")?);
        let frame = self
            .capturer
            .capture(vec![name])
            .await
            .map_err(anyhow::Error::msg)?
            .remove(0);
        self.end();
        self.publish(
            frame,
            size,
            rect.unwrap_or([0., 0., size.0, size.1]),
            options.save,
        );
        Ok(())
    }
    async fn pick(self: &Rc<Self>, options: Options) -> Result<()> {
        let monitors = hypr("monitors")?;
        let clients = hypr("clients")?;
        let cursor = hypr("cursorpos")?;
        let radius = hypr("getoption decoration:rounding").map_or(0., |v| num(&v["int"]));
        self.set_radius(radius);
        let mut views = vec![];
        for view in self.sync_views() {
            let name = view.monitor.connector().map(|c| c.to_string());
            let Some(monitor) = monitors
                .as_array()
                .into_iter()
                .flatten()
                .find(|m| m["name"].as_str() == name.as_deref())
            else {
                continue;
            };
            *view.name.borrow_mut() = name.unwrap_or_default();
            view.reset(monitor, &clients, &cursor);
            if view.pointer.get().is_some() {
                *self.focused.borrow_mut() = Rc::downgrade(&view);
            }
            views.push(view);
        }
        ensure!(!views.is_empty(), "No monitor to capture");
        if !options.live {
            let names = views.iter().map(|v| v.name.borrow().clone()).collect();
            let frames = self
                .capturer
                .capture(names)
                .await
                .map_err(anyhow::Error::msg)?;
            for (view, frame) in views.iter().zip(frames) {
                let texture = gdk::MemoryTexture::new(
                    frame.width as i32,
                    frame.height as i32,
                    gdk::MemoryFormat::B8g8r8a8,
                    &frame.data,
                    frame.stride as usize,
                );
                view.picture.set_paintable(Some(&texture));
                *view.frame.borrow_mut() = Some(frame);
            }
        }
        for view in &views {
            view.picture.set_visible(!options.live);
            view.show();
        }
        if options.live {
            // Windows keep moving under a live picker.
            let weak = Rc::downgrade(self);
            let id = glib::timeout_add_local(Duration::from_millis(250), move || {
                let Some(s) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                if let (Ok(monitors), Ok(clients)) = (hypr("monitors"), hypr("clients")) {
                    for v in s.active.borrow().iter() {
                        let name = v.name.borrow();
                        if let Some(m) = monitors
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|m| m["name"].as_str() == Some(name.as_str()))
                        {
                            *v.windows.borrow_mut() = windows(m, &clients);
                            if let (None, Some(p)) = (v.drag.get(), v.pointer.get()) {
                                v.hover(p);
                            }
                        }
                    }
                }
                glib::ControlFlow::Continue
            });
            *self.refresh.borrow_mut() = Some(id);
        }
        *self.active.borrow_mut() = views;
        Ok(())
    }
    fn stop_refresh(&self) {
        if let Some(id) = self.refresh.borrow_mut().take() {
            id.remove();
        }
    }
    fn finish(self: &Rc<Self>, view: &Rc<View>, rect: Rect) {
        let Some(options) = self.options.get() else {
            return;
        };
        if self.finishing.replace(true) {
            return;
        }
        self.stop_refresh();
        let frame = view.frame.borrow().clone();
        let size = view.size.get();
        if let Some(frame) = frame {
            // The image is already here: export it while the picker fades out.
            self.publish(frame, size, rect, options.save);
            for v in self.active.borrow().iter() {
                v.close();
            }
            return;
        }
        // Live: the picker must be off the screen before the screen is copied.
        for v in self.active.borrow().iter() {
            v.hide();
        }
        self.active.borrow_mut().clear();
        if let Some(display) = gdk::Display::default() {
            display.sync();
        }
        if options.record {
            self.end();
            if let Err(e) = self.start_recording(view, rect) {
                self.recording_failed(&format!("{e:#}"));
            }
            return;
        }
        let name = view.name.borrow().clone();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = this.capturer.capture(vec![name]).await;
            this.end();
            match result {
                Ok(mut frames) => this.publish(frames.remove(0), size, rect, options.save),
                Err(e) => this.fail(&e),
            }
        });
    }
    fn cancel(self: &Rc<Self>) {
        if self.finishing.get() {
            return;
        }
        for v in self.active.borrow().iter() {
            v.hide();
        }
        self.end();
    }
    /// A view finished fading out.
    fn closed(&self) {
        if self.active.borrow().iter().all(|v| !v.window.is_visible()) {
            self.end();
        }
    }
    fn end(&self) {
        self.stop_refresh();
        for v in self.active.borrow_mut().drain(..) {
            v.hide();
        }
        self.options.set(None);
        self.finishing.set(false);
    }
    /// Records `rect` of the view's monitor, the whole monitor by name so that
    /// wf-recorder follows it as is.
    fn start_recording(self: &Rc<Self>, view: &View, rect: Rect) -> Result<()> {
        let size = view.size.get();
        let mut recorder = Command::new("wf-recorder");
        if rect == [0., 0., size.0, size.1] {
            recorder.args(["-o", view.name.borrow().as_str()]);
        } else {
            let (x, y) = view.origin.get();
            // H.264 wants even dimensions.
            let even = |v: f64| ((v.round() as i64) & !1).max(2);
            recorder.args([
                "-g",
                &format!(
                    "{},{} {}x{}",
                    (x + rect[0]).round(),
                    (y + rect[1]).round(),
                    even(rect[2]),
                    even(rect[3])
                ),
            ]);
        }
        let folder = glib::user_special_dir(glib::UserDirectory::Videos)
            .unwrap_or_else(|| glib::home_dir().join("Videos"))
            .join("recordings");
        std::fs::create_dir_all(&folder)?;
        let path = unique(&folder, "Recording", "mp4");
        let child = recorder
            .arg("-f")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("wf-recorder is not installed")?;
        *self.recording.borrow_mut() = Some(Recording {
            recorder: child,
            path,
        });
        self.recording_changed(Some(Instant::now()));
        // wf-recorder fails at once when it cannot capture or encode.
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(600), move || {
            let Some(this) = weak.upgrade() else { return };
            let exited = this
                .recording
                .borrow_mut()
                .as_mut()
                .and_then(|r| r.recorder.try_wait().ok().flatten());
            if let Some(status) = exited {
                this.recording.borrow_mut().take();
                this.recording_changed(None);
                this.recording_failed(&format!("wf-recorder exited: {status}"));
            }
        });
        Ok(())
    }
    /// Ends the recording in progress, if any, and reports the video once
    /// wf-recorder has written it out.
    fn stop_recording(self: &Rc<Self>) -> bool {
        let Some(Recording { mut recorder, path }) = self.recording.borrow_mut().take() else {
            return false;
        };
        self.recording_changed(None);
        let (tx, rx) = async_channel::bounded(1);
        thread::spawn(move || {
            // SIGINT lets wf-recorder finish the file; SIGKILL would leave it unreadable.
            let _ = Command::new("kill")
                .args(["-INT", &recorder.id().to_string()])
                .status();
            let _ = recorder.wait();
            let _ = tx.send_blocking(());
        });
        let this = self.clone();
        glib::spawn_future_local(async move {
            let _ = rx.recv().await;
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                this.notify(
                    "Recording saved",
                    &format!("Saved as {}", glib::markup_escape_text(&name)),
                    None,
                );
            } else {
                this.recording_failed("The recording is empty");
            }
        });
        true
    }
    /// Crops, encodes and copies in a worker thread, then reports it.
    fn publish(self: &Rc<Self>, frame: Frame, size: (f64, f64), rect: Rect, save: bool) {
        let area = device_box(rect, size, (frame.width, frame.height));
        let (tx, rx) = async_channel::bounded(1);
        thread::spawn(move || {
            let _ = tx.send_blocking(export(&frame, area, save).map_err(|e| format!("{e:#}")));
        });
        let this = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(shot)) => this.report(shot),
                Ok(Err(e)) => this.fail(&e),
                Err(_) => {}
            }
        });
    }
    fn report(&self, shot: Exported) {
        let body = match &shot.path {
            Some(path) => format!(
                "Copied to the clipboard and saved as {}",
                glib::markup_escape_text(&path.file_name().unwrap_or_default().to_string_lossy())
            ),
            None => "Copied to the clipboard".into(),
        };
        let (width, height, data) = shot.thumbnail;
        let image = Image::Pixels(Pixels {
            width: width as i32,
            height: height as i32,
            rowstride: width as i32 * 3,
            has_alpha: false,
            data: glib::Bytes::from_owned(data),
        });
        self.notify("Screenshot taken", &body, Some(image));
    }
    fn fail(&self, error: &str) {
        eprintln!("Screenshot failed: {error}");
        self.notify("Screenshot failed", &glib::markup_escape_text(error), None);
    }
    fn recording_failed(&self, error: &str) {
        eprintln!("Recording failed: {error}");
        self.notify("Recording failed", &glib::markup_escape_text(error), None);
    }
    fn notify(&self, summary: &str, body: &str, image: Option<Image>) {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        self.server.post(
            0,
            Notification {
                id: 0,
                app_name: "Screenshot".into(),
                app_icon: Some(Image::Named("camera-photo-symbolic".into())),
                image,
                summary: summary.into(),
                body: body.into(),
                actions: vec![],
                urgency: Urgency::Normal,
                timeout: Some(Duration::from_secs(3)),
                transient: true,
                resident: false,
                value: None,
                desktop_entry: None,
                // A new screenshot replaces the previous notice.
                stack_tag: Some("nexus-screenshot".into()),
                time,
            },
        );
    }
}

/// A new file in `folder` named after the current time.
fn unique(folder: &Path, prefix: &str, extension: &str) -> PathBuf {
    let stamp = glib::DateTime::now_local()
        .and_then(|t| t.format("%Y-%m-%d_%H-%M-%S"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    let mut path = folder.join(format!("{prefix}_{stamp}.{extension}"));
    for n in 2.. {
        if !path.exists() {
            break;
        }
        path = folder.join(format!("{prefix}_{stamp}-{n}.{extension}"));
    }
    path
}
struct Exported {
    path: Option<PathBuf>,
    thumbnail: (u32, u32, Vec<u8>),
}
fn export(frame: &Frame, (x, y, w, h): (u32, u32, u32, u32), save: bool) -> Result<Exported> {
    ensure!(w > 0 && h > 0, "The selection is empty");
    let data = &frame.data[..];
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for row in y..y + h {
        let start = row as usize * frame.stride as usize + x as usize * 4;
        for px in data[start..start + w as usize * 4].as_chunks::<4>().0 {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let image = RgbImage::from_raw(w, h, rgb).context("Bad crop")?;
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(image.as_raw(), w, h, ExtendedColorType::Rgb8)?;
    let mut copy = Command::new("wl-copy")
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        // wl-copy stays behind to serve the clipboard; it must not hold our pipes.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("wl-copy is not installed")?;
    copy.stdin.take().unwrap().write_all(&png)?;
    ensure!(copy.wait()?.success(), "Could not copy to the clipboard");
    let path = if save {
        let folder = glib::user_special_dir(glib::UserDirectory::Pictures)
            .unwrap_or_else(|| glib::home_dir().join("Pictures"))
            .join("screenshots");
        std::fs::create_dir_all(&folder)?;
        let path = unique(&folder, "Screenshot", "png");
        std::fs::write(&path, &png)?;
        Some(path)
    } else {
        None
    };
    let scale = 128. / w.max(h) as f64;
    let thumb = if scale < 1. {
        image::imageops::thumbnail(
            &image,
            ((w as f64 * scale).round() as u32).max(1),
            ((h as f64 * scale).round() as u32).max(1),
        )
    } else {
        image
    };
    Ok(Exported {
        path,
        thumbnail: (thumb.width(), thumb.height(), thumb.into_raw()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_scale_maps_to_whole_pixels() {
        let size = (2048., 1280.);
        assert_eq!(
            device_box([80., 40., 400., 240.], size, (2560, 1600)),
            (100, 50, 500, 300)
        );
        assert_eq!(
            device_box([0., 0., 2048., 1280.], size, (2560, 1600)),
            (0, 0, 2560, 1600)
        );
        assert_eq!(
            device_box([1., 1., 1., 1.], (100., 100.), (125, 125)),
            (1, 1, 2, 2)
        );
    }
    #[test]
    fn rectangles_are_cut_to_the_monitor() {
        assert_eq!(
            within([-50., -10., 150., 100.], (200., 100.)),
            [0., 0., 100., 90.]
        );
    }
    #[test]
    fn windows_are_relative_and_topmost_first() {
        let monitor = serde_json::json!({"x": -1920, "y": 100, "activeWorkspace": {"id": 4}});
        let clients = serde_json::json!([
            {"at": [-1900, 120], "size": [500, 300], "mapped": true, "workspace": {"id": 4}},
            {"at": [-1850, 120], "size": [500, 300], "mapped": true, "workspace": {"id": 4}, "floating": true},
            {"at": [-1800, 120], "size": [500, 300], "mapped": true, "workspace": {"id": 2}}
        ]);
        assert_eq!(
            windows(&monitor, &clients),
            vec![[70., 20., 500., 300.], [20., 20., 500., 300.]]
        );
    }
    #[test]
    fn options() {
        let o = Options::parse("--live --save").unwrap();
        assert!(o.live && o.save && !o.record && o.mode == Mode::Region);
        let o = Options::parse("--record").unwrap();
        assert!(
            o.record && o.live,
            "recordings are picked on the live screen"
        );
        assert_eq!(Options::parse("window").unwrap().mode, Mode::Window);
        assert!(Options::parse("--bogus").is_err());
    }
}
