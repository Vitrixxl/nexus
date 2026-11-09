//! Nexus Files: a small file manager. One resident process serves every
//! window, `org.freedesktop.FileManager1` and the FileChooser portal backend,
//! so later windows and file dialogs open without starting anything.
mod browser;
mod listing;
mod ops;
mod portal;
mod thumbs;
use browser::Browser;
use gtk::{gio, glib, prelude::*};
use nexus_control::theme;
use ops::Loc;

const APP_ID: &str = "io.github.vitrixxl.NexusFiles";
const FILE_MANAGER: &str = r#"<node><interface name="org.freedesktop.FileManager1">
<method name="ShowFolders"><arg type="as" direction="in"/><arg type="s" direction="in"/></method>
<method name="ShowItems"><arg type="as" direction="in"/><arg type="s" direction="in"/></method>
<method name="ShowItemProperties"><arg type="as" direction="in"/><arg type="s" direction="in"/></method>
</interface></node>"#;
/// Styles of the file views, on top of the shared Nexus stylesheet.
const CSS: &str = r#"
.files .files-sidebar { background: alpha(@nexus_fg, 0.045); border-right: 1px solid alpha(@nexus_fg, 0.06); }
.files list.files-sidebar { padding: 10px 8px; }
.files .files-sidebar row { border-radius: 8px; padding: 4px 8px; margin: 1px 0; color: alpha(@nexus_fg, 0.85); min-height: 0; }
.files .files-sidebar row:hover { background: alpha(@nexus_fg, 0.05); color: @nexus_fg; }
.files .files-sidebar row:selected { background: alpha(@nexus_fg, 0.08); color: @nexus_fg; font-weight: 600; }
.files .files-sidebar .place-icon { color: @nexus_muted; min-width: 24px; min-height: 24px; }
.files .files-sidebar row:selected .place-icon { color: @nexus_accent_fg; }
.files .files-sidebar .sidebar-section { font-size: 11px; font-weight: 600; color: @nexus_muted; padding: 14px 8px 4px; }
.files .files-header { padding: 8px 10px; }
.files .files-header > * { margin: 0; }
.files .files-header button, .files .files-header menubutton > button { min-width: 32px; min-height: 32px; padding: 0; border-radius: 8px; background: transparent; color: alpha(@nexus_fg, 0.8); }
.files .files-header button:hover, .files .files-header menubutton > button:hover { background: alpha(@nexus_fg, 0.08); color: @nexus_fg; }
.files .files-header button:checked { background: alpha(@nexus_accent, 0.18); color: @nexus_accent_fg; }
.files .files-header button:disabled { opacity: 0.35; background: transparent; }
.files .files-header entry { min-height: 32px; padding: 0 10px; border-radius: 8px; }
.files .files-header entry.path { margin: 0 6px; }
.files gridview, .files listview { background: transparent; padding: 6px 10px; }
.files gridview > child { padding: 10px 4px 12px; border-radius: 12px; }
.files .band { color: @nexus_accent; }
.files listview > row { min-height: 36px; padding: 0 8px; border-radius: 8px; }
.files gridview > child:hover, .files listview > row:hover { background: alpha(@nexus_fg, 0.05); }
.files gridview > child:selected, .files listview > row:selected { background: alpha(@nexus_accent, 0.16); color: @nexus_fg; }
.files .grid-cell label { font-size: 13px; }
.files .tile { background: alpha(@nexus_fg, 0.08); color: @nexus_muted; border-radius: 12px; }
.files .list-cell .tile { border-radius: 8px; }
.files .tile.folder { background: alpha(@nexus_accent, 0.16); color: @nexus_accent_fg; }
.files :selected .tile.folder { background: alpha(@nexus_accent, 0.28); }
.files .file-meta { color: @nexus_muted; font-size: 12px; font-feature-settings: "tnum"; }
.files .footer { padding: 8px 12px; border-top: 1px solid alpha(@nexus_fg, 0.05); }
.files .footer button { min-height: 32px; padding: 0 16px; border-radius: 8px; }
.files .footer entry { min-height: 32px; padding: 0 10px; border-radius: 8px; }
.files .footer dropdown > button { min-height: 32px; padding: 0 10px; }
.files .footer .error { color: @nexus_error; }
.files rubberband { background: alpha(@nexus_accent, 0.15); border: 1px solid @nexus_accent; border-radius: 6px; }
.files .prompt { padding: 20px; }
"#;

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.add_main_option(
        "phone",
        glib::Char::from(b'p'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Browse the paired phone, at PATH if given",
        None,
    );
    // Started by D-Bus for the portal or FileManager1: claim the names before
    // GTK starts, since GTK's start asks xdg-desktop-portal for settings while
    // xdg-desktop-portal may itself be waiting for these names.
    let early = std::env::args().any(|a| a == "--gapplication-service");
    if early
        && let Ok(connection) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
    {
        serve(connection);
    }
    APP.set(Some(app.clone()));
    app.connect_startup(move |app| {
        // Resident: the next window, or file dialog, is only a D-Bus call away.
        std::mem::forget(app.hold());
        install_theme();
        for (action, accels) in [
            ("win.back", &["<Alt>Left"][..]),
            ("win.forward", &["<Alt>Right"]),
            ("win.up", &["<Alt>Up"]),
            ("win.location", &["<Control>l"]),
            ("win.hidden", &["<Control>h"]),
            ("win.new-folder", &["<Control><Shift>n"]),
            ("win.grid", &["<Control>g"]),
            ("win.new-window", &["<Control>n"]),
            ("window.close", &["<Control>w"]),
        ] {
            app.set_accels_for_action(action, accels);
        }
        if !early && let Some(connection) = app.dbus_connection() {
            serve(connection);
        }
    });
    app.connect_command_line(|app, line| {
        let args: Vec<_> = line.arguments().into_iter().skip(1).collect();
        if line.options_dict().contains("phone") {
            let path = args.first().map(|a| a.to_string_lossy().into_owned());
            Browser::open(app, &Loc::Phone(path.unwrap_or_default()), &[], None);
        } else if args.is_empty() {
            Browser::open(app, &Loc::Local(glib::home_dir()), &[], None);
        } else {
            for arg in &args {
                show(app, &line.create_file_for_arg(arg));
            }
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}

/// Folders open as they are; anything else opens its folder with it selected.
fn show(app: &gtk::Application, file: &gio::File) {
    let Some(path) = file.path() else { return };
    match path.parent().filter(|_| !path.is_dir()) {
        Some(parent) => Browser::open(
            app,
            &Loc::Local(parent.to_path_buf()),
            &[Loc::Local(path.clone()).name()],
            None,
        ),
        None => Browser::open(app, &Loc::Local(path), &[], None),
    };
}

/// Follows the Nexus theme, live: the daemon rewrites settings.json on change.
fn install_theme() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let apply = move || {
        let settings = theme::load();
        provider.load_from_data(&(theme::css(&settings) + CSS));
        // GTK 4.20 replaced the prefer-dark switch with this preference.
        let scheme = if settings.dark() { "dark" } else { "light" };
        if let Some(gtk_settings) = gtk::Settings::default()
            && let Some(spec) = gtk_settings.find_property("gtk-interface-color-scheme")
            && let Some(value) = glib::EnumClass::with_type(spec.value_type())
                .and_then(|c| c.to_value_by_nick(scheme))
        {
            gtk_settings.set_property_from_value("gtk-interface-color-scheme", &value);
        }
    };
    apply();
    let file = gio::File::for_path(theme::config_dir().join("settings.json"));
    if let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
        monitor.connect_changed(move |_, _, _, _| apply());
        // Lives as long as the process.
        std::mem::forget(monitor);
    }
}

thread_local! {
    static APP: std::cell::Cell<Option<gtk::Application>> = const { std::cell::Cell::new(None) };
}
/// A pending D-Bus call, answered on the GTK thread.
struct Call(gio::DBusMethodInvocation);
// SAFETY: GDBusMethodInvocation may be returned from any thread.
unsafe impl Send for Call {}

/// Serves FileManager1 (browsers' "Show in folder") and the FileChooser portal.
/// A thread of its own answers the bus, so that introspection and properties
/// get answers even while GTK starts; the calls themselves run on the GTK thread.
fn serve(connection: gio::DBusConnection) {
    std::thread::spawn(move || {
        let context = glib::MainContext::new();
        let _ = context.with_thread_default(|| {
            for (xml, interface, path, name) in [
                (
                    FILE_MANAGER,
                    "org.freedesktop.FileManager1",
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1",
                ),
                (
                    portal::XML,
                    portal::CHOOSER,
                    "/org/freedesktop/portal/desktop",
                    portal::NAME,
                ),
            ] {
                let Some(info) = gio::DBusNodeInfo::for_xml(xml)
                    .ok()
                    .and_then(|n| n.lookup_interface(interface))
                else {
                    continue;
                };
                let registered = connection
                    .register_object(path, &info)
                    .method_call(move |connection, _, _, _, method, params, invocation| {
                        let (method, call) = (method.to_owned(), Call(invocation));
                        glib::MainContext::default()
                            .invoke(move || dispatch(connection, interface, &method, params, call));
                    })
                    .build();
                match registered {
                    Ok(_) => {
                        gio::bus_own_name_on_connection(
                            &connection,
                            name,
                            gio::BusNameOwnerFlags::NONE,
                            |_, _| {},
                            |_, _| {},
                        );
                    }
                    Err(e) => eprintln!("Could not serve {interface}: {e}"),
                }
            }
            glib::MainLoop::new(Some(&context), false).run();
        });
    });
}
fn dispatch(
    connection: gio::DBusConnection,
    interface: &str,
    method: &str,
    params: glib::Variant,
    call: Call,
) {
    let Some(app) = APP.take() else { return };
    APP.set(Some(app.clone()));
    if interface == portal::CHOOSER {
        return portal::open(&app, &connection, method, &params, call.0);
    }
    let uris = params
        .child_value(0)
        .get::<Vec<String>>()
        .unwrap_or_default();
    for file in uris.iter().map(gio::File::for_commandline_arg) {
        if method == "ShowFolders"
            && let Some(path) = file.path()
        {
            Browser::open(&app, &Loc::Local(path), &[], None);
        } else {
            show(&app, &file);
        }
    }
    call.0.return_value(None);
}
