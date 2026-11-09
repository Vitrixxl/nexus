//! Holds its child where its parent puts it, posed by springs without moving
//! anything else: a panel opening under the bar, a popup card swiped away, the
//! pill behind the active workspace.
use gtk::{glib, gsk, prelude::*, subclass::prelude::*};
use std::cell::{Cell, RefCell};

/// Where the child stands, relative to its allocation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub x: f32,
    pub y: f32,
    pub opacity: f32,
}
impl Pose {
    pub const REST: Self = Self {
        x: 0.,
        y: 0.,
        opacity: 1.,
    };
    fn channels(self) -> [f32; 3] {
        [self.x, self.y, self.opacity]
    }
    fn from_channels([x, y, opacity]: [f32; 3]) -> Self {
        Self { x, y, opacity }
    }
}
impl Default for Pose {
    fn default() -> Self {
        Self::REST
    }
}
/// A spring in SwiftUI's terms: how long a swing takes, in seconds, and how
/// little it overshoots (1 settles without overshooting).
#[derive(Clone, Copy)]
pub struct Spring {
    pub response: f32,
    pub damping: f32,
}
/// Things arriving: quick, and still without passing where they are going.
pub const ARRIVE: Spring = Spring {
    response: 0.15,
    damping: 1.,
};
/// Things leaving, quickly and without bouncing.
pub const LEAVE: Spring = Spring {
    response: 0.1,
    damping: 1.,
};
/// Controls following a choice, like the workspace pill.
pub const SNAP: Spring = Spring {
    response: 0.13,
    damping: 1.,
};
/// One step of a damped spring toward `target`, stable whatever the frame rate.
pub fn step(spring: Spring, value: f32, velocity: f32, target: f32, dt: f32) -> (f32, f32) {
    let (mut value, mut velocity) = (value, velocity);
    let omega = std::f32::consts::TAU / spring.response.max(0.01);
    const SUBSTEP: f32 = 1. / 960.;
    let mut left = dt.min(0.05);
    while left > 0. {
        let h = left.min(SUBSTEP);
        velocity += (omega * omega * (target - value) - 2. * spring.damping * omega * velocity) * h;
        value += velocity * h;
        left -= h;
    }
    (value, velocity)
}

mod imp {
    use super::*;
    #[derive(Default)]
    pub struct Slide {
        pub pose: Cell<Pose>,
        pub velocity: Cell<[f32; 3]>,
        /// Bumped by every spring or drag, so that an older motion stops.
        pub motion: Cell<u32>,
        /// The width its height is measured at when none is given.
        pub width: Cell<i32>,
    }
    #[glib::object_subclass]
    impl ObjectSubclass for Slide {
        const NAME: &'static str = "NexusSlide";
        type Type = super::Slide;
        type ParentType = gtk::Widget;
    }
    impl ObjectImpl for Slide {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }
    impl WidgetImpl for Slide {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            // A revealer asks for a height at no particular width, for which
            // wrapped text answers as if squeezed narrow.
            let width = self.width.get();
            let for_size = if for_size < 0 && orientation == gtk::Orientation::Vertical && width > 0
            {
                width
            } else {
                for_size
            };
            self.obj()
                .first_child()
                .map_or((0, 0, -1, -1), |c| c.measure(orientation, for_size))
        }
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.obj().first_child() {
                let pose = self.pose.get();
                // Translation only: under any other scale GSK rasterizes the text
                // anew on every frame, three times the work of the whole frame.
                let shift =
                    gsk::Transform::new().translate(&gtk::graphene::Point::new(pose.x, pose.y));
                child.allocate(width, height, baseline, Some(shift));
            }
        }
    }
}
glib::wrapper! {
    pub struct Slide(ObjectSubclass<imp::Slide>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}
impl Default for Slide {
    fn default() -> Self {
        glib::Object::new()
    }
}
impl Slide {
    pub fn set_child(&self, child: &impl IsA<gtk::Widget>) {
        while let Some(old) = self.first_child() {
            old.unparent();
        }
        child.set_parent(self);
    }
    /// Measures its height at `width` when asked for one at no width.
    pub fn set_measure_width(&self, width: i32) {
        self.imp().width.set(width);
        self.queue_resize();
    }
    /// The size it asks for, which it may not have been given yet.
    pub fn natural_size(&self) -> (f32, f32) {
        let (_, width, _, _) = self.measure(gtk::Orientation::Horizontal, -1);
        let (_, height, _, _) = self.measure(gtk::Orientation::Vertical, width);
        (width as f32, height as f32)
    }
    pub fn pose(&self) -> Pose {
        self.imp().pose.get()
    }
    fn apply(&self, pose: Pose) {
        self.imp().pose.set(pose);
        self.set_opacity(f64::from(pose.opacity.clamp(0., 1.)));
        self.queue_allocate();
    }
    /// Puts the child there at once, stopping any motion.
    pub fn place(&self, pose: Pose) {
        let imp = self.imp();
        imp.motion.set(imp.motion.get().wrapping_add(1));
        imp.velocity.set([0.; 3]);
        self.apply(pose);
    }
    /// Follows a finger: the child fades as it moves its own size away.
    pub fn drag_to(&self, x: f32, y: f32) {
        let (width, height) = self.natural_size();
        let away = (x.abs() / width.max(1.)).max(y.abs() / height.max(1.));
        self.place(Pose {
            x,
            y,
            opacity: 1. - away.min(1.) * 0.7,
        });
    }
    /// Springs to `target` from wherever the child is, keeping its speed when
    /// it was already moving, then calls `done`.
    pub fn spring_to(&self, target: Pose, spring: Spring, done: impl FnOnce() + 'static) {
        let imp = self.imp();
        imp.motion.set(imp.motion.get().wrapping_add(1));
        let motion = imp.motion.get();
        let last = Cell::new(None::<i64>);
        let done = RefCell::new(Some(done));
        let goal = target.channels();
        self.add_tick_callback(move |slide, clock| {
            let imp = slide.imp();
            if imp.motion.get() != motion {
                return glib::ControlFlow::Break;
            }
            let now = clock.frame_time();
            let dt = last
                .replace(Some(now))
                .map_or(0., |t| (now - t) as f32 / 1e6);
            let mut values = imp.pose.get().channels();
            let mut speeds = imp.velocity.get();
            let mut settled = true;
            for i in 0..3 {
                (values[i], speeds[i]) = step(spring, values[i], speeds[i], goal[i], dt);
                // Pixels for the first two, a fraction for opacity.
                let epsilon = if i < 2 { 0.2 } else { 0.002 };
                settled &= (values[i] - goal[i]).abs() < epsilon && speeds[i].abs() < epsilon * 8.;
            }
            if !settled {
                imp.velocity.set(speeds);
                slide.apply(Pose::from_channels(values));
                return glib::ControlFlow::Continue;
            }
            imp.velocity.set([0.; 3]);
            slide.apply(target);
            if let Some(done) = done.take() {
                done();
            }
            glib::ControlFlow::Break
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn run(spring: Spring, hz: f32) -> (f32, f32) {
        let (mut value, mut speed, mut peak) = (0f32, 0f32, 0f32);
        for _ in 0..(2. * hz) as usize {
            (value, speed) = step(spring, value, speed, 1., 1. / hz);
            peak = peak.max(value);
        }
        (value, peak)
    }
    #[test]
    fn springs_settle_alike_at_any_frame_rate() {
        for hz in [30., 60., 144., 240.] {
            // Critically damped: never past the target.
            for spring in [ARRIVE, LEAVE, SNAP] {
                let (end, peak) = run(spring, hz);
                assert!(
                    (end - 1.).abs() < 0.001 && peak <= 1.0001,
                    "{hz} Hz: {end} {peak}"
                );
            }
        }
    }
}
