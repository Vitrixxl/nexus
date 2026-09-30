mod ui;
use gtk::{gio, glib, prelude::*};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use nexus_control::*;
use std::{
    cell::{Cell, RefCell},
    io::Write,
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};

const PAGES: [(&str, &str, &str); 6] = [
    ("wifi", "Wi-Fi", "network-wireless-symbolic"),
    ("bluetooth", "Bluetooth", "bluetooth-symbolic"),
    ("sound", "Sound", "audio-volume-high-symbolic"),
    ("display", "Display", "display-brightness-symbolic"),
    (
        "appearance",
        "Appearance",
        "preferences-desktop-wallpaper-symbolic",
    ),
    ("power", "Power", "system-shutdown-symbolic"),
];
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!(
            "Nexus — desktop control center\n\nnexus [launcher|wifi|bluetooth|sound|display|appearance|power|shell]\nnexus --page PAGE\nnexus status\nnexus init-theme"
        );
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "init-theme") {
        theme::write_theme(&theme::load())?;
        return Ok(());
    }
    ensure_daemon()?;
    if args.first().is_some_and(|a| a == "status") {
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
    if !PAGES.iter().any(|p| p.0 == page) && !["launcher", "shell", "close"].contains(&page) {
        anyhow::bail!("Unknown page: {page}");
    }
    let activation =
        if page == "launcher" && args.first().is_some_and(|s| s == "launcher") && args.len() > 1 {
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
    let catalogue = request(&Request::Applications)?
        .catalogue
        .ok_or_else(|| anyhow::anyhow!("Restart nexusd to load the application catalogue"))?;
    // NON_UNIQUE supports Artix sessions without a session D-Bus. Our private socket provides single-instance activation.
    let app = gtk::Application::builder()
        .application_id("io.github.vitrixxl.Nexus")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let page = activation;
    let page_rx = Rc::new(RefCell::new(Some(page_rx)));
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        if let Some(rx) = page_rx.borrow_mut().take() {
            build(app, &page, rx, catalogue.clone());
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
fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}
fn hbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, spacing)
}
fn button(text: &str) -> gtk::Button {
    gtk::Button::with_label(text)
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
#[derive(Clone)]
struct Ui {
    tx: mpsc::Sender<Event>,
    settings: Rc<RefCell<theme::Settings>>,
    updating: Rc<Cell<bool>>,
}
enum Event {
    State(Box<Snapshot>),
    Catalogue(Box<launcher::Catalogue>),
    Done(Result<(), String>),
}
impl Ui {
    fn send(&self, req: Request) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = request(&req).map_err(|e| e.to_string()).and_then(|r| {
                if r.ok {
                    Ok(())
                } else {
                    Err(r.error.unwrap_or_else(|| "Operation failed".into()))
                }
            });
            let _ = tx.send(Event::Done(result));
            if let Ok(reply) = request(&Request::Status)
                && let Some(state) = reply.state
            {
                let _ = tx.send(Event::State(Box::new(state)));
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
    let b = vbox(8);
    b.append(&label(title, "title"));
    b.append(&label(subtitle, "muted"));
    b
}
fn radio_header(title: &str, toggle: &gtk::Switch, scan: &gtk::Button) -> gtk::Box {
    let b = hbox(12);
    b.add_css_class("card");
    let l = label(title, "row-title");
    l.set_hexpand(true);
    b.append(&l);
    b.append(scan);
    b.append(toggle);
    b
}
fn page_box() -> gtk::Box {
    let b = vbox(22);
    b.set_margin_top(32);
    b.set_margin_bottom(28);
    b.set_margin_start(32);
    b.set_margin_end(32);
    b
}
fn slider(ui: &Ui, title: &str, op: &str) -> (gtk::Box, gtk::Scale) {
    let b = vbox(12);
    b.add_css_class("card");
    b.append(&label(title, "row-title"));
    let s = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        if op == "brightness" { 1. } else { 0. },
        100.,
        1.,
    );
    s.set_draw_value(true);
    s.set_value_pos(gtk::PositionType::Right);
    s.set_hexpand(true);
    s.set_digits(0);
    b.append(&s);
    let pending = Rc::new(RefCell::new(None::<glib::SourceId>));
    let ui = ui.clone();
    let op = op.to_owned();
    s.connect_value_changed(move |s| {
        if ui.updating.get() {
            return;
        }
        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }
        let value = s.value().round().to_string();
        let ui = ui.clone();
        let op = op.clone();
        let pending2 = pending.clone();
        *pending.borrow_mut() = Some(glib::timeout_add_local_once(
            Duration::from_millis(180),
            move || {
                pending2.borrow_mut().take();
                ui.send(action(&op, "", &value));
            },
        ));
    });
    (b, s)
}
fn audio_dropdown() -> gtk::DropDown {
    let dropdown = gtk::DropDown::from_strings(&[]);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(42);
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
fn build(
    app: &gtk::Application,
    initial: &str,
    page_rx: async_channel::Receiver<String>,
    catalogue: launcher::Catalogue,
) {
    let (initial, initial_query) = initial.split_once('\t').unwrap_or((initial, ""));
    let shell = ui::shell::Surface::new(app);
    let window = shell.window.clone();
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&theme::css(&theme::load()));
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(&window),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let settings = Rc::new(RefCell::new(theme::load()));
    let (tx, rx) = mpsc::channel();
    let ui = Ui {
        tx: tx.clone(),
        settings: settings.clone(),
        updating: Rc::new(Cell::new(false)),
    };
    let root = hbox(0);
    root.set_size_request(860, 650);
    let sidebar = vbox(8);
    sidebar.add_css_class("sidebar");
    sidebar.set_width_request(172);
    sidebar.append(&label("nexus", "brand"));
    let subtitle = label("CONTROL CENTER", "eyebrow");
    subtitle.set_margin_bottom(30);
    sidebar.append(&subtitle);
    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);
    stack.set_vexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(140);
    let mut navs = vec![];
    for (id, title, icon) in PAGES {
        let b = gtk::Button::new();
        let row = hbox(10);
        row.append(&gtk::Image::from_icon_name(icon));
        row.append(&label(title, ""));
        b.set_child(Some(&row));
        let st = stack.clone();
        let shell = shell.clone();
        let sidebar_nav = sidebar.clone();
        let root = root.clone();
        b.connect_clicked(move |_| {
            sidebar_nav.set_visible(id != "power");
            root.set_size_request(860, if id == "power" { 430 } else { 650 });
            st.set_visible_child_name(id);
            shell.show(id, None);
        });
        sidebar.append(&b);
        navs.push((id, b));
    }
    let spacer = vbox(0);
    spacer.set_vexpand(true);
    sidebar.append(&spacer);
    sidebar.append(&label("A little more in control.", "muted"));
    let content = vbox(0);
    content.set_hexpand(true);
    content.append(&stack);
    let status = label("", "muted");
    status.set_margin_start(32);
    status.set_margin_end(32);
    status.set_margin_bottom(12);
    content.append(&status);
    root.append(&sidebar);
    root.append(&content);
    shell.content.add_named(&root, Some("settings"));
    let wifi = page_box();
    wifi.append(&heading("Wi-Fi", "Your networks, one connection away."));
    let wifi_toggle = toggle(&ui, "wifi-power");
    let scan = button("Refresh");
    ui.bind(&scan, action("wifi-scan", "", ""));
    wifi.append(&radio_header("Wireless", &wifi_toggle, &scan));
    let wifi_error = label("", "error");
    wifi.append(&wifi_error);
    let wifi_list = vbox(8);
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&wifi_list)
        .build();
    wifi.append(&scroll);
    stack.add_named(&wifi, Some("wifi"));
    let bt = page_box();
    bt.append(&heading(
        "Bluetooth",
        "Make room for your favorite devices.",
    ));
    let bt_toggle = toggle(&ui, "bluetooth-power");
    let scan = button("Find devices");
    ui.bind(&scan, action("bluetooth-scan", "", ""));
    bt.append(&radio_header("Bluetooth", &bt_toggle, &scan));
    let bt_error = label("", "error");
    bt.append(&bt_error);
    let bt_list = vbox(8);
    bt.append(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&bt_list)
            .build(),
    );
    stack.add_named(&bt, Some("bluetooth"));
    let sound = page_box();
    sound.append(&heading("Sound", "Set the right level."));
    let (b, volume) = slider(&ui, "Output volume", "volume");
    let mute = toggle(&ui, "mute");
    let mute_row = hbox(12);
    mute_row.append(&label("Mute output", "muted"));
    mute_row.append(&mute);
    b.append(&mute_row);
    let outputs = audio_dropdown();
    b.append(&outputs);
    sound.append(&b);
    let (b, microphone) = slider(&ui, "Microphone", "microphone");
    let mic_mute = toggle(&ui, "mic-mute");
    let row = hbox(12);
    row.append(&label("Mute microphone", "muted"));
    row.append(&mic_mute);
    b.append(&row);
    let inputs = audio_dropdown();
    b.append(&inputs);
    sound.append(&b);
    let audio_error = label("", "error");
    sound.append(&audio_error);
    stack.add_named(&sound, Some("sound"));
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
    display.append(&heading("Display", "A comfortable view, day or night."));
    let (b, brightness) = slider(&ui, "Brightness", "brightness");
    display.append(&b);
    let light_error = label("", "error");
    display.append(&light_error);
    let appearance_shortcut = button("Wallpaper & theme →");
    let st = stack.clone();
    appearance_shortcut.connect_clicked(move |_| st.set_visible_child_name("appearance"));
    display.append(&appearance_shortcut);
    stack.add_named(&display, Some("display"));
    let appearance = page_box();
    appearance.append(&heading("Appearance", "Make yourself at home."));
    let themes = hbox(12);
    for (mode, title) in [("light", "☀  Light"), ("dark", "☾  Dark")] {
        let b = button(title);
        b.set_hexpand(true);
        let ui = ui.clone();
        b.connect_clicked(move |_| {
            ui.settings.borrow_mut().mode = mode.into();
            ui.save_theme();
        });
        themes.append(&b);
    }
    appearance.append(&themes);
    let preview = gtk::Picture::new();
    preview.set_can_shrink(true);
    preview.set_content_fit(gtk::ContentFit::Cover);
    preview.set_height_request(190);
    if let Some(path) = &settings.borrow().wallpaper {
        preview.set_filename(Some(path));
    }
    appearance.append(&preview);
    let wallpaper_name = label(
        settings
            .borrow()
            .wallpaper
            .as_deref()
            .unwrap_or("Your current wallpaper is kept until you choose an image."),
        "muted",
    );
    wallpaper_name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    wallpaper_name.set_wrap(false);
    appearance.append(&wallpaper_name);
    let choose = button("Choose wallpaper…");
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
        w.set_keyboard_mode(KeyboardMode::OnDemand);
        let focus_window = w.clone();
        dialog.open(Some(&w), None::<&gio::Cancellable>, move |result| {
            focus_window.set_keyboard_mode(KeyboardMode::Exclusive);
            if let Ok(file) = result
                && let Some(path) = file.path()
            {
                ui.settings.borrow_mut().wallpaper = Some(path.to_string_lossy().into());
                ui.save_theme();
            }
        });
    });
    appearance.append(&choose);
    let colors = gtk::Switch::new();
    colors.set_active(settings.borrow().wallpaper_colors);
    colors.set_valign(gtk::Align::Center);
    let row = hbox(16);
    row.add_css_class("card");
    let text = vbox(6);
    text.set_hexpand(true);
    text.append(&label("Colors from wallpaper", "row-title"));
    text.append(&label(
        "Use an image-inspired accent across your shell.",
        "muted",
    ));
    row.append(&text);
    row.append(&colors);
    let ui2 = ui.clone();
    colors.connect_active_notify(move |s| {
        if !ui2.updating.get() {
            ui2.settings.borrow_mut().wallpaper_colors = s.is_active();
            ui2.save_theme();
        }
    });
    appearance.append(&row);
    stack.add_named(&appearance, Some("appearance"));
    let power = page_box();
    power.set_valign(gtk::Align::Center);
    power.set_halign(gtk::Align::Center);
    power.append(&heading("Take a break.", "What would you like to do?"));
    let tiles = hbox(18);
    for (op, title, icon) in [
        ("sleep", "Sleep", "weather-clear-night-symbolic"),
        ("restart", "Restart", "view-refresh-symbolic"),
        ("shutdown", "Shutdown", "system-shutdown-symbolic"),
    ] {
        let b = gtk::Button::new();
        b.add_css_class("power-tile");
        if op == "shutdown" {
            b.add_css_class("danger");
        }
        let content = vbox(20);
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(48);
        content.append(&image);
        let text = gtk::Label::new(Some(title));
        content.append(&text);
        b.set_child(Some(&content));
        let w = window.clone();
        let ui = ui.clone();
        b.connect_clicked(move |_| {
            confirm(
                &w,
                &format!("{title} this computer?"),
                if op == "sleep" {
                    "Your session will be suspended."
                } else {
                    "Save your work before continuing."
                },
                &ui,
                action(op, "", ""),
            );
        });
        tiles.append(&b);
    }
    power.append(&tiles);
    let back = button("Back to desktop · Esc");
    let w = window.clone();
    back.connect_clicked(move |_| w.close());
    power.append(&back);
    stack.add_named(&power, Some("power"));
    let shell2 = shell.clone();
    let initial_revision = catalogue.revision;
    let launcher = ui::launcher::Launcher::new(catalogue, Rc::new(move || shell2.hide()));
    shell.content.add_named(&launcher.widget, Some("launcher"));
    let shell2 = shell.clone();
    let stack2 = stack.clone();
    let search = launcher.search.clone();
    let sidebar2 = sidebar.clone();
    let root2 = root.clone();
    let open: ui::shell::Open = Rc::new(move |page, monitor| {
        if shell2.window.is_visible() && shell2.page.borrow().as_str() == page {
            shell2.hide();
            return;
        }
        sidebar2.set_visible(page != "power");
        root2.set_size_request(860, if page == "power" { 430 } else { 650 });
        if page != "launcher" {
            stack2.set_visible_child_name(page);
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
    let bars = ui::shell::Bars::new(app, open.clone());
    if initial != "shell" {
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
        while running.load(std::sync::atomic::Ordering::Relaxed) {
            match request(&Request::Status) {
                Ok(reply) => {
                    if let Some(state) = reply.state {
                        if catalogue_revision != state.applications_revision
                            && let Ok(reply) = request(&Request::Applications)
                            && let Some(catalogue) = reply.catalogue
                        {
                            catalogue_revision = catalogue.revision;
                            let _ = tx.send(Event::Catalogue(Box::new(catalogue)));
                        }
                        if tx.send(Event::State(Box::new(state))).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Event::Done(Err(e.to_string())));
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
    let command_search = launcher.search.clone();
    glib::spawn_future_local(async move {
        while let Ok(message) = page_rx.recv().await {
            let (page, query) = message.split_once('\t').unwrap_or((&message, ""));
            if page == "close" {
                command_shell.hide();
            } else if page == "launcher" && !query.is_empty() {
                command_shell.show(page, command_monitor.borrow().as_ref());
                command_search.set_text(query);
                command_search.set_position(-1);
                let entry = command_search.clone();
                glib::idle_add_local_once(move || {
                    entry.grab_focus();
                });
            } else if PAGES.iter().any(|p| p.0 == page) || page == "launcher" {
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
                let list = gtk::gdk::Display::default().unwrap().monitors();
                *workspace_monitor.borrow_mut() = (0..list.n_items())
                    .filter_map(|i| list.item(i).and_downcast::<gtk::gdk::Monitor>())
                    .find(|m| m.connector().as_deref() == Some(name));
            }
        }
    });
    glib::timeout_add_local(Duration::from_millis(100), move || {
        for (id, b) in &navs {
            if stack.visible_child_name().as_deref() == Some(*id) {
                b.add_css_class("nav-active");
            } else {
                b.remove_css_class("nav-active");
            }
        }
        for event in rx.try_iter() {
            match event {
                Event::Done(result) => match result {
                    Ok(()) => {
                        status.remove_css_class("error");
                        status.set_text("Done");
                    }
                    Err(e) => {
                        status.add_css_class("error");
                        status.set_text(&e);
                    }
                },
                Event::Catalogue(catalogue) => launcher.update(*catalogue),
                Event::State(state) => {
                    let s = *state;
                    bars.update(&s);
                    ui.updating.set(true);
                    wifi_toggle.set_active(s.wifi);
                    bt_toggle.set_active(s.bluetooth);
                    wifi_error.set_text(s.wifi_error.as_deref().unwrap_or(""));
                    bt_error.set_text(s.bluetooth_error.as_deref().unwrap_or(""));
                    audio_error.set_text(s.audio_error.as_deref().unwrap_or(""));
                    light_error.set_text(s.brightness_error.as_deref().unwrap_or(""));
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
                    mute.set_active(s.muted);
                    mic_mute.set_active(s.mic_muted);
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
                            wifi_list.append(&label(
                                if s.wifi {
                                    "No networks found. Try Refresh."
                                } else {
                                    "Turn on Wi-Fi to discover networks."
                                },
                                "muted",
                            ));
                        }
                        for net in &s.networks {
                            let row = hbox(12);
                            row.add_css_class("card");
                            let text = vbox(5);
                            text.set_hexpand(true);
                            text.append(&label(&net.name, "row-title"));
                            text.append(&label(
                                &format!(
                                    "{}%  ·  {}  ·  {}",
                                    net.signal,
                                    if net.security == "none" {
                                        "Open"
                                    } else {
                                        &net.security
                                    },
                                    net.state
                                ),
                                "muted",
                            ));
                            row.append(&text);
                            let connected = matches!(net.state.as_str(), "ready" | "online");
                            let b = button(if connected { "Disconnect" } else { "Connect" });
                            ui.bind(
                                &b,
                                action(
                                    if connected {
                                        "wifi-disconnect"
                                    } else {
                                        "wifi-connect"
                                    },
                                    &net.path,
                                    "",
                                ),
                            );
                            row.append(&b);
                            if net.saved {
                                let b = button("Forget");
                                bind_forget(&b, &window, &ui, "wifi-forget", &net.path, &net.name);
                                row.append(&b);
                            }
                            wifi_list.append(&row);
                        }
                        last_networks = s.networks;
                    }
                    if first || s.devices != last_devices {
                        clear(&bt_list);
                        if s.devices.is_empty() {
                            bt_list.append(&label(
                                "No devices yet. Turn on Bluetooth and select Find devices.",
                                "muted",
                            ));
                        }
                        for device in &s.devices {
                            let row = hbox(12);
                            row.add_css_class("card");
                            let text = vbox(5);
                            text.set_hexpand(true);
                            text.append(&label(&device.name, "row-title"));
                            text.append(&label(
                                &format!(
                                    "{} · {}",
                                    device.address,
                                    if device.connected {
                                        "Connected"
                                    } else if device.paired {
                                        "Paired"
                                    } else {
                                        "Available"
                                    }
                                ),
                                "muted",
                            ));
                            row.append(&text);
                            let b = button(if device.connected {
                                "Disconnect"
                            } else if device.paired {
                                "Connect"
                            } else {
                                "Pair"
                            });
                            ui.bind(
                                &b,
                                action(
                                    if device.connected {
                                        "bluetooth-disconnect"
                                    } else if device.paired {
                                        "bluetooth-connect"
                                    } else {
                                        "bluetooth-pair"
                                    },
                                    &device.path,
                                    "",
                                ),
                            );
                            row.append(&b);
                            if device.paired {
                                let b = button("Forget");
                                bind_forget(
                                    &b,
                                    &window,
                                    &ui,
                                    "bluetooth-forget",
                                    &device.path,
                                    &device.name,
                                );
                                row.append(&b);
                            }
                            bt_list.append(&row);
                        }
                        last_devices = s.devices;
                    }
                    let theme_key = serde_json::to_string(&s.settings).unwrap_or_default();
                    if theme_key != last_theme {
                        if let Some(gtk_settings) = gtk::Settings::default() {
                            gtk_settings
                                .set_gtk_application_prefer_dark_theme(s.settings.mode == "dark");
                        }
                        provider.load_from_data(&theme::css(&s.settings));
                        colors.set_active(s.settings.wallpaper_colors);
                        if let Some(path) = &s.settings.wallpaper {
                            preview.set_filename(Some(path));
                            wallpaper_name.set_text(path);
                        }
                        *settings.borrow_mut() = s.settings;
                        last_theme = theme_key;
                    }
                    let prompt_id = s.prompt.as_ref().map(|p| p.id);
                    if shown_prompt.as_ref().map(|p| p.0) != prompt_id {
                        if let Some((_, dialog)) = shown_prompt.take() {
                            dialog.close();
                        }
                        if let Some(prompt) = s.prompt {
                            let dialog = show_prompt(&window, &ui, &prompt);
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
        confirm(
            &parent,
            &format!("Forget {name}?"),
            "Saved credentials or pairing will be removed.",
            &ui,
            req.clone(),
        );
    });
}
fn confirm(parent: &gtk::ApplicationWindow, title: &str, detail: &str, ui: &Ui, req: Request) {
    let dialog = gtk::AlertDialog::builder()
        .message(title)
        .detail(detail)
        .buttons(["Cancel", "Confirm"])
        .cancel_button(0)
        .default_button(0)
        .modal(true)
        .build();
    let ui = ui.clone();
    parent.set_keyboard_mode(KeyboardMode::OnDemand);
    let focus_window = parent.clone();
    dialog.choose(Some(parent), None::<&gio::Cancellable>, move |result| {
        focus_window.set_keyboard_mode(KeyboardMode::Exclusive);
        if matches!(result, Ok(1)) {
            ui.send(req);
        }
    });
}
fn show_prompt(parent: &gtk::ApplicationWindow, ui: &Ui, p: &Prompt) -> gtk::Window {
    let dialog = gtk::Window::builder()
        .title(&p.title)
        .transient_for(parent)
        .modal(true)
        .default_width(440)
        .build();
    dialog.add_css_class("nexus");
    dialog.init_layer_shell();
    dialog.set_namespace(Some("nexus-prompt"));
    dialog.set_layer(Layer::Overlay);
    dialog.set_monitor(parent.monitor().as_ref());
    dialog.set_keyboard_mode(KeyboardMode::Exclusive);
    dialog.set_exclusive_zone(-1);
    let content = page_box();
    content.append(&heading(&p.title, &p.detail));
    let mut entries = vec![];
    for field in &p.fields {
        content.append(&label(field, "row-title"));
        let entry = gtk::Entry::new();
        entry.set_visibility(!matches!(field.as_str(), "Passphrase" | "Password" | "PIN"));
        content.append(&entry);
        entries.push((field.clone(), entry));
    }
    let row = hbox(12);
    let cancel = button(if p.display_only { "Close" } else { "Cancel" });
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
