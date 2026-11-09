//! The page editor: one text view, the title on its first line and one block
//! per line after it (see `doc`). Markers stay in the text, hidden, so undo,
//! redo and selection span blocks; bullets, boxes and backgrounds are drawn
//! for the visible lines only, so a long page costs what is on screen.
use crate::doc::{self, Kind, Line, Mark, SOFT};
use crate::vim;
use gtk::{gdk, gio, glib, graphene, gsk, pango, prelude::*, subclass::prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

/// Left margin of the text: room for the drag handle.
const M: i32 = 40;
/// The text column, margins included.
const COLUMN: i32 = 800;
const LEVELS: i32 = 12;
/// How far each level of nesting moves a block in: a list item's marker room.
const UNIT: i32 = 28;

#[derive(Clone)]
pub struct Colors {
    fg: gdk::RGBA,
    muted: gdk::RGBA,
    accent: gdk::RGBA,
    on_accent: gdk::RGBA,
}
impl Default for Colors {
    fn default() -> Self {
        Self::new("#f4f4f5", "#b4b4bb", "#0a84ff")
    }
}
impl Colors {
    pub fn new(fg: &str, muted: &str, accent: &str) -> Self {
        let p = |s: &str| gdk::RGBA::parse(s).unwrap_or(gdk::RGBA::BLACK);
        let accent = p(accent);
        let luma = 0.2126 * accent.red() + 0.7152 * accent.green() + 0.0722 * accent.blue();
        let on_accent = if luma > 0.6 {
            p("#16201b")
        } else {
            gdk::RGBA::WHITE
        };
        Self {
            fg: p(fg),
            muted: p(muted),
            accent,
            on_accent,
        }
    }
    fn fg(&self, a: f32) -> gdk::RGBA {
        self.fg.with_alpha(a)
    }
}

mod imp {
    use super::*;
    #[derive(Default)]
    pub struct NoteView {
        pub hover: Cell<i32>,
        pub drop: Cell<i32>,
        /// Whole blocks selected: first and last line.
        pub blocks: Cell<Option<(i32, i32)>>,
        pub colors: RefCell<Colors>,
        pub gutters: RefCell<Option<(gtk::Box, gtk::Box)>>,
    }
    #[glib::object_subclass]
    impl ObjectSubclass for NoteView {
        const NAME: &'static str = "NexusNoteView";
        type Type = super::NoteView;
        type ParentType = gtk::TextView;
    }
    impl ObjectImpl for NoteView {}
    impl WidgetImpl for NoteView {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            // Centres the column; set after this pass, the next one uses it.
            let side = ((width - COLUMN) / 2).max(0);
            if let Some((l, r)) = self.gutters.borrow().clone()
                && l.width_request() != side
            {
                glib::idle_add_local_once(move || {
                    l.set_width_request(side);
                    r.set_width_request(side);
                });
            }
        }
    }
    impl TextViewImpl for NoteView {
        fn snapshot_layer(&self, layer: gtk::TextViewLayer, snapshot: gtk::Snapshot) {
            super::draw(&self.obj(), layer, &snapshot);
        }
    }
}
glib::wrapper! {
    pub struct NoteView(ObjectSubclass<imp::NoteView>)
        @extends gtk::TextView, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

fn text_of(b: &gtk::TextBuffer, l: i32) -> String {
    let Some(s) = b.iter_at_line(l) else {
        return String::new();
    };
    let mut e = s;
    if !e.ends_line() {
        e.forward_to_line_end();
    }
    b.text(&s, &e, true).into()
}
fn at(b: &gtk::TextBuffer, l: i32, off: usize) -> gtk::TextIter {
    b.iter_at_line_offset(l, off as i32)
        .unwrap_or_else(|| b.end_iter())
}
fn line_end(b: &gtk::TextBuffer, l: i32) -> gtk::TextIter {
    let mut e = b.iter_at_line(l).unwrap_or_else(|| b.end_iter());
    if !e.ends_line() {
        e.forward_to_line_end();
    }
    e
}
fn cursor(b: &gtk::TextBuffer) -> gtk::TextIter {
    b.iter_at_mark(&b.get_insert())
}
fn classify(b: &gtk::TextBuffer, l: i32) -> (String, Line) {
    let t = text_of(b, l);
    let line = doc::classify(&t);
    (t, line)
}
fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}
/// Where the drawn marker of a block sits left of its text.
fn marker_room(kind: Kind) -> i32 {
    match kind {
        k if k.is_list() => 28,
        Kind::Quote => 18,
        Kind::Code => 18,
        Kind::Callout => 44,
        _ => 0,
    }
}

/// The nesting level a line's tag says.
fn level(b: &gtk::TextBuffer, l: i32) -> i32 {
    let Some(iter) = b.iter_at_line(l) else {
        return 0;
    };
    let table = b.tag_table();
    (0..LEVELS)
        .find(|n| {
            table
                .lookup(&format!("lvl{n}"))
                .is_some_and(|t| iter.has_tag(&t))
        })
        .unwrap_or(0)
}

/// What a click or the drawing needs to know of a block on screen.
struct Geo {
    kind: Kind,
    /// Where the text starts: x of the line's margin, y and height of its
    /// first row, in buffer coordinates. Measured at the line start, since the
    /// toolkit places a character after hidden ones as if they were shown.
    text: gdk::Rectangle,
    left: i32,
}
fn geometry(v: &NoteView, l: i32) -> Option<Geo> {
    let b = v.buffer();
    let (_, line) = classify(&b, l);
    let text = v.iter_location(&at(&b, l, 0));
    let left = text.x() - marker_room(line.kind);
    Some(Geo {
        kind: line.kind,
        text,
        left,
    })
}
fn rect(x: i32, y: i32, w: i32, h: i32) -> graphene::Rect {
    graphene::Rect::new(x as f32, y as f32, w as f32, h as f32)
}
fn fill(s: &gtk::Snapshot, r: graphene::Rect, radius: f32, c: &gdk::RGBA) {
    s.push_rounded_clip(&gsk::RoundedRect::from_rect(r, radius));
    s.append_color(c, &r);
    s.pop();
}
fn icon(v: &NoteView, s: &gtk::Snapshot, name: &str, x: i32, y: i32, size: i32, c: &gdk::RGBA) {
    let theme = gtk::IconTheme::for_display(&v.display());
    let paintable = theme.lookup_icon(
        name,
        &[],
        size,
        v.scale_factor(),
        gtk::TextDirection::None,
        gtk::IconLookupFlags::FORCE_SYMBOLIC,
    );
    s.save();
    s.translate(&graphene::Point::new(x as f32, y as f32));
    paintable.snapshot_symbolic(
        s.upcast_ref::<gdk::Snapshot>(),
        size as f64,
        size as f64,
        std::slice::from_ref(c),
    );
    s.restore();
}
fn text_layout(
    v: &NoteView,
    text: &str,
    scale: f64,
    weight: Option<pango::Weight>,
) -> pango::Layout {
    let layout = v.create_pango_layout(Some(text));
    let attrs = pango::AttrList::new();
    attrs.insert(pango::AttrFloat::new_scale(scale));
    if let Some(w) = weight {
        attrs.insert(pango::AttrInt::new_weight(w));
    }
    layout.set_attributes(Some(&attrs));
    layout
}

fn draw(v: &NoteView, layer: gtk::TextViewLayer, s: &gtk::Snapshot) {
    let imp = v.imp();
    let c = imp.colors.borrow().clone();
    let b = v.buffer();
    let vis = v.visible_rect();
    let right = vis.width() - M;
    if layer == gtk::TextViewLayer::AboveText {
        let drop = imp.drop.get();
        if drop >= 1 {
            let y = match b.iter_at_line(drop) {
                Some(i) if drop < b.line_count() => v.line_yrange(&i).0,
                _ => {
                    let (y, h) = v.line_yrange(&b.end_iter());
                    y + h
                }
            };
            fill(s, rect(M, y - 1, right - M, 2), 1., &c.accent);
        }
        return;
    }
    // Selected blocks: one shape from the first to the last.
    if let Some((lo, hi)) = imp.blocks.get()
        && let (Some(first), Some(last)) = (b.iter_at_line(lo), b.iter_at_line(hi))
        && let Some(g) = geometry(v, lo)
    {
        let top = v.line_yrange(&first).0;
        let (y, h) = v.line_yrange(&last);
        let r = rect(g.left - 8, top, right - g.left + 16, y + h - top);
        fill(s, r, 8., &c.accent.with_alpha(0.16));
    }
    let cursor_line = cursor(&b).line();
    let (mut iter, _) = v.line_at_y(vis.y());
    loop {
        let l = iter.line();
        let (y, h) = v.line_yrange(&iter);
        if y > vis.y() + vis.height() {
            break;
        }
        if l == 0 {
            if text_of(&b, 0).is_empty() {
                let layout = text_layout(v, "Untitled", 2.0, Some(pango::Weight::Bold));
                s.save();
                s.translate(&graphene::Point::new(M as f32, y as f32));
                s.append_layout(&layout, &c.fg(0.28));
                s.restore();
            }
        } else if let Some(g) = geometry(v, l) {
            draw_block(v, s, &c, l, &g, y, h, right, cursor_line == l);
        }
        if !iter.forward_line() {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_block(
    v: &NoteView,
    s: &gtk::Snapshot,
    c: &Colors,
    l: i32,
    g: &Geo,
    y: i32,
    h: i32,
    right: i32,
    caret: bool,
) {
    let b = v.buffer();
    let t = g.text;
    let cy = t.y() + t.height() / 2;
    match g.kind {
        Kind::Code => {
            fill(
                s,
                rect(g.left, y + 4, right - g.left, h - 8),
                8.,
                &c.fg(0.055),
            );
            let (text, _) = classify(&b, l);
            let lang: String = text
                .split(SOFT)
                .next()
                .unwrap_or("")
                .trim_start_matches(['`', '~', ' '])
                .trim()
                .to_string();
            if !lang.is_empty() {
                let layout = text_layout(v, &lang, 0.78, None);
                let (w, _) = layout.pixel_size();
                s.save();
                s.translate(&graphene::Point::new(
                    (right - 12 - w) as f32,
                    (y + 10) as f32,
                ));
                s.append_layout(&layout, &c.muted);
                s.restore();
            }
        }
        Kind::Callout => {
            fill(
                s,
                rect(g.left, y + 4, right - g.left, h - 8),
                10.,
                &c.fg(0.055),
            );
            icon(
                v,
                s,
                "dialog-information-symbolic",
                g.left + 16,
                cy - 8,
                16,
                &c.accent,
            );
        }
        Kind::Quote => fill(s, rect(g.left + 2, y + 3, 2, h - 6), 1., &c.fg(0.25)),
        Kind::Divider => fill(
            s,
            rect(g.left, y + h / 2, right - g.left, 1),
            0.,
            &c.fg(0.12),
        ),
        Kind::Bullet => {
            let x = t.x() - 16;
            match level(&b, l) % 3 {
                0 => fill(s, rect(x - 3, cy - 3, 6, 6), 3., &c.fg(0.8)),
                1 => {
                    let r = gsk::RoundedRect::from_rect(rect(x - 3, cy - 3, 6, 6), 3.);
                    s.append_border(&r, &[1.3; 4], &[c.fg(0.8); 4]);
                }
                _ => fill(s, rect(x - 2, cy - 2, 5, 5), 1., &c.fg(0.8)),
            }
        }
        Kind::Number => {
            let (text, line) = classify(&b, l);
            let above: Vec<String> = (1..l).rev().take(2000).map(|k| text_of(&b, k)).collect();
            let n = doc::number_of(above.iter().map(String::as_str), &line, &text);
            let layout = text_layout(v, &format!("{n}."), 1.0, None);
            let (w, lh) = layout.pixel_size();
            s.save();
            s.translate(&graphene::Point::new(
                (t.x() - 7 - w) as f32,
                (t.y() + (t.height() - lh) / 2) as f32,
            ));
            s.append_layout(&layout, &c.fg(0.62));
            s.restore();
        }
        Kind::Todo(done) => {
            let r = rect(t.x() - 26, cy - 8, 16, 16);
            if done {
                fill(s, r, 4., &c.accent);
                icon(
                    v,
                    s,
                    "object-select-symbolic",
                    t.x() - 26,
                    cy - 8,
                    16,
                    &c.on_accent,
                );
            } else {
                let rr = gsk::RoundedRect::from_rect(r, 4.);
                s.append_border(&rr, &[1.5; 4], &[c.fg(0.42); 4]);
            }
        }
        _ => {}
    }
    if v.imp().hover.get() == l {
        icon(
            v,
            s,
            "list-drag-handle-symbolic",
            g.left - 24,
            cy - 8,
            16,
            &c.fg(0.4),
        );
    }
    if caret && v.has_focus() && g.kind != Kind::Divider {
        let (text, line) = classify(&b, l);
        if doc::plain(&text, &line).is_empty() {
            let (hint, scale, weight) = match g.kind {
                Kind::H1 => ("Heading 1", 1.6, Some(pango::Weight::Bold)),
                Kind::H2 => ("Heading 2", 1.32, Some(pango::Weight::Semibold)),
                Kind::H3 => ("Heading 3", 1.12, Some(pango::Weight::Semibold)),
                Kind::Bullet | Kind::Number => ("List", 1.0, None),
                Kind::Todo(_) => ("To-do", 1.0, None),
                Kind::Quote => ("Quote", 1.0, None),
                Kind::Callout => ("Callout", 1.0, None),
                Kind::Code => ("Code", 0.9, None),
                _ => ("Type / for blocks", 1.0, None),
            };
            let layout = text_layout(v, hint, scale, weight);
            let (_, lh) = layout.pixel_size();
            s.save();
            s.translate(&graphene::Point::new(
                t.x() as f32,
                (t.y() + (t.height() - lh) / 2) as f32,
            ));
            s.append_layout(&layout, &c.fg(0.32));
            s.restore();
        }
    }
}

const BLOCKS: [(&str, &str, Kind, &str); 11] = [
    ("Text", "", Kind::Para, "paragraph plain p"),
    ("Heading 1", "#", Kind::H1, "h1 title big"),
    ("Heading 2", "##", Kind::H2, "h2 subtitle"),
    ("Heading 3", "###", Kind::H3, "h3"),
    ("Bulleted list", "-", Kind::Bullet, "ul bullet unordered"),
    ("Numbered list", "1.", Kind::Number, "ol ordered number"),
    ("To-do", "[]", Kind::Todo(false), "todo task check box"),
    ("Quote", ">", Kind::Quote, "blockquote citation"),
    ("Code", "```", Kind::Code, "snippet pre mono"),
    ("Divider", "---", Kind::Divider, "hr rule line separator"),
    ("Callout", "", Kind::Callout, "note info aside tip"),
];

pub struct Editor {
    pub view: NoteView,
    pub buffer: gtk::TextBuffer,
    loading: Cell<bool>,
    generation: Rc<Cell<u64>>,
    /// How the page's file laid out its blocks; each unusual gap is kept on a
    /// mark at the start of the block after it, so that it follows edits.
    layout: RefCell<doc::Layout>,
    gaps: RefCell<Vec<(gtk::TextMark, String)>>,
    pending: Cell<Option<(i32, i32)>>,
    slash_typed: Cell<Option<i32>>,
    shortcut: Cell<bool>,
    clamping: Cell<bool>,
    /// A reload keeps the view where it is.
    keep_view: Cell<bool>,
    /// The primary selection is being updated.
    primary: Cell<bool>,
    menu: gtk::Popover,
    menu_list: gtk::ListBox,
    /// Offset of the `/` that opened the menu.
    menu_at: Cell<Option<i32>>,
    drag_from: Cell<Option<(i32, f64)>>,
    drag_shift: Cell<bool>,
    /// The anchor and head lines of a block selection.
    block_ends: Cell<(i32, i32)>,
    /// Our own edits and moves, which keep a block selection.
    quiet: Cell<bool>,
    /// A style asked for with nothing selected: the next typed character gets it.
    pending_mark: Cell<Option<&'static str>>,
    typed: Cell<Option<i32>>,
    /// Vim keys, when on.
    vim: RefCell<Option<vim::Vim>>,
    /// The vim mode, and the pattern asked for by / or ?.
    pub status: gtk::Box,
    vim_label: gtk::Label,
    vim_entry: gtk::Entry,
    pub on_change: RefCell<Option<Box<dyn Fn()>>>,
}

impl Editor {
    pub fn new() -> Rc<Self> {
        let view: NoteView = glib::Object::new();
        view.add_css_class("note");
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_left_margin(M);
        view.set_right_margin(M);
        view.set_top_margin(48);
        view.set_bottom_margin(240);
        view.set_pixels_above_lines(5);
        view.set_pixels_below_lines(5);
        view.set_pixels_inside_wrap(3);
        view.set_hexpand(true);
        view.set_vexpand(true);
        let (gl, gr) = (gtk::Box::default(), gtk::Box::default());
        view.set_gutter(gtk::TextWindowType::Left, Some(&gl));
        view.set_gutter(gtk::TextWindowType::Right, Some(&gr));
        view.imp().gutters.replace(Some((gl, gr)));
        view.imp().hover.set(-1);
        view.imp().drop.set(-1);
        let buffer = view.buffer();
        buffer.set_enable_undo(true);
        make_tags(&buffer);

        let menu_list = gtk::ListBox::new();
        menu_list.set_selection_mode(gtk::SelectionMode::Single);
        menu_list.add_css_class("block-menu");
        for (name, hint, _, _) in BLOCKS {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            let label = gtk::Label::new(Some(name));
            label.set_xalign(0.);
            label.set_hexpand(true);
            row.append(&label);
            let h = gtk::Label::new(Some(hint));
            h.add_css_class("hint");
            row.append(&h);
            menu_list.append(&row);
        }
        let empty = gtk::Label::new(Some("No matching block"));
        empty.add_css_class("hint");
        empty.set_margin_top(8);
        empty.set_margin_bottom(8);
        menu_list.set_placeholder(Some(&empty));
        let menu = gtk::Popover::builder()
            .autohide(false)
            .has_arrow(false)
            .position(gtk::PositionType::Bottom)
            .halign(gtk::Align::Start)
            .child(&menu_list)
            .build();
        menu.add_css_class("block-menu");
        menu.set_parent(&view);
        menu.set_can_focus(false);
        menu_list.set_can_focus(false);
        let status = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        status.add_css_class("vim-status");
        status.set_halign(gtk::Align::End);
        status.set_valign(gtk::Align::End);
        status.set_visible(false);
        let vim_label = gtk::Label::new(None);
        let vim_entry = gtk::Entry::builder().width_chars(18).visible(false).build();
        status.append(&vim_label);
        status.append(&vim_entry);

        let e = Rc::new(Self {
            view,
            buffer,
            loading: Cell::new(false),
            generation: Rc::new(Cell::new(0)),
            layout: RefCell::default(),
            gaps: RefCell::default(),
            pending: Cell::new(None),
            slash_typed: Cell::new(None),
            shortcut: Cell::new(false),
            clamping: Cell::new(false),
            keep_view: Cell::new(false),
            primary: Cell::new(false),
            menu,
            menu_list,
            menu_at: Cell::new(None),
            drag_from: Cell::new(None),
            drag_shift: Cell::new(false),
            block_ends: Cell::new((0, 0)),
            quiet: Cell::new(false),
            pending_mark: Cell::new(None),
            typed: Cell::new(None),
            vim: RefCell::new(None),
            status,
            vim_label,
            vim_entry,
            on_change: RefCell::new(None),
        });
        e.connect();
        e
    }

    pub fn set_colors(&self, colors: Colors) {
        let table = self.buffer.tag_table();
        let set = |tag: &str, prop: &str, c: gdk::RGBA| {
            if let Some(t) = table.lookup(tag) {
                t.set_property(prop, c);
            }
        };
        set("icode", "background-rgba", colors.fg(0.08));
        set("link", "underline-rgba", colors.fg(0.4));
        set("done", "foreground-rgba", colors.muted);
        set("quote", "foreground-rgba", colors.fg(0.86));
        set("find", "background-rgba", colors.accent.with_alpha(0.3));
        self.view.imp().colors.replace(colors);
        self.view.queue_draw();
    }

    fn connect(self: &Rc<Self>) {
        let b = &self.buffer;
        let weak = Rc::downgrade(self);
        b.connect_insert_text(glib::clone!(
            #[strong]
            weak,
            move |_, iter, text| {
                let Some(e) = weak.upgrade() else { return };
                let first = iter.line();
                let last = first + text.matches('\n').count() as i32;
                e.mark_pending(first, last);
                if text == "/" {
                    e.slash_typed.set(Some(iter.offset()));
                }
                if text == " " || text == "`" {
                    e.shortcut.set(true);
                }
                if e.pending_mark.get().is_some()
                    && !e.quiet.get()
                    && text.chars().count() == 1
                    && !text.starts_with(char::is_control)
                {
                    e.typed.set(Some(iter.offset()));
                }
            }
        ));
        b.connect_delete_range(glib::clone!(
            #[strong]
            weak,
            move |_, s, _| {
                if let Some(e) = weak.upgrade() {
                    e.mark_pending(s.line(), s.line());
                }
            }
        ));
        b.connect_changed(glib::clone!(
            #[strong]
            weak,
            move |_| {
                let Some(e) = weak.upgrade() else { return };
                if e.loading.get() {
                    return;
                }
                if let Some((a, z)) = e.pending.take() {
                    e.restyle_range(a, z);
                }
                if !e.quiet.get() {
                    e.clear_blocks();
                }
                if let Some(at) = e.typed.take()
                    && let Some(mark) = e.pending_mark.take()
                {
                    let w = Rc::downgrade(&e);
                    glib::idle_add_local_once(move || {
                        if let Some(e) = w.upgrade() {
                            e.style_typed(at, mark);
                        }
                    });
                }
                if let Some(at) = e.slash_typed.take() {
                    e.maybe_open_menu(at);
                }
                if e.shortcut.take() {
                    let w = Rc::downgrade(&e);
                    glib::idle_add_local_once(move || {
                        if let Some(e) = w.upgrade() {
                            e.shortcuts();
                        }
                    });
                }
                e.update_menu();
                if let Some(f) = e.on_change.borrow().as_ref() {
                    f();
                }
            }
        ));
        b.connect_mark_set(glib::clone!(
            #[strong]
            weak,
            move |b, _, mark| {
                let Some(e) = weak.upgrade() else { return };
                if *mark != b.get_insert() || e.clamping.get() || e.loading.get() {
                    return;
                }
                if !e.quiet.get() {
                    e.clear_blocks();
                    e.pending_mark.set(None);
                }
                e.clamp();
                e.update_menu();
                e.view.queue_draw();
            }
        ));

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(glib::clone!(
            #[strong]
            weak,
            move |_, key, _, state| {
                let Some(e) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if e.key(key, state) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.view.add_controller(keys);

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(glib::clone!(
            #[strong]
            weak,
            move |_, x, y| {
                let Some(e) = weak.upgrade() else { return };
                let l = e.line_at(x, y).unwrap_or(-1);
                let imp = e.view.imp();
                if imp.hover.replace(l) != l {
                    e.view.queue_draw();
                }
            }
        ));
        motion.connect_leave(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(e) = weak.upgrade() {
                    e.view.imp().hover.set(-1);
                    e.view.queue_draw();
                }
            }
        ));
        self.view.add_controller(motion);

        let drag = gtk::GestureDrag::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        drag.connect_drag_begin(glib::clone!(
            #[strong]
            weak,
            move |g, x, y| {
                let Some(e) = weak.upgrade() else { return };
                if e.press(g, x, y) {
                    g.set_state(gtk::EventSequenceState::Claimed);
                } else {
                    g.set_state(gtk::EventSequenceState::Denied);
                }
            }
        ));
        drag.connect_drag_update(glib::clone!(
            #[strong]
            weak,
            move |g, _, dy| {
                let Some(e) = weak.upgrade() else { return };
                let Some((_, y0)) = e.drag_from.get() else {
                    return;
                };
                let Some((x, _)) = g.start_point() else {
                    return;
                };
                let target = e.drop_line(x, y0 + dy);
                if e.view.imp().drop.replace(target) != target {
                    e.view.queue_draw();
                }
            }
        ));
        drag.connect_drag_end(glib::clone!(
            #[strong]
            weak,
            move |_, _, dy| {
                let Some(e) = weak.upgrade() else { return };
                let Some((from, _)) = e.drag_from.take() else {
                    return;
                };
                let to = e.view.imp().drop.replace(-1);
                e.view.queue_draw();
                let selected = e
                    .view
                    .imp()
                    .blocks
                    .get()
                    .filter(|(a, z)| (*a..=*z).contains(&from));
                if dy.abs() < 4. || to < 1 {
                    let shift = e.drag_shift.get();
                    match e.view.imp().blocks.get() {
                        Some(_) if shift => e.select_blocks(e.block_ends.get().0, from),
                        _ => e.select_blocks(from, from),
                    }
                } else if let Some((a, z)) = selected {
                    if let Some(at) = e.move_range(a, z - a + 1, to) {
                        e.select_blocks(at, at + z - a);
                    }
                } else {
                    e.move_line(from, to);
                }
            }
        ));
        self.view.add_controller(drag);

        for (signal, cut) in [("copy-clipboard", false), ("cut-clipboard", true)] {
            let weak = weak.clone();
            self.view.connect_local(signal, false, move |args| {
                if let Some(e) = weak.upgrade() {
                    e.copy(cut);
                    args[0]
                        .get::<gtk::TextView>()
                        .unwrap()
                        .stop_signal_emission_by_name(signal);
                }
                None
            });
        }
        self.view.connect_paste_clipboard(glib::clone!(
            #[strong]
            weak,
            move |v| {
                v.stop_signal_emission_by_name("paste-clipboard");
                let weak = weak.clone();
                v.clipboard()
                    .read_text_async(gio::Cancellable::NONE, move |r| {
                        if let (Some(e), Ok(Some(t))) = (weak.upgrade(), r) {
                            e.paste(&t);
                        }
                    });
            }
        ));
        self.view.set_has_tooltip(true);
        self.view.connect_query_tooltip(glib::clone!(
            #[strong]
            weak,
            move |_, x, y, keyboard, tip| {
                let Some(e) = weak.upgrade() else {
                    return false;
                };
                if keyboard {
                    return false;
                }
                match e.link_at(x as f64, y as f64) {
                    Some(url) => {
                        tip.set_text(Some(&format!("{url}\nCtrl+click to open")));
                        true
                    }
                    None => false,
                }
            }
        ));
        // The toolkit would offer the selection as it is in the buffer, its
        // line breaks U+2028: the primary selection gets Markdown instead.
        self.view.connect_realize(glib::clone!(
            #[strong]
            weak,
            move |v| {
                if let Some(e) = weak.upgrade() {
                    e.buffer.remove_selection_clipboard(&v.primary_clipboard());
                }
            }
        ));
        // Given back before the toolkit's unrealize removes it, which would warn.
        self.view.connect_unrealize(glib::clone!(
            #[strong]
            weak,
            move |v| {
                if let Some(e) = weak.upgrade() {
                    e.buffer.add_selection_clipboard(&v.primary_clipboard());
                }
            }
        ));
        b.connect_mark_set(glib::clone!(
            #[strong]
            weak,
            move |b, _, mark| {
                let Some(e) = weak.upgrade() else { return };
                let ends = *mark == b.selection_bound() || *mark == b.get_insert();
                if !ends || !b.has_selection() || e.primary.replace(true) {
                    return;
                }
                let w = Rc::downgrade(&e);
                glib::idle_add_local_once(move || {
                    if let Some(e) = w.upgrade() {
                        e.primary.set(false);
                        if let Some(md) = e.selection_markdown() {
                            e.view.primary_clipboard().set_text(&md);
                        }
                    }
                });
            }
        ));
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(e) = weak.upgrade() {
                    e.close_menu();
                    e.view.queue_draw();
                }
            }
        ));
        self.view.add_controller(focus);
        self.menu_list.connect_row_activated(glib::clone!(
            #[strong]
            weak,
            move |_, row| {
                if let Some(e) = weak.upgrade() {
                    e.apply_menu(row.index() as usize);
                }
            }
        ));
        self.vim_entry.connect_activate(glib::clone!(
            #[strong]
            weak,
            move |entry| {
                let Some(e) = weak.upgrade() else { return };
                let pattern = entry.text().to_string();
                let forward = e.vim_label.text().starts_with('/');
                e.vim_done();
                if let Some(v) = e.vim.borrow_mut().as_mut() {
                    v.search(&mut Gtk(&e), &pattern, forward);
                }
                e.vim_status();
            }
        ));
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[strong]
            weak,
            move |_, key, _, _| match weak.upgrade() {
                Some(e) if key == gdk::Key::Escape => {
                    e.vim_done();
                    e.vim_status();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        ));
        self.vim_entry.add_controller(keys);
    }

    /// Turns vim keys on or off.
    pub fn set_vim(&self, on: bool) {
        self.vim.replace(on.then(vim::Vim::new));
        self.status.set_visible(on);
        self.vim_status();
    }
    /// Insert mode, for what is about to be typed (a title being renamed).
    pub fn vim_insert(&self) {
        if let Some(v) = self.vim.borrow_mut().as_mut() {
            v.reset(true);
        }
        self.vim_status();
    }
    fn vim_status(&self) {
        let vim = self.vim.borrow();
        let Some(v) = vim.as_ref() else {
            self.view.set_overwrite(false);
            return;
        };
        // A block cursor in normal mode, a line in insert mode.
        self.view.set_overwrite(v.mode != vim::Mode::Insert);
        if !WidgetExt::is_visible(&self.vim_entry) {
            let pending = v.pending();
            self.vim_label.set_text(&if pending.is_empty() {
                v.name().to_string()
            } else {
                format!("{}  {pending}", v.name())
            });
        }
    }
    fn vim_done(&self) {
        self.vim_entry.set_visible(false);
        self.view.grab_focus();
    }
    /// A key in normal or visual mode: it never types.
    fn vim_key(&self, key: gdk::Key, mods: gdk::ModifierType) -> bool {
        use gdk::{Key as K, ModifierType as Mod};
        let ctrl = mods.contains(Mod::CONTROL_MASK);
        let k = match key {
            K::Escape => vim::ESC,
            K::bracketleft if ctrl => vim::ESC,
            K::r if ctrl => vim::CTRL_R,
            // Copy, undo, redo, select all and scrolling work as usual.
            K::c | K::z | K::Z | K::a if ctrl => return false,
            K::Page_Up | K::Page_Down => return false,
            _ if ctrl || mods.intersects(Mod::ALT_MASK | Mod::SUPER_MASK) => return true,
            K::Left | K::KP_Left | K::BackSpace => 'h',
            K::Right | K::KP_Right => 'l',
            K::Up | K::KP_Up => 'k',
            K::Down | K::KP_Down => 'j',
            K::Home => '0',
            K::End => '$',
            K::Return | K::KP_Enter => 'j',
            _ => match key.to_unicode() {
                Some(c) if !c.is_control() => c,
                _ => return true,
            },
        };
        self.vim_feed(k);
        true
    }
    fn vim_feed(&self, k: char) {
        let prompt = {
            let mut vim = self.vim.borrow_mut();
            let Some(v) = vim.as_mut() else { return };
            v.key(&mut Gtk(self), k);
            v.prompt.take()
        };
        if let Some(forward) = prompt {
            self.vim_label.set_text(if forward { "/" } else { "?" });
            self.vim_entry.set_text("");
            self.vim_entry.set_visible(true);
            self.vim_entry.grab_focus();
        }
        self.vim_status();
    }

    fn mark_pending(&self, a: i32, z: i32) {
        let (a, z) = match self.pending.get() {
            Some((pa, pz)) => (pa.min(a), pz.max(z)),
            None => (a, z),
        };
        self.pending.set(Some((a, z)));
    }

    /// Shows a page, its Markdown `body` under the title on the first line.
    pub fn load(&self, title: &str, body: &str, focus_title: bool) {
        self.close_menu();
        self.loading.set(true);
        let (body, layout) = doc::to_editor(body);
        let b = &self.buffer;
        for (m, _) in self.gaps.take() {
            b.delete_mark(&m);
        }
        b.begin_irreversible_action();
        b.set_text(&format!("{title}\n{body}"));
        b.end_irreversible_action();
        // Block k is line k + 1: its gap sits at the start of line k + 2.
        let gaps = layout
            .gaps
            .iter()
            .filter_map(|(k, g)| {
                let at = b.iter_at_line(*k as i32 + 2)?;
                Some((b.create_mark(None, &at, true), g.clone()))
            })
            .collect();
        self.gaps.replace(gaps);
        self.layout.replace(layout);
        self.pending.set(None);
        if let Some(v) = self.vim.borrow_mut().as_mut() {
            v.reset(focus_title);
        }
        self.vim_status();
        let n = b.line_count();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.restyle_lines(0, n.min(400));
        b.set_modified(false);
        let start = if focus_title || n < 2 {
            line_end(b, 0)
        } else {
            let (_, line) = classify(b, 1);
            at(b, 1, line.start)
        };
        b.place_cursor(&start);
        self.loading.set(false);
        // Deferred until the lines are laid out; the top of the page shows first.
        if !self.keep_view.get() {
            self.view.scroll_to_mark(&b.get_insert(), 0., true, 0., 0.);
        }
        // The rest is styled in slices, so a long page opens at once.
        let mut from = 400;
        let buffer = b.downgrade();
        if from < n {
            let latest = self.generation.clone();
            glib::idle_add_local(move || {
                let Some(b) = buffer.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                if latest.get() != generation {
                    return glib::ControlFlow::Break;
                }
                let to = (from + 800).min(b.line_count());
                restyle_lines(&b, from, to);
                from = to;
                if from >= b.line_count() {
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
        }
    }

    /// The page again as changed on disk, keeping the caret and the scroll.
    pub fn reload(&self, body: &str) {
        let offset = self.buffer.cursor_position();
        let y = self.view.vadjustment().map(|a| a.value());
        self.keep_view.set(true);
        self.load(&self.title(), body, false);
        self.keep_view.set(false);
        self.buffer
            .place_cursor(&self.buffer.iter_at_offset(offset));
        if let (Some(adj), Some(y)) = (self.view.vadjustment(), y) {
            // After the new lines are measured.
            glib::idle_add_local_once(move || adj.set_value(y));
        }
    }

    pub fn title(&self) -> String {
        text_of(&self.buffer, 0)
    }
    /// The page as Markdown.
    pub fn body(&self) -> String {
        let b = &self.buffer;
        let text = b.text(&b.start_iter(), &b.end_iter(), true);
        let mut layout = self.layout.borrow().clone();
        // Where the gaps are now; one whose line was joined to another is gone.
        layout.gaps = self
            .gaps
            .borrow()
            .iter()
            .filter_map(|(m, g)| {
                let at = b.iter_at_mark(m);
                (at.starts_line() && at.line() >= 2).then(|| (at.line() as usize - 2, g.clone()))
            })
            .collect();
        doc::from_editor(text.split('\n').skip(1), &layout)
    }
    /// Selects the first match of `query` in the body.
    pub fn find(&self, query: &str) {
        let b = &self.buffer;
        let Some(start) = b.iter_at_line(1) else {
            return;
        };
        if let Some((s, e)) =
            start.forward_search(query, gtk::TextSearchFlags::CASE_INSENSITIVE, None)
        {
            b.select_range(&s, &e);
            let mut s = s;
            self.view.scroll_to_iter(&mut s, 0.1, true, 0., 0.3);
        }
    }

    fn restyle_lines(&self, a: i32, z: i32) {
        restyle_lines(&self.buffer, a, z);
    }
    /// Restyles changed lines, and the nested blocks below whose level follows them.
    fn restyle_range(&self, a: i32, z: i32) {
        let b = &self.buffer;
        let n = b.line_count();
        let z = z.min(n - 1);
        let mut end = z + 1;
        while end < n && end - z < 400 {
            let (t, line) = classify(b, end);
            if !t.trim().is_empty() && line.spaces == 0 {
                break;
            }
            end += 1;
        }
        restyle_lines(b, a.max(0), end);
    }

    /// Keeps the caret off hidden markers.
    fn clamp(&self) {
        let b = &self.buffer;
        if b.has_selection() {
            return;
        }
        let c = cursor(b);
        let l = c.line();
        if l == 0 {
            return;
        }
        let (_, line) = classify(b, l);
        let off = c.line_offset() as usize;
        let to = off.clamp(line.start, line.end.max(line.start));
        if to != off {
            self.clamping.set(true);
            b.place_cursor(&at(b, l, to));
            self.clamping.set(false);
        }
    }

    fn line_at(&self, x: f64, y: f64) -> Option<i32> {
        let (_, by) =
            self.view
                .window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let (iter, top) = self.view.line_at_y(by);
        let (_, h) = self.view.line_yrange(&iter);
        (by >= top && by < top + h && iter.line() > 0).then_some(iter.line())
    }
    fn drop_line(&self, x: f64, y: f64) -> i32 {
        let (_, by) =
            self.view
                .window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let (iter, top) = self.view.line_at_y(by);
        let (_, h) = self.view.line_yrange(&iter);
        let l = iter.line() + i32::from(by > top + h / 2);
        l.clamp(1, self.buffer.line_count())
    }

    /// A press on a handle, a checkbox or a link with Ctrl held.
    fn press(&self, g: &gtk::GestureDrag, x: f64, y: f64) -> bool {
        let state = g.current_event_state();
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        self.drag_shift
            .set(state.contains(gdk::ModifierType::SHIFT_MASK));
        if ctrl && let Some(url) = self.link_at(x, y) {
            let root = self.view.root().and_downcast::<gtk::Window>();
            gtk::UriLauncher::new(&url).launch(root.as_ref(), gio::Cancellable::NONE, |_| {});
            return true;
        }
        let Some(l) = self.line_at(x, y) else {
            return false;
        };
        let (bx, by) =
            self.view
                .window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let Some(g) = geometry(&self.view, l) else {
            return false;
        };
        let cy = g.text.y() + g.text.height() / 2;
        let inside = |x0: i32, w: i32| bx >= x0 && bx < x0 + w && (by - cy).abs() <= 12;
        if inside(g.left - 28, 24) {
            self.drag_from.set(Some((l, y)));
            return true;
        }
        if matches!(g.kind, Kind::Todo(_)) && inside(g.text.x() - 30, 26) {
            self.toggle_todo(l);
            return true;
        }
        self.clear_blocks();
        false
    }

    fn link_at(&self, x: f64, y: f64) -> Option<String> {
        let (bx, by) =
            self.view
                .window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let iter = self.view.iter_at_location(bx, by)?;
        let l = iter.line();
        if l == 0 {
            return None;
        }
        let (text, line) = classify(&self.buffer, l);
        if line.kind == Kind::Code {
            return None;
        }
        let cs = chars(&text);
        let off = iter.line_offset() as usize;
        doc::inline(&cs, line.start, line.end)
            .into_iter()
            .find_map(|(r, m)| match m {
                Mark::Link(url) if r.contains(&off) => Some(cs[url].iter().collect()),
                _ => None,
            })
    }

    fn edit(&self, f: impl FnOnce(&gtk::TextBuffer)) {
        self.buffer.begin_user_action();
        f(&self.buffer);
        self.buffer.end_user_action();
    }
    fn replace(&self, l: i32, from: usize, to: usize, text: &str) {
        let b = &self.buffer;
        let mut s = at(b, l, from);
        let mut e = at(b, l, to);
        b.delete(&mut s, &mut e);
        let mut s = at(b, l, from);
        b.insert(&mut s, text);
    }

    fn set_kind(&self, l: i32, kind: Kind) {
        let b = &self.buffer;
        let (text, line) = classify(b, l);
        let plain = doc::plain(&text, &line);
        let caret = (cursor(b).line_offset() as usize).saturating_sub(line.start);
        let new = doc::wrap(kind, line.spaces, &plain);
        let n = text.chars().count();
        self.edit(|_| self.replace(l, 0, n, &new));
        let nl = doc::classify(&new);
        let off = (nl.start + caret.min(plain.chars().count())).min(nl.end.max(nl.start));
        b.place_cursor(&at(b, l, off));
    }

    fn toggle_todo(&self, l: i32) {
        let (_, line) = classify(&self.buffer, l);
        if let Kind::Todo(done) = line.kind {
            let box_at = line.spaces + 3;
            self.edit(|_| self.replace(l, box_at, box_at + 1, if done { " " } else { "x" }));
        }
    }

    /// Selects whole blocks from `anchor` to `head`, with what is nested in the last.
    pub fn select_blocks(&self, anchor: i32, head: i32) {
        let b = &self.buffer;
        let n = b.line_count();
        if n < 2 {
            return;
        }
        let (anchor, head) = (anchor.clamp(1, n - 1), head.clamp(1, n - 1));
        self.block_ends.set((anchor, head));
        let (lo, hi) = (anchor.min(head), anchor.max(head));
        let hi = (hi + self.span_of(hi) - 1).min(n - 1);
        self.view.imp().blocks.set(Some((lo, hi)));
        self.quiet.set(true);
        b.place_cursor(&at(b, head, classify(b, head).1.start));
        self.quiet.set(false);
        self.view.set_cursor_visible(false);
        self.view.grab_focus();
        self.view.queue_draw();
    }
    fn clear_blocks(&self) {
        if self.view.imp().blocks.take().is_some() {
            self.view.set_cursor_visible(true);
            self.view.queue_draw();
        }
    }
    /// The selected blocks' Markdown.
    fn blocks_markdown(&self, lo: i32, hi: i32) -> String {
        let lines: Vec<String> = (lo..=hi).map(|l| text_of(&self.buffer, l)).collect();
        doc::from_editor(lines.iter().map(String::as_str), &doc::Layout::default())
    }
    fn delete_lines(&self, lo: i32, hi: i32) {
        let b = &self.buffer;
        let n = b.line_count();
        self.edit(|b| {
            let (mut s, mut e) = if hi + 1 < n {
                (b.iter_at_line(lo).unwrap(), b.iter_at_line(hi + 1).unwrap())
            } else {
                (line_end(b, lo - 1), b.end_iter())
            };
            b.delete(&mut s, &mut e);
        });
        let l = lo.min(b.line_count() - 1);
        b.place_cursor(&at(b, l, classify(b, l).1.start));
    }

    /// Keys while blocks are selected; None when none are.
    fn block_key(&self, key: gdk::Key, mods: gdk::ModifierType) -> Option<bool> {
        use gdk::ModifierType as Mod;
        let (lo, hi) = self.view.imp().blocks.get()?;
        let (anchor, head) = self.block_ends.get();
        let b = &self.buffer;
        let n = b.line_count();
        let ctrl = mods == Mod::CONTROL_MASK;
        let clip = |md: String| self.view.clipboard().set_text(&md);
        match key {
            gdk::Key::Escape => self.clear_blocks(),
            gdk::Key::Up | gdk::Key::Down if mods.is_empty() => {
                let l = if key == gdk::Key::Up { lo - 1 } else { hi + 1 };
                self.select_blocks(l, l);
            }
            gdk::Key::Up | gdk::Key::Down if mods == Mod::SHIFT_MASK => {
                let d = if key == gdk::Key::Up { -1 } else { 1 };
                self.select_blocks(anchor, head + d);
            }
            gdk::Key::a if ctrl => self.select_blocks(1, n - 1),
            gdk::Key::c if ctrl => clip(self.blocks_markdown(lo, hi)),
            gdk::Key::x if ctrl => {
                clip(self.blocks_markdown(lo, hi));
                self.delete_lines(lo, hi);
            }
            gdk::Key::BackSpace | gdk::Key::Delete | gdk::Key::KP_Delete => {
                self.delete_lines(lo, hi)
            }
            gdk::Key::d if ctrl => {
                let text: Vec<String> = (lo..=hi).map(|l| text_of(b, l)).collect();
                self.edit(|b| {
                    let mut e = line_end(b, hi);
                    b.insert(&mut e, &format!("\n{}", text.join("\n")));
                });
                self.select_blocks(hi + 1, hi + 1 + hi - lo);
            }
            gdk::Key::Up | gdk::Key::Down if mods == Mod::ALT_MASK => {
                let count = hi - lo + 1;
                let to = if key == gdk::Key::Up {
                    lo - 1
                } else if hi + 1 < n {
                    hi + 1 + self.span_of(hi + 1)
                } else {
                    return Some(true);
                };
                if let Some(at) = self.move_range(lo, count, to) {
                    self.select_blocks(at, at + count - 1);
                }
            }
            gdk::Key::Tab | gdk::Key::ISO_Left_Tab => {
                let deeper = mods.is_empty();
                self.quiet.set(true);
                self.nest(lo, hi - lo + 1, deeper);
                self.quiet.set(false);
                self.select_blocks(anchor, head);
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                self.clear_blocks();
                b.place_cursor(&at(b, head, classify(b, head).1.end));
            }
            gdk::Key::Shift_L
            | gdk::Key::Shift_R
            | gdk::Key::Control_L
            | gdk::Key::Control_R
            | gdk::Key::Alt_L
            | gdk::Key::Alt_R => {}
            _ => {
                // Anything else edits text again, at the end of the last block.
                self.clear_blocks();
                b.place_cursor(&at(b, head, classify(b, head).1.end));
                return Some(false);
            }
        }
        Some(true)
    }

    /// How many lines a block spans: it takes the blocks nested under it.
    fn span_of(&self, l: i32) -> i32 {
        let b = &self.buffer;
        let (_, line) = classify(b, l);
        let mut k = 1;
        while l + k < b.line_count() {
            let (t, next) = classify(b, l + k);
            if t.trim().is_empty() || next.spaces <= line.spaces {
                break;
            }
            k += 1;
        }
        k
    }

    /// Moves the block at line `from`, with what is nested under it, to before line `to`.
    fn move_line(&self, from: i32, to: i32) {
        let caret = cursor(&self.buffer);
        let (row, col) = (caret.line() - from, caret.line_offset());
        let k = self.span_of(from);
        if let Some(dest) = self.move_range(from, k, to)
            && (0..k).contains(&row)
        {
            let b = &self.buffer;
            b.place_cursor(&at(b, dest + row, col as usize));
            self.clamp();
        }
    }

    /// Moves `k` lines from `from` to before line `to`; returns where they start.
    fn move_range(&self, from: i32, k: i32, to: i32) -> Option<i32> {
        let b = &self.buffer;
        let n = b.line_count();
        if from < 1 || to < 1 || from + k > n || (from..=from + k).contains(&to) {
            return None;
        }
        let lines: Vec<String> = (from..from + k).map(|l| text_of(b, l)).collect();
        let text = lines.join("\n");
        let dest = if to > from { to - k } else { to };
        self.quiet.set(true);
        self.edit(|b| {
            let (mut s, mut e) = if from + k < n {
                (
                    b.iter_at_line(from).unwrap(),
                    b.iter_at_line(from + k).unwrap(),
                )
            } else {
                (line_end(b, from - 1), b.end_iter())
            };
            b.delete(&mut s, &mut e);
            if dest >= b.line_count() {
                let mut end = b.end_iter();
                b.insert(&mut end, &format!("\n{text}"));
            } else {
                let mut s = b.iter_at_line(dest).unwrap();
                b.insert(&mut s, &format!("{text}\n"));
            }
        });
        self.quiet.set(false);
        Some(dest)
    }

    /// Handles a key; true when it was used.
    fn key(&self, key: gdk::Key, state: gdk::ModifierType) -> bool {
        use gdk::ModifierType as Mod;
        let mods = state & (Mod::CONTROL_MASK | Mod::SHIFT_MASK | Mod::ALT_MASK | Mod::SUPER_MASK);
        let b = &self.buffer;
        let mode = self.vim.borrow().as_ref().map(|v| v.mode);
        if mode.is_some_and(|m| m != vim::Mode::Insert) {
            if self.view.imp().blocks.get().is_some() && self.block_key(key, mods) == Some(true) {
                return true;
            }
            return self.vim_key(key, mods);
        }
        if let Some(used) = self.block_key(key, mods) {
            return used;
        }
        let esc = (key == gdk::Key::Escape && mods.is_empty())
            || (key == gdk::Key::bracketleft && mods == Mod::CONTROL_MASK);
        if mode.is_some() && esc {
            self.close_menu();
            self.vim_feed(vim::ESC);
            return true;
        }
        if self.menu_at.get().is_some() {
            match key {
                gdk::Key::Up | gdk::Key::Down => {
                    self.menu_step(if key == gdk::Key::Up { -1 } else { 1 });
                    return true;
                }
                gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Tab => {
                    if let Some(row) = self.menu_list.selected_row() {
                        self.apply_menu(row.index() as usize);
                        return true;
                    }
                    self.close_menu();
                }
                gdk::Key::Escape => {
                    self.close_menu();
                    return true;
                }
                _ => {}
            }
        }
        if key == gdk::Key::Escape && mods.is_empty() {
            let l = cursor(b).line();
            if l >= 1 {
                self.select_blocks(l, l);
            }
            return true;
        }
        let ctrl = mods == Mod::CONTROL_MASK;
        if ctrl {
            match key {
                gdk::Key::b => return self.inline_toggle("**"),
                gdk::Key::i => return self.inline_toggle("*"),
                gdk::Key::e => return self.inline_toggle("`"),
                gdk::Key::k => return self.link_prompt(),
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    self.toggle_todo(cursor(b).line());
                    return true;
                }
                _ => return false,
            }
        }
        if mods == Mod::ALT_MASK && matches!(key, gdk::Key::Up | gdk::Key::Down) {
            let l = cursor(b).line();
            if key == gdk::Key::Up && l > 1 {
                self.move_line(l, l - 1);
            } else if key == gdk::Key::Down && l >= 1 {
                let below = l + self.span_of(l);
                if below < b.line_count() {
                    self.move_line(l, below + self.span_of(below));
                }
            }
            return true;
        }
        let c = cursor(b);
        let l = c.line();
        let off = c.line_offset() as usize;
        let plain_key = mods.is_empty() && !b.has_selection();
        match key {
            gdk::Key::Return | gdk::Key::KP_Enter if mods.is_empty() => self.enter(l, off),
            gdk::Key::Return | gdk::Key::KP_Enter if mods == Mod::SHIFT_MASK => {
                self.soft_break(l, off)
            }
            gdk::Key::BackSpace if plain_key => self.backspace(l, off),
            gdk::Key::Delete | gdk::Key::KP_Delete if plain_key => self.delete(l, off),
            gdk::Key::Tab if mods.is_empty() => self.indent(l, true),
            gdk::Key::ISO_Left_Tab | gdk::Key::Tab if mods == Mod::SHIFT_MASK => {
                self.indent(l, false)
            }
            // Arrows step over hidden markers one at a time, so that the caret
            // can sit inside or outside a bold span at the same visual place,
            // and cross into the next block at its text.
            gdk::Key::Left if plain_key && l >= 1 => {
                let (text, line) = classify(b, l);
                if off <= line.start {
                    let to = if l == 1 {
                        line_end(b, 0)
                    } else {
                        at(b, l - 1, classify(b, l - 1).1.end)
                    };
                    b.place_cursor(&to);
                    return true;
                }
                match self.hidden_runs(&text, &line).iter().find(|r| r.end == off) {
                    Some(r) => {
                        b.place_cursor(&at(b, l, r.start));
                        true
                    }
                    None => false,
                }
            }
            gdk::Key::Right if plain_key && l >= 1 => {
                let (text, line) = classify(b, l);
                if off >= line.end {
                    if l + 1 < b.line_count() {
                        let (_, next) = classify(b, l + 1);
                        b.place_cursor(&at(b, l + 1, next.start));
                    }
                    return true;
                }
                match self
                    .hidden_runs(&text, &line)
                    .iter()
                    .find(|r| r.start == off)
                {
                    Some(r) => {
                        b.place_cursor(&at(b, l, r.end));
                        true
                    }
                    None => false,
                }
            }
            _ => {
                // Typing on a divider starts a paragraph after it.
                if l >= 1
                    && (mods.is_empty() || mods == Mod::SHIFT_MASK)
                    && key.to_unicode().is_some_and(|c| !c.is_control())
                    && classify(b, l).1.kind == Kind::Divider
                {
                    let mut e = line_end(b, l);
                    self.edit(|b| b.insert(&mut e, "\n"));
                    b.place_cursor(&b.iter_at_line(l + 1).unwrap());
                }
                false
            }
        }
    }

    fn enter(&self, l: i32, off: usize) -> bool {
        let b = &self.buffer;
        if l == 0 {
            // The title ends; the page starts below it.
            let mut e = line_end(b, 0);
            if off as i32 == e.line_offset() {
                if b.line_count() < 2 || !text_of(b, 1).is_empty() {
                    self.edit(|b| b.insert(&mut e, "\n"));
                }
                b.place_cursor(&b.iter_at_line(1).unwrap());
                return true;
            }
            return false;
        }
        let (text, line) = classify(b, l);
        let plain = doc::plain(&text, &line);
        let cs = chars(&text);
        let pad = " ".repeat(line.spaces);
        match line.kind {
            Kind::Para if line.spaces == 0 => false,
            Kind::Divider => {
                let mut e = line_end(b, l);
                self.edit(|b| b.insert(&mut e, &format!("\n{pad}")));
                b.place_cursor(&b.iter_at_line(l + 1).unwrap());
                true
            }
            Kind::Code | Kind::Quote | Kind::Callout => {
                // Inside: a line break. On an empty last line: out of the block.
                let last_empty = off >= line.end
                    && line.end > line.start
                    && (plain.ends_with(SOFT) || plain.is_empty());
                if plain.is_empty() && line.kind != Kind::Code {
                    self.set_kind(l, Kind::Para);
                } else if last_empty {
                    let cut = cs[..line.end]
                        .iter()
                        .rposition(|c| *c == SOFT)
                        .unwrap_or(line.end);
                    self.edit(|b| {
                        self.replace(l, cut, line.end, "");
                        let mut e = line_end(b, l);
                        b.insert(&mut e, &format!("\n{pad}"));
                    });
                    b.place_cursor(&line_end(b, l + 1));
                } else {
                    return self.soft_break(l, off);
                }
                true
            }
            kind => {
                if plain.is_empty() && (kind.is_list() || kind == Kind::Para) {
                    if line.spaces > 0 {
                        return self.indent(l, false);
                    }
                    self.set_kind(l, Kind::Para);
                    return true;
                }
                let next = match kind {
                    Kind::Todo(_) => Kind::Todo(false),
                    k if k.is_list() => k,
                    _ => Kind::Para,
                };
                let prefix = if kind == Kind::Number {
                    // The next number, in the same style.
                    let marker: String = cs[line.spaces..line.start].iter().collect();
                    let digits: String = marker.chars().take_while(char::is_ascii_digit).collect();
                    let n: u64 = digits.parse().unwrap_or(0);
                    format!("{pad}{}{}", n + 1, &marker[digits.len()..])
                } else {
                    doc::wrap(next, line.spaces, "")
                };
                if off <= line.start && !plain.is_empty() {
                    // At the start: an empty block of the same kind above.
                    let mut s = b.iter_at_line(l).unwrap();
                    let above =
                        doc::wrap(if kind.is_list() { next } else { kind }, line.spaces, "");
                    self.edit(|b| b.insert(&mut s, &format!("{above}\n")));
                    let (_, nl) = classify(b, l + 1);
                    b.place_cursor(&at(b, l + 1, nl.start));
                } else {
                    let mut c = cursor(b);
                    self.edit(|b| b.insert(&mut c, &format!("\n{prefix}")));
                    b.place_cursor(&at(b, l + 1, prefix.chars().count()));
                }
                true
            }
        }
    }

    fn soft_break(&self, l: i32, _off: usize) -> bool {
        let b = &self.buffer;
        if l == 0 {
            return true;
        }
        let (_, line) = classify(b, l);
        if matches!(line.kind, Kind::H1 | Kind::H2 | Kind::H3 | Kind::Divider) {
            return true;
        }
        let text = format!("{SOFT}{}", doc::continuation(&line));
        let mut c = cursor(b);
        self.edit(|b| b.insert(&mut c, &text));
        true
    }

    fn backspace(&self, l: i32, off: usize) -> bool {
        let b = &self.buffer;
        if l == 0 {
            return false;
        }
        let (text, line) = classify(b, l);
        let cs = chars(&text);
        if off <= line.start {
            if line.spaces > 0 {
                return self.indent(l, false);
            }
            if line.kind != Kind::Para {
                self.set_kind(l, Kind::Para);
                return true;
            }
            if l == 1 {
                return false;
            }
            let (_, prev) = classify(b, l - 1);
            if prev.kind == Kind::Divider {
                self.edit(|b| {
                    let mut s = b.iter_at_line(l - 1).unwrap();
                    let mut e = b.iter_at_line(l).unwrap();
                    b.delete(&mut s, &mut e);
                });
                return true;
            }
            // Joins this block's text to the end of the one above.
            let into = prev.end;
            self.edit(|b| {
                let mut s = line_end(b, l - 1);
                let mut e = line_end(b, l);
                b.delete(&mut s, &mut e);
                let mut at_ = at(b, l - 1, into);
                b.insert(&mut at_, &text);
            });
            b.place_cursor(&at(b, l - 1, into));
            return true;
        }
        // A hidden run just before the caret: delete what the reader sees.
        let runs = self.hidden_runs(&text, &line);
        if let Some(r) = runs.iter().find(|r| r.end == off && r.start > line.start) {
            // After a soft break the break goes with it; else the letter before.
            let to = if cs[r.start - 1] == SOFT {
                off
            } else {
                r.start
            };
            self.edit(|_| self.replace(l, r.start - 1, to, ""));
            return true;
        }
        false
    }

    fn delete(&self, l: i32, off: usize) -> bool {
        let b = &self.buffer;
        let (text, line) = if l == 0 {
            let t = text_of(b, 0);
            let n = t.chars().count();
            (
                t,
                Line {
                    kind: Kind::Para,
                    spaces: 0,
                    start: 0,
                    end: n,
                    hidden: vec![],
                },
            )
        } else {
            classify(b, l)
        };
        let cs = chars(&text);
        if off >= line.end {
            if l + 1 >= b.line_count() {
                return true;
            }
            let (ntext, next) = classify(b, l + 1);
            if next.kind == Kind::Divider {
                self.edit(|b| {
                    let mut s = line_end(b, l);
                    let mut e = line_end(b, l + 1);
                    b.delete(&mut s, &mut e);
                });
                return true;
            }
            let moved = doc::plain(&ntext, &next);
            self.edit(|b| {
                let mut s = line_end(b, l);
                let mut e = line_end(b, l + 1);
                b.delete(&mut s, &mut e);
                let mut a = at(b, l, line.end);
                b.insert(&mut a, &moved);
            });
            b.place_cursor(&at(b, l, line.end));
            return true;
        }
        let runs = self.hidden_runs(&text, &line);
        if cs.get(off) == Some(&SOFT)
            && let Some(r) = runs.iter().find(|r| r.start == off + 1)
        {
            let end = r.end;
            self.edit(|_| self.replace(l, off, end, ""));
            return true;
        }
        if let Some(r) = runs.iter().find(|r| r.start == off && r.end < line.end) {
            let end = r.end;
            self.edit(|_| self.replace(l, end, end + 1, ""));
            return true;
        }
        false
    }

    /// Hidden ranges inside the text of a block: continuations and inline marks.
    fn hidden_runs(&self, text: &str, line: &Line) -> Vec<std::ops::Range<usize>> {
        let mut runs: Vec<_> = line
            .hidden
            .iter()
            .filter(|r| r.start >= line.start)
            .cloned()
            .collect();
        if !matches!(line.kind, Kind::Code | Kind::Divider) {
            let cs = chars(text);
            runs.extend(
                doc::inline(&cs, line.start, line.end)
                    .into_iter()
                    .filter(|(_, m)| *m == Mark::Hide)
                    .map(|(r, _)| r),
            );
        }
        runs
    }

    /// Tab nests a block under the one above; Shift+Tab lifts it. In a code
    /// block Tab is four spaces; Esc then Tab nests the code block itself.
    fn indent(&self, l: i32, deeper: bool) -> bool {
        let b = &self.buffer;
        if l == 0 {
            return true;
        }
        let (_, line) = classify(b, l);
        if line.kind == Kind::Code {
            if deeper {
                let mut c = cursor(b);
                self.edit(|b| b.insert(&mut c, "    "));
            }
            return true;
        }
        let caret = cursor(b).line_offset() as isize;
        if let Some(delta) = self.nest(l, 1, deeper) {
            b.place_cursor(&at(b, l, (caret + delta).max(0) as usize));
            self.clamp();
        }
        true
    }

    /// The indent Tab (or Shift+Tab) gives the block at `l`: under the
    /// previous block of its level, as Markdown nests it. Deeper than a
    /// paragraph allows four spaces would read as code elsewhere, so a block
    /// nests under a list item, or one level under any other block.
    fn nest_target(&self, l: i32, deeper: bool) -> Option<usize> {
        let b = &self.buffer;
        let (_, line) = classify(b, l);
        let mut target = None;
        for k in (1..l).rev().take(2000) {
            let (t, up) = classify(b, k);
            if t.trim().is_empty() || (up.spaces > line.spaces && deeper) {
                continue;
            }
            if deeper {
                if up.spaces == line.spaces {
                    target = Some(doc::child_offset(&up));
                }
                break;
            }
            if up.spaces < line.spaces {
                target = Some(up.spaces);
                break;
            }
        }
        if !deeper && target.is_none() && line.spaces > 0 {
            target = Some(0);
        }
        let to = target?;
        if deeper {
            // What the indent counts from: the text of the list item it is in.
            let base = (1..l)
                .rev()
                .take(2000)
                .map(|k| classify(b, k))
                .filter(|(t, _)| !t.trim().is_empty())
                .find(|(_, up)| up.kind.is_list() && doc::child_offset(up) <= to)
                .map_or(0, |(_, up)| doc::child_offset(&up));
            if to - base > 3 {
                return None;
            }
        }
        Some(to)
    }

    /// Nests or lifts `count` blocks from `l`, with what is nested under them.
    /// Returns how far the first one moved.
    fn nest(&self, l: i32, count: i32, deeper: bool) -> Option<isize> {
        let b = &self.buffer;
        let (_, first) = classify(b, l);
        let to = self.nest_target(l, deeper)?;
        let delta = to as isize - first.spaces as isize;
        let last = l + count - 1;
        let end = last + self.span_of(last);
        self.edit(|b| {
            for k in l..end.min(b.line_count()) {
                let (t, kl) = classify(b, k);
                if t.trim().is_empty() {
                    continue;
                }
                let spaces = (kl.spaces as isize + delta).max(0) as usize;
                let n = t.chars().count();
                self.replace(k, 0, n, &doc::reindent(&t, spaces));
            }
        });
        Some(delta)
    }

    /// The selection, or with `word` the word at the caret, shrunk off hidden marks.
    fn span(&self, word: bool) -> Option<(i32, usize, usize)> {
        let b = &self.buffer;
        let (s, e) = b.selection_bounds().or_else(|| {
            if !word {
                return None;
            }
            let c = cursor(b);
            let (mut s, mut e) = (c, c);
            if !c.inside_word() && !c.ends_word() {
                return None;
            }
            if !s.starts_word() {
                s.backward_word_start();
            }
            if !e.ends_word() {
                e.forward_word_end();
            }
            Some((s, e))
        })?;
        if s.line() != e.line() || s.line() == 0 {
            return None;
        }
        let l = s.line();
        let (text, line) = classify(b, l);
        let runs = self.hidden_runs(&text, &line);
        let hidden = |i: usize| runs.iter().any(|r| r.contains(&i));
        let (mut a, mut z) = (
            (s.line_offset() as usize).max(line.start),
            (e.line_offset() as usize).min(line.end),
        );
        while a < z && hidden(a) {
            a += 1;
        }
        while z > a && hidden(z - 1) {
            z -= 1;
        }
        (a < z).then_some((l, a, z))
    }

    fn inline_toggle(&self, mark: &'static str) -> bool {
        let b = &self.buffer;
        let Some((l, a, z)) = self.span(false) else {
            // Nothing to wrap: in the style, step out of it; else the next
            // typed character takes it.
            let c = cursor(b);
            if c.line() == 0 || b.has_selection() {
                return true;
            }
            let (text, line) = classify(b, c.line());
            let off = c.line_offset() as usize;
            let want = match mark {
                "**" => Mark::Bold,
                "*" => Mark::Italic,
                _ => Mark::Code,
            };
            let inside = doc::inline(&chars(&text), line.start, line.end)
                .into_iter()
                .find(|(r, m)| *m == want && r.start <= off && off <= r.end);
            match inside {
                Some((r, _)) => {
                    self.quiet.set(true);
                    b.place_cursor(&at(b, c.line(), r.end + mark.len()));
                    self.quiet.set(false);
                }
                None => {
                    let on = self.pending_mark.get() == Some(mark);
                    self.pending_mark.set((!on).then_some(mark));
                }
            }
            return true;
        };
        let cs = chars(&text_of(b, l));
        let m: Vec<char> = mark.chars().collect();
        let k = m.len();
        let wrapped =
            a >= k && cs.get(a - k..a) == Some(&m[..]) && cs.get(z..z + k) == Some(&m[..]);
        self.edit(|_| {
            if wrapped {
                self.replace(l, z, z + k, "");
                self.replace(l, a - k, a, "");
            } else {
                self.replace(l, z, z, mark);
                self.replace(l, a, a, mark);
            }
        });
        let (a, z) = if wrapped {
            (a - k, z - k)
        } else {
            (a + k, z + k)
        };
        b.select_range(&at(b, l, a), &at(b, l, z));
        true
    }

    /// Wraps the character typed at `offset` in `mark`; typing goes on inside.
    fn style_typed(&self, offset: i32, mark: &str) {
        let b = &self.buffer;
        let s = b.iter_at_offset(offset);
        let mut e = s;
        e.forward_char();
        let ch = b.text(&s, &e, true).to_string();
        if ch.chars().count() != 1 || s.line() == 0 {
            return;
        }
        let (l, off) = (s.line(), s.line_offset() as usize);
        self.quiet.set(true);
        self.edit(|_| self.replace(l, off, off + 1, &format!("{mark}{ch}{mark}")));
        b.place_cursor(&at(b, l, off + mark.len() + 1));
        self.quiet.set(false);
    }

    /// Copies the selection as Markdown, line breaks as newlines.
    fn copy(&self, cut: bool) {
        if let Some((lo, hi)) = self.view.imp().blocks.get() {
            self.view
                .clipboard()
                .set_text(&self.blocks_markdown(lo, hi));
            if cut {
                self.delete_lines(lo, hi);
            }
            return;
        }
        let Some(md) = self.selection_markdown() else {
            return;
        };
        self.view.clipboard().set_text(&md);
        if cut {
            self.edit(|b| {
                b.delete_selection(true, true);
            });
        }
    }

    /// The selected text as Markdown, its line breaks real newlines.
    fn selection_markdown(&self) -> Option<String> {
        let b = &self.buffer;
        let (s, e) = b.selection_bounds()?;
        Some(if s.line() == e.line() && s.line() > 0 {
            // Inside one block: what the reader sees of it, minus block markers.
            let (text, line) = classify(b, s.line());
            let (a, z) = (s.line_offset() as usize, e.line_offset() as usize);
            text.chars()
                .enumerate()
                .filter(|(i, _)| {
                    (a.max(line.start)..z.min(line.end)).contains(i)
                        && !line.hidden.iter().any(|h| h.contains(i))
                })
                .map(|(_, c)| if c == SOFT { '\n' } else { c })
                .collect()
        } else {
            let text = b.text(&s, &e, true).to_string();
            let mut md = doc::from_editor(text.split('\n'), &doc::Layout::default());
            md.pop();
            md
        })
    }

    /// Pastes text: one line goes in as typed; Markdown of several lines
    /// becomes blocks, or stays code inside a code block.
    fn paste(&self, text: &str) {
        let b = &self.buffer;
        let text = text.replace("\r\n", "\n");
        self.clear_blocks();
        b.begin_user_action();
        b.delete_selection(true, true);
        let c = cursor(b);
        let (l, off) = (c.line(), c.line_offset() as usize);
        let (t, line) = classify(b, l);
        let mut c = c;
        if !text.contains('\n') {
            b.insert(&mut c, &text);
        } else if l == 0 {
            b.insert(&mut c, &text.replace('\n', " "));
        } else if line.kind == Kind::Code {
            let pad = " ".repeat(line.spaces);
            b.insert(&mut c, &text.replace('\n', &format!("{SOFT}{pad}")));
        } else {
            let (blocks, _) = doc::to_editor(&text);
            let count = blocks.matches('\n').count() as i32 + 1;
            let last = if doc::plain(&t, &line).is_empty() {
                let n = t.chars().count();
                self.replace(l, 0, n, &blocks);
                l + count - 1
            } else if off <= line.start {
                let mut s = b.iter_at_line(l).unwrap();
                b.insert(&mut s, &format!("{blocks}\n"));
                l + count - 1
            } else {
                let rest = if off < line.end {
                    format!("\n{}", " ".repeat(line.spaces))
                } else {
                    String::new()
                };
                b.insert(&mut c, &format!("\n{blocks}{rest}"));
                l + count
            };
            b.place_cursor(&line_end(b, last));
        }
        b.end_user_action();
        self.clamp();
    }

    fn link_prompt(&self) -> bool {
        let Some((l, a, z)) = self.span(true) else {
            return true;
        };
        let b = &self.buffer;
        let entry = gtk::Entry::builder()
            .placeholder_text("Paste or type a link")
            .width_chars(32)
            .build();
        let pop = gtk::Popover::builder()
            .child(&entry)
            .has_arrow(false)
            .halign(gtk::Align::Start)
            .build();
        pop.add_css_class("link-prompt");
        pop.set_parent(&self.view);
        let r = self.view.iter_location(&at(b, l, a));
        let (x, y) = self
            .view
            .buffer_to_window_coords(gtk::TextWindowType::Widget, r.x(), r.y());
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, r.height())));
        let view = self.view.clone();
        let buffer = b.clone();
        entry.connect_activate(glib::clone!(
            #[weak]
            pop,
            move |entry| {
                // Once: a second Enter while it closes must not link twice.
                if !pop.is_visible() {
                    return;
                }
                let url = entry.text().trim().to_string();
                if !url.is_empty() {
                    buffer.begin_user_action();
                    let mut e = at(&buffer, l, z);
                    buffer.insert(&mut e, &format!("]({url})"));
                    let mut s = at(&buffer, l, a);
                    buffer.insert(&mut s, "[");
                    buffer.end_user_action();
                }
                pop.popdown();
                view.grab_focus();
            }
        ));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
        entry.grab_focus();
        true
    }

    /// Markdown typed at the start of a block that the text alone does not make.
    fn shortcuts(&self) {
        let b = &self.buffer;
        let l = cursor(b).line();
        if l == 0 {
            return;
        }
        let text = text_of(b, l);
        let spaces = text.chars().take_while(|c| *c == ' ').count();
        let kind = match &text[spaces..] {
            "[] " | "[ ] " => Kind::Todo(false),
            "[x] " => Kind::Todo(true),
            "```" => Kind::Code,
            _ => return,
        };
        let new = doc::wrap(kind, spaces, "");
        let n = text.chars().count();
        self.edit(|_| self.replace(l, 0, n, &new));
        let nl = doc::classify(&new);
        b.place_cursor(&at(b, l, nl.start));
    }

    fn maybe_open_menu(&self, at_: i32) {
        let b = &self.buffer;
        let slash = b.iter_at_offset(at_);
        if slash.line() == 0 {
            return;
        }
        let (_, line) = classify(b, slash.line());
        if line.kind == Kind::Code {
            return;
        }
        let off = slash.line_offset() as usize;
        let mut before = slash;
        let ok = off <= line.start || (before.backward_char() && before.char().is_whitespace());
        if !ok {
            return;
        }
        self.menu_at.set(Some(at_));
        self.update_menu();
    }

    fn update_menu(&self) {
        let Some(at_) = self.menu_at.get() else {
            return;
        };
        let b = &self.buffer;
        let c = cursor(b);
        let slash = b.iter_at_offset(at_);
        if c.offset() <= at_ || c.line() != slash.line() || slash.char() != '/' {
            return self.close_menu();
        }
        let mut s = slash;
        s.forward_char();
        let query = b.text(&s, &c, true).to_lowercase();
        if query.contains(char::is_whitespace) || query.chars().count() > 24 {
            return self.close_menu();
        }
        let mut first = None;
        for (i, (name, hint, _, words)) in BLOCKS.iter().enumerate() {
            let hit = query.is_empty()
                || name.to_lowercase().contains(&query)
                || words.split(' ').any(|w| w.starts_with(&query))
                || (!hint.is_empty() && hint.starts_with(&query));
            if let Some(row) = self.menu_list.row_at_index(i as i32) {
                row.set_visible(hit);
                if hit && first.is_none() {
                    first = Some(row);
                }
            }
        }
        let keep = self
            .menu_list
            .selected_row()
            .filter(|r| r.is_visible())
            .or(first);
        self.menu_list.select_row(keep.as_ref());
        let r = self.view.iter_location(&slash);
        let (x, y) = self
            .view
            .buffer_to_window_coords(gtk::TextWindowType::Widget, r.x(), r.y());
        self.menu
            .set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, r.height())));
        if !self.menu.is_visible() {
            self.menu.popup();
        }
    }

    fn menu_step(&self, d: i32) {
        let rows: Vec<gtk::ListBoxRow> = (0..BLOCKS.len() as i32)
            .filter_map(|i| self.menu_list.row_at_index(i))
            .filter(|r| r.is_visible())
            .collect();
        if rows.is_empty() {
            return;
        }
        let cur = self
            .menu_list
            .selected_row()
            .and_then(|s| rows.iter().position(|r| *r == s))
            .unwrap_or(0) as i32;
        let next = (cur + d).rem_euclid(rows.len() as i32) as usize;
        self.menu_list.select_row(Some(&rows[next]));
    }

    fn close_menu(&self) {
        if self.menu_at.take().is_some() {
            self.menu.popdown();
        }
    }

    fn apply_menu(&self, i: usize) {
        let Some(at_) = self.menu_at.get() else {
            return;
        };
        self.close_menu();
        let b = &self.buffer;
        let kind = BLOCKS[i].2;
        let slash = b.iter_at_offset(at_);
        let l = slash.line();
        let from = slash.line_offset() as usize;
        let to = cursor(b).line_offset() as usize;
        self.edit(|_| self.replace(l, from, to, ""));
        b.place_cursor(&at(b, l, from));
        if kind == Kind::Divider {
            let (text, line) = classify(b, l);
            let empty = doc::plain(&text, &line).is_empty();
            let rule = doc::wrap(Kind::Divider, line.spaces, "");
            let pad = " ".repeat(line.spaces);
            self.edit(|b| {
                if empty {
                    let n = text.chars().count();
                    self.replace(l, 0, n, &rule);
                }
                let mut e = line_end(b, l);
                b.insert(
                    &mut e,
                    &if empty {
                        format!("\n{pad}")
                    } else {
                        format!("\n{rule}")
                    },
                );
            });
            if empty {
                b.place_cursor(&b.iter_at_line(l + 1).unwrap());
            }
            return;
        }
        // Edits from here on are one step to undo with the slash removal above.
        self.set_kind(l, kind);
    }
}

/// The editor as vim's engine sees it: line 0 is the title, the rest blocks.
struct Gtk<'a>(&'a Editor);
impl vim::Buf for Gtk<'_> {
    fn lines(&self) -> usize {
        self.0.buffer.line_count() as usize
    }
    fn line(&self, l: usize) -> vim::VLine {
        let t = text_of(&self.0.buffer, l as i32);
        if l == 0 {
            vim::VLine::plain(&t)
        } else {
            vim::VLine::block(&t)
        }
    }
    fn first(&self) -> usize {
        1
    }
    fn cursor(&self) -> vim::Pos {
        let c = cursor(&self.0.buffer);
        vim::pos(c.line() as usize, c.line_offset() as usize)
    }
    fn set_cursor(&mut self, p: vim::Pos) {
        let b = &self.0.buffer;
        b.place_cursor(&at(b, p.line as i32, p.col));
        self.0.view.scroll_mark_onscreen(&b.get_insert());
    }
    fn select(&mut self, a: vim::Pos, z: vim::Pos, lines: bool) {
        let b = &self.0.buffer;
        let (lo, hi) = (a.min(z), a.max(z));
        let (s, e) = if lines {
            (at(b, lo.line as i32, 0), line_end(b, hi.line as i32))
        } else {
            let mut e = at(b, hi.line as i32, hi.col);
            e.forward_char();
            (at(b, lo.line as i32, lo.col), e)
        };
        // The moving end holds the caret, so that the view follows it.
        if z >= a {
            b.select_range(&e, &s);
        } else {
            b.select_range(&s, &e);
        }
        self.0.view.scroll_mark_onscreen(&b.get_insert());
    }
    fn replace(&mut self, from: vim::Pos, to: vim::Pos, text: &str) {
        self.0.edit(|b| {
            let mut s = at(b, from.line as i32, from.col);
            let mut e = at(b, to.line as i32, to.col);
            b.delete(&mut s, &mut e);
            let mut s = at(b, from.line as i32, from.col);
            b.insert(&mut s, text);
        });
    }
    fn open(&mut self, below: bool) {
        let b = &self.0.buffer;
        let c = self.cursor();
        let l = c.line as i32;
        if l == 0 {
            let mut e = line_end(b, 0);
            self.0.edit(|b| b.insert(&mut e, "\n"));
            return b.place_cursor(&at(b, 1, 0));
        }
        let (text, line) = classify(b, l);
        if matches!(line.kind, Kind::Code | Kind::Quote | Kind::Callout) {
            // A row in the block.
            let v = vim::VLine::block(&text);
            let r = v.row(c.col);
            let (end, home) = (r.end, v.home(&r));
            let new = format!("{SOFT}{}", doc::continuation(&line));
            let at_ = if below { end } else { home };
            self.0.edit(|b| b.insert(&mut at(b, l, at_), &new));
            let to = if below {
                end + new.chars().count()
            } else {
                home
            };
            return b.place_cursor(&at(b, l, to));
        }
        let prefix = match line.kind {
            Kind::Todo(_) => doc::wrap(Kind::Todo(false), line.spaces, ""),
            k if k.is_list() => doc::wrap(k, line.spaces, ""),
            _ => " ".repeat(line.spaces),
        };
        let n = prefix.chars().count();
        if below {
            self.0
                .edit(|b| b.insert(&mut line_end(b, l), &format!("\n{prefix}")));
            b.place_cursor(&at(b, l + 1, n));
        } else {
            self.0
                .edit(|b| b.insert(&mut at(b, l, 0), &format!("{prefix}\n")));
            b.place_cursor(&at(b, l, n));
        }
    }
    fn nest(&mut self, lo: usize, hi: usize, deeper: bool) {
        self.0.nest(lo as i32, (hi - lo + 1) as i32, deeper);
    }
    fn undo(&mut self) {
        if self.0.buffer.can_undo() {
            self.0.buffer.undo();
        }
    }
    fn redo(&mut self) {
        if self.0.buffer.can_redo() {
            self.0.buffer.redo();
        }
    }
}

fn make_tags(b: &gtk::TextBuffer) {
    let t = |name: &str, props: &[(&str, &dyn ToValue)]| {
        let tag = gtk::TextTag::new(Some(name));
        for (k, v) in props {
            tag.set_property_from_value(k, &v.to_value());
        }
        b.tag_table().add(&tag);
    };
    let mono = "Geist Mono, JetBrains Mono, monospace";
    t(
        "title",
        &[
            ("scale", &2.0f64),
            ("weight", &700i32),
            ("pixels-below-lines", &20i32),
        ],
    );
    t(
        "h1",
        &[
            ("scale", &1.6f64),
            ("weight", &700i32),
            ("pixels-above-lines", &26i32),
            ("pixels-below-lines", &6i32),
        ],
    );
    t(
        "h2",
        &[
            ("scale", &1.32f64),
            ("weight", &650i32),
            ("pixels-above-lines", &20i32),
            ("pixels-below-lines", &4i32),
        ],
    );
    t(
        "h3",
        &[
            ("scale", &1.12f64),
            ("weight", &650i32),
            ("pixels-above-lines", &14i32),
            ("pixels-below-lines", &2i32),
        ],
    );
    t("quote", &[]);
    t(
        "callout",
        &[
            ("right-margin", &(M + 16)),
            ("pixels-above-lines", &14i32),
            ("pixels-below-lines", &14i32),
        ],
    );
    t(
        "code",
        &[
            ("family", &mono),
            ("scale", &0.9f64),
            ("right-margin", &(M + 18)),
            ("pixels-above-lines", &16i32),
            ("pixels-below-lines", &16i32),
            ("pixels-inside-wrap", &2i32),
        ],
    );
    t(
        "divider",
        &[("pixels-above-lines", &8i32), ("pixels-below-lines", &8i32)],
    );
    t("hidden", &[("invisible", &true)]);
    t("bold", &[("weight", &700i32)]);
    t("italic", &[("style", &pango::Style::Italic)]);
    t("icode", &[("family", &mono), ("scale", &0.9f64)]);
    t("link", &[("underline", &pango::Underline::Single)]);
    t("done", &[("strikethrough", &true)]);
    t("find", &[]);
    // Markers of nesting; where the text starts is a margin tag of its own.
    for n in 0..LEVELS {
        t(&format!("lvl{n}"), &[]);
    }
}

/// The tag that starts text `px` from the left, made once.
fn margin_tag(b: &gtk::TextBuffer, px: i32) -> String {
    let name = format!("m{px}");
    if b.tag_table().lookup(&name).is_none() {
        let tag = gtk::TextTag::new(Some(&name));
        tag.set_left_margin(px);
        b.tag_table().add(&tag);
    }
    name
}

fn restyle_lines(b: &gtk::TextBuffer, a: i32, z: i32) {
    for l in a..z.min(b.line_count()) {
        restyle(b, l);
    }
}

fn restyle(b: &gtk::TextBuffer, l: i32) {
    let Some(s) = b.iter_at_line(l) else { return };
    let mut e = s;
    e.forward_line();
    b.remove_all_tags(&s, &e);
    let tag = |name: &str, from: usize, to: usize| {
        let base = s.offset();
        b.apply_tag_by_name(
            name,
            &b.iter_at_offset(base + from as i32),
            &b.iter_at_offset(base + to as i32),
        );
    };
    let text = text_of(b, l);
    let n = text.chars().count();
    if l == 0 {
        b.apply_tag_by_name("title", &s, &e);
        return;
    }
    let line = doc::classify(&text);
    let whole = |name: &str| b.apply_tag_by_name(name, &s, &e);
    match line.kind {
        Kind::H1 => whole("h1"),
        Kind::H2 => whole("h2"),
        Kind::H3 => whole("h3"),
        Kind::Quote => whole("quote"),
        Kind::Callout => whole("callout"),
        Kind::Code => whole("code"),
        Kind::Divider => whole("divider"),
        Kind::Todo(true) => tag("done", line.start, n),
        _ => {}
    }
    // Nested under the nearest block above with less indent.
    let mut lvl = 0;
    if line.spaces > 0 {
        for k in (1..l).rev().take(400) {
            let t = text_of(b, k);
            if t.trim().is_empty() {
                continue;
            }
            if doc::classify(&t).spaces < line.spaces {
                lvl = (level(b, k) + 1).min(LEVELS - 1);
                break;
            }
        }
    }
    whole(&format!("lvl{lvl}"));
    whole(&margin_tag(b, M + lvl * UNIT + marker_room(line.kind)));
    for r in &line.hidden {
        tag("hidden", r.start, r.end);
    }
    if !matches!(line.kind, Kind::Code | Kind::Divider) {
        let cs = chars(&text);
        for (r, m) in doc::inline(&cs, line.start, line.end) {
            let name = match m {
                Mark::Hide => "hidden",
                Mark::Bold => "bold",
                Mark::Italic => "italic",
                Mark::Code => "icode",
                Mark::Link(_) => "link",
            };
            tag(name, r.start, r.end);
        }
    }
}
