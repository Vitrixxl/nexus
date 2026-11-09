//! The clipboard history (`nexus clipboard`, Super+Shift+V): a card at the
//! pointer listing what was copied lately, here or on the phone. Picking one
//! puts it back on the clipboard. The daemon keeps the history.
use super::super::{caption, hbox, label, vbox};
use super::phone::call;
use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nexus_control::{backend::hypr_socket, phone::Clip};
use serde_json::Value;
use std::{cell::RefCell, rc::Rc};

const WIDTH: i32 = 380;
const HEIGHT: i32 = 440;

pub struct Clipboard {
    window: gtk::ApplicationWindow,
    card: gtk::Box,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    scroll: gtk::ScrolledWindow,
    empty: gtk::Label,
    clips: RefCell<Vec<Clip>>,
}
impl Clipboard {
    pub fn new(app: &gtk::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Clipboard")
            .build();
        window.add_css_class("nexus");
        window.add_css_class("clipboard-layer");
        window.init_layer_shell();
        window.set_namespace(Some("nexus-clipboard"));
        window.set_layer(Layer::Overlay);
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::Exclusive);

        // The whole screen, see-through: a click beside the card closes it.
        let backdrop = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let card = vbox(8);
        card.add_css_class("clipboard-card");
        card.set_halign(gtk::Align::Start);
        card.set_valign(gtk::Align::Start);
        card.set_size_request(WIDTH, -1);
        backdrop.append(&card);
        window.set_child(Some(&backdrop));

        let head = hbox(8);
        let title = label("Clipboard", "row-title");
        title.set_hexpand(true);
        title.set_xalign(0.);
        head.append(&title);
        head.append(&caption("Enter to copy"));
        card.append(&head);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search"));
        card.append(&search);
        let list = gtk::ListBox::new();
        list.add_css_class("clipboard-list");
        list.set_activate_on_single_click(true);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();
        card.append(&scroll);
        let empty = caption("Nothing copied yet");
        empty.set_margin_top(10);
        empty.set_margin_bottom(10);
        card.append(&empty);

        let c = Rc::new(Self {
            window,
            card,
            search,
            list,
            scroll,
            empty,
            clips: RefCell::default(),
        });

        let click = gtk::GestureClick::new();
        let w = Rc::downgrade(&c);
        click.connect_pressed(move |g, _, x, y| {
            let Some(c) = w.upgrade() else { return };
            let inside = g
                .widget()
                .and_then(|b| {
                    b.compute_point(&c.card, &gtk::graphene::Point::new(x as f32, y as f32))
                })
                .is_some_and(|p| c.card.contains(p.x() as f64, p.y() as f64));
            if !inside {
                c.hide();
            }
        });
        backdrop.add_controller(click);
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(&c);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(c) = w.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Escape => c.hide(),
                gdk::Key::Down if c.search.has_focus() => {
                    if let Some(row) = c.first_shown() {
                        c.list.select_row(Some(&row));
                        row.grab_focus();
                    }
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        c.window.add_controller(keys);
        let w = Rc::downgrade(&c);
        c.list.connect_row_activated(move |_, row| {
            if let Some(c) = w.upgrade() {
                c.pick(row.index());
            }
        });
        let w = Rc::downgrade(&c);
        c.search.connect_search_changed(move |s| {
            if let Some(c) = w.upgrade() {
                let query = s.text().to_lowercase();
                let clips = c.clips.borrow();
                let mut i = 0;
                while let Some(row) = c.list.row_at_index(i) {
                    let text = clips.get(i as usize).map_or("", |c| c.text.as_str());
                    row.set_visible(query.is_empty() || text.to_lowercase().contains(&query));
                    i += 1;
                }
            }
        });
        let w = Rc::downgrade(&c);
        c.search.connect_activate(move |_| {
            if let Some(c) = w.upgrade()
                && let Some(row) = c.first_shown()
            {
                c.pick(row.index());
            }
        });
        c
    }

    fn first_shown(&self) -> Option<gtk::ListBoxRow> {
        let mut i = 0;
        while let Some(row) = self.list.row_at_index(i) {
            if row.is_visible() {
                return Some(row);
            }
            i += 1;
        }
        None
    }

    /// Opens at the pointer, or closes when already open.
    pub fn toggle(self: &Rc<Self>) {
        if self.window.is_visible() {
            return self.hide();
        }
        let c = self.clone();
        call("clipboard", Value::Null, move |r| {
            let clips: Vec<Clip> = r
                .ok()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let height = c.fill(clips);
            c.place(height);
            c.search.set_text("");
            c.window.present();
            c.search.grab_focus();
        });
    }

    fn hide(&self) {
        self.window.set_visible(false);
    }

    /// Lists `clips`, and says about how tall the card gets.
    fn fill(&self, clips: Vec<Clip>) -> f64 {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        self.empty.set_visible(clips.is_empty());
        // Wrapped rows ask for too little height; two lines each, within the card.
        let rows = (clips.len() as i32 * 54).min(HEIGHT - 110);
        self.scroll.set_min_content_height(rows);
        self.scroll.set_visible(!clips.is_empty());
        for clip in &clips {
            let row = hbox(10);
            row.add_css_class("clipboard-row");
            let icon = gtk::Image::from_icon_name(if clip.phone {
                "phone-symbolic"
            } else {
                "edit-paste-symbolic"
            });
            icon.set_pixel_size(16);
            icon.add_css_class(if clip.phone { "accent" } else { "muted" });
            icon.set_valign(gtk::Align::Start);
            icon.set_tooltip_text(Some(if clip.phone {
                "Copied on the phone"
            } else {
                "Copied here"
            }));
            row.append(&icon);
            let text = gtk::Label::new(Some(
                &clip.text.split_whitespace().collect::<Vec<_>>().join(" "),
            ));
            text.set_hexpand(true);
            text.set_xalign(0.);
            text.set_wrap(true);
            text.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            text.set_lines(2);
            text.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.set_max_width_chars(1);
            row.append(&text);
            self.list.append(&row);
        }
        self.clips.replace(clips);
        f64::from(110 + rows)
    }

    /// Puts a corner of the card at the pointer, on its monitor: the top left
    /// one, or another where the card would leave the screen.
    fn place(&self, card_height: f64) {
        let hypr = |q: &str| -> Option<Value> {
            serde_json::from_str(&hypr_socket(".socket.sock", &format!("j/{q}")).ok()?).ok()
        };
        let (Some(cursor), Some(monitors)) = (hypr("cursorpos"), hypr("monitors")) else {
            return;
        };
        let (x, y) = (
            cursor["x"].as_f64().unwrap_or(0.),
            cursor["y"].as_f64().unwrap_or(0.),
        );
        let Some((monitor, left, top, width, height)) =
            monitors.as_array().into_iter().flatten().find_map(|m| {
                let scale = m["scale"].as_f64().unwrap_or(1.).max(0.1);
                let (mx, my) = (m["x"].as_f64()?, m["y"].as_f64()?);
                let (mut w, mut h) = (m["width"].as_f64()? / scale, m["height"].as_f64()? / scale);
                // Turned a quarter, the monitor's width and height swap.
                if m["transform"].as_i64().unwrap_or(0) % 2 == 1 {
                    (w, h) = (h, w);
                }
                (x >= mx && y >= my && x < mx + w && y < my + h).then(|| {
                    (
                        m["name"].as_str().unwrap_or_default().to_owned(),
                        x - mx,
                        y - my,
                        w,
                        h,
                    )
                })
            })
        else {
            return;
        };
        if let Some(display) = gdk::Display::default() {
            let monitors = display.monitors();
            for i in 0..monitors.n_items() {
                if let Some(m) = monitors.item(i).and_downcast::<gdk::Monitor>()
                    && m.connector().as_deref() == Some(monitor.as_str())
                {
                    self.window.set_monitor(Some(&m));
                }
            }
        }
        const GAP: f64 = 8.;
        let card_width = f64::from(WIDTH);
        let left = if left + card_width + GAP <= width {
            left
        } else {
            left - card_width
        };
        let top = if top + card_height + GAP <= height {
            top
        } else {
            top - card_height
        };
        let left = left.clamp(GAP, (width - card_width - GAP).max(GAP));
        let top = top.clamp(GAP, (height - card_height - GAP).max(GAP));
        self.card.set_margin_start(left as i32);
        self.card.set_margin_top(top as i32);
    }

    fn pick(&self, index: i32) {
        let Some(clip) = self.clips.borrow().get(index as usize).cloned() else {
            return;
        };
        self.hide();
        // wl-copy keeps serving it after this card is gone.
        if let Ok(mut child) = std::process::Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(clip.text.as_bytes());
            }
            std::thread::spawn(move || child.wait());
        }
    }
}
