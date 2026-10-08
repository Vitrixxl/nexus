mod lock;
mod screenshot;
mod ui;
use gtk::{gio, glib, prelude::*};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use nexus_control::*;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::Write,
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};
use ui::control::ControlCenter;

const PAGES: [(&str, &str, &str); 7] = [
    ("wifi", "Wi-Fi", "network-wireless-symbolic"),
    ("bluetooth", "Bluetooth", "bluetooth-symbolic"),
    ("sound", "Sound", "audio-volume-high-symbolic"),
    ("display", "Display", "display-brightness-symbolic"),
    (
        "appearance",
        "Appearance",
        "preferences-desktop-wallpaper-symbolic",
    ),
    ("monitor", "Monitor", "power-profile-performance-symbolic"),
    ("power", "Power", "system-shutdown-symbolic"),
];
fn main() -> anyhow::Result<()> {
    // Notifications must arrive on the session bus the desktop's applications
    // share; without an address GLib finds it, a disabled one would hide it.
    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_ok_and(|a| a.starts_with("disabled:")) {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::remove_var("DBUS_SESSION_BUS_ADDRESS") };
    }
    // Vulkan picks the discrete GPU on hybrid laptops: it wakes up (seconds
    // of delay, battery drain) and every frame then crosses to the integrated
    // one. This process only starts helpers that do not use GTK.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("GSK_RENDERER", "gl") };
    }
    // Even with the GL renderer, GDK creates a Vulkan device to list dmabuf
    // formats, and glvnd tries NVIDIA's EGL before Mesa's: each wakes a
    // sleeping discrete GPU, seconds the lock (a new process every time)
    // spent before showing.
    if std::env::var_os("GDK_DISABLE").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("GDK_DISABLE", "vulkan") };
    }
    const MESA_EGL: &str = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
    if std::env::var_os("__EGL_VENDOR_LIBRARY_FILENAMES").is_none()
        && std::path::Path::new(MESA_EGL).exists()
        && !nvidia_drives_display()
    {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", MESA_EGL) };
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!(
            "Nexus — desktop control center\n\nnexus [launcher|tray|battery|control|monitor|notifications|wifi|bluetooth|sound|display|appearance|power|shell]\nnexus --page PAGE\nnexus screenshot [region|screen|window] [--live] [--save]\nnexus lock [--preview]\nnexus status\nnexus init-theme"
        );
        return Ok(());
    }
    // The lock runs on its own, so that it holds whatever happens to the shell.
    if args.first().is_some_and(|a| a == "lock") {
        return lock::run(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "init-theme") {
        theme::write_theme(&theme::load())?;
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "status") {
        ensure_daemon()?;
        let reply = request(&Request::Status)?;
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }
    let page = if args.first().is_some_and(|a| a == "--page") {
        args.get(1)
    } else {
        args.first()
    }
    .map(String::as_str)
    .unwrap_or("launcher");
    if !PAGES.iter().any(|p| p.0 == page)
        && ![
            "launcher",
            "control",
            "notifications",
            "tray",
            "battery",
            "screenshot",
            "shell",
            "close",
        ]
        .contains(&page)
    {
        anyhow::bail!("Unknown page: {page}");
    }
    let activation = if page == "screenshot" {
        let options = args[1..].join(" ");
        screenshot::Options::parse(&options).map_err(anyhow::Error::msg)?;
        format!("screenshot\t{options}")
    } else if page == "launcher" && args.first().is_some_and(|s| s == "launcher") && args.len() > 1
    {
        format!(
            "launcher\t{}",
            args[1..].join(" ").replace(['\n', '\r', '\t'], " ")
        )
    } else {
        page.to_string()
    };
    let socket = runtime()?.join("ui.sock");
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket) {
        writeln!(stream, "{activation}")?;
        return Ok(());
    }
    if page == "close" {
        return Ok(());
    }
    let (listener, _lock) = listener("ui")?;
    let (page_tx, page_rx) = async_channel::unbounded();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if let Ok(line) = read_line(&stream) {
                let _ = page_tx.send_blocking(line.trim().to_string());
            }
        }
    });
    // Before GTK starts, so no other daemon is activated for an early notification.
    let (notice_tx, notice_rx) = async_channel::unbounded();
    let notices = notifications::Server::start(move |change| {
        let _ = notice_tx.try_send(change);
    });
    // NON_UNIQUE supports Artix sessions without a session D-Bus. Our private socket provides single-instance activation.
    // Without a session bus GTK names the window after the program; keep the
    // compositor class equal to the application id either way.
    glib::set_prgname(Some("io.github.vitrixxl.Nexus"));
    let app = gtk::Application::builder()
        .application_id("io.github.vitrixxl.Nexus")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let page = activation;
    let page_rx = Rc::new(RefCell::new(Some((page_rx, notice_rx))));
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        if let Some((rx, notice_rx)) = page_rx.borrow_mut().take() {
            // Map the shell before waiting for D-Bus services or desktop entries.
            // The worker below fills the catalogue as soon as the daemon is ready.
            build(
                app,
                &page,
                rx,
                launcher::Catalogue::default(),
                (notices.clone(), notice_rx),
            );
        }
    });
    app.run_with_args::<&str>(&[]);
    let _ = std::fs::remove_file(socket);
    Ok(())
}
fn label(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.);
    l.set_wrap(true);
    if !class.is_empty() {
        l.add_css_class(class);
    }
    l
}
fn caption(text: &str) -> gtk::Label {
    let l = label(text, "muted");
    l.add_css_class("caption");
    l
}
/// Whether the GPU the firmware booted on, which drives the screen, is NVIDIA's.
fn nvidia_drives_display() -> bool {
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return false;
    };
    cards.flatten().any(|card| {
        let device = card.path().join("device");
        let read = |name| std::fs::read_to_string(device.join(name)).unwrap_or_default();
        read("boot_vga").trim() == "1" && read("vendor").trim() == "0x10de"
    })
}
/// Applies the theme's stylesheet and light or dark preference to `display`.
fn install_theme(display: &gtk::gdk::Display, settings: &theme::Settings) -> gtk::CssProvider {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&theme::css(settings));
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    if let Some(gtk_settings) = gtk::Settings::default() {
        gtk_settings.set_gtk_application_prefer_dark_theme(settings.dark());
    }
    provider
}
fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}
fn hbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, spacing)
}
fn button(text: &str) -> gtk::Button {
    let b = gtk::Button::with_label(text);
    b.set_valign(gtk::Align::Center);
    b
}
fn clear(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}
fn action(op: &str, target: &str, value: &str) -> Request {
    Request::Action {
        op: op.into(),
        target: target.into(),
        value: value.into(),
    }
}
fn signal_icon(signal: u8) -> &'static str {
    match signal {
        75.. => "network-wireless-signal-excellent-symbolic",
        50.. => "network-wireless-signal-good-symbolic",
        25.. => "network-wireless-signal-ok-symbolic",
        _ => "network-wireless-signal-weak-symbolic",
    }
}
fn security_label(security: &str) -> &'static str {
    if matches!(security, "" | "none" | "owe") {
        "Open"
    } else if security.contains("ieee8021x") {
        "Enterprise"
    } else if security.contains("wep") {
        "WEP"
    } else {
        "Secured"
    }
}
fn state_label(state: &str) -> Option<&'static str> {
    match state {
        "connected" => Some("Connected"),
        "connecting" => Some("Connecting…"),
        "disconnecting" => Some("Disconnecting…"),
        "failed" => Some("Connection failed"),
        _ => None,
    }
}
fn busy_text(op: &str) -> &'static str {
    match op {
        "wifi-disconnect" | "bluetooth-disconnect" => "Disconnecting…",
        "bluetooth-pair" => "Pairing…",
        _ => "Connecting…",
    }
}
/// Replaces the button's label with `text` and a spinner, and disables it.
fn set_busy(b: &gtk::Button, text: &str) {
    let content = hbox(6);
    let spinner = gtk::Spinner::new();
    spinner.start();
    content.append(&spinner);
    content.append(&gtk::Label::new(Some(text)));
    b.set_child(Some(&content));
    b.set_sensitive(false);
}
/// Error labels take no space until there is something to report.
fn error_label() -> gtk::Label {
    let l = label("", "error");
    l.add_css_class("banner");
    l.set_visible(false);
    l
}
fn set_error(l: &gtk::Label, error: Option<&str>) {
    let error = error.unwrap_or("");
    l.set_text(error);
    l.set_visible(!error.is_empty());
}
#[derive(Clone)]
struct Ui {
    tx: async_channel::Sender<Event>,
    settings: Rc<RefCell<theme::Settings>>,
    updating: Rc<Cell<bool>>,
    /// Connection operations awaiting the daemon, by network or device, with
    /// the text their button shows meanwhile.
    pending: Rc<RefCell<HashMap<String, &'static str>>>,
}
enum Event {
    State(Box<Snapshot>),
    Catalogue(Box<launcher::Catalogue>),
    Done(Result<(), String>),
    /// The daemon answered a connection operation on this network or device.
    Settled(String),
}
fn perform(req: &Request) -> Result<(), String> {
    request(req).map_err(|e| e.to_string()).and_then(|r| {
        if r.ok {
            Ok(())
        } else {
            Err(r.error.unwrap_or_else(|| "Operation failed".into()))
        }
    })
}
impl Ui {
    fn send(&self, req: Request) {
        self.run(req, None);
    }
    /// Performs `req`, then reports `settled` and the new state.
    fn run(&self, req: Request, settled: Option<String>) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send_blocking(Event::Done(perform(&req)));
            if let Some(target) = settled {
                let _ = tx.send_blocking(Event::Settled(target));
            }
            if let Ok(reply) = request(&Request::Status)
                && let Some(state) = reply.state
            {
                let _ = tx.send_blocking(Event::State(Box::new(state)));
            }
        });
    }
    fn bind(&self, b: &gtk::Button, req: Request) {
        let ui = self.clone();
        b.connect_clicked(move |_| ui.send(req.clone()));
    }
    /// Binds a connection operation on `target`, which `b` shows as in
    /// progress until the daemon answers.
    fn bind_busy(&self, b: &gtk::Button, op: &str, target: &str) {
        let ui = self.clone();
        let (op, target) = (op.to_string(), target.to_string());
        b.connect_clicked(move |b| {
            let text = busy_text(&op);
            ui.pending.borrow_mut().insert(target.clone(), text);
            set_busy(b, text);
            ui.run(action(&op, &target, ""), Some(target.clone()));
        });
    }
    fn save_theme(&self) {
        self.send(Request::Theme {
            settings: self.settings.borrow().clone(),
        });
    }
}
fn toggle(ui: &Ui, op: &str) -> gtk::Switch {
    let s = gtk::Switch::new();
    s.set_valign(gtk::Align::Center);
    let ui = ui.clone();
    let op = op.to_string();
    s.connect_active_notify(move |s| {
        if !ui.updating.get() {
            ui.send(action(
                &op,
                "",
                if s.is_active() { "true" } else { "false" },
            ));
        }
    });
    s
}
fn heading(title: &str, subtitle: &str) -> gtk::Box {
    let b = vbox(4);
    b.append(&label(title, "title"));
    if !subtitle.is_empty() {
        b.append(&label(subtitle, "muted"));
    }
    b
}
/// Page title with its controls floating at the right.
fn radio_heading(
    title: &str,
    scanning: &gtk::Box,
    scan: &gtk::Button,
    toggle: &gtk::Switch,
) -> gtk::Box {
    let row = hbox(12);
    let text = label(title, "title");
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    row.append(&text);
    row.append(scanning);
    scan.set_valign(gtk::Align::Center);
    row.append(scan);
    row.append(toggle);
    row
}
/// Spinner shown in a radio page heading while the radio looks for devices.
fn scan_indicator() -> gtk::Box {
    let b = hbox(6);
    b.set_valign(gtk::Align::Center);
    b.append(&gtk::Spinner::new());
    b.append(&caption("Scanning…"));
    b.set_visible(false);
    b
}
fn set_scanning(indicator: &gtk::Box, scanning: bool) {
    indicator.set_visible(scanning);
    if let Some(spinner) = indicator.first_child().and_downcast::<gtk::Spinner>() {
        spinner.set_spinning(scanning);
    }
}
/// Card with an icon, a title and a one-line status, followed by trailing widgets.
fn row_card(icon: &str, title: &str) -> (gtk::Box, gtk::Label) {
    let row = hbox(14);
    row.add_css_class("card");
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(18);
    image.add_css_class("row-icon");
    row.append(&image);
    let text = vbox(2);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let name = label(title, "row-title");
    name.set_wrap(false);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    text.append(&name);
    let detail = caption("");
    detail.set_wrap(false);
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
    text.append(&detail);
    row.append(&text);
    (row, detail)
}
fn page_box() -> gtk::Box {
    let b = vbox(16);
    b.set_margin_top(30);
    b.set_margin_bottom(24);
    b.set_margin_start(32);
    b.set_margin_end(32);
    b
}
fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(child)
        .build()
}
fn slider(ui: &Ui, op: &str, target: &str) -> gtk::Scale {
    let min = if op == "brightness" { 1. } else { 0. };
    ranged_slider(ui, op, target, (min, 100., 1.), "%")
}
fn ranged_slider(
    ui: &Ui,
    op: &str,
    target: &str,
    (min, max, step): (f64, f64, f64),
    unit: &'static str,
) -> gtk::Scale {
    let s = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    s.set_draw_value(true);
    s.set_value_pos(gtk::PositionType::Right);
    s.set_hexpand(true);
    s.set_digits(0);
    s.set_format_value_func(move |_, v| format!("{v:.0}{unit}"));
    // Levels apply while dragging. One worker per slider sends them in order and
    // skips values it has not reached yet, so the daemon never sees overlapping
    // requests and the latest position always wins.
    let (values, pending) = mpsc::channel::<String>();
    let tx = ui.tx.clone();
    let op = op.to_owned();
    let target = target.to_owned();
    thread::spawn(move || {
        while let Ok(mut value) = pending.recv() {
            while let Ok(newer) = pending.try_recv() {
                value = newer;
            }
            if let Err(e) = perform(&action(&op, &target, &value))
                && tx.send_blocking(Event::Done(Err(e))).is_err()
            {
                break;
            }
        }
    });
    let ui = ui.clone();
    s.connect_value_changed(move |s| {
        if !ui.updating.get() {
            let _ = values.send(s.value().round().to_string());
        }
    });
    s
}
/// Title row with an optional mute switch, then the level and device picker.
fn level_card(
    title: &str,
    scale: &gtk::Scale,
    mute: Option<&gtk::Switch>,
    devices: Option<&gtk::DropDown>,
) -> gtk::Box {
    let card = vbox(10);
    card.add_css_class("card");
    let head = hbox(10);
    let t = label(title, "row-title");
    t.set_hexpand(true);
    head.append(&t);
    if let Some(mute) = mute {
        head.append(&caption("Mute"));
        head.append(mute);
    }
    card.append(&head);
    card.append(scale);
    if let Some(devices) = devices {
        card.append(devices);
    }
    card
}
fn audio_dropdown() -> gtk::DropDown {
    let dropdown = gtk::DropDown::from_strings(&[]);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(48);
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        let text = item.item().and_downcast::<gtk::StringObject>().unwrap();
        label.set_text(&text.string());
        label.set_tooltip_text(Some(&text.string()));
    });
    dropdown.set_factory(Some(&factory));
    dropdown
}
fn network_row(ui: &Ui, window: &gtk::ApplicationWindow, net: &Network) -> gtk::Box {
    let connected = net.connected();
    let name = if net.name.is_empty() {
        "Hidden network"
    } else {
        &net.name
    };
    let (row, detail) = row_card(signal_icon(net.signal), name);
    row.add_css_class("list-row");
    if connected {
        row.add_css_class("active");
    }
    let mut details: Vec<&str> = state_label(&net.state).into_iter().collect();
    details.push(security_label(&net.security));
    if net.saved && !connected {
        details.push("Saved");
    }
    detail.set_text(&details.join("  ·  "));
    row.set_tooltip_text(Some(&format!("Signal {}%", net.signal)));
    if net.saved {
        let b = button("Forget");
        b.add_css_class("flat");
        bind_forget(&b, window, ui, "wifi-forget", &net.id, name);
        row.append(&b);
    }
    let b = button(if connected { "Disconnect" } else { "Connect" });
    // NetworkManager keeps working on the network once Nexus started it.
    let busy = ui
        .pending
        .borrow()
        .get(&net.id)
        .copied()
        .or(match net.state.as_str() {
            "connecting" => Some("Connecting…"),
            "disconnecting" => Some("Disconnecting…"),
            _ => None,
        });
    match busy {
        Some(text) => set_busy(&b, text),
        None => ui.bind_busy(
            &b,
            if connected {
                "wifi-disconnect"
            } else {
                "wifi-connect"
            },
            &net.id,
        ),
    }
    row.append(&b);
    row
}
fn device_row(ui: &Ui, window: &gtk::ApplicationWindow, device: &Device) -> gtk::Box {
    let (row, detail) = row_card("bluetooth-symbolic", &device.name);
    row.add_css_class("list-row");
    if device.connected {
        row.add_css_class("active");
    }
    let busy = ui.pending.borrow().get(&device.path).copied();
    let status = if let Some(text) = busy {
        text
    } else if device.connected {
        "Connected"
    } else if device.paired {
        "Paired"
    } else {
        "Available"
    };
    detail.set_text(&if device.name == device.address {
        status.to_string()
    } else {
        format!("{status}  ·  {}", device.address)
    });
    if device.paired {
        let b = button("Forget");
        b.add_css_class("flat");
        bind_forget(
            &b,
            window,
            ui,
            "bluetooth-forget",
            &device.path,
            &device.name,
        );
        row.append(&b);
    }
    let (text, op) = if device.connected {
        ("Disconnect", "bluetooth-disconnect")
    } else if device.paired {
        ("Connect", "bluetooth-connect")
    } else {
        ("Pair", "bluetooth-pair")
    };
    let b = button(text);
    match busy {
        Some(text) => set_busy(&b, text),
        None => ui.bind_busy(&b, op, &device.path),
    }
    row.append(&b);
    row
}
fn empty_state(text: &str) -> gtk::Label {
    let l = label(text, "empty-state");
    l.set_xalign(0.5);
    l.set_justify(gtk::Justification::Center);
    l
}
fn build(
    app: &gtk::Application,
    initial: &str,
    page_rx: async_channel::Receiver<String>,
    catalogue: launcher::Catalogue,
    (server, notice_rx): (
        Rc<notifications::Server>,
        async_channel::Receiver<notifications::Change>,
    ),
) {
    let (initial, initial_query) = initial.split_once('\t').unwrap_or((initial, ""));
    let shell = ui::shell::Surface::new(app);
    let control = ControlCenter::new(app);
    let window = control.window.clone();
    let stack = control.stack.clone();
    let provider = install_theme(
        &gtk::prelude::WidgetExt::display(&shell.window),
        &theme::load(),
    );
    let settings = Rc::new(RefCell::new(theme::load()));
    let (tx, rx) = async_channel::unbounded();
    let ui = Ui {
        tx: tx.clone(),
        settings: settings.clone(),
        updating: Rc::new(Cell::new(false)),
        pending: Rc::default(),
    };

    let wifi = page_box();
    let wifi_scan = button("Scan");
    ui.bind(&wifi_scan, action("wifi-scan", "", ""));
    let wifi_toggle = toggle(&ui, "wifi-power");
    let wifi_scanning = scan_indicator();
    let wifi_header = radio_heading("Wi-Fi", &wifi_scanning, &wifi_scan, &wifi_toggle);
    wifi.append(&wifi_header);
    let wifi_error = error_label();
    wifi.append(&wifi_error);
    let wifi_list = vbox(8);
    wifi.append(&scrolled(&wifi_list));
    stack.add_named(&wifi, Some("wifi"));

    let bt = page_box();
    let bt_scan = button("Scan");
    ui.bind(&bt_scan, action("bluetooth-scan", "", ""));
    let bt_toggle = toggle(&ui, "bluetooth-power");
    let bt_scanning = scan_indicator();
    let bt_header = radio_heading("Bluetooth", &bt_scanning, &bt_scan, &bt_toggle);
    bt.append(&bt_header);
    let bt_error = error_label();
    bt.append(&bt_error);
    let bt_list = vbox(8);
    bt.append(&scrolled(&bt_list));
    stack.add_named(&bt, Some("bluetooth"));

    let sound = page_box();
    sound.append(&heading("Sound", ""));
    let volume = slider(&ui, "volume", "");
    let mute = toggle(&ui, "mute");
    let outputs = audio_dropdown();
    sound.append(&level_card("Output", &volume, Some(&mute), Some(&outputs)));
    let streams = ui::streams::StreamList::new();
    sound.append(&streams.widget);
    let microphone = slider(&ui, "microphone", "");
    let mic_mute = toggle(&ui, "mic-mute");
    let inputs = audio_dropdown();
    sound.append(&level_card(
        "Microphone",
        &microphone,
        Some(&mic_mute),
        Some(&inputs),
    ));
    let audio_error = error_label();
    sound.append(&audio_error);
    // Many playing applications can outgrow the window.
    let sound_scroll = scrolled(&sound);
    sound_scroll.set_propagate_natural_height(true);
    stack.add_named(&sound_scroll, Some("sound"));
    let output_devices = Rc::new(RefCell::new(Vec::<AudioDevice>::new()));
    let input_devices = Rc::new(RefCell::new(Vec::<AudioDevice>::new()));
    for (dropdown, devices, op) in [
        (&outputs, output_devices.clone(), "audio-output"),
        (&inputs, input_devices.clone(), "audio-input"),
    ] {
        let ui = ui.clone();
        dropdown.connect_selected_notify(move |d| {
            if !ui.updating.get()
                && let Some(device) = devices.borrow().get(d.selected() as usize)
            {
                ui.send(action(op, &device.name, ""));
            }
        });
    }

    let display = page_box();
    display.append(&heading("Display", ""));
    let brightness = slider(&ui, "brightness", "");
    display.append(&level_card("Brightness", &brightness, None, None));
    let light_error = error_label();
    display.append(&light_error);
    let night_card = vbox(10);
    night_card.add_css_class("card");
    let night_head = hbox(10);
    let night_title = label("Night light", "row-title");
    night_title.set_hexpand(true);
    night_head.append(&night_title);
    let night_toggle = toggle(&ui, "night-light");
    night_head.append(&night_toggle);
    night_card.append(&night_head);
    let night_temperature = ranged_slider(
        &ui,
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
    display.append(&night_card);
    let night_error = error_label();
    display.append(&night_error);
    let (shortcut, shortcut_detail) = row_card(
        "preferences-desktop-wallpaper-symbolic",
        "Wallpaper & theme",
    );
    shortcut_detail.set_text("Light or dark mode, wallpaper and accent color");
    let go = button("Open");
    let st = stack.clone();
    go.connect_clicked(move |_| st.set_visible_child_name("appearance"));
    shortcut.append(&go);
    display.append(&shortcut);
    stack.add_named(&display, Some("display"));

    let appearance = page_box();
    appearance.append(&heading("Appearance", ""));
    let (mode_row, mode_detail) = row_card("weather-clear-night-symbolic", "Mode");
    mode_detail.set_text("Surfaces for the bar, launcher and panels");
    let segmented = hbox(0);
    segmented.add_css_class("segmented");
    segmented.set_valign(gtk::Align::Center);
    let light = gtk::ToggleButton::with_label("Light");
    let dark = gtk::ToggleButton::with_label("Dark");
    dark.set_group(Some(&light));
    dark.set_active(settings.borrow().mode == "dark");
    light.set_active(settings.borrow().mode != "dark");
    for (b, mode) in [(&light, "light"), (&dark, "dark")] {
        let ui = ui.clone();
        b.connect_toggled(move |b| {
            if b.is_active() && !ui.updating.get() && ui.settings.borrow().mode != mode {
                ui.settings.borrow_mut().mode = mode.into();
                ui.save_theme();
            }
        });
        segmented.append(b);
    }
    mode_row.append(&segmented);
    appearance.append(&mode_row);
    let wallpaper_card = vbox(12);
    wallpaper_card.add_css_class("card");
    let preview = gtk::Picture::new();
    preview.set_can_shrink(true);
    preview.set_content_fit(gtk::ContentFit::Cover);
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("wallpaper-frame");
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_height_request(170);
    preview.set_vexpand(true);
    frame.append(&preview);
    wallpaper_card.append(&frame);
    let wallpaper_row = hbox(12);
    let text = vbox(2);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    text.append(&label("Wallpaper", "row-title"));
    let wallpaper_name = caption("");
    wallpaper_name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    wallpaper_name.set_wrap(false);
    text.append(&wallpaper_name);
    wallpaper_row.append(&text);
    let show_wallpaper = {
        let preview = preview.clone();
        let frame = frame.clone();
        let name = wallpaper_name.clone();
        move |path: Option<&str>| {
            frame.set_visible(path.is_some());
            preview.set_filename(path);
            match path {
                Some(path) => {
                    let file = std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string());
                    name.set_text(&file);
                    name.set_tooltip_text(Some(path));
                }
                None => {
                    name.set_text("Your current wallpaper is kept until you choose an image.");
                    name.set_tooltip_text(None);
                }
            }
        }
    };
    show_wallpaper(settings.borrow().wallpaper.as_deref());
    let choose = button("Choose…");
    choose.add_css_class("suggested-action");
    let w = window.clone();
    let ui2 = ui.clone();
    choose.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder()
            .title("Choose wallpaper")
            .modal(true)
            .build();
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images (PNG, JPEG, WebP)"));
        for mime in ["image/png", "image/jpeg", "image/webp"] {
            filter.add_mime_type(mime);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        let ui = ui2.clone();
        dialog.open(Some(&w), None::<&gio::Cancellable>, move |result| {
            if let Ok(file) = result
                && let Some(path) = file.path()
            {
                ui.settings.borrow_mut().wallpaper = Some(path.to_string_lossy().into());
                ui.save_theme();
            }
        });
    });
    wallpaper_row.append(&choose);
    wallpaper_card.append(&wallpaper_row);
    appearance.append(&wallpaper_card);
    let (colors_row, colors_detail) =
        row_card("applications-graphics-symbolic", "Colors from wallpaper");
    colors_detail.set_text("Use an image-inspired accent across your shell.");
    let colors = gtk::Switch::new();
    colors.set_active(settings.borrow().wallpaper_colors);
    colors.set_valign(gtk::Align::Center);
    let ui2 = ui.clone();
    colors.connect_active_notify(move |s| {
        if !ui2.updating.get() {
            ui2.settings.borrow_mut().wallpaper_colors = s.is_active();
            ui2.save_theme();
        }
    });
    colors_row.append(&colors);
    appearance.append(&colors_row);
    stack.add_named(&appearance, Some("appearance"));
    stack.add_named(&ui::monitor::page(), Some("monitor"));
    stack.set_visible_child_name("wifi");

    // Power stays a full-screen overlay of floating tiles; choices are confirmed
    // inline because a regular dialog would open underneath the overlay layer.
    let power = vbox(20);
    power.add_css_class("power-page");
    let tiles = hbox(18);
    tiles.set_homogeneous(true);
    power.append(&tiles);
    let confirm_row = hbox(12);
    confirm_row.add_css_class("card");
    confirm_row.set_visible(false);
    let confirm_text = label("", "row-title");
    confirm_text.set_hexpand(true);
    confirm_text.set_valign(gtk::Align::Center);
    confirm_row.append(&confirm_text);
    let confirm_cancel = button("Cancel");
    confirm_cancel.add_css_class("flat");
    confirm_row.append(&confirm_cancel);
    let confirm_accept = button("");
    confirm_row.append(&confirm_accept);
    power.append(&confirm_row);
    let power_error = error_label();
    power.append(&power_error);
    let pending_power = Rc::new(Cell::new(None::<&'static str>));
    for (op, title, icon, question, from) in [
        (
            "sleep",
            "Sleep",
            "weather-clear-night-symbolic",
            "Suspend this computer?",
            "from-left",
        ),
        (
            "restart",
            "Restart",
            "view-refresh-symbolic",
            "Restart now? Unsaved work will be lost.",
            "from-bottom",
        ),
        (
            "shutdown",
            "Shut down",
            "system-shutdown-symbolic",
            "Shut down now? Unsaved work will be lost.",
            "from-right",
        ),
    ] {
        let b = gtk::Button::new();
        b.add_css_class("power-tile");
        b.add_css_class(from);
        if op == "shutdown" {
            b.add_css_class("danger");
        }
        let content = vbox(16);
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(40);
        content.append(&image);
        content.append(&gtk::Label::new(Some(title)));
        b.set_child(Some(&content));
        let tiles2 = tiles.clone();
        let pending = pending_power.clone();
        let row = confirm_row.clone();
        let text = confirm_text.clone();
        let accept = confirm_accept.clone();
        let error = power_error.clone();
        b.connect_clicked(move |b| {
            let mut child = tiles2.first_child();
            while let Some(c) = child {
                c.remove_css_class("selected");
                child = c.next_sibling();
            }
            b.add_css_class("selected");
            pending.set(Some(op));
            set_error(&error, None);
            text.set_text(question);
            accept.set_label(title);
            let (add, remove) = if op == "sleep" {
                ("suggested-action", "destructive-action")
            } else {
                ("destructive-action", "suggested-action")
            };
            accept.remove_css_class(remove);
            accept.add_css_class(add);
            row.set_visible(true);
            accept.grab_focus();
        });
        tiles.append(&b);
    }
    let reset_power: Rc<dyn Fn()> = {
        let tiles = tiles.clone();
        let pending = pending_power.clone();
        let row = confirm_row.clone();
        let error = power_error.clone();
        Rc::new(move || {
            let mut child = tiles.first_child();
            while let Some(c) = child {
                c.remove_css_class("selected");
                child = c.next_sibling();
            }
            pending.set(None);
            row.set_visible(false);
            set_error(&error, None);
        })
    };
    // GTK only replays a CSS animation whose name changed, so each opening
    // switches the tiles between two copies of their entrance animations.
    let enter_power = {
        let tiles = tiles.clone();
        let second = Cell::new(false);
        move || {
            let (add, remove) = if second.replace(!second.get()) {
                ("enter-b", "enter-a")
            } else {
                ("enter-a", "enter-b")
            };
            let mut child = tiles.first_child();
            while let Some(c) = child {
                c.remove_css_class(remove);
                c.add_css_class(add);
                child = c.next_sibling();
            }
        }
    };
    let reset = reset_power.clone();
    confirm_cancel.connect_clicked(move |_| reset());
    let ui2 = ui.clone();
    confirm_accept.connect_clicked(move |_| {
        if let Some(op) = pending_power.get() {
            ui2.send(action(op, "", ""));
        }
    });
    shell.content.add_named(&power, Some("power"));

    let shell2 = shell.clone();
    let initial_revision = catalogue.revision;
    let launcher = ui::launcher::Launcher::new(catalogue, Rc::new(move || shell2.hide()));
    shell.content.add_named(&launcher.widget, Some("launcher"));
    let tray_shell = shell.clone();
    let tray = ui::tray::Tray::new(Rc::new(move || tray_shell.hide()));
    shell.content.add_named(&tray.widget, Some("tray"));
    let battery = ui::battery::Battery::new();
    shell.content.add_named(&battery.widget, Some("battery"));
    let battery_lifetime = battery.clone();
    shell.window.connect_destroy(move |_| {
        let _ = &battery_lifetime;
    });
    let center = ui::notifications::Center::new(server.clone());
    let drawer = ui::shell::Drawer::new(app, &center.widget);
    let popups = ui::notifications::Popups::new(app, server.clone());
    let shell2 = shell.clone();
    let drawer2 = drawer.clone();
    let control2 = control.clone();
    let search = launcher.search.clone();
    let (center2, popups2) = (center.clone(), popups.clone());
    let open: ui::shell::Open = Rc::new(move |page, monitor| {
        if page == "notifications" {
            if shell2.window.is_visible() {
                shell2.hide();
            }
            if drawer2.is_open() {
                drawer2.hide();
            } else {
                // Everything the popups show is in the list now.
                popups2.clear();
                center2.refresh();
                drawer2.show(monitor);
            }
            return;
        }
        if drawer2.is_open() {
            drawer2.hide();
        }
        if !["launcher", "power", "tray", "battery"].contains(&page) {
            if shell2.window.is_visible() {
                shell2.hide();
            }
            control2.toggle(page);
            return;
        }
        if shell2.is_open(page) {
            shell2.hide();
            return;
        }
        if page == "power" {
            reset_power();
            enter_power();
        }
        shell2.show(page, monitor);
        if page == "launcher" {
            search.set_text("");
            let search = search.clone();
            glib::idle_add_local_once(move || {
                search.grab_focus();
            });
        }
    });
    let open2 = open.clone();
    control.power.connect_clicked(move |_| open2("power", None));
    let bars = ui::shell::Bars::new(app, open.clone(), shell.clone());
    let tray_bars = bars.clone();
    let tray_shell = shell.clone();
    tray.start(move |count| {
        tray_bars.tray(count);
        if count == 0 && tray_shell.is_open("tray") {
            tray_shell.hide();
        }
    });
    // Keep the host alive for the shell's lifetime.
    let tray_lifetime = tray.clone();
    shell.window.connect_destroy(move |_| {
        let _ = &tray_lifetime;
    });
    let sync_notices: Rc<dyn Fn()> = {
        let (bars, center, server, drawer) =
            (bars.clone(), center.clone(), server.clone(), drawer.clone());
        Rc::new(move || {
            if drawer.is_open() {
                center.refresh();
            }
            bars.notifications(server.count(), center.quiet.is_active());
        })
    };
    {
        let (popups, sync) = (popups.clone(), sync_notices.clone());
        center.quiet.connect_active_notify(move |s| {
            popups.set_quiet(s.is_active());
            sync();
        });
    }
    {
        let (popups, drawer) = (popups.clone(), drawer.clone());
        glib::spawn_future_local(async move {
            while let Ok(change) = notice_rx.recv().await {
                match change {
                    // An open list already shows it.
                    notifications::Change::Posted(n) if !drawer.is_open() => popups.show(&n),
                    notifications::Change::Closed(id) => popups.hide(id, false),
                    _ => {}
                }
                sync_notices();
            }
        });
    }
    let shot = screenshot::Screenshot::new(app, server.clone());
    let record_bars = bars.clone();
    shot.connect_recording(move |started| record_bars.set_recording(started));
    if initial == "screenshot" {
        if let Ok(options) = screenshot::Options::parse(initial_query) {
            shot.take(options);
        }
    } else if initial != "shell" {
        open(initial, None);
        if initial == "launcher" {
            launcher.search.set_text(initial_query);
        }
    }
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let running2 = running.clone();
    app.connect_shutdown(move |_| running2.store(false, std::sync::atomic::Ordering::Relaxed));
    let (workspace_tx, workspace_rx) = async_channel::unbounded();
    ui::workspaces::watch(workspace_tx, running.clone());
    thread::spawn(move || {
        let mut catalogue_revision = initial_revision;
        if let Err(e) = ensure_daemon() {
            let _ = tx.send_blocking(Event::Done(Err(e.to_string())));
        }
        // The battery is slow to read and slow to change: every 7th poll (~5 s).
        let mut tick = 0u32;
        while running.load(std::sync::atomic::Ordering::Relaxed) {
            if tick.is_multiple_of(7) {
                ui::shell::read_battery();
            }
            tick = tick.wrapping_add(1);
            match request(&Request::Status) {
                Ok(reply) => {
                    if let Some(state) = reply.state {
                        if catalogue_revision != state.applications_revision
                            && let Ok(reply) = request(&Request::Applications)
                            && let Some(catalogue) = reply.catalogue
                        {
                            catalogue_revision = catalogue.revision;
                            let _ = tx.send_blocking(Event::Catalogue(Box::new(catalogue)));
                        }
                        if tx.send_blocking(Event::State(Box::new(state))).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send_blocking(Event::Done(Err(e.to_string())));
                }
            }
            thread::sleep(Duration::from_millis(700));
        }
    });
    let mut last_networks = vec![];
    let mut last_devices = vec![];
    let mut first = true;
    let mut last_theme = String::new();
    let mut shown_prompt = None::<(u64, gtk::Window)>;
    let focused_monitor = Rc::new(RefCell::new(None::<gtk::gdk::Monitor>));
    let command_monitor = focused_monitor.clone();
    let command_open = open.clone();
    let command_shell = shell.clone();
    let command_drawer = drawer.clone();
    let command_search = launcher.search.clone();
    glib::spawn_future_local(async move {
        while let Ok(message) = page_rx.recv().await {
            let (page, query) = message.split_once('\t').unwrap_or((&message, ""));
            if page == "close" {
                command_shell.hide();
                command_drawer.hide();
            } else if page == "screenshot" {
                match screenshot::Options::parse(query) {
                    Ok(options) => shot.take(options),
                    Err(e) => eprintln!("{e}"),
                }
            } else if page == "launcher" && !query.is_empty() {
                command_shell.show(page, command_monitor.borrow().as_ref());
                command_search.set_text(query);
                command_search.set_position(-1);
                let entry = command_search.clone();
                glib::idle_add_local_once(move || {
                    entry.grab_focus();
                });
            } else if PAGES.iter().any(|p| p.0 == page)
                || ["launcher", "control", "notifications", "tray", "battery"].contains(&page)
            {
                command_open(page, command_monitor.borrow().as_ref());
            }
        }
    });
    let workspace_bars = bars.clone();
    let workspace_monitor = focused_monitor.clone();
    glib::spawn_future_local(async move {
        while let Ok((ws, monitors)) = workspace_rx.recv().await {
            workspace_bars.workspaces(&ws, &monitors);
            if let Some(name) = monitors
                .as_array()
                .and_then(|all| all.iter().find(|m| m["focused"].as_bool() == Some(true)))
                .and_then(|m| m["name"].as_str())
            {
                *workspace_monitor.borrow_mut() = screenshot::gdk_monitor(name);
            }
        }
    });
    // Events are handled as they arrive, without waking up to poll for them.
    glib::spawn_future_local(async move {
        while let Ok(event) = rx.recv().await {
            match event {
                Event::Done(result) => {
                    if shell.is_open("power") {
                        match &result {
                            Ok(()) => shell.hide(),
                            Err(e) => set_error(&power_error, Some(e)),
                        }
                    }
                    control.report(result);
                }
                Event::Settled(target) => {
                    ui.pending.borrow_mut().remove(&target);
                    // Rebuild the rows with the state that follows.
                    last_networks.clear();
                    last_devices.clear();
                }
                Event::Catalogue(catalogue) => launcher.update(*catalogue),
                Event::State(state) => {
                    let s = *state;
                    bars.update(&s);
                    ui.updating.set(true);
                    wifi_toggle.set_active(s.wifi);
                    wifi_scan.set_sensitive(s.wifi && !s.wifi_scanning);
                    set_scanning(&wifi_scanning, s.wifi_scanning);
                    bt_toggle.set_active(s.bluetooth);
                    bt_scan.set_sensitive(s.bluetooth && !s.bluetooth_scanning);
                    set_scanning(&bt_scanning, s.bluetooth_scanning);
                    set_error(&wifi_error, s.wifi_error.as_deref());
                    set_error(&bt_error, s.bluetooth_error.as_deref());
                    set_error(&audio_error, s.audio_error.as_deref());
                    set_error(&light_error, s.brightness_error.as_deref());
                    set_error(&night_error, s.night_light_error.as_deref());
                    let network = s.networks.iter().find(|n| n.connected());
                    let connected = s.devices.iter().filter(|d| d.connected).count();
                    control.set_detail(
                        "wifi",
                        match network {
                            _ if !s.wifi => "Off",
                            Some(n) => &n.name,
                            None => "On",
                        },
                    );
                    control.set_detail(
                        "bluetooth",
                        &match connected {
                            _ if !s.bluetooth => "Off".to_string(),
                            0 => "On".to_string(),
                            n => format!("{n} connected"),
                        },
                    );
                    control.set_detail(
                        "sound",
                        &match s.volume {
                            _ if s.muted => "Muted".to_string(),
                            Some(v) => format!("{v}%"),
                            None => String::new(),
                        },
                    );
                    control.set_detail(
                        "display",
                        &s.brightness.map(|v| format!("{v}%")).unwrap_or_default(),
                    );
                    control.set_detail(
                        "appearance",
                        if s.settings.mode == "dark" {
                            "Dark"
                        } else {
                            "Light"
                        },
                    );
                    for (slider, value) in [
                        (&volume, s.volume),
                        (&microphone, s.microphone),
                        (&brightness, s.brightness),
                    ] {
                        slider.set_sensitive(value.is_some());
                        if !slider.has_focus()
                            && let Some(v) = value
                        {
                            slider.set_value(f64::from(v));
                        }
                    }
                    let night = s.night_light.clone().unwrap_or_default();
                    night_toggle.set_sensitive(s.night_light.is_some());
                    night_toggle.set_active(night.enabled);
                    night_temperature.set_sensitive(s.night_light.is_some());
                    if !night_temperature.has_focus() && s.night_light.is_some() {
                        night_temperature.set_value(f64::from(night.temperature));
                    }
                    mute.set_active(s.muted);
                    mic_mute.set_active(s.mic_muted);
                    streams.update(&ui, &s.streams);
                    for (dropdown, stored, devices, default) in [
                        (&outputs, &output_devices, &s.outputs, &s.default_output),
                        (&inputs, &input_devices, &s.inputs, &s.default_input),
                    ] {
                        if *stored.borrow() != *devices {
                            let names: Vec<_> =
                                devices.iter().map(|d| d.description.as_str()).collect();
                            dropdown.set_model(Some(&gtk::StringList::new(&names)));
                            *stored.borrow_mut() = devices.clone();
                        }
                        dropdown.set_visible(!devices.is_empty());
                        dropdown.set_selected(
                            devices
                                .iter()
                                .position(|d| d.name == *default)
                                .map(|n| n as u32)
                                .unwrap_or(gtk::INVALID_LIST_POSITION),
                        );
                    }
                    if first || s.networks != last_networks {
                        clear(&wifi_list);
                        if s.networks.is_empty() {
                            wifi_list.append(&empty_state(if s.wifi {
                                "No networks found.\nSelect Scan to look again."
                            } else {
                                "Wi-Fi is off.\nTurn it on to discover networks."
                            }));
                        }
                        for net in &s.networks {
                            wifi_list.append(&network_row(&ui, &window, net));
                        }
                        last_networks = s.networks;
                    }
                    if first || s.devices != last_devices {
                        clear(&bt_list);
                        if s.devices.is_empty() {
                            bt_list.append(&empty_state(if s.bluetooth {
                                "No devices yet.\nSelect Scan to find nearby devices."
                            } else {
                                "Bluetooth is off.\nTurn it on to connect devices."
                            }));
                        }
                        for device in &s.devices {
                            bt_list.append(&device_row(&ui, &window, device));
                        }
                        last_devices = s.devices;
                    }
                    let theme_key = serde_json::to_string(&s.settings).unwrap_or_default();
                    if theme_key != last_theme {
                        if let Some(gtk_settings) = gtk::Settings::default() {
                            gtk_settings.set_gtk_application_prefer_dark_theme(s.settings.dark());
                        }
                        provider.load_from_data(&theme::css(&s.settings));
                        colors.set_active(s.settings.wallpaper_colors);
                        light.set_active(s.settings.mode != "dark");
                        dark.set_active(s.settings.mode == "dark");
                        show_wallpaper(s.settings.wallpaper.as_deref());
                        *settings.borrow_mut() = s.settings;
                        last_theme = theme_key;
                    }
                    let prompt_id = s.prompt.as_ref().map(|p| p.id);
                    if shown_prompt.as_ref().map(|p| p.0) != prompt_id {
                        if let Some((_, dialog)) = shown_prompt.take() {
                            dialog.close();
                        }
                        if let Some(prompt) = s.prompt {
                            let dialog = show_prompt(&ui, &prompt);
                            shown_prompt = Some((prompt.id, dialog));
                        }
                    }
                    first = false;
                    ui.updating.set(false);
                }
            }
        }
        glib::ControlFlow::Continue
    });
}
fn bind_forget(
    button: &gtk::Button,
    parent: &gtk::ApplicationWindow,
    ui: &Ui,
    op: &str,
    path: &str,
    name: &str,
) {
    let parent = parent.clone();
    let ui = ui.clone();
    let req = action(op, path, "");
    let name = name.to_string();
    button.connect_clicked(move |_| {
        let dialog = gtk::AlertDialog::builder()
            .message(format!("Forget {name}?"))
            .detail("Saved credentials or pairing will be removed.")
            .buttons(["Cancel", "Forget"])
            .cancel_button(0)
            .default_button(0)
            .modal(true)
            .build();
        let ui = ui.clone();
        let req = req.clone();
        dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
            if matches!(result, Ok(1)) {
                ui.send(req);
            }
        });
    });
}
/// Credential and pairing prompts may arrive while no Nexus window is open, so
/// they use their own overlay surface with exclusive keyboard focus.
fn show_prompt(ui: &Ui, p: &Prompt) -> gtk::Window {
    let dialog = gtk::Window::builder()
        .title(&p.title)
        .default_width(420)
        .build();
    dialog.add_css_class("nexus");
    dialog.add_css_class("prompt-window");
    dialog.init_layer_shell();
    dialog.set_namespace(Some("nexus-prompt"));
    dialog.set_layer(Layer::Overlay);
    dialog.set_keyboard_mode(KeyboardMode::Exclusive);
    dialog.set_exclusive_zone(-1);
    let content = vbox(16);
    content.add_css_class("prompt");
    content.append(&heading(&p.title, &p.detail));
    let mut entries = vec![];
    for field in &p.fields {
        let group = vbox(6);
        group.append(&caption(field));
        let entry = gtk::Entry::new();
        entry.set_visibility(!matches!(field.as_str(), "Passphrase" | "Password" | "PIN"));
        group.append(&entry);
        content.append(&group);
        entries.push((field.clone(), entry));
    }
    let row = hbox(10);
    row.set_halign(gtk::Align::End);
    row.set_margin_top(6);
    let cancel = button(if p.display_only { "Close" } else { "Cancel" });
    cancel.add_css_class("flat");
    row.append(&cancel);
    let accept = button(if p.fields.is_empty() {
        "Confirm"
    } else {
        "Connect"
    });
    accept.add_css_class("suggested-action");
    if !p.display_only {
        row.append(&accept);
    }
    content.append(&row);
    dialog.set_child(Some(&content));
    for (_, entry) in &entries {
        let accept = accept.clone();
        entry.connect_activate(move |_| accept.emit_clicked());
    }
    if let Some((_, entry)) = entries.first() {
        let entry = entry.clone();
        glib::idle_add_local_once(move || {
            entry.grab_focus();
        });
    }
    let id = p.id;
    let answered = Rc::new(Cell::new(false));
    let ui2 = ui.clone();
    let d = dialog.clone();
    let answered2 = answered.clone();
    accept.connect_clicked(move |_| {
        answered2.set(true);
        let values = entries
            .iter()
            .map(|(k, v)| (k.clone(), v.text().to_string()))
            .collect();
        ui2.send(Request::Answer {
            id,
            values,
            accepted: true,
        });
        for (_, e) in &entries {
            e.set_text("");
        }
        d.close();
    });
    let d = dialog.clone();
    cancel.connect_clicked(move |_| d.close());
    let key = gtk::EventControllerKey::new();
    let d = dialog.clone();
    key.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            d.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    dialog.add_controller(key);
    let ui = ui.clone();
    dialog.connect_close_request(move |_| {
        if !answered.get() {
            ui.send(Request::Answer {
                id,
                values: Default::default(),
                accepted: false,
            });
        }
        glib::Propagation::Proceed
    });
    dialog.present();
    dialog
}
