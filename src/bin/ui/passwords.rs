//! `nexus passwords`: the vault's window. Nothing secret stays here longer than
//! it is on screen: the key is `nexus-vaultd`'s, and the entries come from it.
use super::super::{caption, clear, hbox, install_theme, label, vbox};
use super::phone::call as daemon;
use gtk::{gdk, glib, pango, prelude::*};
use nexus_control::{phone::VaultSync, theme, vault};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

const APP_ID: &str = "io.github.vitrixxl.NexusPasswords";
const POLL: Duration = Duration::from_secs(2);
const CLIPBOARD_SECONDS: u32 = 30;
const NOTICE: Duration = Duration::from_secs(4);
/// Dots of a fixed length, so that the mask says nothing of the password's.
const MASK: &str = "••••••••••••";

pub fn run() -> anyhow::Result<()> {
    glib::set_prgname(Some(APP_ID));
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            return window.present();
        }
        if let Some(display) = gdk::Display::default() {
            install_theme(&display, &theme::load());
        }
        Ui::new(app).window.present();
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

/// A vault request off the main thread; PBKDF2 takes a moment.
fn call(mut req: Value, spawn: bool, done: impl FnOnce(Result<Value, String>) + 'static) {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let reply = vault::request(&req, spawn).map_err(|e| e.to_string());
        vault::scrub(&mut req);
        let _ = tx.send_blocking(reply);
    });
    glib::spawn_future_local(async move {
        if let Ok(reply) = rx.recv().await {
            done(reply);
        }
    });
}

#[derive(Clone)]
struct Item {
    id: String,
    title: String,
    urls: Vec<String>,
    username: String,
    updated: i64,
}
impl Item {
    fn of(v: &Value) -> Self {
        Self {
            id: text(v, "id"),
            title: text(v, "title"),
            urls: strings(v, "urls"),
            username: text(v, "username"),
            updated: v["updated"].as_i64().unwrap_or(0),
        }
    }
    fn found(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        self.title.to_lowercase().contains(&q)
            || self.username.to_lowercase().contains(&q)
            || self.urls.iter().any(|u| u.to_lowercase().contains(&q))
    }
}
fn text(v: &Value, field: &str) -> String {
    v[field].as_str().unwrap_or_default().to_owned()
}
fn strings(v: &Value, field: &str) -> Vec<String> {
    v[field]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u.as_str().map(str::to_owned))
        .collect()
}
/// A site as typed: `example.com` means https.
fn normalize_url(url: &str) -> String {
    let url = url.trim();
    if url.contains("://") {
        url.to_owned()
    } else {
        format!("https://{url}")
    }
}
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .to_owned()
}
fn initial(title: &str) -> String {
    title
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or("?".to_owned(), |c| c.to_uppercase().to_string())
}
fn day(ms: i64) -> String {
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|d| d.format("%-d %b %Y"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}
fn tile(title: &str, class: &str, size: i32) -> gtk::Label {
    let t = gtk::Label::new(Some(&initial(title)));
    t.add_css_class("tile");
    t.add_css_class(class);
    t.set_size_request(size, size);
    t.set_valign(gtk::Align::Center);
    t
}
fn icon_button(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("flat");
    b.add_css_class("icon");
    b.set_tooltip_text(Some(tip));
    b.set_valign(gtk::Align::Center);
    b
}
fn button(text: &str, class: &str) -> gtk::Button {
    let b = gtk::Button::with_label(text);
    if !class.is_empty() {
        b.add_css_class(class);
    }
    b.set_valign(gtk::Align::Center);
    b
}
fn password_entry(placeholder: &str) -> gtk::PasswordEntry {
    let e = gtk::PasswordEntry::new();
    e.set_show_peek_icon(true);
    e.set_placeholder_text(Some(placeholder));
    e
}
fn wrapped(text: &str, class: &str) -> gtk::Label {
    let l = label(text, class);
    l.set_wrap_mode(pango::WrapMode::WordChar);
    l.set_selectable(true);
    // Selectable and focusable, the first such label takes the focus and shows
    // itself all selected whenever a page opens.
    l.set_focusable(false);
    l
}
fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::new();
    s.set_hscrollbar_policy(gtk::PolicyType::Never);
    s.set_vexpand(true);
    s.set_child(Some(child));
    s
}
/// A centred column for the screens outside the vault.
fn gate_column() -> (gtk::Box, gtk::Box) {
    let outer = vbox(0);
    outer.set_valign(gtk::Align::Center);
    outer.set_vexpand(true);
    let inner = vbox(10);
    inner.set_halign(gtk::Align::Center);
    inner.set_size_request(340, -1);
    outer.append(&inner);
    (outer, inner)
}
fn gate_icon() -> gtk::Image {
    let i = gtk::Image::from_icon_name("changes-prevent-symbolic");
    i.add_css_class("gate-icon");
    i.set_halign(gtk::Align::Center);
    i
}
fn field(parent: &gtk::Box, name: &str, input: &impl IsA<gtk::Widget>) {
    let b = vbox(5);
    b.append(&caption(name));
    b.append(input);
    parent.append(&b);
}
/// A secret field's validity shown the same way everywhere.
fn flag(entry: &gtk::PasswordEntry, invalid: bool) {
    if invalid {
        entry.add_css_class("invalid");
    } else {
        entry.remove_css_class("invalid");
    }
}

/// Shown only when there is something to say: an empty label still takes a line.
fn say(label: &gtk::Label, text: &str) {
    label.set_text(text);
    label.set_visible(!text.is_empty());
}
/// Plays the shake once more each time: two copies, as on the lock screen.
fn shake(entry: &gtk::PasswordEntry) {
    let (add, remove) = if entry.has_css_class("shake-a") {
        ("shake-b", "shake-a")
    } else {
        ("shake-a", "shake-b")
    };
    entry.remove_css_class(remove);
    entry.add_css_class(add);
}

/// What the top of the window asks of the user, if anything.
#[derive(PartialEq, Clone)]
enum Banner {
    None,
    Ask(String),
    Different(String),
}

struct Ui {
    app: gtk::Application,
    window: gtk::ApplicationWindow,
    /// create, locked or main.
    root: gtk::Stack,
    banner: gtk::Box,
    banner_text: gtk::Label,
    banner_yes: gtk::Button,
    banner_no: gtk::Button,
    banner_shown: RefCell<Banner>,
    create_password: gtk::PasswordEntry,
    create_again: gtk::PasswordEntry,
    create_button: gtk::Button,
    create_error: gtk::Label,
    unlock_password: gtk::PasswordEntry,
    unlock_button: gtk::Button,
    unlock_error: gtk::Label,
    search: gtk::SearchEntry,
    /// list, empty or nothing.
    list_stack: gtk::Stack,
    list: gtk::ListBox,
    nothing: gtk::Label,
    /// hint, detail, edit or settings.
    pane: gtk::Stack,
    detail: gtk::Box,
    settings: gtk::Box,
    status: gtk::Label,
    edit: Editor,
    page: Cell<&'static str>,
    items: RefCell<Vec<Item>>,
    /// The entry on screen, with its password. Dropped when it leaves the screen.
    current: RefCell<Option<Value>>,
    /// Which entry the editor is changing; None for a new one.
    editing: RefCell<Option<String>>,
    file_stamp: Cell<Option<std::time::SystemTime>>,
    damaged: gtk::Label,
    phone: RefCell<Option<String>>,
    sync: RefCell<VaultSync>,
    clip: RefCell<Option<Clip>>,
    notice: RefCell<Option<glib::SourceId>>,
}
/// What is on the clipboard for now, and what clears it. The hold keeps the
/// process, and so the clipboard, alive past a closed window.
struct Clip {
    provider: gdk::ContentProvider,
    timer: glib::SourceId,
    _hold: gtk::gio::ApplicationHoldGuard,
}
#[derive(Clone)]
struct Editor {
    heading: gtk::Label,
    title: gtk::Entry,
    urls: gtk::Entry,
    username: gtk::Entry,
    password: gtk::PasswordEntry,
    length: gtk::Scale,
    symbols: gtk::Switch,
    notes: gtk::TextView,
    error: gtk::Label,
    save: gtk::Button,
    cancel: gtk::Button,
    regenerate: gtk::Button,
}

impl Ui {
    fn new(app: &gtk::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Passwords")
            .default_width(880)
            .default_height(600)
            .css_classes(["nexus", "passwords"])
            .build();
        let outer = vbox(0);

        let banner = hbox(10);
        banner.add_css_class("sync-banner");
        banner.set_visible(false);
        let banner_text = label("", "");
        banner_text.set_hexpand(true);
        banner.append(&banner_text);
        let banner_no = button("No", "");
        let banner_yes = button("Yes", "suggested-action");
        banner.append(&banner_no);
        banner.append(&banner_yes);
        outer.append(&banner);

        let root = gtk::Stack::new();
        root.set_vexpand(true);
        outer.append(&root);

        // First run.
        let create_password = password_entry("Master password");
        let create_again = password_entry("Repeat it");
        let create_button = button("Create vault", "suggested-action");
        let create_error = label("", "error");
        create_error.set_visible(false);
        create_error.set_xalign(0.5);
        let (page, column) = gate_column();
        column.append(&gate_icon());
        let heading = label("Create your vault", "title");
        heading.set_xalign(0.5);
        column.append(&heading);
        let hint = caption(
            "One master password opens everything. There is no way to recover it, so keep it somewhere safe.",
        );
        hint.set_justify(gtk::Justification::Center);
        hint.set_xalign(0.5);
        hint.set_max_width_chars(46);
        column.append(&hint);
        column.append(&create_password);
        column.append(&create_again);
        column.append(&create_error);
        column.append(&create_button);
        root.add_named(&page, Some("create"));

        // Locked.
        let unlock_password = password_entry("Master password");
        let unlock_button = button("Unlock", "suggested-action");
        let unlock_error = label("", "error");
        unlock_error.set_xalign(0.5);
        unlock_error.set_visible(false);
        let (page, column) = gate_column();
        column.append(&gate_icon());
        let heading = label("Passwords are locked", "title");
        heading.set_xalign(0.5);
        column.append(&heading);
        column.append(&unlock_password);
        column.append(&unlock_error);
        column.append(&unlock_button);
        root.add_named(&page, Some("locked"));

        // The vault.
        let main = hbox(0);
        let side = vbox(0);
        side.add_css_class("passwords-side");
        side.set_size_request(300, -1);
        // Its title's hexpand would otherwise claim half of the window.
        side.set_hexpand(false);
        let head = hbox(4);
        head.add_css_class("passwords-head");
        let title = label("Passwords", "section-title");
        title.set_hexpand(true);
        let add = icon_button("list-add-symbolic", "New password");
        let settings_button = icon_button("emblem-system-symbolic", "Settings");
        let lock = icon_button("changes-prevent-symbolic", "Lock");
        head.append(&title);
        head.append(&add);
        head.append(&settings_button);
        head.append(&lock);
        side.append(&head);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search"));
        search.add_css_class("passwords-search");
        search.set_width_chars(12);
        side.append(&search);
        let list = gtk::ListBox::new();
        list.add_css_class("passwords-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        let list_stack = gtk::Stack::new();
        list_stack.set_vexpand(true);
        list_stack.add_named(&scrolled(&list), Some("list"));
        let empty = vbox(10);
        empty.set_valign(gtk::Align::Center);
        let add_first = button("Add a password", "suggested-action");
        add_first.set_halign(gtk::Align::Center);
        let none_yet = label("No passwords yet", "row-title");
        none_yet.set_xalign(0.5);
        empty.append(&none_yet);
        empty.append(&add_first);
        list_stack.add_named(&empty, Some("empty"));
        let nothing = caption("");
        nothing.set_valign(gtk::Align::Center);
        nothing.set_xalign(0.5);
        nothing.set_justify(gtk::Justification::Center);
        nothing.set_margin_start(20);
        nothing.set_margin_end(20);
        list_stack.add_named(&nothing, Some("nothing"));
        side.append(&list_stack);
        // Quiet while nothing is wrong; a line only when an entry won't decrypt.
        let damaged = caption("");
        damaged.add_css_class("error");
        damaged.add_css_class("passwords-status");
        damaged.set_visible(false);
        side.append(&damaged);
        let status = caption("");
        status.add_css_class("passwords-status");
        status.set_visible(false);
        status.set_ellipsize(pango::EllipsizeMode::End);
        side.append(&status);
        main.append(&side);

        let pane = gtk::Stack::new();
        pane.set_hexpand(true);
        let hint = caption("Select a password to see it here.");
        hint.set_valign(gtk::Align::Center);
        hint.set_xalign(0.5);
        pane.add_named(&hint, Some("hint"));
        let detail = vbox(14);
        detail.add_css_class("passwords-detail");
        pane.add_named(&scrolled(&detail), Some("detail"));
        let (editor_page, edit) = editor();
        pane.add_named(&scrolled(&editor_page), Some("edit"));
        let settings = vbox(10);
        settings.add_css_class("passwords-detail");
        pane.add_named(&scrolled(&settings), Some("settings"));
        main.append(&pane);
        root.add_named(&main, Some("main"));

        window.set_child(Some(&outer));

        let ui = Rc::new(Self {
            app: app.clone(),
            window,
            root,
            banner,
            banner_text,
            banner_yes,
            banner_no,
            banner_shown: RefCell::new(Banner::None),
            create_password,
            create_again,
            create_button,
            create_error,
            unlock_password,
            unlock_button,
            unlock_error,
            search,
            list_stack,
            list,
            nothing,
            pane,
            detail,
            settings,
            status,
            edit,
            page: Cell::new(""),
            items: RefCell::new(Vec::new()),
            current: RefCell::new(None),
            editing: RefCell::new(None),
            file_stamp: Cell::new(None),
            damaged,
            phone: RefCell::new(None),
            sync: RefCell::default(),
            clip: RefCell::new(None),
            notice: RefCell::new(None),
        });
        ui.connect(&add, &add_first, &settings_button, &lock);
        ui.poll();
        let polling = ui.clone();
        glib::timeout_add_local(POLL, move || {
            polling.poll();
            glib::ControlFlow::Continue
        });
        ui
    }

    fn connect(
        self: &Rc<Self>,
        add: &gtk::Button,
        add_first: &gtk::Button,
        settings: &gtk::Button,
        lock: &gtk::Button,
    ) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let ui = self.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            if key == gdk::Key::Escape {
                ui.escape();
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::f
                && state.contains(gdk::ModifierType::CONTROL_MASK)
                && ui.page.get() == "main"
            {
                ui.search.grab_focus();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);

        let ui = self.clone();
        self.create_button.connect_clicked(move |_| ui.create());
        for entry in [&self.create_password, &self.create_again] {
            let ui = self.clone();
            entry.connect_activate(move |_| ui.create());
        }
        let ui = self.clone();
        self.unlock_button.connect_clicked(move |_| ui.unlock());
        let ui = self.clone();
        self.unlock_password.connect_activate(move |_| ui.unlock());
        let ui = self.clone();
        self.unlock_password.connect_changed(move |e| {
            flag(e, false);
            say(&ui.unlock_error, "");
        });

        let ui = self.clone();
        self.banner_yes.connect_clicked(move |_| ui.banner_yes());
        let ui = self.clone();
        self.banner_no.connect_clicked(move |_| ui.banner_no());

        let ui = self.clone();
        self.search
            .connect_search_changed(move |_| ui.rebuild_list());
        let ui = self.clone();
        self.list
            .connect_row_activated(move |_, row| ui.open(&row.widget_name()));
        for button in [add, add_first] {
            let ui = self.clone();
            button.connect_clicked(move |_| ui.open_editor(None));
        }
        let ui = self.clone();
        settings.connect_clicked(move |_| ui.open_settings());
        let ui = self.clone();
        lock.connect_clicked(move |_| ui.lock());

        let ui = self.clone();
        self.edit.cancel.connect_clicked(move |_| ui.leave_pane());
        let ui = self.clone();
        self.edit.regenerate.connect_clicked(move |_| ui.generate());
        let ui = self.clone();
        self.edit.save.connect_clicked(move |_| ui.save_entry());
        let ui = self.clone();
        self.edit.title.connect_activate(move |_| ui.save_entry());
        let ui = self.clone();
        self.edit
            .username
            .connect_activate(move |_| ui.save_entry());
        let ui = self.clone();
        self.edit
            .password
            .connect_activate(move |_| ui.save_entry());
        let ui = self.clone();
        self.edit.urls.connect_activate(move |_| ui.save_entry());
    }

    fn escape(self: &Rc<Self>) {
        match self.pane.visible_child_name().as_deref() {
            Some("edit" | "settings") if self.page.get() == "main" => {
                self.leave_pane();
                return;
            }
            _ => {}
        }
        if !self.search.text().is_empty() {
            self.search.set_text("");
            return;
        }
        self.window.close();
    }

    /// The vault's state and the phone's, from wherever they are known.
    fn poll(self: &Rc<Self>) {
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let status = vault::request(&json!({ "op": "status" }), false).ok();
            let snapshot = nexus_control::request(&nexus_control::Request::Status)
                .ok()
                .and_then(|r| r.state)
                .map(|s| s.phone);
            let _ = tx.send_blocking((status, snapshot));
        });
        let ui = self.clone();
        glib::spawn_future_local(async move {
            if let Ok((status, phone)) = rx.recv().await {
                ui.apply(status, phone);
            }
        });
    }

    fn apply(
        self: &Rc<Self>,
        status: Option<Value>,
        phone: Option<nexus_control::phone::PhoneState>,
    ) {
        let exists = status
            .as_ref()
            .and_then(|s| s["exists"].as_bool())
            .unwrap_or_else(|| vault::path().exists());
        let unlocked = status.as_ref().is_some_and(|s| s["unlocked"] == true);
        let damaged = status
            .as_ref()
            .map_or(0, |s| s["damaged"].as_u64().unwrap_or(0));
        say(
            &self.damaged,
            &if damaged > 0 {
                format!("{damaged} entries could not be read.")
            } else {
                String::new()
            },
        );
        let page = match (exists, unlocked) {
            (false, _) => "create",
            (true, false) => "locked",
            (true, true) => "main",
        };
        if page != self.page.get() {
            self.page.set(page);
            self.root.set_visible_child_name(page);
            self.leave(page);
        }
        if page == "main" {
            let stamp = std::fs::metadata(vault::path())
                .and_then(|m| m.modified())
                .ok();
            if stamp != self.file_stamp.get() {
                self.file_stamp.set(stamp);
                self.reload();
            }
        }
        let (name, sync) = phone.map_or((None, VaultSync::default()), |p| (p.name, p.vault));
        self.show_banner(&name, &sync);
        if self.page.get() == "main"
            && self.pane.visible_child_name().as_deref() == Some("settings")
            && (*self.phone.borrow() != name || *self.sync.borrow() != sync)
        {
            *self.phone.borrow_mut() = name.clone();
            *self.sync.borrow_mut() = sync.clone();
            self.render_settings();
        }
        *self.phone.borrow_mut() = name;
        *self.sync.borrow_mut() = sync;
    }

    /// Entering another of the three screens: whatever was on screen goes.
    fn leave(self: &Rc<Self>, page: &str) {
        self.current.replace(None);
        self.items.borrow_mut().clear();
        self.clear_clipboard();
        self.pane.set_visible_child_name("hint");
        clear(&self.detail);
        clear(&self.settings);
        self.editing.replace(None);
        self.clear_editor();
        self.search.set_text("");
        self.unlock_password.set_text("");
        say(&self.unlock_error, "");
        self.create_password.set_text("");
        self.create_again.set_text("");
        say(&self.create_error, "");
        self.file_stamp.set(None);
        match page {
            "locked" => {
                self.unlock_password.grab_focus();
            }
            "create" => {
                self.create_password.grab_focus();
            }
            _ => {}
        }
    }

    fn show_banner(&self, name: &Option<String>, sync: &VaultSync) {
        let next = match name {
            Some(phone) if sync.here == "ask" => Banner::Ask(phone.clone()),
            Some(phone) if sync.status == "different" && self.page.get() == "main" => {
                Banner::Different(phone.clone())
            }
            _ => Banner::None,
        };
        if *self.banner_shown.borrow() == next {
            return;
        }
        self.banner.remove_css_class("different");
        match &next {
            Banner::None => self.banner.set_visible(false),
            Banner::Ask(phone) => {
                self.banner_text.set_text(&format!(
                    "Sync your passwords with {phone}? Nothing moves until both sides say yes."
                ));
                self.banner_yes.set_label("Sync");
                self.banner_no.set_label("Don't sync");
                self.banner_no.set_visible(true);
                self.banner.set_visible(true);
            }
            Banner::Different(phone) => {
                self.banner_text.set_text(&format!(
                    "{phone} has a different vault. Nothing was synced."
                ));
                self.banner_yes.set_label("Merge");
                self.banner_no.set_visible(false);
                self.banner.add_css_class("different");
                self.banner.set_visible(true);
            }
        }
        self.banner_shown.replace(next);
    }
    fn banner_yes(self: &Rc<Self>) {
        match self.banner_shown.borrow().clone() {
            Banner::Ask(_) => self.answer("yes"),
            Banner::Different(_) => self.open_settings(),
            Banner::None => {}
        }
    }
    fn banner_no(self: &Rc<Self>) {
        if matches!(*self.banner_shown.borrow(), Banner::Ask(_)) {
            self.answer("no");
        }
    }
    fn answer(self: &Rc<Self>, answer: &str) {
        let ui = self.clone();
        daemon("vault_answer", json!({ "answer": answer }), move |r| {
            if let Err(e) = r {
                ui.notify(&e, true);
            }
            ui.poll();
        });
    }

    fn notify(self: &Rc<Self>, text: &str, error: bool) {
        self.status.set_text(text);
        if error {
            self.status.add_css_class("error");
        } else {
            self.status.remove_css_class("error");
        }
        self.status.set_visible(true);
        if let Some(old) = self.notice.take() {
            old.remove();
        }
        let ui = self.clone();
        self.notice
            .replace(Some(glib::timeout_add_local_once(NOTICE, move || {
                ui.status.set_visible(false);
                ui.notice.replace(None);
            })));
    }

    fn create(self: &Rc<Self>) {
        let password = self.create_password.text().to_string();
        if password.chars().count() < 8 {
            say(&self.create_error, "Use at least 8 characters");
            return;
        }
        if password != self.create_again.text().as_str() {
            say(&self.create_error, "The two passwords differ");
            return;
        }
        self.create_button.set_sensitive(false);
        let ui = self.clone();
        call(
            json!({ "op": "create", "password": password }),
            true,
            move |r| {
                ui.create_button.set_sensitive(true);
                match r {
                    Ok(_) => ui.poll(),
                    Err(e) => say(&ui.create_error, &e),
                }
            },
        );
    }
    fn unlock(self: &Rc<Self>) {
        let password = self.unlock_password.text().to_string();
        if password.is_empty() {
            self.unlock_password.grab_focus();
            return;
        }
        self.unlock_button.set_sensitive(false);
        let ui = self.clone();
        call(
            json!({ "op": "unlock", "password": password }),
            true,
            move |r| {
                ui.unlock_button.set_sensitive(true);
                match r {
                    Ok(_) => ui.poll(),
                    Err(e) => {
                        ui.unlock_password.set_text("");
                        ui.unlock_password.grab_focus();
                        flag(&ui.unlock_password, true);
                        shake(&ui.unlock_password);
                        say(&ui.unlock_error, &e);
                    }
                }
            },
        );
    }
    fn lock(self: &Rc<Self>) {
        let ui = self.clone();
        call(json!({ "op": "lock" }), false, move |_| ui.poll());
    }

    fn reload(self: &Rc<Self>) {
        let ui = self.clone();
        call(json!({ "op": "list" }), true, move |r| {
            let Ok(list) = r else { return };
            *ui.items.borrow_mut() = list
                .as_array()
                .map_or(Vec::new(), |a| a.iter().map(Item::of).collect());
            ui.rebuild_list();
            let opened = ui.current.borrow().as_ref().map(|c| text(c, "id"));
            if let Some(id) = opened
                && ui.pane.visible_child_name().as_deref() == Some("detail")
            {
                ui.open(&id);
            }
        });
    }
    fn rebuild_list(self: &Rc<Self>) {
        let query = self.search.text();
        let items = self.items.borrow();
        let shown: Vec<&Item> = items.iter().filter(|i| i.found(&query)).collect();
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        if items.is_empty() {
            self.list_stack.set_visible_child_name("empty");
            return;
        }
        if shown.is_empty() {
            self.nothing.set_text(&format!("Nothing matches “{query}”"));
            self.list_stack.set_visible_child_name("nothing");
            return;
        }
        self.list_stack.set_visible_child_name("list");
        let current = self.current.borrow().as_ref().map(|c| text(c, "id"));
        for item in shown {
            let row = gtk::ListBoxRow::new();
            row.set_widget_name(&item.id);
            let content = hbox(10);
            content.append(&tile(&item.title, "small", 34));
            let lines = vbox(1);
            lines.set_valign(gtk::Align::Center);
            let title = label(&item.title, "row-title");
            title.set_wrap(false);
            title.set_ellipsize(pango::EllipsizeMode::End);
            lines.append(&title);
            let sub = if item.username.is_empty() {
                item.urls.first().map(|u| host_of(u)).unwrap_or_default()
            } else {
                item.username.clone()
            };
            let sub_line = caption(&sub);
            sub_line.set_wrap(false);
            sub_line.set_ellipsize(pango::EllipsizeMode::End);
            lines.append(&sub_line);
            content.append(&lines);
            row.set_child(Some(&content));
            self.list.append(&row);
            if current.as_deref() == Some(item.id.as_str()) {
                self.list.select_row(Some(&row));
            }
        }
    }

    fn open(self: &Rc<Self>, id: &str) {
        let ui = self.clone();
        call(json!({ "op": "get", "id": id }), true, move |r| match r {
            Ok(entry) => ui.render_detail(entry),
            Err(e) => ui.notify(&e, true),
        });
    }

    fn render_detail(self: &Rc<Self>, entry: Value) {
        clear(&self.detail);
        self.detail.set_spacing(14);
        self.current.replace(Some(entry));
        let current = self.current.borrow();
        let entry = current.as_ref().unwrap();
        let title_text = text(entry, "title");
        let head = hbox(14);
        head.append(&tile(&title_text, "large", 52));
        let lines = vbox(2);
        lines.set_valign(gtk::Align::Center);
        lines.set_hexpand(true);
        lines.append(&label(&title_text, "title"));
        let urls = strings(entry, "urls");
        if let Some(site) = urls.first() {
            let host = host_of(site);
            let site_line = caption(&host);
            site_line.set_ellipsize(pango::EllipsizeMode::End);
            lines.append(&site_line);
        }
        head.append(&lines);
        let edit = button("Edit", "");
        head.append(&edit);
        let id = text(entry, "id");
        let ui = self.clone();
        let editing = entry.clone();
        edit.connect_clicked(move |_| ui.open_editor(Some(&editing)));
        self.detail.append(&head);

        let group = vbox(0);
        group.add_css_class("group");
        let username = text(entry, "username");
        if !username.is_empty() {
            group.append(&self.secret_row("Username", &username, None));
        }
        let password = text(entry, "password");
        group.append(&self.secret_row("Password", &password, Some(MASK)));
        for url in &urls {
            group.append(&self.detail_row("Website", url));
        }
        self.detail.append(&group);

        let notes = text(entry, "notes");
        if !notes.is_empty() {
            let group = vbox(0);
            group.add_css_class("group");
            let row = vbox(4);
            row.add_css_class("row");
            row.append(&caption("Notes"));
            row.append(&wrapped(&notes, ""));
            group.append(&row);
            self.detail.append(&group);
        }

        if let Some(history) = entry["history"].as_array().filter(|h| !h.is_empty()) {
            let group = vbox(0);
            group.add_css_class("group");
            for old in history {
                let row = hbox(8);
                row.add_css_class("row");
                let when = caption(&format!(
                    "Replaced {}",
                    day(old["changed"].as_i64().unwrap_or(0))
                ));
                when.set_hexpand(true);
                row.append(&when);
                let copy = icon_button("edit-copy-symbolic", "Copy old password");
                let ui = self.clone();
                let secret = text(old, "password");
                copy.connect_clicked(move |_| ui.copy_secret(&secret, "Old password"));
                row.append(&copy);
                group.append(&row);
            }
            let earlier = gtk::Expander::new(Some("Earlier passwords"));
            earlier.add_css_class("passwords-earlier");
            earlier.set_child(Some(&group));
            self.detail.append(&earlier);
        }

        let footer = hbox(8);
        footer.set_margin_top(6);
        let changed = self
            .items
            .borrow()
            .iter()
            .find(|i| i.id == id)
            .map_or(0, |i| i.updated);
        let updated = caption(&if changed > 0 {
            format!("Changed {}", day(changed))
        } else {
            String::new()
        });
        updated.set_hexpand(true);
        footer.append(&updated);
        let delete = button("Delete", "flat");
        delete.add_css_class("passwords-delete");
        let armed = Rc::new(Cell::new(false));
        let ui = self.clone();
        delete.connect_clicked(move |b| {
            if !armed.replace(true) {
                b.set_label("Really delete?");
                b.add_css_class("destructive-action");
                let (b, armed) = (b.clone(), armed.clone());
                glib::timeout_add_local_once(Duration::from_secs(4), move || {
                    b.set_label("Delete");
                    b.remove_css_class("destructive-action");
                    armed.set(false);
                });
                return;
            }
            let id = id.clone();
            let ui2 = ui.clone();
            call(json!({ "op": "delete", "id": id }), true, move |r| {
                if let Err(e) = r {
                    ui2.notify(&e, true);
                }
                ui2.current.replace(None);
                ui2.pane.set_visible_child_name("hint");
                ui2.reload();
            });
        });
        footer.append(&delete);
        self.detail.append(&footer);
        self.pane.set_visible_child_name("detail");
    }
    fn detail_row(&self, name: &str, value: &str) -> gtk::Box {
        let row = hbox(10);
        row.add_css_class("row");
        let key = caption(name);
        key.set_size_request(84, -1);
        key.set_xalign(0.);
        row.append(&key);
        let text = wrapped(value, "");
        text.set_hexpand(true);
        row.append(&text);
        row
    }
    fn secret_row(self: &Rc<Self>, name: &str, value: &str, mask: Option<&str>) -> gtk::Box {
        let row = hbox(10);
        row.add_css_class("row");
        let key = caption(name);
        key.set_size_request(84, -1);
        key.set_xalign(0.);
        row.append(&key);
        let shown = wrapped(
            mask.unwrap_or(value),
            if mask.is_some() { "secret" } else { "" },
        );
        shown.set_hexpand(true);
        row.append(&shown);
        let secret = value.to_owned();
        if mask.is_some() {
            let reveal = gtk::Button::from_icon_name("view-reveal-symbolic");
            reveal.add_css_class("flat");
            reveal.add_css_class("icon");
            reveal.set_tooltip_text(Some("Show"));
            reveal.set_valign(gtk::Align::Center);
            let (s, m) = (secret.clone(), mask.unwrap_or_default().to_owned());
            let visible = Cell::new(false);
            reveal.connect_clicked(move |b| {
                if visible.get() {
                    shown.set_text(&m);
                    b.set_icon_name("view-reveal-symbolic");
                    b.set_tooltip_text(Some("Show"));
                } else {
                    shown.set_text(&s);
                    b.set_icon_name("view-conceal-symbolic");
                    b.set_tooltip_text(Some("Hide"));
                }
                visible.set(!visible.get());
            });
            row.append(&reveal);
        }
        let copy = icon_button("edit-copy-symbolic", &format!("Copy {name}"));
        let ui = self.clone();
        let label = name.to_owned();
        copy.connect_clicked(move |_| ui.copy_secret(&secret, &label));
        row.append(&copy);
        row
    }

    /// On the clipboard, marked secret, for 30 seconds. The window's process
    /// must outlive its window that long: Wayland clipboards die with their owner.
    fn copy_secret(self: &Rc<Self>, value: &str, what: &str) {
        let bytes = glib::Bytes::from(value.as_bytes());
        let mut providers: Vec<gdk::ContentProvider> = [
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "TEXT",
            "STRING",
        ]
        .iter()
        .map(|mime| gdk::ContentProvider::for_bytes(mime, &bytes))
        .collect();
        providers.push(gdk::ContentProvider::for_bytes(
            "x-kde-passwordManagerHint",
            &glib::Bytes::from_static(b"secret"),
        ));
        let provider = gdk::ContentProvider::new_union(&providers);
        self.clear_clipboard();
        if self
            .window
            .clipboard()
            .set_content(Some(&provider))
            .is_err()
        {
            self.notify("The clipboard is not available", true);
            return;
        }
        self.show_clip(what, CLIPBOARD_SECONDS);
        let left = Cell::new(CLIPBOARD_SECONDS);
        let what = what.to_owned();
        let ui = self.clone();
        let timer = glib::timeout_add_local(Duration::from_secs(1), move || {
            let seconds = left.get() - 1;
            left.set(seconds);
            if seconds == 0 {
                if let Some(clip) = ui.clip.take() {
                    ui.wipe(&clip.provider);
                }
                return glib::ControlFlow::Break;
            }
            ui.show_clip(&what, seconds);
            glib::ControlFlow::Continue
        });
        *self.clip.borrow_mut() = Some(Clip {
            provider,
            timer,
            _hold: self.app.hold(),
        });
    }
    fn show_clip(&self, what: &str, seconds: u32) {
        self.status.remove_css_class("error");
        self.status
            .set_text(&format!("{what} copied. Cleared in {seconds} s."));
        self.status.set_visible(true);
    }
    /// Whatever is on the clipboard stays if something else has been copied since.
    fn wipe(&self, provider: &gdk::ContentProvider) {
        let clipboard = self.window.clipboard();
        if clipboard.content().as_ref() == Some(provider) {
            let _ = clipboard.set_content(None::<&gdk::ContentProvider>);
        }
        self.status.set_visible(false);
    }
    fn clear_clipboard(&self) {
        if let Some(clip) = self.clip.take() {
            clip.timer.remove();
            self.wipe(&clip.provider);
        }
    }

    fn open_editor(self: &Rc<Self>, entry: Option<&Value>) {
        self.clear_editor();
        let e = &self.edit;
        if let Some(entry) = entry {
            e.heading.set_text("Edit password");
            e.title.set_text(&text(entry, "title"));
            e.urls.set_text(&strings(entry, "urls").join(", "));
            e.username.set_text(&text(entry, "username"));
            e.password.set_text(&text(entry, "password"));
            e.notes.buffer().set_text(&text(entry, "notes"));
            self.editing.replace(Some(text(entry, "id")));
        } else {
            e.heading.set_text("New password");
            self.editing.replace(None);
            self.generate();
        }
        self.pane.set_visible_child_name("edit");
        self.edit.title.grab_focus();
    }
    fn clear_editor(&self) {
        let e = &self.edit;
        e.title.set_text("");
        e.urls.set_text("");
        e.username.set_text("");
        e.password.set_text("");
        e.notes.buffer().set_text("");
        say(&e.error, "");
    }
    /// Back from the editor or the settings to the entry, if one is open.
    fn leave_pane(self: &Rc<Self>) {
        self.clear_editor();
        clear(&self.settings);
        let back = if self.current.borrow().is_some() {
            "detail"
        } else {
            "hint"
        };
        self.pane.set_visible_child_name(back);
    }
    fn generate(self: &Rc<Self>) {
        let (length, symbols) = (
            self.edit.length.value() as u64,
            self.edit.symbols.is_active(),
        );
        let ui = self.clone();
        call(
            json!({ "op": "generate", "length": length, "symbols": symbols }),
            true,
            move |r| {
                if let Ok(v) = r {
                    ui.edit.password.set_text(&text(&v, "password"));
                }
            },
        );
    }
    fn save_entry(self: &Rc<Self>) {
        let e = &self.edit;
        let title = e.title.text().trim().to_owned();
        if title.is_empty() {
            say(&e.error, "Give it a name");
            e.title.grab_focus();
            return;
        }
        let urls: Vec<String> = e
            .urls
            .text()
            .split(',')
            .filter(|u| !u.trim().is_empty())
            .map(normalize_url)
            .collect();
        let buffer = e.notes.buffer();
        let notes = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        let mut req = json!({
            "op": "save",
            "title": title,
            "urls": urls,
            "username": e.username.text().as_str(),
            "password": e.password.text().as_str(),
            "notes": notes.as_str(),
        });
        if let Some(id) = self.editing.borrow().as_ref() {
            req["id"] = id.as_str().into();
        }
        e.save.set_sensitive(false);
        let ui = self.clone();
        call(req, true, move |r| {
            ui.edit.save.set_sensitive(true);
            match r {
                Ok(saved) => {
                    let id = text(&saved, "id");
                    ui.clear_editor();
                    ui.current.replace(Some(json!({ "id": id })));
                    ui.search.set_text("");
                    ui.reload();
                    ui.open(&id);
                }
                Err(e) => say(&ui.edit.error, &e),
            }
        });
    }

    fn open_settings(self: &Rc<Self>) {
        self.render_settings();
        self.pane.set_visible_child_name("settings");
    }
    fn render_settings(self: &Rc<Self>) {
        clear(&self.settings);
        self.settings.append(&label("Settings", "title"));

        self.settings.append(&section_title("Phone sync"));
        let group = vbox(0);
        group.add_css_class("group");
        let sync = self.sync.borrow().clone();
        match self.phone.borrow().clone() {
            None => {
                let row = vbox(2);
                row.add_css_class("row");
                row.append(&label("No phone connected", "row-title"));
                row.append(&caption(
                    "Passwords stay on this computer. Connect your phone in Settings → Phone to sync.",
                ));
                group.append(&row);
            }
            Some(phone) => {
                let row = hbox(10);
                row.add_css_class("row");
                let lines = vbox(2);
                lines.set_hexpand(true);
                lines.append(&label(&format!("Sync with {phone}"), "row-title"));
                lines.append(&caption(match (sync.here.as_str(), sync.status.as_str()) {
                    ("no", _) => "Nothing is shared with this phone.",
                    (_, "synced") => "Up to date. Both sides keep their own copy.",
                    (_, "different") => "This phone has a different vault.",
                    ("yes", "waiting") => "Waiting for the phone to agree.",
                    _ => "Nothing moves until both sides say yes.",
                }));
                row.append(&lines);
                let toggle = gtk::Switch::new();
                toggle.set_valign(gtk::Align::Center);
                toggle.set_active(sync.here == "yes");
                let ui = self.clone();
                toggle.connect_active_notify(move |t| {
                    ui.answer(if t.is_active() { "yes" } else { "no" });
                });
                row.append(&toggle);
                group.append(&row);
            }
        }
        self.settings.append(&group);
        if sync.status == "different" {
            self.settings.append(&self.merge_form());
        }

        self.settings.append(&section_title("Master password"));
        self.settings.append(&self.change_password_form());

        let lock = button("Lock now", "");
        lock.set_halign(gtk::Align::Start);
        lock.set_margin_top(12);
        let ui = self.clone();
        lock.connect_clicked(move |_| ui.lock());
        self.settings.append(&lock);
    }
    fn merge_form(self: &Rc<Self>) -> gtk::Box {
        let form = vbox(8);
        form.set_margin_top(4);
        let phone = self
            .phone
            .borrow()
            .clone()
            .unwrap_or_else(|| "the phone".into());
        let hint = caption(&format!(
            "Merge {phone}'s vault into this one by typing its master password. Its entries are re-encrypted here, then {phone} takes this vault and keeps a backup of its own."
        ));
        form.append(&hint);
        let password = password_entry(&format!("{phone}'s master password"));
        form.append(&password);
        let error = label("", "error");
        error.set_visible(false);
        form.append(&error);
        let merge = button("Merge", "suggested-action");
        merge.set_halign(gtk::Align::Start);
        form.append(&merge);
        let ui = self.clone();
        let (p, e, m) = (password.clone(), error, merge.clone());
        let hide = e.clone();
        p.connect_changed(move |entry| {
            flag(entry, false);
            say(&hide, "");
        });
        let submit = Rc::new(move || ui.merge_other(&p, &e, &m));
        let run = submit.clone();
        merge.connect_clicked(move |_| run());
        password.connect_activate(move |_| submit());
        form
    }
    fn merge_other(
        self: &Rc<Self>,
        password: &gtk::PasswordEntry,
        error: &gtk::Label,
        merge: &gtk::Button,
    ) {
        let typed = password.text().to_string();
        if typed.is_empty() {
            password.grab_focus();
            return;
        }
        merge.set_sensitive(false);
        say(error, "");
        let (ui, password, error, merge) =
            (self.clone(), password.clone(), error.clone(), merge.clone());
        daemon("vault_fetch_other", json!({}), move |r| {
            if let Err(e) = r {
                say(&error, &e);
                merge.set_sensitive(true);
                return;
            }
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = (|| -> anyhow::Result<Value> {
                    let path = nexus_control::runtime()?.join("vault-other.json");
                    let file: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                    let _ = std::fs::remove_file(path);
                    let mut req = json!({ "op": "merge_other", "vault": file, "password": typed });
                    let reply = vault::request(&req, true);
                    vault::scrub(&mut req);
                    reply
                })();
                let _ = tx.send_blocking(result.map_err(|e| e.to_string()));
            });
            glib::spawn_future_local(async move {
                let Ok(result) = rx.recv().await else { return };
                match result {
                    Ok(counts) => {
                        let n = counts["imported"].as_u64().unwrap_or(0);
                        let entries = if n == 1 { "entry" } else { "entries" };
                        ui.notify(&format!("Merged {n} {entries} from the phone."), false);
                        password.set_text("");
                        merge.set_sensitive(true);
                        ui.reload();
                        // Lets the phone take this vault, keeping a backup of its own.
                        daemon("vault_adopt", json!({}), {
                            let ui = ui.clone();
                            move |r| {
                                if let Err(e) = r {
                                    ui.notify(&e, true);
                                }
                                ui.poll();
                            }
                        });
                    }
                    Err(e) => {
                        say(&error, &e);
                        merge.set_sensitive(true);
                        password.set_text("");
                        flag(&password, true);
                    }
                }
            });
        });
    }
    fn change_password_form(self: &Rc<Self>) -> gtk::Box {
        let form = vbox(8);
        let old = password_entry("Current password");
        let new = password_entry("New password");
        let again = password_entry("Repeat it");
        let error = label("", "error");
        error.set_visible(false);
        let save = button("Change password", "suggested-action");
        save.set_halign(gtk::Align::Start);
        form.append(&old);
        form.append(&new);
        form.append(&again);
        form.append(&error);
        form.append(&save);
        let ui = self.clone();
        let (old2, new2, again2, error2, save2) = (
            old.clone(),
            new.clone(),
            again.clone(),
            error.clone(),
            save.clone(),
        );
        let submit = move || {
            let (o, n) = (old2.text().to_string(), new2.text().to_string());
            if n != again2.text().as_str() {
                say(&error2, "The two passwords differ");
                return;
            }
            say(&error2, "");
            save2.set_sensitive(false);
            let (old3, new3, again3, error3, save3, ui2) = (
                old2.clone(),
                new2.clone(),
                again2.clone(),
                error2.clone(),
                save2.clone(),
                ui.clone(),
            );
            call(
                json!({ "op": "change_password", "old": o, "new": n }),
                true,
                move |r| {
                    save3.set_sensitive(true);
                    match r {
                        Ok(_) => {
                            for e in [&old3, &new3, &again3] {
                                e.set_text("");
                            }
                            ui2.notify("Password changed.", false);
                        }
                        Err(e) => say(&error3, &e),
                    }
                },
            );
        };
        let submit = Rc::new(submit);
        let run = submit.clone();
        save.connect_clicked(move |_| run());
        let run = submit.clone();
        again.connect_activate(move |_| run());
        new.connect_activate(move |_| submit());
        form
    }
}

fn section_title(text: &str) -> gtk::Label {
    label(text, "group-title")
}

/// The add/edit form, without its signal handlers.
fn editor() -> (gtk::Box, Editor) {
    let page = vbox(14);
    page.add_css_class("passwords-detail");
    let head = hbox(8);
    let heading = label("", "title");
    heading.set_hexpand(true);
    head.append(&heading);
    let cancel = button("Cancel", "flat");
    let save = button("Save", "suggested-action");
    head.append(&cancel);
    head.append(&save);
    page.append(&head);
    let title = gtk::Entry::new();
    title.set_placeholder_text(Some("Example"));
    field(&page, "Name", &title);
    let urls = gtk::Entry::new();
    urls.set_placeholder_text(Some("example.com, separated by commas"));
    field(&page, "Websites", &urls);
    let username = gtk::Entry::new();
    field(&page, "Username", &username);
    let password = gtk::PasswordEntry::new();
    password.set_show_peek_icon(true);
    field(&page, "Password", &password);

    let generator = hbox(10);
    generator.add_css_class("passwords-generator");
    generator.append(&caption("Length"));
    let length = gtk::Scale::with_range(gtk::Orientation::Horizontal, 8., 64., 1.);
    length.set_value(20.);
    length.set_hexpand(true);
    length.set_draw_value(true);
    length.set_digits(0);
    generator.append(&length);
    generator.append(&caption("Symbols"));
    let symbols = gtk::Switch::new();
    symbols.set_active(true);
    symbols.set_valign(gtk::Align::Center);
    generator.append(&symbols);
    let regenerate = gtk::Button::with_label("Generate");
    regenerate.set_valign(gtk::Align::Center);
    generator.append(&regenerate);
    page.append(&generator);

    let notes = gtk::TextView::new();
    notes.set_wrap_mode(gtk::WrapMode::WordChar);
    notes.add_css_class("passwords-notes");
    notes.set_size_request(-1, 96);
    field(&page, "Notes", &notes);

    let error = label("", "error");
    error.set_visible(false);
    page.append(&error);

    let edit = Editor {
        heading,
        title,
        urls,
        username,
        password,
        length,
        symbols,
        notes,
        error,
        save,
        cancel,
        regenerate,
    };
    (page, edit)
}
