//! The dock: the applications used most and those running, along the bottom of
//! the screen. It stays out of the way of windows: it rests in view on an empty
//! workspace, and otherwise rises when the pointer reaches the edge below it.
//! Icons grow under the pointer through transforms alone, and only the plate is
//! laid out again while they move.
use super::slide::{self, Pose, Slide, Spring};
use gtk::{gio, glib, gsk, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nexus_control::{Request, launcher::Catalogue, request};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

/// An icon at rest.
const ICON: f32 = 48.;
/// How much an icon grows under the pointer; icons are drawn at this size.
const MAGNIFY: f32 = 1.25;
const DRAWN: i32 = (ICON * MAGNIFY) as i32;
/// How far the pointer's pull reaches.
const REACH: f32 = 70.;
const SPACING: f32 = 4.;
const PADDING: f32 = 8.;
const PLATE: f32 = ICON + 2. * PADDING;
/// Between the plate and the bottom of the screen.
const MARGIN: f32 = 8.;
/// How high a launch lifts its icon, and for how long.
const HOP: f32 = 8.;
const HOP_SECONDS: f32 = 0.2;
/// A second click this soon after a launch would only open a second copy.
const STARTING: Duration = Duration::from_secs(2);
/// Room above the plate for grown and lifted icons.
const HEIGHT: i32 = (PLATE + MARGIN + ICON * (MAGNIFY - 1.) + HOP) as i32;
/// The share of the canvas every icon's drawing covers, and how far across it
/// a sparse one may reach for that.
const COVERED: f32 = 0.7;
const REACHED: f32 = 0.94;
/// How much of its tile a monochrome drawing fills, and the corners of what has
/// none of its own, as a share of its side.
const GLYPH: f32 = 0.7;
const ROUNDING: f32 = 0.225;
const DIVIDER: f32 = 17.;
/// How many of the most used applications stay in the dock.
const KEPT: usize = 7;
const GROW: Spring = Spring {
    response: 0.11,
    damping: 1.,
};

/// One application in the dock.
#[derive(Clone, PartialEq)]
pub struct App {
    /// Its desktop entry, or its window class for an application without one.
    key: String,
    /// The desktop entry that launches it.
    id: Option<String>,
    name: String,
    icon: Option<String>,
    kept: bool,
    /// Its windows, the most recently focused first.
    windows: Vec<String>,
    /// One of them has the focus.
    focused: bool,
}
#[derive(Clone, Default, PartialEq)]
struct Entry {
    id: String,
    name: String,
    icon: Option<String>,
}
/// What is known of the installed applications.
#[derive(Default)]
pub struct Apps {
    by_class: HashMap<String, Entry>,
    kept: Vec<Entry>,
    /// What was pinned and removed by hand; until then, the most used.
    pinned: Option<Vec<String>>,
}
fn pinned_path() -> PathBuf {
    nexus_control::theme::config_dir().join("dock.json")
}
impl Apps {
    /// With the choices made in earlier sessions.
    pub fn saved() -> Self {
        Self {
            pinned: std::fs::read(pinned_path())
                .ok()
                .and_then(|file| serde_json::from_slice(&file).ok()),
            ..Self::default()
        }
    }
    /// Pins the application with this desktop entry, or takes it off the dock.
    pub fn set_kept(&mut self, id: &str, keep: bool) {
        self.kept.retain(|entry| entry.id != id);
        if keep && let Some(entry) = self.by_class.values().find(|e| e.id == id) {
            self.kept.push(entry.clone());
        }
        self.pinned = Some(self.kept.iter().map(|e| e.id.clone()).collect());
    }
    pub fn save(&self) {
        let written =
            serde_json::to_vec(&self.pinned).map(|json| std::fs::write(pinned_path(), json));
        if let Ok(Err(e)) = written {
            eprintln!("Could not save the dock: {e}");
        }
    }
    pub fn update(&mut self, catalogue: &Catalogue) {
        self.by_class.clear();
        let mut all: Vec<(u32, Entry)> = vec![];
        for app in &catalogue.applications {
            let entry = Entry {
                id: app.id.clone(),
                name: app.name.clone(),
                icon: app.icon.clone(),
            };
            let stem = app.id.trim_end_matches(".desktop");
            self.by_class.insert(stem.to_lowercase(), entry.clone());
            // Reverse-domain entries are often named by their last part alone.
            if let Some(last) = stem.rsplit('.').next() {
                self.by_class
                    .entry(last.to_lowercase())
                    .or_insert_with(|| entry.clone());
            }
            if let Some(class) =
                gio::DesktopAppInfo::new(&app.id).and_then(|info| info.startup_wm_class())
            {
                self.by_class.insert(class.to_lowercase(), entry.clone());
            }
            all.push((
                catalogue.history.0.get(&app.id).copied().unwrap_or(0),
                entry,
            ));
        }
        if let Some(ids) = &self.pinned {
            self.kept = ids
                .iter()
                .filter_map(|id| all.iter().find(|(_, e)| e.id == *id))
                .map(|(_, e)| e.clone())
                .collect();
        // Chosen once a session, so that icons do not change places under the hand.
        } else if self.kept.is_empty() {
            all.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
            self.kept = all
                .into_iter()
                .filter(|(uses, _)| *uses > 0)
                .take(KEPT)
                .map(|(_, entry)| entry)
                .collect();
        }
    }
    /// The name of the application a window of `class` belongs to.
    pub fn name(&self, class: &str) -> String {
        self.by_class.get(&class.to_lowercase()).map_or_else(
            // A reverse-domain class says its name last.
            || class.rsplit('.').next().unwrap_or(class).to_string(),
            |entry| entry.name.clone(),
        )
    }
    /// The kept applications in their places, then the others running.
    pub fn docked(&self, clients: &Value) -> Vec<App> {
        let mut apps: Vec<App> = self
            .kept
            .iter()
            .map(|entry| App {
                key: entry.id.clone(),
                id: Some(entry.id.clone()),
                name: entry.name.clone(),
                icon: entry.icon.clone(),
                kept: true,
                windows: vec![],
                focused: false,
            })
            .collect();
        let mut windows: Vec<&Value> = clients
            .as_array()
            .map_or(vec![], |all| all.iter().collect());
        windows.sort_by_key(|c| c["focusHistoryID"].as_i64().unwrap_or(i64::MAX));
        for window in windows {
            let (Some(class), Some(address)) =
                (window["class"].as_str(), window["address"].as_str())
            else {
                continue;
            };
            // Nexus's own windows are reached from the bar.
            if class.is_empty() || class.starts_with("io.github.vitrixxl.Nexus") {
                continue;
            }
            let entry = self.by_class.get(&class.to_lowercase());
            let key = entry.map_or(class, |e| e.id.as_str());
            if let Some(app) = apps.iter_mut().find(|app| app.key == key) {
                app.windows.push(address.into());
                app.focused |= window["focusHistoryID"].as_i64() == Some(0);
            } else {
                apps.push(App {
                    key: key.into(),
                    id: entry.map(|e| e.id.clone()),
                    name: entry.map_or_else(|| class.to_string(), |e| e.name.clone()),
                    icon: entry.and_then(|e| e.icon.clone()),
                    kept: false,
                    windows: vec![address.into()],
                    focused: window["focusHistoryID"].as_i64() == Some(0),
                });
            }
        }
        apps
    }
}

struct Tile {
    app: RefCell<App>,
    button: gtk::Button,
    dot: gtk::Box,
    /// Size relative to rest: 0 on its way in or out, 1, and more under the pointer.
    size: Cell<(f32, f32)>,
    leaving: Cell<bool>,
    /// Seconds into a launch's hop.
    hop: Cell<Option<f32>>,
    launched: Cell<Option<Instant>>,
}
pub struct Dock {
    window: gtk::ApplicationWindow,
    /// Rises and sinks as a whole.
    slide: Slide,
    fixed: gtk::Fixed,
    plate: gtk::Box,
    divider: gtk::Box,
    monitor: gtk::gdk::Monitor,
    tiles: RefCell<Vec<Rc<Tile>>>,
    pointer: Cell<Option<f32>>,
    /// Stays up by itself: there is nothing on the workspace to be in the way of.
    resting: Cell<bool>,
    raised: Cell<bool>,
    animating: Cell<bool>,
    /// The plate's width on the last input region.
    region_width: Cell<f32>,
    hide_timer: RefCell<Option<glib::SourceId>>,
    /// The right-click menu, shared by the icons: one may leave while it is open.
    menu: gtk::Popover,
    menu_open: Cell<bool>,
    pin: Pin,
    this: std::rc::Weak<Self>,
}
/// Tells the shell to pin an application (desktop entry, keep).
pub type Pin = Rc<dyn Fn(&str, bool)>;
impl Dock {
    pub fn new(app: &gtk::Application, monitor: &gtk::gdk::Monitor, pin: Pin) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Nexus dock")
            .build();
        window.add_css_class("nexus");
        window.add_css_class("dock");
        window.init_layer_shell();
        window.set_namespace(Some("nexus-dock"));
        window.set_monitor(Some(monitor));
        window.set_layer(Layer::Top);
        window.set_keyboard_mode(KeyboardMode::None);
        for edge in [Edge::Left, Edge::Right, Edge::Bottom] {
            window.set_anchor(edge, true);
        }
        // Over windows when it rises, not beside them.
        window.set_exclusive_zone(0);
        let fixed = gtk::Fixed::new();
        fixed.set_size_request(-1, HEIGHT);
        let plate = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        plate.add_css_class("dock-plate");
        plate.set_can_target(false);
        fixed.put(&plate, 0., 0.);
        let divider = gtk::Box::new(gtk::Orientation::Vertical, 0);
        divider.add_css_class("dock-divider");
        divider.set_size_request(1, (ICON - 8.) as i32);
        divider.set_visible(false);
        divider.set_can_target(false);
        fixed.put(&divider, 0., 0.);
        let slide = Slide::default();
        slide.set_child(&fixed);
        slide.set_valign(gtk::Align::End);
        window.set_child(Some(&slide));
        let menu = gtk::Popover::new();
        menu.add_css_class("tray-menu");
        menu.set_has_arrow(false);
        menu.set_position(gtk::PositionType::Top);
        menu.set_parent(&fixed);
        let this = Rc::new_cyclic(|this| Self {
            window,
            slide,
            fixed,
            plate,
            divider,
            monitor: monitor.clone(),
            tiles: RefCell::default(),
            pointer: Cell::new(None),
            resting: Cell::new(false),
            raised: Cell::new(false),
            animating: Cell::new(false),
            region_width: Cell::new(0.),
            hide_timer: RefCell::default(),
            menu,
            menu_open: Cell::new(false),
            pin,
            this: this.clone(),
        });
        let dock = this.this.clone();
        this.menu.connect_closed(move |_| {
            if let Some(dock) = dock.upgrade() {
                dock.menu_open.set(false);
                dock.settle();
            }
        });
        this.slide.place(Pose {
            y: HEIGHT as f32,
            ..Pose::REST
        });
        let motion = gtk::EventControllerMotion::new();
        let dock = this.this.clone();
        motion.connect_enter(move |_, x, _| {
            if let Some(dock) = dock.upgrade() {
                dock.pointer_at(x as f32);
            }
        });
        let dock = this.this.clone();
        motion.connect_motion(move |_, x, _| {
            if let Some(dock) = dock.upgrade() {
                dock.pointer_at(x as f32);
            }
        });
        let dock = this.this.clone();
        motion.connect_leave(move |_| {
            if let Some(dock) = dock.upgrade() {
                dock.pointer_left();
            }
        });
        this.window.add_controller(motion);
        this.window.present();
        this.lay_out();
        this.update_region();
        this
    }
    pub fn close(&self) {
        self.menu.unparent();
        self.window.close();
    }
    fn screen_width(&self) -> f32 {
        self.monitor.geometry().width() as f32
    }
    pub fn update(&self, apps: &[App], resting: bool) {
        {
            let mut tiles = self.tiles.borrow_mut();
            for tile in tiles.iter() {
                let wanted = apps.iter().any(|app| app.key == tile.app.borrow().key);
                tile.leaving.set(!wanted);
            }
            let mut ordered: Vec<Rc<Tile>> = vec![];
            for app in apps {
                let tile = tiles
                    .iter()
                    .find(|tile| tile.app.borrow().key == app.key)
                    .cloned()
                    .unwrap_or_else(|| self.tile(app));
                tile.dot.set_visible(!app.windows.is_empty());
                if app.focused {
                    tile.dot.add_css_class("focused");
                } else {
                    tile.dot.remove_css_class("focused");
                }
                tile.button.set_tooltip_text(Some(&app.name));
                *tile.app.borrow_mut() = app.clone();
                ordered.push(tile);
            }
            // Those leaving keep their place until they have shrunk away.
            for (i, tile) in tiles.iter().enumerate() {
                if tile.leaving.get() {
                    ordered.insert(i.min(ordered.len()), tile.clone());
                }
            }
            *tiles = ordered;
        }
        self.resting.set(resting);
        self.settle();
        self.animate();
    }
    fn tile(&self, app: &App) -> Rc<Tile> {
        let button = gtk::Button::new();
        button.add_css_class("dock-tile");
        button.update_property(&[gtk::accessible::Property::Label(&app.name)]);
        button.set_size_request(DRAWN, DRAWN);
        let icon = app.icon.as_deref().and_then(|name| {
            let display = WidgetExt::display(&self.window);
            app_image(&display, name, DRAWN, self.monitor.scale_factor())
        });
        match icon {
            Some(image) => button.set_child(Some(&image)),
            None => button.set_child(Some(&letter(&app.name, "dock-letter"))),
        }
        self.fixed.put(&button, 0., 0.);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("dock-dot");
        dot.set_can_target(false);
        self.fixed.put(&dot, 0., 0.);
        let tile = Rc::new(Tile {
            app: RefCell::new(app.clone()),
            button: button.clone(),
            dot,
            size: Cell::new((0., 0.)),
            leaving: Cell::new(false),
            hop: Cell::new(None),
            launched: Cell::new(None),
        });
        let weak = Rc::downgrade(&tile);
        let dock = self.this.clone();
        button.connect_clicked(move |_| {
            let (Some(tile), Some(dock)) = (weak.upgrade(), dock.upgrade()) else {
                return;
            };
            let app = tile.app.borrow().clone();
            if app.windows.is_empty() {
                if tile.launched.get().is_some_and(|t| t.elapsed() < STARTING) {
                    return;
                }
                tile.launched.set(Some(Instant::now()));
                tile.hop.set(Some(0.));
                dock.animate();
            }
            std::thread::spawn(move || activate(&app));
        });
        let right = gtk::GestureClick::new();
        right.set_button(gtk::gdk::BUTTON_SECONDARY);
        let weak = Rc::downgrade(&tile);
        let dock = self.this.clone();
        right.connect_pressed(move |_, _, _, _| {
            if let (Some(tile), Some(dock)) = (weak.upgrade(), dock.upgrade()) {
                dock.open_menu(&tile);
            }
        });
        button.add_controller(right);
        tile
    }
    fn open_menu(&self, tile: &Tile) {
        let app = tile.app.borrow().clone();
        let rows = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let row = |label: &str, action: Box<dyn Fn()>| {
            let b = gtk::Button::with_label(label);
            b.set_halign(gtk::Align::Fill);
            if let Some(text) = b.child().and_downcast::<gtk::Label>() {
                text.set_xalign(0.);
            }
            let menu = self.menu.clone();
            b.connect_clicked(move |_| {
                menu.popdown();
                action();
            });
            rows.append(&b);
        };
        if let Some(id) = app.id.clone() {
            let pin = self.pin.clone();
            let keep = !app.kept;
            let label = if keep {
                "Pin to dock"
            } else {
                "Remove from dock"
            };
            row(label, Box::new(move || pin(&id, keep)));
        }
        if !app.windows.is_empty() {
            let label = match app.windows.len() {
                1 => "Close".to_owned(),
                n => format!("Close {n} windows"),
            };
            let windows = app.windows.clone();
            row(
                &label,
                Box::new(move || {
                    let windows = windows.clone();
                    std::thread::spawn(move || close_windows(&windows));
                }),
            );
        }
        let Some(bounds) = tile.button.compute_bounds(&self.fixed) else {
            return;
        };
        self.menu.set_child(Some(&rows));
        self.menu.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
            bounds.x() as i32,
            bounds.y() as i32,
            bounds.width() as i32,
            bounds.height() as i32,
        )));
        self.menu_open.set(true);
        self.settle();
        self.menu.popup();
    }
    fn pointer_at(&self, x: f32) {
        self.pointer.set(Some(x));
        if let Some(timer) = self.hide_timer.take() {
            timer.remove();
        }
        self.settle();
        self.animate();
    }
    fn pointer_left(&self) {
        self.pointer.set(None);
        self.animate();
        // A short grace, so that grazing the edge does not make it flicker.
        let dock = self.this.clone();
        let timer = glib::timeout_add_local_once(Duration::from_millis(180), move || {
            if let Some(dock) = dock.upgrade() {
                dock.hide_timer.take();
                dock.settle();
            }
        });
        if let Some(old) = self.hide_timer.replace(Some(timer)) {
            old.remove();
        }
    }
    /// Rises or sinks if it should.
    fn settle(&self) {
        let raised = self.resting.get()
            || self.pointer.get().is_some()
            || self.menu_open.get()
            || self.hide_timer.borrow().is_some();
        if raised == self.raised.replace(raised) {
            return;
        }
        let target = if raised {
            Pose::REST
        } else {
            Pose {
                y: HEIGHT as f32,
                ..Pose::REST
            }
        };
        let spring = if raised { slide::ARRIVE } else { slide::LEAVE };
        self.slide.spring_to(target, spring, || {});
        self.update_region();
    }
    /// Lays the icons out every frame while anything moves, and only then.
    fn animate(&self) {
        if self.animating.replace(true) {
            return;
        }
        let dock = self.this.clone();
        let last = Cell::new(None::<i64>);
        self.fixed.add_tick_callback(move |_, clock| {
            let Some(dock) = dock.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let now = clock.frame_time();
            let dt = last
                .replace(Some(now))
                .map_or(0., |t| (now - t) as f32 / 1e6);
            if dock.advance(dt) {
                glib::ControlFlow::Continue
            } else {
                dock.animating.set(false);
                glib::ControlFlow::Break
            }
        });
    }
    /// Where the plate starts when no icon is grown.
    fn rest_width(&self, count: usize, divided: bool) -> f32 {
        let count = count as f32;
        2. * PADDING
            + count * ICON
            + (count - 1.).max(0.) * SPACING
            + if divided { DIVIDER } else { 0. }
    }
    fn advance(&self, dt: f32) -> bool {
        let tiles = self.tiles.borrow().clone();
        let mut moving = false;
        // The rest position of each icon, which the pointer's pull is measured from.
        let shown: Vec<_> = tiles.iter().filter(|t| !t.leaving.get()).collect();
        let divided =
            shown.iter().any(|t| t.app.borrow().kept) && shown.iter().any(|t| !t.app.borrow().kept);
        let rest = self.rest_width(shown.len(), divided);
        let pointer = self
            .pointer
            .get()
            .map(|x| x - (self.screen_width() - rest) / 2.);
        let mut at_rest = PADDING;
        let mut was_kept = true;
        for tile in &tiles {
            if tile.leaving.get() {
                self.step_tile(tile, 0., dt, &mut moving);
                continue;
            }
            let kept = tile.app.borrow().kept;
            if was_kept && !kept && divided {
                at_rest += DIVIDER;
            }
            was_kept = kept;
            let centre = at_rest + ICON / 2.;
            at_rest += ICON + SPACING;
            let pull = pointer.map_or(0., |x| (-((x - centre) / REACH).powi(2)).exp());
            self.step_tile(tile, 1. + (MAGNIFY - 1.) * pull, dt, &mut moving);
        }
        // Those fully gone leave the dock now.
        self.tiles.borrow_mut().retain(|tile| {
            let gone = tile.leaving.get() && tile.size.get().0 < 0.02;
            if gone {
                self.fixed.remove(&tile.button);
                self.fixed.remove(&tile.dot);
            }
            !gone
        });
        self.lay_out();
        moving
    }
    fn step_tile(&self, tile: &Tile, target: f32, dt: f32, moving: &mut bool) {
        let (size, speed) = tile.size.get();
        let (size, speed) = slide::step(GROW, size, speed, target, dt);
        tile.size.set((size, speed));
        *moving |= (size - target).abs() > 0.002 || speed.abs() > 0.02;
        if let Some(t) = tile.hop.get() {
            let t = t + dt;
            tile.hop.set((t < HOP_SECONDS).then_some(t));
            *moving = true;
        }
    }
    /// Places everything from the sizes of the icons, in transforms.
    fn lay_out(&self) {
        let tiles = self.tiles.borrow();
        let divided = tiles
            .iter()
            .any(|t| t.app.borrow().kept && !t.leaving.get())
            && tiles
                .iter()
                .any(|t| !t.app.borrow().kept && !t.leaving.get());
        // Icons run from one padding in; the last spacing gives way to the other.
        let mut cursor = PADDING;
        let mut last_kept = true;
        let mut divider_at = None;
        let mut spans = Vec::with_capacity(tiles.len());
        for tile in tiles.iter() {
            let kept = tile.app.borrow().kept;
            if last_kept && !kept && divided && !tile.leaving.get() {
                divider_at = Some(cursor - SPACING / 2. + DIVIDER / 2. - 0.5);
                cursor += DIVIDER;
            }
            last_kept = kept;
            let size = tile.size.get().0.max(0.);
            spans.push((cursor, size));
            cursor += ICON * size + SPACING * size.min(1.);
        }
        let width = (cursor - SPACING + PADDING).max(2. * PADDING);
        let left = (self.screen_width() - width) / 2.;
        let bottom = HEIGHT as f32 - MARGIN;
        let move_to = |widget: &gtk::Widget, x: f32, y: f32, scale: f32| {
            let transform = gsk::Transform::new()
                .translate(&gtk::graphene::Point::new(x, y))
                .scale(scale, scale);
            self.fixed.set_child_transform(widget, Some(&transform));
        };
        self.plate
            .set_size_request(width.round() as i32, PLATE as i32);
        move_to(self.plate.upcast_ref(), left, bottom - PLATE, 1.);
        self.plate.set_visible(!tiles.is_empty());
        self.divider.set_visible(divider_at.is_some());
        if let Some(x) = divider_at {
            move_to(
                self.divider.upcast_ref(),
                left + x,
                bottom - PLATE + 12.,
                1.,
            );
        }
        for (tile, (x, size)) in tiles.iter().zip(spans) {
            let drawn = ICON * size;
            // A launch lifts the icon once, and it comes straight back.
            let hop = tile
                .hop
                .get()
                .map_or(0., |t| (t / HOP_SECONDS * std::f32::consts::PI).sin() * HOP);
            let scale = drawn / DRAWN as f32;
            let top = bottom - PADDING - drawn - hop;
            move_to(tile.button.upcast_ref(), left + x, top, scale);
            move_to(
                tile.dot.upcast_ref(),
                left + x + drawn / 2. - 2.,
                bottom - 6.,
                1.,
            );
            tile.button.set_opacity(f64::from(size.min(1.)));
        }
        let change = (self.region_width.get() - width).abs();
        if change >= 2. {
            self.region_width.set(width);
            self.update_region();
        }
    }
    /// Only the dock takes the pointer: along the plate when it is up, and the
    /// last pixels of the screen below it when it is not.
    fn update_region(&self) {
        let Some(surface) = self.window.surface() else {
            return;
        };
        let width = self.region_width.get().max(240.);
        let x = ((self.screen_width() - width) / 2.) as i32;
        // Reaches up to grown icons only while the pointer is on them.
        let height = if !self.raised.get() {
            3
        } else if self.pointer.get().is_some() {
            HEIGHT
        } else {
            (PLATE + MARGIN) as i32
        };
        let rect = gtk::cairo::RectangleInt::new(x, HEIGHT - height, width as i32, height);
        surface.set_input_region(&gtk::cairo::Region::create_rectangle(&rect));
    }
}
/// The drawing at `path` on a transparent square of `side` pixels, scaled so that it
/// covers the same area as any other: a shield, a disc and a full square then look
/// the same size, whatever margins and shapes their authors gave them.
fn evened(path: &Path, side: i32) -> Option<(gtk::glib::Bytes, usize)> {
    use gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};
    let source = Pixbuf::from_file_at_scale(path, side, side, true).ok()?;
    let (width, height) = (source.width() as usize, source.height() as usize);
    let channels = source.n_channels() as usize;
    let stride = source.rowstride() as usize;
    let bytes = source.read_pixel_bytes();
    let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
    let mut drawn = 0usize;
    let (mut level, mut chroma) = (0usize, 0usize);
    for y in 0..height {
        for x in 0..width {
            if channels < 4 || bytes[y * stride + x * channels + 3] > 40 {
                drawn += 1;
                let rgb = &bytes[y * stride + x * channels..][..3];
                let (max, min) = (*rgb.iter().max()? as usize, *rgb.iter().min()? as usize);
                level += max;
                chroma += max - min;
                left = left.min(x);
                right = right.max(x + 1);
                top = top.min(y);
                bottom = bottom.max(y + 1);
            }
        }
    }
    if drawn == 0 {
        return None;
    }
    let (w, h) = (right - left, bottom - top);
    let canvas = side as f32;
    let opaque = |x: usize, y: usize| channels < 4 || bytes[y * stride + x * channels + 3] > 200;
    let (ix, iy) = (w / 32, h / 32);
    let square = [left + ix, right - 1 - ix]
        .iter()
        .all(|x| opaque(*x, top + iy) && opaque(*x, bottom - 1 - iy));
    // Most of its box filled: already a tile of its own, whatever its colour.
    let backing = (drawn * 100 < w * h * 85)
        .then(|| backing(level / drawn, chroma / drawn))
        .flatten();
    // A tile the size of an icon drawn edge to edge, the glyph well inside it.
    let tile = (COVERED.sqrt() * canvas) as i32;
    let scale = match backing {
        Some(_) => GLYPH * tile as f32 / w.max(h) as f32,
        None => even_scale(drawn as f32, w.max(h) as f32, canvas),
    };
    let cropped = source.new_subpixbuf(left as i32, top as i32, w as i32, h as i32);
    let out = Pixbuf::new(Colorspace::Rgb, true, 8, side, side)?;
    out.fill(0);
    let (dw, dh) = ((w as f32 * scale) as i32, (h as f32 * scale) as i32);
    let (dx, dy) = ((side - dw) / 2, (side - dh) / 2);
    let inset = (side - tile) / 2;
    if let Some(color) = backing {
        out.new_subpixbuf(inset, inset, tile, tile).fill(color);
    }
    cropped.composite(
        &out,
        dx,
        dy,
        dw,
        dh,
        f64::from(dx),
        f64::from(dy),
        f64::from(scale),
        f64::from(scale),
        InterpType::Bilinear,
        255,
    );
    let stride = out.rowstride() as usize;
    let mut pixels = out.read_pixel_bytes().to_vec();
    if backing.is_some() {
        round_corners(&mut pixels, stride, (inset, inset, tile, tile));
    } else if square {
        round_corners(&mut pixels, stride, (dx, dy, dw, dh));
    }
    Some((gtk::glib::Bytes::from_owned(pixels), stride))
}
/// The tile a monochrome drawing needs to show on any plate, by its mean level and
/// colourfulness out of 255: a dark one on light, a pale one on dark. None for the
/// others, which hold on both.
fn backing(level: usize, chroma: usize) -> Option<u32> {
    match (chroma, level) {
        (16.., _) => None,
        (_, ..120) => Some(0xe8e8ecff),
        (_, 200..) => Some(0x2c2c30ff),
        _ => None,
    }
}
/// Cuts the corners of `(x, y, w, h)` in straight RGBA, as app icons are drawn.
fn round_corners(pixels: &mut [u8], stride: usize, (x, y, w, h): (i32, i32, i32, i32)) {
    let radius = w.min(h) as f32 * ROUNDING;
    let corner = radius.ceil() as i32;
    for py in y..y + h {
        for px in x..x + w {
            // How far past the arc's centre this pixel lies, on each axis.
            let dx = (x + corner - px).max(px - (x + w - 1 - corner)) as f32;
            let dy = (y + corner - py).max(py - (y + h - 1 - corner)) as f32;
            if dx <= 0. || dy <= 0. {
                continue;
            }
            let cover = (radius + 0.5 - dx.hypot(dy)).clamp(0., 1.);
            let alpha = &mut pixels[py as usize * stride + px as usize * 4 + 3];
            *alpha = (f32::from(*alpha) * cover) as u8;
        }
    }
}
/// What to scale a drawing of `drawn` pixels by, its longest side being `longest`.
fn even_scale(drawn: f32, longest: f32, canvas: f32) -> f32 {
    (COVERED * canvas * canvas / drawn)
        .sqrt()
        .min(REACHED * canvas / longest)
}
/// An application's icon, evened out with the others once decoded; none if neither
/// the theme nor the application's own folder has it.
pub(super) fn app_image(
    display: &gtk::gdk::Display,
    name: &str,
    size: i32,
    scale: i32,
) -> Option<gtk::Image> {
    let theme = gtk::IconTheme::for_display(display);
    let icon = gio::Icon::for_string(name)
        .ok()
        .filter(|icon| theme.has_gicon(icon))
        .or_else(|| {
            let file = gio::File::for_path(bundled_icon(name)?);
            Some(gio::FileIcon::new(&file).upcast())
        })?;
    let paintable = theme.lookup_by_gicon(
        &icon,
        size,
        scale,
        gtk::TextDirection::None,
        gtk::IconLookupFlags::empty(),
    );
    let image = gtk::Image::new();
    image.set_pixel_size(size);
    let path = paintable
        .file()
        .and_then(|file| file.path())
        .filter(|_| !paintable.is_symbolic());
    let Some(path): Option<PathBuf> = path else {
        // A resource, or a monochrome drawing to recolour: as it comes.
        image.set_paintable(Some(&paintable));
        return Some(image);
    };
    let shown = image.clone();
    glib::spawn_future_local(async move {
        let side = size * scale;
        match gio::spawn_blocking(move || evened(&path, side)).await {
            Ok(Some(pixels)) => {
                let texture = gtk::gdk::MemoryTexture::new(
                    side,
                    side,
                    gtk::gdk::MemoryFormat::R8g8b8a8,
                    &pixels.0,
                    pixels.1,
                );
                shown.set_paintable(Some(&texture));
            }
            _ => shown.set_paintable(Some(&paintable)),
        }
    });
    Some(image)
}
/// An icon an application keeps beside itself under /opt instead of installing it
/// in a theme, as Stremio's package does: `/opt/stremio/icons/<name>_128.png`.
/// The largest one.
fn bundled_icon(name: &str) -> Option<PathBuf> {
    // ponytail: /opt only, and only one level of icons/; other layouts get the letter.
    std::fs::read_dir("/opt")
        .ok()?
        .flatten()
        .flat_map(|app| [app.path(), app.path().join("icons")])
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .map(|file| file.path())
        .filter_map(|path| Some((bundled_size(&path, name)?, path)))
        .max_by_key(|(size, _)| *size)
        .map(|(_, path)| path)
}
/// The size of an icon file if it is `name`, in `name.png` or `name_128.png` form.
fn bundled_size(path: &Path, name: &str) -> Option<u32> {
    if path.extension()? != "png" {
        return None;
    }
    let rest = path.file_stem()?.to_str()?.strip_prefix(name)?;
    match rest.strip_prefix(['_', '-']) {
        Some(size) => size.parse().ok(),
        None => rest.is_empty().then_some(0),
    }
}
/// An application without an icon: its initial, on a tile of its own.
pub(super) fn letter(name: &str, class: &str) -> gtk::Label {
    let initial: String = name
        .chars()
        .next()
        .into_iter()
        .flat_map(char::to_uppercase)
        .collect();
    let letter = gtk::Label::new(Some(&initial));
    letter.add_css_class(class);
    letter.set_halign(gtk::Align::Center);
    letter.set_valign(gtk::Align::Center);
    letter
}
/// Says so on screen: a dock icon that does nothing would otherwise look broken.
fn report_failure(name: &str, why: &str) {
    eprintln!("Could not open {name}: {why}");
    let sent = zbus::blocking::Connection::session().and_then(|bus| {
        bus.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Dock",
                0u32,
                "dialog-error-symbolic",
                format!("Could not open {name}"),
                why,
                Vec::<&str>::new(),
                HashMap::<&str, zbus::zvariant::Value>::new(),
                -1i32,
            ),
        )
    });
    if let Err(e) = sent {
        eprintln!("Could not say so: {e}");
    }
}
/// Asks them to close, as their own close button would: unsaved work still asks.
fn close_windows(windows: &[String]) {
    for address in windows {
        let _ = nexus_control::backend::run(
            "hyprctl",
            &[
                "dispatch",
                &format!("hl.dsp.window.close({{ window = \"address:{address}\" }})"),
            ],
        );
    }
}
/// Brings up a window of the application, or starts it.
fn activate(app: &App) {
    // Already in front: on to its other windows, the longest unused first.
    let window = if app.focused {
        app.windows.last()
    } else {
        app.windows.first()
    };
    if let Some(address) = window {
        let _ = nexus_control::backend::run(
            "hyprctl",
            &[
                "dispatch",
                &format!("hl.dsp.focus({{ window = \"address:{address}\" }})"),
            ],
        );
    } else if let Some(id) = &app.id {
        match request(&Request::Launch { id: id.clone() }) {
            Ok(reply) if !reply.ok => report_failure(
                &app.name,
                reply.error.as_deref().unwrap_or("It did not start."),
            ),
            Err(e) => report_failure(&app.name, &e.to_string()),
            _ => {}
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use nexus_control::launcher::{Application, History};
    fn application(id: &str, name: &str) -> Application {
        Application {
            id: id.into(),
            name: name.into(),
            description: String::new(),
            keywords: String::new(),
            icon: None,
        }
    }
    #[test]
    fn dock_keeps_the_most_used_and_adds_what_runs() {
        let catalogue = Catalogue {
            applications: vec![
                application("firefox.desktop", "Firefox"),
                application("foot.desktop", "Foot"),
                application("org.gnome.Nautilus.desktop", "Files"),
                application("unused.desktop", "Unused"),
            ],
            history: History(
                [("firefox.desktop".into(), 3), ("foot.desktop".into(), 9)]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        };
        let mut apps = Apps::default();
        apps.update(&catalogue);
        let clients = serde_json::json!([
            { "class": "foot", "address": "0x1", "focusHistoryID": 1 },
            { "class": "foot", "address": "0x2", "focusHistoryID": 0 },
            { "class": "org.gnome.Nautilus", "address": "0x3", "focusHistoryID": 2 },
            { "class": "nameless", "address": "0x4", "focusHistoryID": 3 },
            { "class": "io.github.vitrixxl.Nexus", "address": "0x5", "focusHistoryID": 4 },
        ]);
        let docked = apps.docked(&clients);
        let names: Vec<_> = docked.iter().map(|a| a.name.as_str()).collect();
        // Most used first; then running, known or not; never Nexus's own windows.
        assert_eq!(names, ["Foot", "Firefox", "Files", "nameless"]);
        // The most recently focused window comes up first.
        assert_eq!(docked[0].windows, ["0x2", "0x1"]);
        assert!(docked[1].windows.is_empty() && docked[1].kept);
        assert!(!docked[2].kept && docked[2].id.is_some());
        assert!(docked[3].id.is_none());
        assert_eq!(apps.name("FOOT"), "Foot");
        assert_eq!(apps.name("nameless"), "nameless");
    }
    #[test]
    fn pins_outlast_the_most_used_and_keep_their_order() {
        let catalogue = Catalogue {
            applications: vec![
                application("a.desktop", "A"),
                application("b.desktop", "B"),
                application("c.desktop", "C"),
            ],
            history: History(
                [("a.desktop".into(), 5), ("b.desktop".into(), 3)]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        };
        let mut apps = Apps::default();
        apps.update(&catalogue);
        apps.set_kept("a.desktop", false);
        apps.set_kept("c.desktop", true);
        let names = |apps: &Apps| -> Vec<String> {
            let docked = apps.docked(&serde_json::json!([]));
            docked.into_iter().map(|a| a.name).collect()
        };
        assert_eq!(names(&apps), ["B", "C"]);
        // The catalogue comes back: the choice stands, a removed one stays out.
        apps.update(&catalogue);
        assert_eq!(names(&apps), ["B", "C"]);
        // Everything removed is a choice too, not a reason to start over.
        apps.set_kept("b.desktop", false);
        apps.set_kept("c.desktop", false);
        apps.update(&catalogue);
        assert!(names(&apps).is_empty());
    }
    #[test]
    fn square_icons_lose_their_corners_alone() {
        let mut pixels = vec![255; 40 * 40 * 4];
        round_corners(&mut pixels, 160, (0, 0, 40, 40));
        let alpha = |x: usize, y: usize| pixels[y * 160 + x * 4 + 3];
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(39, 39), 0);
        assert_eq!(alpha(20, 0), 255);
        assert_eq!(alpha(20, 20), 255);
    }
    #[test]
    fn only_monochrome_extremes_get_a_tile() {
        assert_eq!(backing(70, 4), Some(0xe8e8ecff));
        assert_eq!(backing(240, 0), Some(0x2c2c30ff));
        assert_eq!(backing(160, 4), None);
        assert_eq!(backing(70, 60), None);
    }
    #[test]
    fn bundled_icons_match_their_name_and_sizes_only() {
        let size = |file: &str| bundled_size(Path::new(file), "smartcode-stremio");
        assert_eq!(size("/opt/s/icons/smartcode-stremio_128.png"), Some(128));
        assert_eq!(size("/opt/s/smartcode-stremio.png"), Some(0));
        assert_eq!(size("/opt/s/icons/smartcode-stremio-tray_128.png"), None);
        assert_eq!(size("/opt/s/icons/smartcode-stremio_128.svg"), None);
    }
    #[test]
    fn icons_cover_the_same_area_unless_too_sparse() {
        let canvas = 100.;
        let covered = |drawn: f32, longest: f32| {
            let scale = even_scale(drawn, longest, canvas);
            drawn * scale * scale
        };
        // A full square with wide margins and a tight disc end up alike.
        let square = covered(60. * 60., 60.);
        let disc = covered(std::f32::consts::FRAC_PI_4 * 95. * 95., 95.);
        assert!((square - disc).abs() < 0.03 * square, "{square} {disc}");
        // A thin drawing grows only as far as the canvas allows.
        assert!((even_scale(500., 80., canvas) * 80. - REACHED * canvas).abs() < 0.01);
    }
}
