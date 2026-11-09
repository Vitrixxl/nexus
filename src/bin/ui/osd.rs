//! Volume and brightness as they change, in a capsule under the bar: the keys
//! that change them are the compositor's, so nothing else would show it.
use super::{
    super::hbox,
    shell::{BAR, GAP},
    slide::{self, Pose, Slide},
};
use gtk::{glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::{cell::Cell, cell::RefCell, rc::Rc, time::Duration};

const LINGER: Duration = Duration::from_millis(1100);
// Not faded, for the same reason as the panels: the blur would arrive late.
const AWAY: Pose = Pose {
    x: 0.,
    y: -6.,
    opacity: 1.,
};

pub struct Osd {
    window: gtk::ApplicationWindow,
    slide: Slide,
    icon: gtk::Image,
    bar: gtk::ProgressBar,
    value: gtk::Label,
    /// The level the bar is gliding to, where it stands and how fast it goes.
    level: Cell<(f32, f32, f32)>,
    gliding: Cell<bool>,
    timer: RefCell<Option<glib::SourceId>>,
}
impl Osd {
    pub fn new(app: &gtk::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Nexus level")
            .build();
        window.add_css_class("nexus");
        window.add_css_class("osd");
        window.init_layer_shell();
        window.set_namespace(Some("nexus-osd"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(-1);
        window.set_anchor(Edge::Top, true);
        window.set_margin(Edge::Top, BAR + GAP);
        let card = hbox(12);
        card.add_css_class("osd-card");
        // Room for its shadow, inside the window.
        card.set_margin_start(28);
        card.set_margin_end(28);
        card.set_margin_top(4);
        card.set_margin_bottom(34);
        let icon = gtk::Image::new();
        icon.set_pixel_size(18);
        card.append(&icon);
        let bar = gtk::ProgressBar::new();
        bar.set_width_request(190);
        bar.set_valign(gtk::Align::Center);
        card.append(&bar);
        let value = gtk::Label::new(None);
        value.add_css_class("osd-value");
        value.set_xalign(1.);
        card.append(&value);
        let slide = Slide::default();
        slide.set_child(&card);
        window.set_child(Some(&slide));
        // It only ever shows: clicks go through to whatever is below.
        window.connect_map(|window| {
            if let Some(surface) = window.surface() {
                surface.set_input_region(&gtk::cairo::Region::create());
            }
        });
        Rc::new(Self {
            window,
            slide,
            icon,
            bar,
            value,
            level: Cell::new((0., 0., 0.)),
            gliding: Cell::new(false),
            timer: RefCell::default(),
        })
    }
    pub fn show(self: &Rc<Self>, icon: &str, percent: u8) {
        self.icon.set_icon_name(Some(icon));
        self.value.set_text(&format!("{percent}%"));
        let target = f32::from(percent) / 100.;
        let (_, at, speed) = self.level.get();
        if self.window.is_visible() {
            self.level.set((target, at, speed));
            self.glide();
        } else {
            // A new appearance starts at its level; only changes glide.
            self.level.set((target, target, 0.));
            self.bar.set_fraction(f64::from(target));
            self.slide.place(AWAY);
            self.window.present();
        }
        self.slide.spring_to(Pose::REST, slide::ARRIVE, || {});
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        let s = self.clone();
        *self.timer.borrow_mut() = Some(glib::timeout_add_local_once(LINGER, move || {
            s.timer.take();
            let window = s.window.clone();
            s.slide
                .spring_to(AWAY, slide::LEAVE, move || window.set_visible(false));
        }));
    }
    fn glide(self: &Rc<Self>) {
        if self.gliding.replace(true) {
            return;
        }
        let s = self.clone();
        let last = Cell::new(None::<i64>);
        self.bar.add_tick_callback(move |bar, clock| {
            let now = clock.frame_time();
            let dt = last
                .replace(Some(now))
                .map_or(0., |t| (now - t) as f32 / 1e6);
            let (target, at, speed) = s.level.get();
            let (at, speed) = slide::step(slide::SNAP, at, speed, target, dt);
            s.level.set((target, at, speed));
            bar.set_fraction(f64::from(at.clamp(0., 1.)));
            if (at - target).abs() < 0.001 && speed.abs() < 0.01 {
                s.gliding.set(false);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
}
