//! Holds its child where its parent puts it, shifted by an offset without
//! moving anything else: a popup card swiped away, the notification panel
//! flying in from the corner.
use gtk::{glib, gsk, prelude::*, subclass::prelude::*};
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;
    #[derive(Default)]
    pub struct Slide {
        pub offset: Cell<(f32, f32)>,
        /// Bumped by every glide or drag, so that an older glide stops.
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
                let (x, y) = self.offset.get();
                let shift = gsk::Transform::new().translate(&gtk::graphene::Point::new(x, y));
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
    /// Moves the child at once, stopping any glide.
    pub fn drag_to(&self, x: f32, y: f32) {
        let motion = &self.imp().motion;
        motion.set(motion.get().wrapping_add(1));
        self.set_offset(x, y);
    }
    /// The child fades as it moves its own size away.
    fn set_offset(&self, x: f32, y: f32) {
        self.imp().offset.set((x, y));
        let (width, height) = self.natural_size();
        let away = (x.abs() / width.max(1.)).max(y.abs() / height.max(1.));
        self.set_opacity(f64::from(1. - away.min(1.) * 0.7));
        self.queue_allocate();
    }
    /// Eases the child to `target` over `millis`, then calls `done`.
    pub fn glide(&self, target: (f32, f32), millis: u32, done: impl FnOnce() + 'static) {
        let from = self.imp().offset.get();
        self.drag_to(from.0, from.1);
        let motion = self.imp().motion.get();
        let start = Cell::new(None::<i64>);
        let done = RefCell::new(Some(done));
        self.add_tick_callback(move |slide, clock| {
            if slide.imp().motion.get() != motion {
                return glib::ControlFlow::Break;
            }
            let begun = *start.get().get_or_insert(clock.frame_time());
            start.set(Some(begun));
            let t = ((clock.frame_time() - begun) as f32 / (millis as f32 * 1000.)).min(1.);
            let eased = 1. - (1. - t).powi(3);
            slide.set_offset(
                from.0 + (target.0 - from.0) * eased,
                from.1 + (target.1 - from.1) * eased,
            );
            if t < 1. {
                return glib::ControlFlow::Continue;
            }
            if let Some(done) = done.take() {
                done();
            }
            glib::ControlFlow::Break
        });
    }
}
