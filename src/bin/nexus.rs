mod lock;
mod phonectl;
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

/// What `nexus PAGE` may name: the window's pages, the panels, and what unfolds in
/// Control Center.
const PAGES: [&str; 17] = [
    "wifi",
    "bluetooth",
    "sound",
    "display",
    "appearance",
    "phone",
    "monitor",
    "packages",
    "power",
    "launcher",
    "control",
    "notifications",
    "tray",
    "battery",
    "quick",
    "system",
    "clipboard",
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
            "Nexus — desktop control center\n\nnexus [launcher|clipboard|tray|battery|control|monitor|notifications|wifi|bluetooth|phone|sound|display|appearance|power|shell]\nnexus --page PAGE\nnexus screenshot [region|screen|window] [--live] [--save]\nnexus lock [--preview]\nnexus messages\nnexus passwords\nnexus paint\nnexus status\nnexus init-theme\n\n{}",
            phonectl::USAGE
        );
        return Ok(());
    }
    // Bare `nexus phone` opens the page; with a command, it talks to the phone.
    if args.len() == 2 && args[0] == "phone" && args[1] == "mirror" {
        return ui::mirror::run();
    }
    if args.first().is_some_and(|a| a == "phone") && args.len() > 1 {
        return phonectl::run(&args[1..]);
    }
    // The lock runs on its own, so that it holds whatever happens to the shell.
    if args.first().is_some_and(|a| a == "lock") {
        return lock::run(&args[1..]);
    }
    // An application of its own, beside the shell.
    if args.first().is_some_and(|a| a == "messages") {
        return ui::messages::run();
    }
    if args.first().is_some_and(|a| a == "call") {
        return ui::call::run(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "passwords") {
        return ui::passwords::run();
    }
    if args.first().is_some_and(|a| a == "paint") {
        return ui::paint::run();
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
    if !PAGES.contains(&page) && !["screenshot", "shell", "close"].contains(&page) {
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
/// A spinner that turns only while it is on screen. Spinning, it redraws its
/// window every frame, even from a page of the control center that is not
/// shown: a scan in the background kept the whole window repainting.
/// Started once mapped: started while its window maps, it stays still.
fn spinner() -> gtk::Spinner {
    let spinner = gtk::Spinner::new();
    spinner.connect_map(|s| {
        let s = s.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(s) = s.upgrade().filter(|s| s.is_mapped()) {
                s.stop();
                s.start();
            }
        });
    });
    spinner.connect_unmap(|s| s.stop());
    spinner
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
    /// Has the state polled at once, then quickly for a moment.
    nudge: mpsc::Sender<()>,
    settings: Rc<RefCell<theme::Settings>>,
    updating: Rc<Cell<bool>>,
    /// A material slider moved and is not saved yet: state read now is older than it.
    adjusting: Rc<Cell<bool>>,
    /// Operations awaiting the daemon, by network or device with the text
    /// their button shows meanwhile, or by toggle with no text.
    pending: Rc<RefCell<HashMap<String, &'static str>>>,
    /// Passwords of the saved networks shown unfolded, by network.
    passwords: Rc<RefCell<HashMap<String, String>>>,
}
enum Event {
    State(Box<Snapshot>),
    Catalogue(Box<launcher::Catalogue>),
    Done(Result<(), String>),
    /// What the power overlay's confirmation answered, and only that.
    Power(Result<(), String>),
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
                STATE_WANTED.store(true, std::sync::atomic::Ordering::Relaxed);
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
            ui.pending.borrow_mut().insert(op.clone(), "");
            ui.run(
                action(&op, "", if s.is_active() { "true" } else { "false" }),
                Some(op.clone()),
            );
        }
    });
    s
}
/// Shows the daemon's `value` on the toggle for `op`. Not while the user's own
/// change is on its way, which a state read before it would undo; and never
/// the value it already has: setting it stops the switch's slide midway, the
/// knob stranded and the change never made.
fn sync_toggle(ui: &Ui, s: &gtk::Switch, op: &str, value: bool) {
    if s.is_active() != value && !ui.pending.borrow().contains_key(op) {
        s.set_active(value);
    }
}
/// An action has settled: the next state goes to the UI even if nothing changed,
/// for it to take back what it showed ahead of the result.
static STATE_WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Sound and brightness change by keys the compositor handles, and the overlay
/// showing it should not lag behind them. Sound announces its changes, and so does
/// the backlight, through `poll` on its `actual_brightness`: the kernel flags that
/// file on any change made through sysfs (brightnessctl, logind) or by a hotkey.
fn watch_levels(nudge: mpsc::Sender<()>, brightness: async_channel::Sender<u8>) {
    thread::spawn(move || {
        loop {
            if let Ok(mut child) = std::process::Command::new("pactl")
                .arg("subscribe")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                if let Some(out) = child.stdout.take() {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                        if line.contains("on sink #") && nudge.send(()).is_err() {
                            let _ = child.kill();
                            let _ = child.wait();
                            return;
                        }
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
    thread::spawn(move || {
        use std::{
            io::{Read, Seek},
            os::fd::AsRawFd,
        };
        let open = || {
            std::fs::read_dir("/sys/class/backlight")
                .ok()
                .and_then(|dir| dir.flatten().map(|e| e.path()).min())
                .and_then(|device| std::fs::File::open(device.join("actual_brightness")).ok())
        };
        let mut last = None;
        let mut flag = open();
        let mut woken = false;
        // A machine without a backlight only ever pays for a slow retry.
        loop {
            // Reading the file is what arms it, so before the level: a change after
            // this read wakes the poll below at once.
            let unreadable = flag.as_mut().is_some_and(|file| {
                file.rewind()
                    .and_then(|_| file.read_to_end(&mut vec![]))
                    .is_err()
            });
            let Ok(value) = backend::brightness() else {
                woken = false;
                thread::sleep(Duration::from_secs(10));
                flag = open();
                continue;
            };
            let changed = last.replace(value) != Some(value);
            if changed && brightness.send_blocking(value).is_err() {
                return;
            }
            if flag_is_stale(unreadable, woken, changed) {
                flag = open();
                woken = false;
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            // Still a look every second, for a driver that does not flag its changes.
            match &flag {
                Some(file) => {
                    let mut fd = libc::pollfd {
                        fd: file.as_raw_fd(),
                        events: libc::POLLPRI,
                        revents: 0,
                    };
                    // SAFETY: one valid pollfd, for the duration of the call.
                    woken = unsafe { libc::poll(&mut fd, 1, 1000) } != 0;
                }
                None => {
                    thread::sleep(Duration::from_secs(1));
                    flag = open();
                }
            }
        }
    });
}
/// A flag that cannot be read, or that wakes the poll with nothing changed, likely
/// belongs to a device that went away: poll would then return at once, forever.
fn flag_is_stale(unreadable: bool, woken: bool, changed: bool) -> bool {
    unreadable || (woken && !changed)
}
/// The page to open for `page`, and what unfolds in Control Center if it is one of
/// those: from the bar and from their commands alike.
fn panel_page(page: &str) -> (&str, Option<&str>) {
    let name = page.strip_prefix("quick-").unwrap_or(page);
    if ["wifi", "bluetooth", "sound", "display"].contains(&name) {
        ("quick", Some(name))
    } else {
        (page, None)
    }
}
/// A choice among a few, one always on. `chosen` hears the user's choices alone.
fn segmented(
    ui: &Ui,
    choices: &[(&'static str, &str)],
    chosen: impl Fn(&'static str) + Clone + 'static,
) -> (gtk::Box, Vec<(&'static str, gtk::ToggleButton)>) {
    let root = hbox(0);
    root.add_css_class("segmented");
    root.set_valign(gtk::Align::Center);
    let mut buttons: Vec<(&'static str, gtk::ToggleButton)> = vec![];
    for (id, name) in choices {
        let b = gtk::ToggleButton::with_label(name);
        if let Some((_, first)) = buttons.first() {
            b.set_group(Some(first));
        }
        let (id, ui, chosen) = (*id, ui.clone(), chosen.clone());
        b.connect_toggled(move |b| {
            if b.is_active() && !ui.updating.get() {
                chosen(id);
            }
        });
        root.append(&b);
        buttons.push((id, b));
    }
    (root, buttons)
}
/// A titled block of settings on one surface; returns where its rows go.
fn group(page: &gtk::Box, title: &str) -> gtk::Box {
    let head = hbox(0);
    let name = label(title, "group-title");
    name.set_hexpand(true);
    head.append(&name);
    page.append(&head);
    let rows = vbox(0);
    rows.add_css_class("group");
    rows.set_overflow(gtk::Overflow::Hidden);
    page.append(&rows);
    rows
}
/// One setting: its name and at most a line about it. Its control is appended.
fn row(group: &gtk::Box, title: &str, about: &str) -> gtk::Box {
    let row = hbox(12);
    row.add_css_class("row");
    let text = vbox(1);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let name = label(title, "row-title");
    name.set_wrap(false);
    text.append(&name);
    if !about.is_empty() {
        text.append(&caption(about));
    }
    row.append(&text);
    group.append(&row);
    row
}
/// The opacity of what floats. It shows at once while dragging, and is saved when
/// the slider has been still for a moment.
fn opacity_slider(ui: &Ui, provider: &gtk::CssProvider) -> gtk::Scale {
    let scale = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        f64::from(theme::MIN_OPACITY),
        100.,
        1.,
    );
    scale.set_size_request(260, -1);
    scale.set_valign(gtk::Align::Center);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_format_value_func(|_, v| format!("{v:.0}%"));
    let ui = ui.clone();
    let provider = provider.clone();
    let pending = Rc::new(RefCell::new(None::<glib::SourceId>));
    let painting = Rc::new(Cell::new(false));
    scale.connect_value_changed(move |scale| {
        if ui.updating.get() {
            return;
        }
        ui.settings.borrow_mut().opacity = Some(scale.value().round() as u8);
        // At most a repaint every 33 ms however fast the thumb moves.
        if !painting.replace(true) {
            let (ui, provider, painting) = (ui.clone(), provider.clone(), painting.clone());
            glib::timeout_add_local_once(Duration::from_millis(33), move || {
                painting.set(false);
                provider.load_from_data(&theme::css(&ui.settings.borrow()));
            });
        }
        if let Some(old) = pending.take() {
            old.remove();
        }
        ui.adjusting.set(true);
        let (saving, slot) = (ui.clone(), pending.clone());
        *pending.borrow_mut() = Some(glib::timeout_add_local_once(
            Duration::from_millis(250),
            move || {
                slot.take();
                saving.adjusting.set(false);
                saving.save_theme();
            },
        ));
    });
    scale
}
/// The Appearance page and what it shows of the settings.
struct Appearance {
    widget: gtk::ScrolledWindow,
    modes: Vec<(&'static str, gtk::ToggleButton)>,
    styles: Vec<(&'static str, gtk::ToggleButton)>,
    opacity: gtk::Scale,
    opacity_row: gtk::Box,
    accents: Vec<(&'static str, gtk::Button)>,
    colors: gtk::Switch,
    locks: Vec<(&'static str, gtk::ToggleButton)>,
    thumbnail: gtk::Picture,
    wallpaper_name: gtk::Label,
    /// Decoding it takes long enough to be left alone until it changes.
    shown_wallpaper: RefCell<Option<Option<String>>>,
}
impl Appearance {
    fn new(ui: &Ui, provider: &gtk::CssProvider, window: &gtk::ApplicationWindow) -> Self {
        let page = page_box();
        page.set_spacing(0);
        page.append(&label("Appearance", "title"));

        let theme_rows = group(&page, "Theme");
        let (choice, modes) = {
            let ui2 = ui.clone();
            segmented(
                ui,
                &[("light", "Light"), ("dark", "Dark")],
                move |mode: &'static str| {
                    if ui2.settings.borrow().mode != mode {
                        ui2.settings.borrow_mut().mode = mode.into();
                        ui2.save_theme();
                    }
                },
            )
        };
        row(&theme_rows, "Mode", "").append(&choice);
        let (choice, styles) = {
            let ui2 = ui.clone();
            let choices: Vec<_> = theme::STYLES.iter().map(|s| (s.id, s.name)).collect();
            segmented(ui, &choices, move |id: &'static str| {
                let mut settings = ui2.settings.borrow_mut();
                if settings.style != id {
                    settings.style = id.into();
                    settings.opacity = None;
                    drop(settings);
                    ui2.save_theme();
                }
            })
        };
        row(
            &theme_rows,
            "Material",
            "The bar, dock and panels. Windows stay solid.",
        )
        .append(&choice);
        let opacity = opacity_slider(ui, provider);
        let opacity_row = row(&theme_rows, "Opacity", "");
        opacity_row.append(&opacity);

        let color_rows = group(&page, "Wallpaper and Color");
        let wallpaper_row = row(&color_rows, "Wallpaper", "");
        let thumbnail = gtk::Picture::new();
        thumbnail.set_can_shrink(true);
        thumbnail.set_content_fit(gtk::ContentFit::Cover);
        thumbnail.set_size_request(96, 60);
        thumbnail.add_css_class("wallpaper-thumb");
        thumbnail.set_overflow(gtk::Overflow::Hidden);
        wallpaper_row.prepend(&thumbnail);
        let wallpaper_name = caption("");
        wallpaper_name.set_wrap(false);
        wallpaper_name.set_xalign(0.);
        wallpaper_name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        wallpaper_name.set_max_width_chars(1);
        if let Some(text) = thumbnail.next_sibling().and_downcast::<gtk::Box>() {
            text.append(&wallpaper_name);
        }
        let choose = button("Choose…");
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

        let colors = gtk::Switch::new();
        colors.set_valign(gtk::Align::Center);
        let ui2 = ui.clone();
        colors.connect_active_notify(move |s| {
            if !ui2.updating.get() {
                ui2.settings.borrow_mut().wallpaper_colors = s.is_active();
                ui2.save_theme();
            }
        });
        row(
            &color_rows,
            "Match Wallpaper",
            "Take the accent from the image.",
        )
        .append(&colors);
        let accent_row = row(&color_rows, "Accent", "");
        let dots = hbox(10);
        dots.set_valign(gtk::Align::Center);
        let mut accents = vec![];
        for (i, (name, color)) in theme::ACCENTS.iter().enumerate() {
            let b = gtk::Button::new();
            b.add_css_class("accent-dot");
            b.add_css_class(&format!("accent-{i}"));
            b.set_tooltip_text(Some(name));
            b.update_property(&[gtk::accessible::Property::Label(name)]);
            b.set_valign(gtk::Align::Center);
            let ui = ui.clone();
            b.connect_clicked(move |_| {
                {
                    let mut settings = ui.settings.borrow_mut();
                    settings.accent = (*color).into();
                    settings.wallpaper_colors = false;
                }
                ui.save_theme();
            });
            dots.append(&b);
            accents.push((*color, b));
        }
        accent_row.append(&dots);

        let lock_rows = group(&page, "Lock Screen");
        let mut locks: Vec<(&'static str, gtk::ToggleButton)> = vec![];
        for (id, name, about) in theme::LOCK_STYLES.iter() {
            let b = gtk::ToggleButton::new();
            b.add_css_class("lock-choice");
            if let Some((_, first)) = locks.first() {
                b.set_group(Some(first));
            }
            let line = hbox(12);
            let text = vbox(1);
            text.set_hexpand(true);
            let title = label(name, "lock-name");
            title.set_wrap(false);
            text.append(&title);
            text.append(&caption(about));
            line.append(&text);
            let tick = gtk::Image::from_icon_name("object-select-symbolic");
            tick.add_css_class("lock-tick");
            line.append(&tick);
            b.set_child(Some(&line));
            let (id, ui) = (*id, ui.clone());
            b.connect_toggled(move |b| {
                if b.is_active() && !ui.updating.get() && ui.settings.borrow().lock_style != id {
                    ui.settings.borrow_mut().lock_style = id.into();
                    ui.save_theme();
                }
            });
            lock_rows.append(&b);
            locks.push((id, b));
        }
        // Beside the group's title, where the choices leave it out of their way.
        let preview = button("Preview");
        preview.add_css_class("flat");
        preview.add_css_class("group-action");
        preview.set_tooltip_text(Some("Escape closes the preview"));
        preview.connect_clicked(|_| ui::shell::run_nexus(&["lock", "--preview"]));
        if let Some(head) = lock_rows.prev_sibling().and_downcast::<gtk::Box>() {
            head.append(&preview);
        }

        Self {
            widget: scrolled(&page),
            modes,
            styles,
            opacity,
            opacity_row,
            accents,
            colors,
            locks,
            thumbnail,
            wallpaper_name,
            shown_wallpaper: RefCell::default(),
        }
    }
    /// Call with `ui.updating` set so programmatic changes are not sent back.
    fn show(&self, settings: &theme::Settings) {
        let pick = |choices: &[(&'static str, gtk::ToggleButton)], id: &str| {
            if let Some((_, b)) = choices.iter().find(|(choice, _)| *choice == id) {
                b.set_active(true);
            }
        };
        pick(&self.modes, if settings.dark() { "dark" } else { "light" });
        pick(&self.styles, settings.style().id);
        pick(&self.locks, &settings.lock_style);
        self.opacity_row.set_visible(settings.style().id == "blur");
        if !self.opacity.has_focus() {
            self.opacity.set_value(f64::from(settings.opacity()));
        }
        self.colors.set_active(settings.wallpaper_colors);
        for (color, b) in &self.accents {
            if !settings.wallpaper_colors && settings.accent == *color {
                b.add_css_class("selected");
            } else {
                b.remove_css_class("selected");
            }
        }
        let wallpaper = settings.wallpaper.as_deref();
        if self.shown_wallpaper.borrow().as_ref().map(|w| w.as_deref()) != Some(wallpaper) {
            self.show_wallpaper(wallpaper);
            *self.shown_wallpaper.borrow_mut() = Some(wallpaper.map(str::to_owned));
        }
    }
    /// Off the main thread: decoding a 4K image would stall every animation.
    fn show_wallpaper(&self, path: Option<&str>) {
        self.thumbnail.set_paintable(None::<&gtk::gdk::Paintable>);
        self.wallpaper_name.set_tooltip_text(path);
        let Some(path) = path else {
            self.wallpaper_name
                .set_text("The desktop keeps its wallpaper until you choose one.");
            return;
        };
        let file = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        self.wallpaper_name.set_text(&file);
        let (tx, rx) = async_channel::bounded(1);
        let path = path.to_owned();
        thread::spawn(move || {
            let image = image::ImageReader::open(path)
                .ok()
                .and_then(|r| r.with_guessed_format().ok())
                .and_then(|r| r.decode().ok())
                .map(|image| image.thumbnail(320, 200).to_rgba8());
            let _ = tx.send_blocking(image);
        });
        let thumbnail = self.thumbnail.downgrade();
        glib::spawn_future_local(async move {
            if let (Ok(Some(image)), Some(thumbnail)) = (rx.recv().await, thumbnail.upgrade()) {
                let (width, height) = image.dimensions();
                let texture = gtk::gdk::MemoryTexture::new(
                    width as i32,
                    height as i32,
                    gtk::gdk::MemoryFormat::R8g8b8a8,
                    &glib::Bytes::from_owned(image.into_raw()),
                    width as usize * 4,
                );
                thumbnail.set_paintable(Some(&texture));
            }
        });
    }
}
/// What the mark at the start of the bar opens, where power now lives too.
fn system_menu(open: ui::shell::Open, close: Rc<dyn Fn()>) -> gtk::Box {
    let menu = vbox(0);
    menu.add_css_class("system-menu");
    menu.set_width_request(220);
    let entry = |title: &str, icon: &str| {
        let b = gtk::Button::new();
        let row = hbox(10);
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(16);
        row.append(&image);
        let text = label(title, "");
        text.set_wrap(false);
        row.append(&text);
        b.set_child(Some(&row));
        b
    };
    for (title, icon, page) in [
        ("Settings", "emblem-system-symbolic", "control"),
        (
            "Appearance",
            "preferences-desktop-wallpaper-symbolic",
            "appearance",
        ),
        ("Monitor", "power-profile-performance-symbolic", "monitor"),
    ] {
        let b = entry(title, icon);
        let open = open.clone();
        b.connect_clicked(move |_| open(page, None));
        menu.append(&b);
    }
    menu.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let lock = entry("Lock Screen", "system-lock-screen-symbolic");
    lock.connect_clicked(move |_| {
        close();
        ui::shell::run_nexus(&["lock"]);
    });
    menu.append(&lock);
    let power = entry("Sleep, Restart, Shut Down", "system-shutdown-symbolic");
    power.connect_clicked(move |_| open("power", None));
    menu.append(&power);
    menu
}
fn heading(title: &str, subtitle: &str) -> gtk::Box {
    let b = vbox(4);
    b.append(&label(title, "title"));
    if !subtitle.is_empty() {
        b.append(&label(subtitle, "muted"));
    }
    b
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
    let (nudge, nudge_rx) = mpsc::channel::<()>();
    let ui = Ui {
        tx: tx.clone(),
        nudge: nudge.clone(),
        settings: settings.clone(),
        updating: Rc::new(Cell::new(false)),
        adjusting: Rc::default(),
        pending: Rc::default(),
        passwords: Rc::default(),
    };

    let appearance = Appearance::new(&ui, &provider, &window);
    ui.updating.set(true);
    appearance.show(&settings.borrow());
    ui.updating.set(false);
    stack.add_named(&appearance.widget, Some("appearance"));
    let monitor_page = page_box();
    monitor_page.append(&label("Monitor", "title"));
    monitor_page.append(&ui::monitor::page());
    stack.add_named(&monitor_page, Some("monitor"));
    let packages = ui::packages::Packages::new();
    stack.add_named(&packages.widget, Some("packages"));
    // Its widgets only hold on to it weakly.
    window.connect_destroy(move |_| {
        let _ = &packages;
    });
    let phone_page = ui::phone::Page::new();
    stack.add_named(&phone_page.widget, Some("phone"));
    stack.set_visible_child_name("appearance");

    // Power stays a full-screen overlay of floating tiles; choices are confirmed
    // inline because a regular dialog would open underneath the overlay layer.
    let power = vbox(20);
    power.add_css_class("power-page");
    let tiles = hbox(12);
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
        let content = vbox(10);
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(28);
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
    let power_tx = ui.tx.clone();
    confirm_accept.connect_clicked(move |_| {
        if let Some(op) = pending_power.get() {
            let tx = power_tx.clone();
            thread::spawn(move || {
                let _ = tx.send_blocking(Event::Power(perform(&action(op, "", ""))));
            });
        }
    });
    shell.content.add_named(&power, Some("power"));

    let shell2 = shell.clone();
    let initial_revision = catalogue.revision;
    let launcher_catalogue = catalogue.clone();
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
    let closing = drawer.clone();
    *center.close.borrow_mut() = Rc::new(move || closing.hide());
    let d = drawer.clone();
    phone_page.on_leave(move || d.hide());
    let popups = ui::notifications::Popups::new(app, server.clone(), gtk4_layer_shell::Edge::Right);
    let phone_popups =
        ui::notifications::Popups::new(app, server.clone(), gtk4_layer_shell::Edge::Left);
    let shell2 = shell.clone();
    let drawer2 = drawer.clone();
    let control2 = control.clone();
    let search = launcher.search.clone();
    let (center2, popups2, phone_popups2) = (center.clone(), popups.clone(), phone_popups.clone());
    // Nothing opens over a prompt: on the same layer, it would hide it.
    let prompting = Rc::new(Cell::new(false));
    let prompt_open = prompting.clone();
    let open: ui::shell::Open = Rc::new(move |page, monitor| {
        if prompt_open.get() {
            return;
        }
        let (page, from_quick) = page
            .strip_suffix(":quick")
            .map_or((page, false), |page| (page, true));
        let (page, unfold) = panel_page(page);
        if page == "notifications" {
            if shell2.window.is_visible() {
                shell2.hide();
            }
            if drawer2.is_open() {
                drawer2.hide();
            } else {
                // Everything the popups show is in the list now.
                popups2.clear();
                phone_popups2.clear();
                center2.today();
                center2.offer_back(from_quick, monitor);
                center2.refresh();
                if let Some(m) = monitor.cloned().or_else(|| {
                    gtk::gdk::Display::default()
                        .and_then(|d| d.monitors().item(0))
                        .and_downcast()
                }) {
                    center2.fit(m.geometry().height());
                }
                drawer2.show(monitor);
            }
            return;
        }
        if drawer2.is_open() {
            drawer2.hide();
        }
        if !["launcher", "power", "tray", "battery", "quick", "system"].contains(&page) {
            if shell2.window.is_visible() {
                shell2.hide();
            }
            control2.toggle(page);
            return;
        }
        if page == "quick"
            && let Some(quick) = shell2
                .content
                .child_by_name("quick")
                .and_downcast::<gtk::Stack>()
        {
            let open_now = shell2.is_open("quick");
            // The same command again closes it; another page's slides over.
            if open_now && (unfold.is_none() || quick.visible_child_name().as_deref() == unfold) {
                shell2.hide();
                return;
            }
            quick.set_transition_type(if open_now {
                gtk::StackTransitionType::SlideLeft
            } else {
                gtk::StackTransitionType::None
            });
            quick.set_visible_child_name(unfold.unwrap_or("main"));
        } else if shell2.is_open(page) {
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
            // An empty search stays empty, and the selection where it was left.
            search.emit_by_name::<()>("changed", &[]);
            let search = search.clone();
            glib::idle_add_local_once(move || {
                search.grab_focus();
            });
        }
    });
    let back = Rc::downgrade(&open);
    *center.go_back.borrow_mut() = Rc::new(move |monitor| {
        if let Some(open) = back.upgrade() {
            open("quick", monitor);
        }
    });
    let open2 = open.clone();
    control.power.connect_clicked(move |_| open2("power", None));
    let close_shell: Rc<dyn Fn()> = {
        let shell = shell.clone();
        Rc::new(move || shell.hide())
    };
    let quick =
        ui::quick::Quick::new_shared(&ui, open.clone(), center.quiet.clone(), close_shell.clone());
    shell.content.add_named(&quick.widget, Some("quick"));
    shell
        .content
        .add_named(&system_menu(open.clone(), close_shell), Some("system"));
    let bars = ui::shell::Bars::new(app, open.clone(), shell.clone());
    bars.catalogue(&launcher_catalogue);
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
        let (popups, phone_popups, sync) =
            (popups.clone(), phone_popups.clone(), sync_notices.clone());
        center.quiet.connect_active_notify(move |s| {
            popups.set_quiet(s.is_active());
            phone_popups.set_quiet(s.is_active());
            sync();
        });
    }
    {
        let (popups, phone_popups, drawer) = (popups.clone(), phone_popups.clone(), drawer.clone());
        glib::spawn_future_local(async move {
            while let Ok(change) = notice_rx.recv().await {
                match change {
                    // An open list already shows it.
                    notifications::Change::Posted(n) if !drawer.is_open() => {
                        if n.phone { &phone_popups } else { &popups }.show(&n)
                    }
                    notifications::Change::Closed(id) => {
                        popups.hide(id, false);
                        phone_popups.hide(id, false);
                    }
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
    let (brightness_tx, brightness_rx) = async_channel::unbounded();
    watch_levels(nudge, brightness_tx);
    thread::spawn(move || {
        let mut burst = 0u32;
        let mut catalogue_revision = initial_revision;
        if let Err(e) = ensure_daemon() {
            let _ = tx.send_blocking(Event::Done(Err(e.to_string())));
        }
        // The battery is slow to read and slow to change: every 5th poll.
        let mut tick = 0u32;
        // What the UI last got, and the polls since: it wakes up only for a change,
        // and every few seconds for what the state alone does not decide, like a
        // slider that has just let go of the focus.
        let mut sent = vec![];
        let mut unsent = 0u32;
        while running.load(std::sync::atomic::Ordering::Relaxed) {
            if tick.is_multiple_of(5) {
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
                        let seen =
                            serde_json::to_vec(&(&state, ui::shell::battery())).unwrap_or_default();
                        unsent += 1;
                        if seen != sent
                            || unsent >= 5
                            || STATE_WANTED.swap(false, std::sync::atomic::Ordering::Relaxed)
                        {
                            sent = seen;
                            unsent = 0;
                            if tx.send_blocking(Event::State(Box::new(state))).is_err() {
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send_blocking(Event::Done(Err(e.to_string())));
                }
            }
            // A burst after a change, for the state that follows; otherwise at ease.
            if nudge_rx
                .recv_timeout(Duration::from_millis(if burst > 0 { 140 } else { 1000 }))
                .is_ok()
            {
                while nudge_rx.try_recv().is_ok() {}
                burst = 3;
            } else {
                burst = burst.saturating_sub(1);
            }
        }
    });
    let levels_bars = bars.clone();
    glib::spawn_future_local(async move {
        while let Ok(value) = brightness_rx.recv().await {
            levels_bars.brightness(value);
        }
    });
    let mut last_theme = String::new();
    let mut shown_prompt = None::<(u64, gtk::Window)>;
    let mut panel_behind = None::<glib::GString>;
    let prompt_return = open.clone();
    let focused_monitor = Rc::new(RefCell::new(None::<gtk::gdk::Monitor>));
    let command_monitor = focused_monitor.clone();
    let clipboard = ui::clipboard::Clipboard::new(app);
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
            } else if page == "clipboard" {
                clipboard.toggle();
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
            } else if PAGES.contains(&page) {
                command_open(page, command_monitor.borrow().as_ref());
            }
        }
    });
    let workspace_bars = bars.clone();
    let workspace_monitor = focused_monitor.clone();
    glib::spawn_future_local(async move {
        while let Ok(state) = workspace_rx.recv().await {
            workspace_bars.workspaces(&state);
            if let Some(name) = state
                .1
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
                Event::Power(result) => {
                    if shell.is_open("power") {
                        match &result {
                            Ok(()) => shell.hide(),
                            Err(e) => set_error(&power_error, Some(e)),
                        }
                    }
                }
                Event::Done(result) => {
                    quick.report(&result);
                    control.report(result);
                }
                Event::Settled(target) => {
                    ui.pending.borrow_mut().remove(&target);
                    // Rebuild the rows with the state that follows.
                    quick.refresh();
                }
                Event::Catalogue(catalogue) => {
                    bars.catalogue(&catalogue);
                    launcher.update(*catalogue);
                }
                Event::State(state) => {
                    let s = *state;
                    bars.update(&s);
                    ui.updating.set(true);
                    quick.update(&s);
                    phone_page.update(&s.phone);
                    center.calendar.update(&s.phone.calendar, drawer.is_open());
                    let theme_key = serde_json::to_string(&s.settings).unwrap_or_default();
                    if theme_key != last_theme && !ui.adjusting.get() {
                        if let Some(gtk_settings) = gtk::Settings::default() {
                            gtk_settings.set_gtk_application_prefer_dark_theme(s.settings.dark());
                        }
                        provider.load_from_data(&theme::css(&s.settings));
                        appearance.show(&s.settings);
                        *settings.borrow_mut() = s.settings;
                        last_theme = theme_key;
                    }
                    let prompt_id = s.prompt.as_ref().map(|p| p.id);
                    if shown_prompt.as_ref().map(|p| p.0) != prompt_id {
                        if let Some((_, dialog)) = shown_prompt.take() {
                            dialog.close();
                        }
                        prompting.set(s.prompt.is_some());
                        if let Some(prompt) = s.prompt {
                            // A password asked for from a panel would open beneath it:
                            // the panel steps aside, and comes back once it is answered.
                            if shell.is_open("quick") {
                                panel_behind = quick.widget.visible_child_name();
                            }
                            shell.hide();
                            let dialog = show_prompt(&ui, &prompt);
                            shown_prompt = Some((prompt.id, dialog));
                        } else if let Some(page) = panel_behind.take() {
                            prompt_return(if page == "main" { "quick" } else { &page }, None);
                        }
                    }
                    ui.updating.set(false);
                }
            }
        }
        glib::ControlFlow::Continue
    });
}
/// The password line under a saved network: the secret, and a copy button.
fn password_line(password: Result<&str, &str>) -> gtk::Box {
    let line = hbox(8);
    line.add_css_class("wifi-password");
    let icon = gtk::Image::from_icon_name("dialog-password-symbolic");
    icon.set_pixel_size(14);
    icon.add_css_class("muted");
    line.append(&icon);
    match password {
        Ok(password) => {
            let secret = label(password, "secret");
            secret.set_selectable(true);
            secret.set_wrap(false);
            secret.set_ellipsize(gtk::pango::EllipsizeMode::End);
            line.append(&secret);
            let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
            copy.add_css_class("flat");
            copy.add_css_class("copy");
            copy.set_tooltip_text(Some("Copy"));
            copy.set_valign(gtk::Align::Center);
            let password = password.to_owned();
            copy.connect_clicked(move |b| {
                b.clipboard().set_text(&password);
                b.set_icon_name("object-select-symbolic");
                b.set_tooltip_text(Some("Copied"));
                let b = b.downgrade();
                glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                    if let Some(b) = b.upgrade() {
                        b.set_icon_name("edit-copy-symbolic");
                        b.set_tooltip_text(Some("Copy"));
                    }
                });
            });
            line.append(&copy);
        }
        Err(e) => line.append(&caption(e)),
    }
    line
}
fn toggle_password(ui: &Ui, text: &gtk::Box, id: &str) {
    if let Some(line) = text
        .last_child()
        .filter(|w| w.has_css_class("wifi-password"))
    {
        text.remove(&line);
        ui.passwords.borrow_mut().remove(id);
        return;
    }
    let req = action("wifi-password", id, "");
    let (tx, rx) = async_channel::bounded(1);
    thread::spawn(move || {
        let r = request(&req)
            .map_err(|e| e.to_string())
            .and_then(|r| match r.ok {
                true => Ok(r
                    .data
                    .and_then(|d| d.as_str().map(String::from))
                    .unwrap_or_default()),
                false => Err(r.error.unwrap_or_default()),
            });
        let _ = tx.send_blocking(r);
    });
    let (ui, text, id) = (ui.clone(), text.downgrade(), id.to_owned());
    glib::spawn_future_local(async move {
        let (Ok(result), Some(text)) = (rx.recv().await, text.upgrade()) else {
            return;
        };
        if text
            .last_child()
            .is_some_and(|w| w.has_css_class("wifi-password"))
        {
            return;
        }
        if let Ok(password) = &result {
            ui.passwords.borrow_mut().insert(id, password.clone());
        }
        text.append(&password_line(result.as_deref().map_err(String::as_str)));
    });
}
/// Whether an answer to a prompt's field is worth sending. A WPA password has 8 to 63
/// characters, or 64 hexadecimal digits.
// ponytail: the prompt doesn't say WEP from WPA, so the 5 of a WEP key is let through (the
// daemon refuses it for WPA) and a WEP passphrase of 1-4, 6 or 7 is not; a field of its own
// in `Prompt` if anyone is still on one.
fn answer_valid(field: &str, value: &str) -> bool {
    let n = value.len();
    match field {
        "Passphrase" => {
            n == 5
                || (8..=63).contains(&n)
                || (n == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        }
        _ => n > 0,
    }
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
    let mut entries: Vec<(String, gtk::Editable)> = vec![];
    for field in &p.fields {
        let group = vbox(6);
        // "Passphrase" is the daemon's key for the answer; people say password.
        group.append(&caption(if field == "Passphrase" {
            "Password"
        } else {
            field
        }));
        let entry: gtk::Editable = if matches!(field.as_str(), "Passphrase" | "Password" | "PIN") {
            let entry = gtk::PasswordEntry::new();
            entry.set_show_peek_icon(true);
            entry.set_activates_default(true);
            entry.upcast()
        } else {
            let entry = gtk::Entry::new();
            entry.set_activates_default(true);
            entry.upcast()
        };
        group.append(&entry);
        if field == "Passphrase" {
            group.append(&caption("At least 8 characters"));
        }
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
    if !p.display_only {
        // Enter does nothing while it is insensitive.
        dialog.set_default_widget(Some(&accept));
    }
    accept.set_sensitive(entries.iter().all(|(k, e)| answer_valid(k, &e.text())));
    // Weakly: an entry's handler holding the entries would outlive the dialog, secret included.
    let weak = Rc::new(
        entries
            .iter()
            .map(|(k, e)| (k.clone(), e.downgrade()))
            .collect::<Vec<_>>(),
    );
    for (_, entry) in &entries {
        let (weak, accept) = (weak.clone(), accept.downgrade());
        entry.connect_changed(move |_| {
            if let Some(accept) = accept.upgrade() {
                accept.set_sensitive(
                    weak.iter()
                        .all(|(k, e)| e.upgrade().is_some_and(|e| answer_valid(k, &e.text()))),
                );
            }
        });
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
#[cfg(test)]
mod tests {
    use super::{answer_valid, flag_is_stale, panel_page};
    #[test]
    fn prompt_answers_are_checked_before_sending() {
        for bad in ["", "1234567", &"z".repeat(64), &"f".repeat(65)] {
            assert!(!answer_valid("Passphrase", bad), "{bad}");
        }
        for good in ["12345", "12345678", &"a".repeat(63), &"f".repeat(64)] {
            assert!(answer_valid("Passphrase", good), "{good}");
        }
        assert!(!answer_valid("Identity", ""));
        assert!(answer_valid("PIN", "0"));
    }
    #[test]
    fn a_flag_that_keeps_waking_for_nothing_is_replaced() {
        assert!(flag_is_stale(true, false, false));
        assert!(flag_is_stale(false, true, false));
        // A real change, and the poll simply timing out.
        assert!(!flag_is_stale(false, true, true));
        assert!(!flag_is_stale(false, false, false));
    }
    #[test]
    fn moved_pages_unfold_in_control_center() {
        for page in ["wifi", "bluetooth", "sound", "display"] {
            assert_eq!(panel_page(page), ("quick", Some(page)));
            assert_eq!(panel_page(&format!("quick-{page}")).1, Some(page));
        }
        assert_eq!(panel_page("quick"), ("quick", None));
        assert_eq!(panel_page("appearance"), ("appearance", None));
    }
}
