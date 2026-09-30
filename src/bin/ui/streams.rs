use super::super::{Ui, action, caption, hbox, label, slider, vbox};
use gtk::prelude::*;
use nexus_control::AppStream;
use std::cell::RefCell;

struct Row {
    id: u32,
    widget: gtk::Box,
    icon: gtk::Image,
    name: gtk::Label,
    detail: gtk::Label,
    mute: gtk::ToggleButton,
    scale: gtk::Scale,
}

/// Per-application playback levels. Rows persist across updates so a slider
/// being dragged is never rebuilt under the pointer.
pub struct StreamList {
    pub widget: gtk::Box,
    list: gtk::Box,
    empty: gtk::Label,
    rows: RefCell<Vec<Row>>,
}
fn app_icon(name: Option<&str>) -> String {
    let theme = gtk::IconTheme::for_display(&gtk::gdk::Display::default().unwrap());
    name.filter(|n| theme.has_icon(n))
        .unwrap_or("audio-x-generic-symbolic")
        .to_string()
}
fn mute_icon(muted: bool) -> &'static str {
    if muted {
        "audio-volume-muted-symbolic"
    } else {
        "audio-volume-high-symbolic"
    }
}
impl StreamList {
    pub fn new() -> Self {
        let widget = vbox(12);
        widget.add_css_class("card");
        widget.append(&label("Applications", "row-title"));
        let list = vbox(14);
        widget.append(&list);
        let empty = caption("No application is playing sound.");
        widget.append(&empty);
        Self {
            widget,
            list,
            empty,
            rows: RefCell::new(vec![]),
        }
    }
    fn row(ui: &Ui, stream: &AppStream) -> Row {
        let widget = vbox(4);
        let top = hbox(12);
        let icon = gtk::Image::new();
        icon.set_pixel_size(24);
        top.append(&icon);
        let text = vbox(1);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        let name = label("", "row-title");
        name.set_wrap(false);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(1);
        text.append(&name);
        let detail = caption("");
        detail.set_wrap(false);
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        detail.set_max_width_chars(1);
        text.append(&detail);
        top.append(&text);
        let mute = gtk::ToggleButton::new();
        mute.add_css_class("flat");
        mute.add_css_class("stream-mute");
        mute.set_valign(gtk::Align::Center);
        let id = stream.id.to_string();
        let ui2 = ui.clone();
        let target = id.clone();
        mute.connect_toggled(move |b| {
            b.set_icon_name(mute_icon(b.is_active()));
            b.set_tooltip_text(Some(if b.is_active() { "Unmute" } else { "Mute" }));
            if !ui2.updating.get() {
                ui2.send(action(
                    "stream-mute",
                    &target,
                    if b.is_active() { "true" } else { "false" },
                ));
            }
        });
        top.append(&mute);
        widget.append(&top);
        let scale = slider(ui, "stream-volume", &id);
        scale.set_margin_start(36);
        widget.append(&scale);
        Row {
            id: stream.id,
            widget,
            icon,
            name,
            detail,
            mute,
            scale,
        }
    }
    /// Call with `ui.updating` set so programmatic changes are not sent back.
    pub fn update(&self, ui: &Ui, streams: &[AppStream]) {
        let mut rows = self.rows.borrow_mut();
        rows.retain(|row| {
            let keep = streams.iter().any(|s| s.id == row.id);
            if !keep {
                self.list.remove(&row.widget);
            }
            keep
        });
        for stream in streams {
            if !rows.iter().any(|r| r.id == stream.id) {
                let row = Self::row(ui, stream);
                self.list.append(&row.widget);
                rows.push(row);
            }
            let row = rows.iter().find(|r| r.id == stream.id).unwrap();
            row.icon
                .set_icon_name(Some(&app_icon(stream.icon.as_deref())));
            row.name.set_text(&stream.name);
            row.detail.set_text(&stream.detail);
            row.detail.set_visible(!stream.detail.is_empty());
            row.mute.set_active(stream.muted);
            row.mute.set_icon_name(mute_icon(stream.muted));
            if !row.scale.has_focus() {
                row.scale.set_value(f64::from(stream.volume));
            }
        }
        self.list.set_visible(!rows.is_empty());
        self.empty.set_visible(rows.is_empty());
    }
}
