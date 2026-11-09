//! Control Center, under the bar: the switches and levels reached for most, on one
//! page, and all of Wi-Fi, Bluetooth, Sound and Display unfolding right here.
use super::{
    super::{
        STATE_WANTED, Ui, action, busy_text, caption, clear, error_label, hbox, label,
        password_line, ranged_slider, security_label, segmented, set_error, signal_icon, slider,
        spinner, state_label, sync_toggle, toggle, toggle_password, vbox,
    },
    shell::{BAR, GAP, Open, run_nexus, sound_icon},
    slide::Slide,
    streams::StreamList,
};
use gtk::{glib, prelude::*};
use gtk4_layer_shell::LayerShell;
use nexus_control::{AudioDevice, Device, Network, Snapshot, backend, request};
use std::{cell::RefCell, rc::Rc, time::Duration};

/// Long enough for the panel to be gone from what a screenshot captures.
const CLEAR_OF_PANEL: Duration = Duration::from_millis(240);
/// The tallest a page's list gets, however large the screen.
const MAX_LIST: i32 = 720;
/// A page's heading and the panel's padding, on top of its list.
const PAGE_CHROME: i32 = 70;
/// A page's list, inside the panel's 380 and its padding.
const LIST_WIDTH: i32 = 360;

#[derive(Clone)]
struct Tile {
    toggle: gtk::Button,
    detail: gtk::Label,
}
impl Tile {
    fn set(&self, on: bool, detail: &str) {
        if on {
            self.toggle.add_css_class("on");
        } else {
            self.toggle.remove_css_class("on");
        }
        self.toggle
            .update_state(&[gtk::accessible::State::Pressed(if on {
                gtk::AccessibleTristate::True
            } else {
                gtk::AccessibleTristate::False
            })]);
        self.detail.set_text(detail);
    }
}
/// What the Wi-Fi page says about the cable, when there is an adapter for one.
struct WiredRow {
    root: gtk::Box,
    detail: gtk::Label,
    choice: gtk::Box,
    ethernet: gtk::ToggleButton,
    wifi: gtk::ToggleButton,
    both: gtk::ToggleButton,
}
/// A radio's page: its switch, its scan and what it finds.
struct Radio {
    switch: gtk::Switch,
    scan: gtk::Button,
    scanning: gtk::Spinner,
    error: gtk::Label,
    list: gtk::Box,
    pointer: gtk::EventControllerMotion,
}
impl Radio {
    /// The pointer or the keyboard is on its rows.
    fn held(&self) -> bool {
        self.list.is_mapped()
            && (self.pointer.contains_pointer() || self.list.focus_child().is_some())
    }
    fn show(&self, ui: &Ui, op: &str, on: bool, scanning: bool, error: Option<&str>) {
        sync_toggle(ui, &self.switch, op, on);
        self.scan.set_visible(!scanning);
        self.scan.set_sensitive(on);
        self.scanning.set_visible(scanning);
        set_error(&self.error, error);
    }
}
/// A level on a page of its own, with what mutes it.
struct Level {
    root: gtk::Box,
    scale: gtk::Scale,
    icon: gtk::Button,
}
impl Level {
    fn show(&self, value: Option<u8>) {
        if !self.scale.has_focus()
            && let Some(v) = value
        {
            self.scale.set_value(f64::from(v));
        }
    }
    fn show_muted(&self, muted: bool, icon: &str) {
        self.icon.set_icon_name(icon);
        self.icon
            .set_tooltip_text(Some(if muted { "Unmute" } else { "Mute" }));
        if muted {
            self.icon.add_css_class("muted-now");
        } else {
            self.icon.remove_css_class("muted-now");
        }
    }
}
pub struct Quick {
    pub widget: gtk::Stack,
    /// One line per page for what an action started there answered.
    reports: [gtk::Label; 5],
    generation: std::cell::Cell<u64>,
    wifi_radio: Radio,
    wired: WiredRow,
    bluetooth_radio: Radio,
    /// What the rows show now: the rows are drawn anew only when it changes.
    shown_networks: RefCell<Option<(&'static str, Vec<Network>)>>,
    shown_devices: RefCell<Option<(&'static str, Vec<Device>)>>,
    shown_outputs: RefCell<Option<(Vec<AudioDevice>, String)>>,
    shown_inputs: RefCell<Option<(Vec<AudioDevice>, String)>>,
    /// The network or device showing its actions, and whether forgetting it awaits
    /// confirmation: a dialog would open beneath the panel.
    unfolded: RefCell<Option<String>>,
    forgetting: std::cell::Cell<bool>,
    /// The Cancel of the forgetting asked about, for the keyboard to land on.
    cancel: RefCell<Option<gtk::Button>>,
    wifi: Tile,
    bluetooth: Tile,
    quiet: Tile,
    night: Tile,
    brightness: Level,
    volume: Level,
    output: Level,
    outputs: gtk::Box,
    streams: StreamList,
    microphone: Level,
    inputs: gtk::Box,
    audio_error: gtk::Label,
    screen: Level,
    light_error: gtk::Label,
    night_switch: gtk::Switch,
    night_temperature: gtk::Scale,
    night_error: gtk::Label,
    ui: Ui,
    quiet_switch: gtk::Switch,
}
/// Brings up a page of the panel. Focus goes nowhere: GTK would hand it to the new
/// page's first control, and the next key would press it, say the Wi-Fi's.
fn go(stack: &gtk::Stack, page: &str, transition: gtk::StackTransitionType) {
    stack.set_transition_type(transition);
    stack.set_visible_child_name(page);
    if let Some(root) = stack.root() {
        gtk::prelude::RootExt::set_focus(&root, None::<&gtk::Widget>);
    }
}
/// The screen the panel is on.
fn panel_monitor(stack: &gtk::Stack) -> Option<gtk::gdk::Monitor> {
    stack
        .root()
        .and_downcast::<gtk::ApplicationWindow>()
        .and_then(|w| w.monitor())
}
/// Looks for what a radio's page lists, as that page comes into view. Nothing is said
/// of a failure nobody asked for: the page's own error tells of a radio out of reach.
fn scan_shown(stack: &gtk::Stack) {
    let op = match stack.visible_child_name().as_deref() {
        Some("wifi") => "wifi-scan",
        Some("bluetooth") => "bluetooth-scan",
        _ => return,
    };
    std::thread::spawn(move || {
        let _ = request(&action(op, "", ""));
    });
}
/// Whether rows under the pointer or the keyboard must be drawn anew all the same:
/// for what they say of a connection, for a row arriving (a headset in pairing mode
/// must not wait for the pointer), or when none remain. Signal, order and rows going
/// wait, or a press or the focus would go with the row.
fn rows_changed<'a, T, K: PartialEq>(
    old: &'a [T],
    new: &'a [T],
    key: &impl Fn(&'a T) -> (&'a str, K),
) -> bool {
    new.is_empty()
        || new.iter().any(|n| {
            let (id, is) = key(n);
            old.iter()
                .find(|o| key(o).0 == id)
                .is_none_or(|o| key(o).1 != is)
        })
}
/// The rows to draw now, if they are to change. Held, they stay where they are, the
/// ones gone included, and arrivals go last.
// ponytail: arrivals redraw every row, so a press in flight at that instant is lost;
// append the new rows alone if that is ever felt.
fn next_rows<'a, T: Clone + PartialEq, K: PartialEq>(
    (shown, nothing): (Option<&'a (&'static str, Vec<T>)>, &str),
    new: &'a [T],
    held: bool,
    key: impl Fn(&'a T) -> (&'a str, K),
) -> Option<Vec<T>> {
    let Some((said, old)) = shown else {
        return Some(new.to_vec());
    };
    if *said == nothing && old == new {
        return None;
    }
    if !held || new.is_empty() {
        return Some(new.to_vec());
    }
    if !rows_changed(old, new, &key) {
        return None;
    }
    let find = |list: &'a [T], row: &'a T| list.iter().find(|r| key(r).0 == key(row).0);
    let mut rows: Vec<T> = old
        .iter()
        .map(|o| find(new, o).unwrap_or(o).clone())
        .collect();
    rows.extend(new.iter().filter(|n| find(old, n).is_none()).cloned());
    Some(rows)
}
fn tile(title: &str, icon: &str, open: impl Fn() + 'static) -> (gtk::Box, Tile) {
    let root = hbox(8);
    root.add_css_class("quick-tile");
    root.set_hexpand(true);
    let toggle = gtk::Button::from_icon_name(icon);
    toggle.add_css_class("quick-toggle");
    toggle.set_valign(gtk::Align::Center);
    toggle.set_focus_on_click(false);
    toggle.set_tooltip_text(Some(title));
    toggle.update_property(&[gtk::accessible::Property::Label(title)]);
    root.append(&toggle);
    let text = vbox(1);
    text.set_valign(gtk::Align::Center);
    text.set_hexpand(true);
    let name = label(title, "quick-title");
    name.set_wrap(false);
    name.set_xalign(0.);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_max_width_chars(1);
    text.append(&name);
    let detail = label("", "quick-detail");
    detail.set_wrap(false);
    detail.set_xalign(0.);
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
    detail.set_max_width_chars(1);
    text.append(&detail);
    // The words lead to what the circle turns on and off.
    let content = hbox(4);
    content.append(&text);
    let chevron = gtk::Image::from_icon_name("go-next-symbolic");
    chevron.set_pixel_size(12);
    chevron.add_css_class("muted");
    content.append(&chevron);
    let more = gtk::Button::new();
    more.add_css_class("quick-label");
    more.set_hexpand(true);
    more.set_focus_on_click(false);
    more.set_child(Some(&content));
    more.connect_clicked(move |_| open());
    root.append(&more);
    (root, Tile { toggle, detail })
}
/// A level's line: its icon, which may mute it, then the slider.
fn level(icon: &str, scale: &gtk::Scale) -> Level {
    let root = hbox(8);
    root.add_css_class("quick-slider");
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("quick-mute");
    button.set_valign(gtk::Align::Center);
    button.set_focus_on_click(false);
    root.append(&button);
    root.append(scale);
    Level {
        root,
        scale: scale.clone(),
        icon: button,
    }
}
/// A level that is not muted by its icon.
fn plain_level(icon: &str, scale: &gtk::Scale) -> Level {
    let level = level(icon, scale);
    level.icon.set_can_target(false);
    level.icon.set_can_focus(false);
    level
}
/// A level that its icon mutes through `op`, whose state the daemon reports.
fn muting_level(ui: &Ui, op: &'static str, icon: &str, scale: &gtk::Scale) -> Level {
    let level = level(icon, scale);
    let ui = ui.clone();
    level.icon.connect_clicked(move |b| {
        let muted = !b.has_css_class("muted-now");
        ui.run(action(op, "", if muted { "true" } else { "false" }), None);
    });
    level
}
/// Stays on one line and in the panel's width however long what it says.
fn note() -> gtk::Label {
    let error = error_label();
    error.set_max_width_chars(44);
    error
}
fn heading(text: &str) -> gtk::Label {
    label(text, "quick-heading")
}
fn icon_button(icon: &str, title: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("quick-more");
    b.set_valign(gtk::Align::Center);
    b.set_focus_on_click(false);
    b.set_tooltip_text(Some(title));
    b.update_property(&[gtk::accessible::Property::Label(title)]);
    b
}
/// A page: back, its name, room in the heading, and a body scrolling on its own.
struct Page {
    head: gtk::Box,
    body: gtk::Box,
    report: gtk::Label,
    scroll: gtk::ScrolledWindow,
}
fn page(stack: &gtk::Stack, id: &str, title: &str) -> Page {
    let root = vbox(6);
    root.add_css_class("quick-panel");
    let head = hbox(6);
    head.add_css_class("quick-head");
    let back = icon_button("go-previous-symbolic", "Back");
    let to = stack.clone();
    back.connect_clicked(move |_| go(&to, "main", gtk::StackTransitionType::SlideRight));
    head.append(&back);
    let name = label(title, "section-title");
    name.set_wrap(false);
    name.set_hexpand(true);
    name.set_xalign(0.);
    name.set_valign(gtk::Align::Center);
    head.append(&name);
    root.append(&head);
    let report = note();
    root.append(&report);
    let body = vbox(2);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(MAX_LIST)
        .child(&body)
        .build();
    root.append(&scroll);
    stack.add_named(&root, Some(id));
    Page {
        head,
        body,
        report,
        scroll,
    }
}
/// A page whose heading also holds the radio's scan and switch.
fn radio_page(
    ui: &Ui,
    stack: &gtk::Stack,
    (id, title): (&str, &str),
    (power, scan_op): (&str, &str),
) -> (Page, Radio) {
    let page = page(stack, id, title);
    let scanning = spinner();
    scanning.set_valign(gtk::Align::Center);
    scanning.set_margin_end(6);
    page.head.append(&scanning);
    let scan = icon_button("view-refresh-symbolic", "Scan");
    ui.bind(&scan, action(scan_op, "", ""));
    page.head.append(&scan);
    let switch = toggle(ui, power);
    switch.set_margin_start(4);
    switch.set_focus_on_click(false);
    page.head.append(&switch);
    let error = note();
    page.body.append(&error);
    let list = vbox(2);
    page.body.append(&list);
    let pointer = gtk::EventControllerMotion::new();
    // What waited for the pointer to leave comes with the next poll.
    pointer.connect_leave(|_| STATE_WANTED.store(true, std::sync::atomic::Ordering::Relaxed));
    list.add_controller(pointer.clone());
    let radio = Radio {
        switch,
        scan,
        scanning,
        error,
        list,
        pointer,
    };
    (page, radio)
}
/// One short line per audio device. What all the devices of a card share says
/// nothing, so it goes: "800 Series … (ACE) Speaker" reads "Speaker". Devices are
/// of one card when their first word matches, as a loopback beside it must not
/// cost the others their short names. A device alone on its card keeps its name.
// ponytail: the first word stands in for the card; two cards of one vendor merge and lose
// that word (the tooltip keeps it). Group on the card from pactl if that ever misleads.
fn short_names(names: &[&str]) -> Vec<String> {
    let first = |name: &str| name.split(' ').next().map(str::to_string);
    let mut short: Vec<String> = names
        .iter()
        .map(|name| {
            let words: Vec<&str> = name.split(' ').collect();
            let card: Vec<&&str> = names.iter().filter(|n| first(n) == first(name)).collect();
            if card.len() < 2 {
                return name.to_string();
            }
            // Each keeps a word at least, even one whose whole name the others begin with.
            let mut common = card
                .iter()
                .map(|n| {
                    let same = n.split(' ').zip(&words).take_while(|(a, b)| a == *b);
                    same.count().min(n.split(' ').count() - 1)
                })
                .min()
                .unwrap_or(0);
            // A card with HDMI outputs alone shares all but the number: "HDMI 3", not "3".
            while common > 0
                && (words[common].parse::<u32>().is_ok()
                    || words[common] == "/"
                    || words[common - 1] == "/")
            {
                common -= 1;
            }
            if common == 0 {
                return name.to_string();
            }
            words[common..]
                .join(" ")
                .replace(" / DisplayPort", "")
                .replace(" Output", "")
                .replace(" (Stereo)", "")
        })
        .collect();
    // Short names that came out alike are no use: back to the full ones, numbered
    // if those are alike too, as with two headsets of one model.
    let alike = short.clone();
    for (i, s) in short.iter_mut().enumerate() {
        if alike.iter().filter(|a| *a == s).count() > 1 {
            *s = names[i].to_string();
        }
    }
    for i in 1..short.len() {
        let before = short[..i].iter().filter(|s| **s == short[i]).count();
        if before > 0 {
            short[i] = format!("{} {}", short[i], before + 1);
        }
    }
    short
}
/// One network or device: the whole row acts, its state said at the end.
struct Entry {
    root: gtk::Box,
    main: gtk::Button,
    line: gtk::Box,
    text: gtk::Label,
    state: gtk::Label,
}
fn entry(icon: &str, name: &str, state: &str, on: bool) -> Entry {
    let root = hbox(0);
    root.add_css_class("quick-row");
    if on {
        root.add_css_class("on");
    }
    let main = gtk::Button::new();
    main.add_css_class("quick-entry");
    main.set_hexpand(true);
    main.set_focus_on_click(false);
    let line = hbox(10);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    line.append(&image);
    let text = label(name, "");
    text.set_wrap(false);
    text.set_xalign(0.);
    text.set_hexpand(true);
    text.set_ellipsize(gtk::pango::EllipsizeMode::End);
    text.set_max_width_chars(1);
    text.set_tooltip_text(Some(name));
    line.append(&text);
    let detail = label(state, "quick-detail");
    detail.set_wrap(false);
    detail.set_visible(!state.is_empty());
    line.append(&detail);
    main.set_child(Some(&line));
    root.append(&main);
    Entry {
        root,
        main,
        line,
        text,
        state: detail,
    }
}
impl Entry {
    /// The row while something is being done to what it stands for.
    fn busy(&self, text: &str) {
        let turning = spinner();
        self.line
            .insert_child_after(&turning, self.state.prev_sibling().as_ref());
        self.state.set_text(text);
        self.state.set_visible(true);
        self.main.set_sensitive(false);
    }
}
fn empty(list: &gtk::Box, text: &str) {
    let l = label(text, "quick-detail");
    l.add_css_class("quick-empty");
    l.set_xalign(0.5);
    list.append(&l);
}
fn action_button(title: &str, on_click: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::with_label(title);
    b.set_focus_on_click(false);
    b.connect_clicked(move |_| on_click());
    b
}
impl Quick {
    pub fn new_shared(
        ui: &Ui,
        open: Open,
        quiet_switch: gtk::Switch,
        close: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let this = Rc::new(Self::new(ui, open, quiet_switch, close));
        // What was unfolded goes with the panel, the passwords on show first of all.
        let weak = Rc::downgrade(&this);
        this.widget.connect_unmap(move |_| {
            if let Some(s) = weak.upgrade() {
                s.ui.passwords.borrow_mut().clear();
                s.unfolded.take();
                s.forgetting.set(false);
                s.redraw();
            }
        });
        this
    }
    fn new(ui: &Ui, open: Open, quiet_switch: gtk::Switch, close: Rc<dyn Fn()>) -> Self {
        let stack = gtk::Stack::new();
        stack.set_hhomogeneous(true);
        stack.set_vhomogeneous(false);
        stack.set_interpolate_size(true);
        stack.set_transition_duration(140);
        stack.set_width_request(380);
        let widget = vbox(8);
        widget.add_css_class("quick-panel");
        let unfold = |page: &'static str| {
            let stack = stack.clone();
            move || go(&stack, page, gtk::StackTransitionType::SlideLeft)
        };
        let (wifi_root, wifi) = tile("Wi-Fi", "network-wireless-symbolic", unfold("wifi"));
        let (bt_root, bluetooth) = tile("Bluetooth", "bluetooth-symbolic", unfold("bluetooth"));
        let drawer = open.clone();
        let on_panel = stack.clone();
        let (quiet_root, quiet) = tile(
            "Do Not Disturb",
            "notifications-disabled-symbolic",
            move || drawer("notifications", panel_monitor(&on_panel).as_ref()),
        );
        let (night_root, night) = tile(
            "Night Light",
            "weather-clear-night-symbolic",
            unfold("display"),
        );
        for (root_a, root_b) in [(&wifi_root, &bt_root), (&quiet_root, &night_root)] {
            let row = hbox(8);
            row.set_homogeneous(true);
            row.append(root_a);
            row.append(root_b);
            widget.append(&row);
        }
        let brightness = plain_level("display-brightness-symbolic", &slider(ui, "brightness", ""));
        let more = icon_button("go-next-symbolic", "Display");
        let to = unfold("display");
        more.connect_clicked(move |_| to());
        brightness.root.append(&more);
        widget.append(&brightness.root);
        let volume = muting_level(
            ui,
            "mute",
            "audio-volume-high-symbolic",
            &slider(ui, "volume", ""),
        );
        let more = icon_button("go-next-symbolic", "Sound");
        let to = unfold("sound");
        more.connect_clicked(move |_| to());
        volume.root.append(&more);
        widget.append(&volume.root);
        let actions = hbox(8);
        actions.set_homogeneous(true);
        let power = gtk::Button::from_icon_name("system-shutdown-symbolic");
        power.add_css_class("quick-action");
        power.set_tooltip_text(Some("Sleep, restart or shut down"));
        power.update_property(&[gtk::accessible::Property::Label("Power")]);
        let to_power = open.clone();
        power.connect_clicked(move |_| to_power("power", None));
        actions.append(&power);
        for (title, icon, args) in [
            ("Lock", "system-lock-screen-symbolic", &["lock"][..]),
            (
                "Screenshot",
                "camera-photo-symbolic",
                &["screenshot", "--save"],
            ),
            (
                "Record the screen",
                "media-record-symbolic",
                &["screenshot", "--record"],
            ),
        ] {
            let b = gtk::Button::from_icon_name(icon);
            b.add_css_class("quick-action");
            b.set_tooltip_text(Some(title));
            b.update_property(&[gtk::accessible::Property::Label(title)]);
            let close = close.clone();
            b.connect_clicked(move |_| {
                close();
                glib::timeout_add_local_once(CLEAR_OF_PANEL, move || run_nexus(args));
            });
            actions.append(&b);
        }
        let settings = gtk::Button::from_icon_name("emblem-system-symbolic");
        settings.add_css_class("quick-action");
        settings.set_tooltip_text(Some("Settings"));
        settings.update_property(&[gtk::accessible::Property::Label("Settings")]);
        let to_settings = open.clone();
        settings.connect_clicked(move |_| to_settings("control", None));
        actions.append(&settings);
        widget.append(&actions);
        // Under everything, so that it moves nothing about to be clicked.
        let main_report = note();
        widget.append(&main_report);
        stack.add_named(&widget, Some("main"));

        let (wifi_page, wifi_radio) =
            radio_page(ui, &stack, ("wifi", "Wi-Fi"), ("wifi-power", "wifi-scan"));
        let wired_root = hbox(10);
        wired_root.add_css_class("quick-card");
        wired_root.set_visible(false);
        let wired_text = vbox(1);
        wired_text.set_hexpand(true);
        wired_text.set_valign(gtk::Align::Center);
        let wired_name = label("Ethernet", "quick-title");
        wired_name.set_wrap(false);
        wired_text.append(&wired_name);
        let wired_detail = label("", "quick-detail");
        wired_detail.set_wrap(false);
        wired_detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        wired_detail.set_max_width_chars(1);
        wired_text.append(&wired_detail);
        wired_root.append(&wired_text);
        let sender = ui.clone();
        let (choice, buttons) = segmented(
            ui,
            &[
                ("ethernet", "Ethernet"),
                ("wifi", "Wi-Fi"),
                ("both", "Both"),
            ],
            move |id| sender.send(action("network-use", "", id)),
        );
        choice.set_valign(gtk::Align::Center);
        wired_root.append(&choice);
        wifi_page.body.prepend(&wired_root);
        let wired = WiredRow {
            root: wired_root,
            detail: wired_detail,
            choice,
            ethernet: buttons[0].1.clone(),
            wifi: buttons[1].1.clone(),
            both: buttons[2].1.clone(),
        };

        let (bluetooth_page, bluetooth_radio) = radio_page(
            ui,
            &stack,
            ("bluetooth", "Bluetooth"),
            ("bluetooth-power", "bluetooth-scan"),
        );

        let sound_page = page(&stack, "sound", "Sound");
        // A scrolled window asks for its height at no width, for which the wrapped
        // error note answers as if squeezed narrow: blank space under the list.
        let sized = Slide::default();
        sized.set_measure_width(LIST_WIDTH);
        sound_page.scroll.set_child(None::<&gtk::Widget>);
        sized.set_child(&sound_page.body);
        sound_page.scroll.set_child(Some(&sized));
        let audio_error = note();
        sound_page.body.append(&audio_error);
        sound_page.body.append(&heading("Output"));
        let output = muting_level(
            ui,
            "mute",
            "audio-volume-high-symbolic",
            &slider(ui, "volume", ""),
        );
        sound_page.body.append(&output.root);
        let outputs = vbox(2);
        outputs.set_margin_top(2);
        sound_page.body.append(&outputs);
        let streams = StreamList::new();
        sound_page.body.append(&heading("Applications"));
        sound_page.body.append(&streams.widget);
        sound_page.body.append(&heading("Microphone"));
        let microphone = muting_level(
            ui,
            "mic-mute",
            "microphone-sensitivity-high-symbolic",
            &slider(ui, "microphone", ""),
        );
        sound_page.body.append(&microphone.root);
        let inputs = vbox(2);
        inputs.set_margin_top(2);
        sound_page.body.append(&inputs);

        let display_page = page(&stack, "display", "Display");
        display_page.body.set_spacing(8);
        let screen = plain_level("display-brightness-symbolic", &slider(ui, "brightness", ""));
        display_page.body.append(&screen.root);
        let light_error = note();
        display_page.body.append(&light_error);
        let night_card = vbox(6);
        night_card.add_css_class("quick-card");
        let night_head = hbox(10);
        let night_text = vbox(1);
        night_text.set_hexpand(true);
        let night_name = label("Night Light", "quick-title");
        night_name.set_wrap(false);
        night_text.append(&night_name);
        // ponytail: says the schedule in night.rs, move both to settings if it ever changes.
        let night_hours = label("From 21:00 to 7:30", "quick-detail");
        night_hours.set_wrap(false);
        night_text.append(&night_hours);
        night_head.append(&night_text);
        let night_switch = toggle(ui, "night-light");
        night_switch.set_focus_on_click(false);
        night_head.append(&night_switch);
        night_card.append(&night_head);
        let night_temperature = ranged_slider(
            ui,
            "night-light-temperature",
            "",
            (
                f64::from(backend::NIGHT_LIGHT_MIN),
                f64::from(backend::NIGHT_LIGHT_MAX),
                100.,
            ),
            " K",
        );
        // Further right means a stronger, warmer filter.
        night_temperature.set_inverted(true);
        night_card.append(&night_temperature);
        display_page.body.append(&night_card);
        let night_error = note();
        display_page.body.append(&night_error);
        let themes = gtk::Button::with_label("Wallpaper and Theme");
        themes.add_css_class("flat");
        themes.set_focus_on_click(false);
        let to_look = open.clone();
        themes.connect_clicked(move |_| to_look("appearance", None));
        display_page.body.append(&themes);

        // The longest of them still leaves the bar in view, on any screen.
        let scrolls = [
            wifi_page.scroll.clone(),
            bluetooth_page.scroll.clone(),
            sound_page.scroll.clone(),
            display_page.scroll.clone(),
        ];
        stack.connect_visible_child_name_notify(|stack| {
            // Unmapped, it is about to open on that page: see the map below.
            if stack.is_mapped() {
                scan_shown(stack);
            }
        });
        stack.connect_map(move |stack| {
            scan_shown(stack);
            let Some(screen) = panel_monitor(stack) else {
                return;
            };
            let room = screen.geometry().height() - BAR - 2 * GAP - PAGE_CHROME;
            for scroll in &scrolls {
                scroll.set_max_content_height(room.clamp(120, MAX_LIST));
            }
        });
        stack.set_visible_child_name("main");

        let this = Self {
            widget: stack,
            reports: [
                main_report,
                wifi_page.report,
                bluetooth_page.report,
                sound_page.report,
                display_page.report,
            ],
            generation: std::cell::Cell::new(0),
            wifi_radio,
            wired,
            bluetooth_radio,
            shown_networks: RefCell::default(),
            shown_devices: RefCell::default(),
            shown_outputs: RefCell::default(),
            shown_inputs: RefCell::default(),
            unfolded: RefCell::default(),
            forgetting: std::cell::Cell::new(false),
            cancel: RefCell::default(),
            wifi,
            bluetooth,
            quiet,
            night,
            brightness,
            volume,
            output,
            outputs,
            streams,
            microphone,
            inputs,
            audio_error,
            screen,
            light_error,
            night_switch,
            night_temperature,
            night_error,
            ui: ui.clone(),
            quiet_switch,
        };
        for (toggle, op) in [
            (&this.wifi.toggle, "wifi-power"),
            (&this.bluetooth.toggle, "bluetooth-power"),
            (&this.night.toggle, "night-light"),
        ] {
            let ui = ui.clone();
            toggle.connect_clicked(move |b| {
                let on = !b.has_css_class("on");
                if on {
                    b.add_css_class("on");
                } else {
                    b.remove_css_class("on");
                }
                ui.pending.borrow_mut().insert(op.to_string(), "");
                ui.run(
                    action(op, "", if on { "true" } else { "false" }),
                    Some(op.to_string()),
                );
            });
        }
        let switch = this.quiet_switch.clone();
        this.quiet
            .toggle
            .connect_clicked(move |_| switch.set_active(!switch.is_active()));
        let tile = this.quiet.clone();
        let show_quiet = move |s: &gtk::Switch| {
            let quiet = s.is_active();
            tile.set(quiet, if quiet { "On" } else { "Off" });
        };
        show_quiet(&this.quiet_switch);
        this.quiet_switch.connect_active_notify(show_quiet);
        this
    }
    /// An operation settled: its rows are drawn anew with the state that follows.
    pub fn refresh(&self) {
        self.shown_networks.take();
        self.shown_devices.take();
    }
    /// Starts a connection operation, its row showing it until the daemon answers.
    fn start(self: &Rc<Self>, op: &str, target: &str) {
        self.ui
            .pending
            .borrow_mut()
            .insert(target.to_string(), busy_text(op));
        self.ui
            .run(action(op, target, ""), Some(target.to_string()));
        // A password prompt can come at any point of the operation, seconds in for
        // Bluetooth: the state is polled quickly for as long as it lasts.
        let _ = self.ui.nudge.send(());
        let (ui, target) = (self.ui.clone(), target.to_string());
        glib::timeout_add_local(Duration::from_millis(400), move || {
            if ui.pending.borrow().contains_key(&target) && ui.nudge.send(()).is_ok() {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
        self.redraw();
    }
    /// Unfolds what is done to this row, or folds it back.
    fn fold(self: &Rc<Self>, id: &str) {
        let open = self.unfolded.borrow().as_deref() == Some(id);
        *self.unfolded.borrow_mut() = (!open).then(|| id.to_string());
        self.forgetting.set(false);
        self.redraw();
    }
    fn redraw(self: &Rc<Self>) {
        self.draw_networks();
        self.draw_devices();
    }
    /// Draws `list` anew. The focused button goes with it, so the keyboard comes back
    /// to the row it was on: its More, or Cancel if forgetting it is being asked,
    /// never the button that forgets. Rows are told apart by their widget names.
    fn draw(self: &Rc<Self>, list: &gtk::Box, fill: fn(&Rc<Self>, &gtk::Box)) {
        let focused = list
            .focus_child()
            .filter(|_| list.is_mapped())
            .map(|row| row.widget_name());
        clear(list);
        fill(self, list);
        let Some(id) = focused else { return };
        let forgetting = self.forgetting.get() && self.unfolded.borrow().as_deref() == Some(&*id);
        let target = self
            .cancel
            .take()
            .filter(|_| forgetting)
            .map(gtk::Button::upcast);
        let target = target.or_else(|| {
            std::iter::successors(list.first_child(), |row| row.next_sibling())
                .find(|row| row.widget_name() == id)?
                .last_child()
        });
        // ponytail: a busy row with no More can't take the focus, which is then lost;
        // moving it elsewhere could put the next key on another network's row.
        if let Some(target) = target {
            target.grab_focus();
        }
    }
    /// What comes under a saved network or a paired device: its facts, the secret if
    /// any, and its actions, forgetting asked once more in place.
    fn actions(
        self: &Rc<Self>,
        facts: &str,
        name: &str,
        forget: Option<(&'static str, &str, &str)>,
        extra: &[&gtk::Button],
    ) -> gtk::Box {
        let root = vbox(6);
        root.add_css_class("quick-unfolded");
        let about = caption(facts);
        about.set_wrap(false);
        about.set_ellipsize(gtk::pango::EllipsizeMode::End);
        about.set_max_width_chars(1);
        root.append(&about);
        let row = hbox(6);
        root.append(&row);
        match forget {
            Some((op, id, what)) if self.forgetting.get() => {
                let ask = label(&format!("Forget {name}? {what}"), "caption");
                ask.set_max_width_chars(36);
                root.insert_child_after(&ask, Some(&about));
                let s = self.clone();
                let cancel = action_button("Cancel", move || {
                    s.forgetting.set(false);
                    s.redraw();
                });
                row.append(&cancel);
                self.cancel.replace(Some(cancel));
                let (s, op, id) = (self.clone(), op.to_string(), id.to_string());
                let confirm = action_button("Forget", move || {
                    // Settling draws the rows without it, held or not.
                    s.ui.run(action(&op, &id, ""), Some(id.clone()));
                    s.unfolded.take();
                    s.forgetting.set(false);
                    s.redraw();
                });
                confirm.add_css_class("destructive-action");
                row.append(&confirm);
            }
            _ => {
                for b in extra {
                    row.append(*b);
                }
                if forget.is_some() {
                    let s = self.clone();
                    row.append(&action_button("Forget", move || {
                        s.forgetting.set(true);
                        s.redraw();
                    }));
                }
            }
        }
        root
    }
    fn more(self: &Rc<Self>, row: &Entry, id: &str) {
        let open = self.unfolded.borrow().as_deref() == Some(id);
        let more = icon_button(
            if open {
                "go-up-symbolic"
            } else {
                "go-down-symbolic"
            },
            if open { "Less" } else { "More" },
        );
        let (s, id) = (self.clone(), id.to_string());
        more.connect_clicked(move |_| s.fold(&id));
        row.root.append(&more);
    }
    fn draw_networks(self: &Rc<Self>) {
        self.draw(&self.wifi_radio.list, Self::fill_networks);
    }
    fn fill_networks(self: &Rc<Self>, list: &gtk::Box) {
        let shown = self.shown_networks.borrow();
        let Some((nothing, networks)) = shown.as_ref() else {
            return;
        };
        if networks.is_empty() {
            empty(list, nothing);
        }
        for net in networks {
            let connected = net.connected();
            let name = if net.name.is_empty() {
                "Hidden network"
            } else {
                &net.name
            };
            let busy =
                self.ui
                    .pending
                    .borrow()
                    .get(&net.id)
                    .copied()
                    .or(match net.state.as_str() {
                        "connecting" => Some("Connecting…"),
                        "disconnecting" => Some("Disconnecting…"),
                        _ => None,
                    });
            let state = state_label(&net.state).unwrap_or(if net.saved { "Saved" } else { "" });
            let row = entry(signal_icon(net.signal), name, state, connected);
            row.root.set_widget_name(&net.id);
            let security = security_label(&net.security);
            let facts = format!("{security}  ·  Signal {}%", net.signal);
            if security != "Open" {
                let lock = gtk::Image::from_icon_name("system-lock-screen-symbolic");
                lock.set_pixel_size(11);
                lock.add_css_class("muted");
                row.line.append(&lock);
            }
            match busy {
                Some(text) => row.busy(text),
                // One stray click must not cut the connection: it unfolds instead.
                None if connected => {
                    let (s, id) = (self.clone(), net.id.clone());
                    row.main.connect_clicked(move |_| s.fold(&id));
                }
                None => {
                    let (s, id) = (self.clone(), net.id.clone());
                    row.main
                        .connect_clicked(move |_| s.start("wifi-connect", &id));
                }
            }
            if connected || net.saved {
                self.more(&row, &net.id);
            }
            list.append(&row.root);
            if self.unfolded.borrow().as_ref() == Some(&net.id) && (connected || net.saved) {
                let mut extra = vec![];
                let mut secret = None;
                if net.saved {
                    let revealed = self.ui.passwords.borrow().contains_key(&net.id);
                    let reveal = gtk::Button::with_label(if revealed {
                        "Hide Password"
                    } else {
                        "Show Password"
                    });
                    reveal.set_focus_on_click(false);
                    extra.push(reveal.clone());
                    secret = Some((reveal, revealed));
                }
                if connected {
                    let (s, id) = (self.clone(), net.id.clone());
                    extra.push(action_button("Disconnect", move || {
                        s.start("wifi-disconnect", &id);
                    }));
                }
                let refs: Vec<&gtk::Button> = extra.iter().collect();
                let unfolded = self.actions(
                    &facts,
                    name,
                    net.saved.then_some((
                        "wifi-forget",
                        net.id.as_str(),
                        "Its saved password goes too.",
                    )),
                    &refs,
                );
                unfolded.set_widget_name(&net.id);
                if let Some((reveal, revealed)) = secret {
                    if let Some(password) = self.ui.passwords.borrow().get(&net.id) {
                        unfolded.append(&password_line(Ok(password)));
                    }
                    let (ui, holder, id) = (self.ui.clone(), unfolded.clone(), net.id.clone());
                    let on_show = std::cell::Cell::new(revealed);
                    reveal.connect_clicked(move |b| {
                        toggle_password(&ui, &holder, &id);
                        let showing = !on_show.get();
                        on_show.set(showing);
                        b.set_label(if showing {
                            "Hide Password"
                        } else {
                            "Show Password"
                        });
                    });
                }
                list.append(&unfolded);
            }
        }
    }
    fn draw_devices(self: &Rc<Self>) {
        self.draw(&self.bluetooth_radio.list, Self::fill_devices);
    }
    fn fill_devices(self: &Rc<Self>, list: &gtk::Box) {
        let shown = self.shown_devices.borrow();
        let Some((nothing, devices)) = shown.as_ref() else {
            return;
        };
        if devices.is_empty() {
            empty(list, nothing);
        }
        for device in devices {
            let state = if device.connected {
                "Connected"
            } else if device.paired {
                "Paired"
            } else {
                ""
            };
            let row = entry("bluetooth-symbolic", &device.name, state, device.connected);
            row.root.set_widget_name(&device.path);
            let busy = self.ui.pending.borrow().get(&device.path).copied();
            match busy {
                Some(text) => row.busy(text),
                None if device.connected => {
                    let (s, id) = (self.clone(), device.path.clone());
                    row.main.connect_clicked(move |_| s.fold(&id));
                }
                None => {
                    let op = if device.paired {
                        "bluetooth-connect"
                    } else {
                        "bluetooth-pair"
                    };
                    let (s, id) = (self.clone(), device.path.clone());
                    row.main.connect_clicked(move |_| s.start(op, &id));
                }
            }
            let known = device.connected || device.paired;
            if known {
                self.more(&row, &device.path);
            }
            list.append(&row.root);
            if known && self.unfolded.borrow().as_ref() == Some(&device.path) {
                let mut extra = vec![];
                if device.connected {
                    let (s, id) = (self.clone(), device.path.clone());
                    extra.push(action_button("Disconnect", move || {
                        s.start("bluetooth-disconnect", &id);
                    }));
                }
                let refs: Vec<&gtk::Button> = extra.iter().collect();
                let unfolded = self.actions(
                    &device.address,
                    &device.name,
                    device.paired.then_some((
                        "bluetooth-forget",
                        device.path.as_str(),
                        "It will need pairing again.",
                    )),
                    &refs,
                );
                unfolded.set_widget_name(&device.path);
                list.append(&unfolded);
            }
        }
    }
    /// Which device a sound goes to or comes from: one click, the current one ticked.
    fn draw_audio(
        &self,
        list: &gtk::Box,
        shown: &RefCell<Option<(Vec<AudioDevice>, String)>>,
        (devices, default): (&[AudioDevice], &str),
        (icon, op): (&str, &'static str),
    ) {
        if shown
            .borrow()
            .as_ref()
            .is_some_and(|(d, c)| d == devices && c == default)
        {
            return;
        }
        clear(list);
        let names: Vec<&str> = devices.iter().map(|d| d.description.as_str()).collect();
        for (device, short) in devices.iter().zip(short_names(&names)) {
            let current = device.name == default;
            let row = entry(icon, &short, "", current);
            row.text.set_tooltip_text(Some(&device.description));
            if current {
                let tick = gtk::Image::from_icon_name("object-select-symbolic");
                tick.set_pixel_size(14);
                row.line.append(&tick);
            }
            let (ui, name) = (self.ui.clone(), device.name.clone());
            row.main
                .connect_clicked(move |_| ui.send(action(op, &name, "")));
            list.append(&row.root);
        }
        *shown.borrow_mut() = Some((devices.to_vec(), default.to_string()));
    }
    /// What went wrong with an action started here, for a few seconds.
    pub fn report(self: &Rc<Self>, result: &Result<(), String>) {
        self.generation.set(self.generation.get() + 1);
        for error in &self.reports {
            set_error(error, result.as_ref().err().map(String::as_str));
        }
        if result.is_err() {
            let (s, generation) = (self.clone(), self.generation.get());
            glib::timeout_add_local_once(Duration::from_secs(6), move || {
                if s.generation.get() == generation {
                    s.report(&Ok(()));
                }
            });
        }
    }
    fn show_wired(&self, s: &Snapshot) {
        let w = &self.wired;
        w.root.set_visible(s.wired.is_some());
        let Some(wired) = &s.wired else {
            return;
        };
        let text = match state_label(&wired.state) {
            _ if !wired.cable => "No cable".to_string(),
            Some(state) => format!("{state}  ·  {}", wired.name),
            None => format!("Not in use  ·  {}", wired.name),
        };
        w.detail.set_text(&text);
        w.detail.set_tooltip_text(Some(&text));
        w.choice.set_visible(wired.cable);
        let on_wifi = s.networks.iter().any(Network::connected);
        match (wired.state == "connected", on_wifi) {
            (true, true) => &w.both,
            (true, false) => &w.ethernet,
            _ => &w.wifi,
        }
        .set_active(true);
    }
    pub fn update(self: &Rc<Self>, s: &Snapshot) {
        let ui = &self.ui;
        self.wifi_radio.show(
            ui,
            "wifi-power",
            s.wifi,
            s.wifi_scanning,
            s.wifi_error.as_deref(),
        );
        self.bluetooth_radio.show(
            ui,
            "bluetooth-power",
            s.bluetooth,
            s.bluetooth_scanning,
            s.bluetooth_error.as_deref(),
        );
        self.show_wired(s);
        let nothing = match (s.wifi, s.wifi_scanning) {
            (false, _) => "Wi-Fi is off",
            (true, true) => "Looking for networks…",
            (true, false) => "No networks found",
        };
        let rows = next_rows(
            (self.shown_networks.borrow().as_ref(), nothing),
            &s.networks,
            self.wifi_radio.held(),
            |n| (n.id.as_str(), (n.state.as_str(), n.saved)),
        );
        if let Some(rows) = rows {
            *self.shown_networks.borrow_mut() = Some((nothing, rows));
            self.draw_networks();
        }
        let nothing = match (s.bluetooth, s.bluetooth_scanning) {
            (false, _) => "Bluetooth is off",
            (true, true) => "Looking for devices…",
            (true, false) => "No devices found",
        };
        let rows = next_rows(
            (self.shown_devices.borrow().as_ref(), nothing),
            &s.devices,
            self.bluetooth_radio.held(),
            |d| (d.path.as_str(), (d.connected, d.paired)),
        );
        if let Some(rows) = rows {
            *self.shown_devices.borrow_mut() = Some((nothing, rows));
            self.draw_devices();
        }
        let pending = |op: &str| ui.pending.borrow().contains_key(op);
        let network = s.networks.iter().find(|n| n.connected());
        let wired = s.wired.as_ref().is_some_and(|w| w.state == "connected");
        if !pending("wifi-power") {
            self.wifi.set(
                s.wifi,
                match network {
                    None if wired => "Ethernet",
                    _ if !s.wifi => "Off",
                    Some(n) => &n.name,
                    None => "Not connected",
                },
            );
        }
        if !pending("bluetooth-power") {
            let connected = s.devices.iter().filter(|d| d.connected).count();
            self.bluetooth.set(
                s.bluetooth,
                &match connected {
                    _ if !s.bluetooth => "Off".to_string(),
                    0 => "On".to_string(),
                    n => format!("{n} connected"),
                },
            );
        }
        self.night.toggle.set_sensitive(s.night_light.is_some());
        let night = s.night_light.clone().unwrap_or_default();
        if !pending("night-light") {
            self.night
                .set(night.enabled, if night.enabled { "On" } else { "Off" });
        }
        sync_toggle(ui, &self.night_switch, "night-light", night.enabled);
        self.night_switch.set_sensitive(s.night_light.is_some());
        self.night_temperature
            .set_sensitive(s.night_light.is_some());
        if !self.night_temperature.has_focus() && s.night_light.is_some() {
            self.night_temperature
                .set_value(f64::from(night.temperature));
        }
        set_error(&self.night_error, s.night_light_error.as_deref());
        set_error(&self.light_error, s.brightness_error.as_deref());
        set_error(&self.audio_error, s.audio_error.as_deref());
        // No backlight: nothing to slide, here or on its page, which Night Light still
        // leads to. No sound: the row stays, the way to the page saying why.
        self.brightness.root.set_visible(s.brightness.is_some());
        self.volume.scale.set_sensitive(s.volume.is_some());
        self.screen.root.set_sensitive(s.brightness.is_some());
        self.output.root.set_sensitive(s.volume.is_some());
        self.microphone.root.set_sensitive(s.microphone.is_some());
        self.brightness.show(s.brightness);
        self.screen.show(s.brightness);
        let icon = sound_icon(s.muted, s.volume.unwrap_or(0));
        for level in [&self.volume, &self.output] {
            level.show(s.volume);
            level.show_muted(s.muted, icon);
        }
        self.microphone.show(s.microphone);
        self.microphone.show_muted(
            s.mic_muted,
            if s.mic_muted {
                "microphone-sensitivity-muted-symbolic"
            } else {
                "microphone-sensitivity-high-symbolic"
            },
        );
        self.streams.update(ui, &s.streams);
        self.draw_audio(
            &self.outputs,
            &self.shown_outputs,
            (&s.outputs, &s.default_output),
            ("audio-speakers-symbolic", "audio-output"),
        );
        self.draw_audio(
            &self.inputs,
            &self.shown_inputs,
            (&s.inputs, &s.default_input),
            ("audio-input-microphone-symbolic", "audio-input"),
        );
    }
}
#[cfg(test)]
mod tests {
    use super::{next_rows, rows_changed, short_names};
    #[test]
    fn audio_devices_read_short_and_stay_unique() {
        const CARD: &str = "800 Series Chipset Family Audio Context Engine (ACE)";
        let with_card = |ends: &[&str]| -> Vec<String> {
            ends.iter().map(|end| format!("{CARD} {end}")).collect()
        };
        let short =
            |names: &[String]| short_names(&names.iter().map(String::as_str).collect::<Vec<_>>());
        // As on the machine this was written on, a loopback among them.
        let mut outputs = with_card(&[
            "HDMI / DisplayPort 3 Output (Stereo)",
            "HDMI / DisplayPort 2 Output (Stereo)",
            "Speaker",
        ]);
        outputs.push("Loopback Analog Stereo".into());
        assert_eq!(
            short(&outputs),
            ["HDMI 3", "HDMI 2", "Speaker", "Loopback Analog Stereo"]
        );
        let inputs = with_card(&["Stereo Microphone", "Digital Microphone"]);
        assert_eq!(short(&inputs), ["Stereo Microphone", "Digital Microphone"]);
        // Nothing shared: nothing to strip.
        assert_eq!(
            short_names(&["Built-in Audio Analog Stereo", "WH-1000XM4", "USB Headset"]),
            ["Built-in Audio Analog Stereo", "WH-1000XM4", "USB Headset"]
        );
        // Only HDMI outputs: all they share but the number.
        assert_eq!(
            short(&with_card(&[
                "HDMI / DisplayPort 1 Output (Stereo)",
                "HDMI / DisplayPort 2 Output (Stereo)"
            ])),
            ["HDMI 1", "HDMI 2"]
        );
        // One name the start of another, and two alike.
        assert_eq!(
            short_names(&["USB Audio", "USB Audio Speaker"]),
            ["Audio", "Audio Speaker"]
        );
        assert_eq!(
            short_names(&["USB Audio", "USB Audio"]),
            ["USB Audio", "USB Audio 2"]
        );
        // Tidied into the same words: the full names tell them apart.
        assert_eq!(
            short_names(&["Card A Output", "Card A"]),
            ["Card A Output", "Card A"]
        );
    }
    fn changed(old: &[(&'static str, bool)], new: &[(&'static str, bool)]) -> bool {
        rows_changed(old, new, &|r: &(&'static str, bool)| (r.0, r.1))
    }
    #[test]
    fn held_rows_redraw_only_for_what_they_say_of_a_connection() {
        let (a, b) = (("a", false), ("b", false));
        assert!(!changed(&[a], &[a]));
        assert!(!changed(&[a, b], &[b, a]), "order");
        assert!(changed(&[a], &[a, b]), "a row arrives");
        assert!(changed(&[], &[a]), "the first row arrives");
        assert!(!changed(&[a, b], &[a]), "a row leaves");
        assert!(changed(&[a], &[("a", true)]), "its state");
        assert!(changed(&[a], &[]), "none remain");
    }
    #[test]
    fn held_rows_stay_in_order_and_arrivals_go_last() {
        type Row = (&'static str, bool);
        let (a, b, c) = (("a", false), ("b", false), ("c", false));
        let next = |old: Vec<Row>, new: &[Row], held| {
            let shown = ("", old);
            next_rows((Some(&shown), ""), new, held, |r: &Row| (r.0, r.1))
        };
        assert_eq!(next(vec![a, b], &[c, b, a], true), Some(vec![a, b, c]));
        assert_eq!(next(vec![a, b], &[b, a], true), None, "only the order");
        assert_eq!(next(vec![a, b], &[b], true), None, "only one gone");
        // What a row says is the new one, where the row was; one gone stays meanwhile.
        let on = ("b", true);
        assert_eq!(next(vec![a, b, c], &[on, c], true), Some(vec![a, on, c]));
        assert_eq!(next(vec![a], &[], true), Some(vec![]), "none remain");
        assert_eq!(next(vec![], &[a], true), Some(vec![a]));
        assert_eq!(next(vec![a, b], &[b, a], false), Some(vec![b, a]));
        assert_eq!(next(vec![a], &[a], true), None);
        assert_eq!(
            next_rows((None, ""), &[a], true, |r: &Row| (r.0, r.1)),
            Some(vec![a])
        );
    }
    #[test]
    #[ignore = "requires an isolated display: run it inside the lab"]
    fn the_keyboard_stays_on_its_row() {
        use super::*;
        gtk::init().unwrap();
        let (nudge, _polls) = std::sync::mpsc::channel();
        let ui = Ui {
            tx: async_channel::unbounded().0,
            nudge,
            settings: Rc::default(),
            updating: Rc::default(),
            adjusting: Rc::default(),
            pending: Rc::default(),
            passwords: Rc::default(),
        };
        let open: Open = Rc::new(|_, _| {});
        let quick = Quick::new_shared(&ui, open, gtk::Switch::new(), Rc::new(|| {}));
        // Made up, and nothing below sends a request: only folding is exercised.
        let network = |id: &str, saved| Network {
            id: id.into(),
            name: id.into(),
            saved,
            security: "psk".into(),
            ..Network::default()
        };
        quick
            .shown_networks
            .replace(Some(("", vec![network("a", true), network("b", true)])));
        let window = gtk::Window::new();
        window.set_child(Some(&quick.widget));
        quick
            .widget
            .set_transition_type(gtk::StackTransitionType::None);
        quick.widget.set_visible_child_name("wifi");
        window.present();
        let context = glib::MainContext::default();
        let settle = || {
            for _ in 0..50 {
                context.iteration(false);
                std::thread::sleep(Duration::from_millis(4));
            }
        };
        settle();
        quick.redraw();
        let list = &quick.wifi_radio.list;
        let row = |id: &str| {
            std::iter::successors(list.first_child(), |w| w.next_sibling())
                .find(|w| w.widget_name() == id && w.has_css_class("quick-row"))
                .unwrap()
        };
        let focus = || gtk::prelude::GtkWindowExt::focus(&window);
        // Nobody on the rows, nothing is focused for them.
        quick.fold("a");
        assert!(list.focus_child().is_none());
        quick.fold("a");
        // On the second row's More, folding it open and shut.
        assert!(row("b").last_child().unwrap().grab_focus());
        quick.fold("b");
        assert_eq!(focus(), row("b").last_child());
        // Asked to forget, on Cancel: Enter again must not forget.
        quick.forgetting.set(true);
        quick.redraw();
        let on = focus().and_downcast::<gtk::Button>().unwrap();
        assert_eq!(on.label().as_deref(), Some("Cancel"));
        assert!(!on.has_css_class("destructive-action"));
        // Back from there, on the row's More.
        on.emit_clicked();
        assert_eq!(focus(), row("b").last_child());
        quick.fold("b");
        assert_eq!(focus(), row("b").last_child());
        window.close();
        settle();
    }
}
