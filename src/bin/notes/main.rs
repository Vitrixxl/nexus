//! Nexus Notes: pages of blocks, kept as plain Markdown files in
//! `~/Documents/Notes` (or `$NEXUS_NOTES_DIR`), folders mirroring nesting.
mod doc;
mod editor;
mod store;
mod vim;
use editor::{Colors, Editor};
use gtk::{gdk, gio, glib, prelude::*};
use nexus_control::theme;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::{Arc, atomic::AtomicU64, mpsc},
    time::Duration,
};
use store::{Cmd, Hit, Rel, Reply, Tree};

const APP_ID: &str = "io.github.vitrixxl.NexusNotes";
/// The icons a page can take: the name kept in its front matter, and the symbol.
const ICONS: [(&str, &str); 20] = [
    ("page", "text-x-generic-symbolic"),
    ("note", "accessories-text-editor-symbolic"),
    ("star", "starred-symbolic"),
    ("pin", "view-pin-symbolic"),
    ("bookmark", "user-bookmarks-symbolic"),
    ("important", "emblem-important-symbolic"),
    ("done", "object-select-symbolic"),
    ("folder", "folder-symbolic"),
    ("home", "user-home-symbolic"),
    ("people", "system-users-symbolic"),
    ("calendar", "x-office-calendar-symbolic"),
    ("time", "preferences-system-time-symbolic"),
    ("mail", "mail-unread-symbolic"),
    ("science", "applications-science-symbolic"),
    ("tools", "applications-engineering-symbolic"),
    ("idea", "emoji-objects-symbolic"),
    ("photo", "camera-photo-symbolic"),
    ("music", "audio-x-generic-symbolic"),
    ("games", "applications-games-symbolic"),
    ("sun", "weather-clear-symbolic"),
];
fn icon_name(key: &str) -> Option<&'static str> {
    ICONS.iter().find(|(k, _)| *k == key).map(|(_, i)| *i)
}
/// Styles of the notes window, on top of the shared Nexus stylesheet.
const CSS: &str = r#"
.notes .notes-sidebar { background: alpha(@nexus_fg, 0.035); border-right: 1px solid alpha(@nexus_fg, 0.06); }
.notes .side-head { padding: 10px 8px 6px 10px; }
.notes .side-head entry { min-height: 32px; }
.notes .side-head button { min-width: 32px; min-height: 32px; padding: 0; border-radius: 8px; background: transparent; color: alpha(@nexus_fg, 0.8); }
.notes .side-head button:hover { background: alpha(@nexus_fg, 0.08); color: @nexus_fg; }
.notes listview.pages, .notes list.results { background: transparent; padding: 2px 8px 12px; }
.notes listview.pages > row { border-radius: 7px; padding: 0 6px 0 2px; min-height: 30px; margin: 1px 0; color: alpha(@nexus_fg, 0.84); }
.notes listview.pages > row:hover, .notes list.results > row:hover { background: alpha(@nexus_fg, 0.05); }
.notes listview.pages > row:selected { background: alpha(@nexus_fg, 0.09); color: @nexus_fg; font-weight: 600; }
.notes listview.pages > row:focus-visible, .notes list.results > row:focus-visible { outline: 2px solid alpha(@nexus_accent, 0.8); outline-offset: -2px; }
.notes listview.pages expander { color: @nexus_muted; min-width: 16px; }
.notes listview.pages :drop(active) { box-shadow: none; outline: none; }
.notes .drop-into { background: alpha(@nexus_accent, 0.2); border-radius: 6px; }
.notes .drop-before { box-shadow: inset 0 2px @nexus_accent; }
.notes .drop-after { box-shadow: inset 0 -2px @nexus_accent; }
.notes list.results > row { border-radius: 8px; padding: 7px 10px; margin: 1px 0; }
.notes list.results > row:selected { background: alpha(@nexus_fg, 0.09); }
.notes .result-title { font-weight: 600; }
.notes .result-path, .notes .snippet { color: @nexus_muted; font-size: 12px; }
.notes .side-note { color: @nexus_muted; font-size: 13px; padding: 16px 12px; }
.notes .page-head { padding: 8px 12px 0 12px; min-height: 30px; }
.notes .crumbs { color: @nexus_muted; font-size: 12px; }
.notes .page-head button { padding: 2px 8px; min-height: 26px; border-radius: 7px; background: transparent; color: @nexus_muted; font-size: 12px; }
.notes .page-head button:hover { background: alpha(@nexus_fg, 0.07); color: @nexus_fg; }
.notes .title-note { color: @nexus_error; font-size: 13px; padding: 6px 20px 0; }
.notes .icon-grid button { min-width: 34px; min-height: 34px; padding: 0; border-radius: 8px; background: transparent; color: @nexus_fg; }
.notes .icon-grid button:hover { background: alpha(@nexus_fg, 0.08); }
.notes .page-icon { color: @nexus_muted; margin-right: 6px; }
.notes listview.pages > row:selected .page-icon { color: @nexus_fg; }
.notes .side-foot { padding: 6px 8px 10px; border-top: 1px solid alpha(@nexus_fg, 0.06); }
.notes .side-foot button, .notes .trash-head button.flat { padding: 4px 8px; color: alpha(@nexus_fg, 0.8); }
.notes .trash-head { padding: 4px 8px 4px 8px; }
.notes .trash-head menubutton > button { background: transparent; color: @nexus_muted; padding: 4px 8px; }
.notes .trash-head menubutton > button:hover { background: alpha(@nexus_fg, 0.07); color: @nexus_fg; }
.notes list.trash { background: transparent; padding: 2px 8px 12px; }
.notes list.trash > row { border-radius: 8px; padding: 6px 4px 6px 10px; }
.notes list.trash > row:hover { background: alpha(@nexus_fg, 0.05); }
.notes list.trash button { min-width: 28px; min-height: 28px; padding: 0 6px; }
.notes .confirm { padding: 10px; }
.notes textview.note > border { background: transparent; }
.notes paned > separator { background: transparent; min-width: 0; }
.notes textview.note, .notes textview.note > text { background: transparent; color: @nexus_fg; caret-color: @nexus_accent; font-size: 16px; }
.notes popover.block-menu > contents { padding: 4px; min-width: 248px; }
.notes .block-menu row { border-radius: 7px; padding: 6px 10px; min-height: 0; }
.notes .block-menu row:selected { background: alpha(@nexus_fg, 0.1); }
.notes .hint { color: @nexus_muted; font-family: 'Geist Mono', monospace; font-size: 12px; }
.notes .toast { background: mix(@nexus_bg, @nexus_fg, 0.09); border-radius: 10px; padding: 6px 6px 6px 14px; margin-bottom: 22px; box-shadow: 0 0 0 1px @nexus_edge, 0 10px 24px -10px @nexus_shadow; }
.notes .toast button { padding: 4px 12px; }
.notes .empty-page { padding: 24px; }
.notes .vim-status { margin: 0 20px 14px 0; }
.notes .vim-status label { font-family: 'Geist Mono', monospace; font-size: 11px; letter-spacing: 0.04em; color: @nexus_muted; padding: 4px 6px; }
.notes .vim-status entry { min-height: 28px; font-family: 'Geist Mono', monospace; font-size: 13px; }
.notes .side-foot button.vim-toggle { font-family: 'Geist Mono', monospace; font-size: 12px; color: @nexus_muted; }
.notes .side-foot button.vim-toggle:checked { background: alpha(@nexus_accent, 0.16); color: @nexus_fg; }
.notes .empty-page .title { font-size: 20px; }
.notes .empty-page .muted { font-size: 14px; }
.notes .empty-page button { margin-top: 10px; padding: 8px 18px; }
"#;

fn notes_dir() -> PathBuf {
    std::env::var_os("NEXUS_NOTES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            glib::user_special_dir(glib::UserDirectory::Documents)
                .unwrap_or_else(|| glib::home_dir().join("Documents"))
                .join("Notes")
        })
}
fn state_file(name: &str) -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| glib::home_dir().join(".local/state"))
        .join("nexus")
        .join(name)
}
fn write_state(name: &str, value: &str) {
    let path = state_file(name);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, value);
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|app| {
        for (action, accels) in [
            ("win.new-page", &["<Control>n"][..]),
            ("win.new-subpage", &["<Control><Shift>n"]),
            ("win.rename", &["F2"]),
            ("win.search", &["<Control>p", "<Control><Shift>f"]),
            ("window.close", &["<Control>w", "<Control>q"]),
        ] {
            app.set_accels_for_action(action, accels);
        }
    });
    // One process per application id: a second launch raises this window.
    app.connect_activate(|app| match app.active_window() {
        Some(w) => w.present(),
        None => Notes::open(app),
    });
    app.run()
}

/// Follows the Nexus theme, live: the daemon rewrites settings.json on change.
fn install_theme(changed: impl Fn(&theme::Settings) + 'static) {
    let Some(display) = gdk::Display::default() else {
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
        let scheme = if settings.dark() { "dark" } else { "light" };
        if let Some(gtk_settings) = gtk::Settings::default()
            && let Some(spec) = gtk_settings.find_property("gtk-interface-color-scheme")
            && let Some(value) = glib::EnumClass::with_type(spec.value_type())
                .and_then(|c| c.to_value_by_nick(scheme))
        {
            gtk_settings.set_property_from_value("gtk-interface-color-scheme", &value);
        }
        changed(&settings);
    };
    apply();
    let file = gio::File::for_path(theme::config_dir().join("settings.json"));
    if let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
        monitor.connect_changed(move |_, _, _, _| apply());
        std::mem::forget(monitor);
    }
}

thread_local! {
    static WINDOW: Cell<Option<Rc<Notes>>> = const { Cell::new(None) };
}

struct Toast {
    revealer: gtk::Revealer,
    label: gtk::Label,
    button: gtk::Button,
    /// Cleared when it fires: removing a source that ran is an error.
    timer: Rc<RefCell<Option<glib::SourceId>>>,
    undo: RefCell<Option<Box<dyn Fn()>>>,
}

struct Notes {
    window: gtk::ApplicationWindow,
    editor: Rc<Editor>,
    cmd: mpsc::Sender<Cmd>,
    root: PathBuf,
    tree: Rc<RefCell<Tree>>,
    roots: gio::ListStore,
    model: gtk::TreeListModel,
    selection: gtk::SingleSelection,
    search: gtk::SearchEntry,
    results: gtk::ListBox,
    side: gtk::Stack,
    page: gtk::Stack,
    crumbs: gtk::Label,
    icon_button: gtk::MenuButton,
    title_note: gtk::Label,
    trash: gtk::ListBox,
    toast: Toast,
    current: RefCell<Option<Rel>>,
    front: RefCell<String>,
    /// The page being loaded: whether to put the caret in it, what to find.
    wanted: RefCell<Option<(Rel, bool, Option<String>)>>,
    expanded: RefCell<HashSet<Rel>>,
    save_timer: RefCell<Option<glib::SourceId>>,
    reload_timer: RefCell<Option<glib::SourceId>>,
    monitor: RefCell<Option<gio::FileMonitor>>,
    /// The folders of the tree, watched for pages that come and go.
    dirs: RefCell<HashMap<Rel, gio::FileMonitor>>,
    scan_timer: RefCell<Option<glib::SourceId>>,
    search_generation: Arc<AtomicU64>,
    syncing: Cell<bool>,
    scanned: Cell<bool>,
    rename_after: Cell<bool>,
    last: RefCell<Option<Rel>>,
}

impl Notes {
    fn open(app: &gtk::Application) {
        let root = notes_dir();
        let (cmd, rx) = mpsc::channel();
        let (tx, replies) = async_channel::unbounded();
        let worker_root = root.clone();
        std::thread::spawn(move || store::Store::new(worker_root).run(rx, tx));
        let _ = cmd.send(Cmd::Scan);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Notes")
            .default_width(1120)
            .default_height(760)
            .decorated(false)
            .build();
        window.add_css_class("nexus");
        window.add_css_class("notes");

        let editor = Editor::new();
        let tree = Rc::new(RefCell::new(Tree::default()));
        let roots = gio::ListStore::new::<gtk::StringObject>();
        let kids_of = tree.clone();
        let model = gtk::TreeListModel::new(roots.clone(), false, false, move |item| {
            let rel = item.downcast_ref::<gtk::StringObject>()?.string();
            let tree = kids_of.borrow();
            let kids = tree.kids(&rel);
            if kids.is_empty() {
                return None;
            }
            let store = gio::ListStore::new::<gtk::StringObject>();
            for k in kids {
                store.append(&gtk::StringObject::new(k));
            }
            Some(store.upcast())
        });
        let selection = gtk::SingleSelection::new(Some(model.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let toast = Toast {
            revealer: gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::Crossfade)
                .transition_duration(120)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::End)
                .build(),
            label: gtk::Label::new(None),
            button: gtk::Button::with_label("Undo"),
            timer: Rc::default(),
            undo: RefCell::new(None),
        };
        let crumbs = gtk::Label::builder()
            .xalign(0.)
            .css_classes(["crumbs"])
            .build();
        crumbs.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

        let notes = Rc::new(Self {
            window,
            editor,
            cmd,
            root,
            tree,
            roots,
            model,
            selection,
            search: gtk::SearchEntry::builder()
                .placeholder_text("Search pages")
                .hexpand(true)
                .build(),
            results: gtk::ListBox::new(),
            side: gtk::Stack::new(),
            page: gtk::Stack::new(),
            crumbs,
            icon_button: gtk::MenuButton::new(),
            title_note: gtk::Label::builder()
                .xalign(0.)
                .halign(gtk::Align::Center)
                .max_width_chars(80)
                .wrap(true)
                .css_classes(["title-note"])
                .visible(false)
                .build(),
            trash: gtk::ListBox::new(),
            toast,
            current: RefCell::new(None),
            front: RefCell::new(String::new()),
            wanted: RefCell::new(None),
            expanded: RefCell::new(HashSet::new()),
            save_timer: RefCell::new(None),
            reload_timer: RefCell::new(None),
            monitor: RefCell::new(None),
            dirs: RefCell::default(),
            scan_timer: RefCell::new(None),
            search_generation: Arc::new(AtomicU64::new(0)),
            syncing: Cell::new(false),
            scanned: Cell::new(false),
            rename_after: Cell::new(false),
            last: RefCell::new(
                std::fs::read_to_string(state_file("notes-last"))
                    .ok()
                    .map(|s| s.trim().to_string()),
            ),
        });
        notes.build();
        let ed = notes.editor.clone();
        install_theme(move |s| {
            let (_, _, fg, muted) = theme::palette(s);
            ed.set_colors(Colors::new(fg, muted, &s.accent));
        });
        let weak = Rc::downgrade(&notes);
        glib::spawn_future_local(async move {
            while let Ok(reply) = replies.recv().await {
                let Some(n) = weak.upgrade() else { break };
                n.reply(reply);
            }
        });
        notes.window.present();
        // The window's state lives as long as the process: there is one window.
        WINDOW.set(Some(notes));
    }

    fn build(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        // Sidebar: search and the page tree, or the results.
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        head.add_css_class("side-head");
        head.append(&self.search);
        let add = gtk::Button::from_icon_name("list-add-symbolic");
        add.set_tooltip_text(Some("New page (Ctrl+N)"));
        add.set_action_name(Some("win.new-page"));
        head.append(&add);

        let list = gtk::ListView::new(Some(self.selection.clone()), Some(self.factory()));
        list.add_css_class("pages");
        let tree_scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        // Dropping on the empty space below the tree: last at the top level.
        let tail = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
        tail.connect_drop(glib::clone!(
            #[strong]
            weak,
            move |_, value, _, _| {
                let (Some(n), Ok(rel)) = (weak.upgrade(), value.get::<String>()) else {
                    return false;
                };
                n.send(Cmd::Move {
                    rel,
                    parent: String::new(),
                    before: None,
                });
                true
            }
        ));
        tree_scroll.add_controller(tail);
        self.results.add_css_class("results");
        self.results.set_selection_mode(gtk::SelectionMode::Browse);
        let results_scroll = gtk::ScrolledWindow::builder()
            .child(&self.results)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        self.side.add_named(&tree_scroll, Some("tree"));
        self.side.add_named(&results_scroll, Some("results"));
        self.side.add_named(&self.trash_view(), Some("trash"));
        let foot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        foot.add_css_class("side-foot");
        let trash_button = gtk::Button::builder()
            .css_classes(["flat"])
            .tooltip_text("Deleted pages, kept for 30 days")
            .build();
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        inner.append(&gtk::Image::from_icon_name("user-trash-symbolic"));
        inner.append(&gtk::Label::new(Some("Trash")));
        trash_button.set_child(Some(&inner));
        trash_button.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(n) = weak.upgrade() {
                    n.search.set_text("");
                    n.side.set_visible_child_name("trash");
                    n.send(Cmd::ListTrash);
                }
            }
        ));
        foot.append(&trash_button);
        let vim_on =
            std::fs::read_to_string(state_file("notes-vim")).is_ok_and(|s| s.trim() == "on");
        let vim = gtk::ToggleButton::builder()
            .label("Vim")
            .active(vim_on)
            .css_classes(["flat", "vim-toggle"])
            .tooltip_text("Vim keys in pages: Esc for normal mode, i to type")
            .hexpand(true)
            .halign(gtk::Align::End)
            .build();
        self.editor.set_vim(vim_on);
        vim.connect_toggled(glib::clone!(
            #[strong]
            weak,
            move |t| {
                if let Some(n) = weak.upgrade() {
                    n.editor.set_vim(t.is_active());
                    write_state("notes-vim", if t.is_active() { "on" } else { "off" });
                }
            }
        ));
        foot.append(&vim);
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.add_css_class("notes-sidebar");
        sidebar.set_width_request(220);
        sidebar.append(&head);
        sidebar.append(&self.side);
        sidebar.append(&foot);

        // The page, or what to do when there is none.
        let scroller = gtk::ScrolledWindow::builder()
            .child(&self.editor.view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let page_head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        page_head.add_css_class("page-head");
        self.icon_button.set_tooltip_text(Some("Page icon"));
        self.icon_button.set_popover(Some(&self.icon_picker()));
        page_head.append(&self.icon_button);
        page_head.append(&self.crumbs);
        column.append(&page_head);
        column.append(&self.title_note);
        column.append(&scroller);
        let empty = gtk::Box::new(gtk::Orientation::Vertical, 6);
        empty.add_css_class("empty-page");
        empty.set_valign(gtk::Align::Center);
        empty.set_halign(gtk::Align::Center);
        let title = gtk::Label::builder()
            .label("No pages yet")
            .css_classes(["title"])
            .build();
        let home = glib::home_dir();
        let shown = match self.root.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => self.root.display().to_string(),
        };
        let about = gtk::Label::builder()
            .label(format!(
                "Each page is a Markdown file in {shown}.\nType / in a page for headings, lists, to-dos, code and more."
            ))
            .justify(gtk::Justification::Center)
            .css_classes(["muted"])
            .build();
        let create = gtk::Button::with_label("New page");
        create.add_css_class("suggested-action");
        create.set_halign(gtk::Align::Center);
        create.set_action_name(Some("win.new-page"));
        create.set_tooltip_text(Some("Ctrl+N"));
        empty.append(&title);
        empty.append(&about);
        empty.append(&create);
        self.page.add_named(&column, Some("page"));
        self.page.add_named(&empty, Some("empty"));
        self.page.add_named(&gtk::Box::default(), Some("blank"));
        self.page.set_visible_child_name("blank");

        let t = &self.toast;
        let tbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        tbox.add_css_class("toast");
        t.label.set_wrap(true);
        t.label.set_max_width_chars(60);
        tbox.append(&t.label);
        t.button.add_css_class("flat");
        tbox.append(&t.button);
        t.revealer.set_child(Some(&tbox));
        t.button.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                let Some(n) = weak.upgrade() else { return };
                if let Some(f) = n.toast.undo.take() {
                    f();
                }
                n.hide_toast();
            }
        ));
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&self.page));
        overlay.add_overlay(&t.revealer);
        overlay.add_overlay(&self.editor.status);

        let paned = gtk::Paned::builder()
            .start_child(&sidebar)
            .end_child(&overlay)
            .resize_start_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(264)
            .build();
        self.window.set_child(Some(&paned));

        self.selection.connect_selected_item_notify(glib::clone!(
            #[strong]
            weak,
            move |sel| {
                let Some(n) = weak.upgrade() else { return };
                if n.syncing.get() {
                    return;
                }
                if let Some(rel) = row_rel(sel.selected_item()) {
                    n.go(&rel, false, None);
                }
            }
        ));
        self.search.connect_search_changed(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(n) = weak.upgrade() {
                    n.run_search();
                }
            }
        ));
        self.search.connect_stop_search(|s| s.set_text(""));
        self.search.connect_activate(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(n) = weak.upgrade()
                    && let Some(row) = n.results.row_at_index(0)
                {
                    row.activate();
                }
            }
        ));
        let down = gtk::EventControllerKey::new();
        down.connect_key_pressed(glib::clone!(
            #[strong]
            weak,
            move |_, key, _, _| {
                let Some(n) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if key == gdk::Key::Down
                    && let Some(row) = n.results.row_at_index(0)
                {
                    n.results.select_row(Some(&row));
                    row.grab_focus();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        ));
        self.search.add_controller(down);
        self.results.connect_row_activated(glib::clone!(
            #[strong]
            weak,
            move |_, row| {
                let Some(n) = weak.upgrade() else { return };
                let rel = row.widget_name().to_string();
                let q = n.search.text().trim().to_string();
                let first = q.split_whitespace().next().map(String::from);
                n.go(&rel, true, first);
            }
        ));

        let actions: [(&str, Option<&glib::VariantTy>); 7] = [
            ("rename-page", Some(glib::VariantTy::STRING)),
            ("new-page", None),
            ("new-subpage", None),
            ("new-child", Some(glib::VariantTy::STRING)),
            ("delete", Some(glib::VariantTy::STRING)),
            ("rename", None),
            ("search", None),
        ];
        for (name, param) in actions {
            let action = gio::SimpleAction::new(name, param);
            action.connect_activate(glib::clone!(
                #[strong]
                weak,
                move |a, p| {
                    let Some(n) = weak.upgrade() else { return };
                    let arg = p.and_then(|p| p.str().map(String::from));
                    n.action(&a.name(), arg);
                }
            ));
            self.window.add_action(&action);
        }

        *self.editor.on_change.borrow_mut() = Some(Box::new(glib::clone!(
            #[strong]
            weak,
            move || {
                if let Some(n) = weak.upgrade() {
                    n.schedule_save();
                }
            }
        )));
        self.window.connect_close_request(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(n) = weak.upgrade() {
                    n.flush();
                }
                glib::Propagation::Proceed
            }
        ));
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            let w = self.window.downgrade();
            glib::unix_signal_add_local(signal, move || {
                if let Some(w) = w.upgrade() {
                    w.close();
                }
                glib::ControlFlow::Break
            });
        }
    }

    fn factory(self: &Rc<Self>) -> gtk::SignalListItemFactory {
        let weak = Rc::downgrade(self);
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::builder()
                .xalign(0.)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let image = gtk::Image::builder().css_classes(["page-icon"]).build();
            row.append(&image);
            row.append(&label);
            let expander = gtk::TreeExpander::new();
            expander.set_child(Some(&row));
            expander.set_indent_for_icon(true);
            let rel_of = |e: &gtk::TreeExpander| row_rel(e.list_row().and_then(|r| r.item()));

            let menu = gtk::GestureClick::builder().button(3).build();
            menu.connect_pressed(glib::clone!(
                #[strong]
                weak,
                #[weak]
                expander,
                move |_, _, x, y| {
                    if let (Some(n), Some(rel)) = (weak.upgrade(), rel_of(&expander)) {
                        n.context_menu(&expander, &rel, x, y);
                    }
                }
            ));
            expander.add_controller(menu);

            let source = gtk::DragSource::new();
            source.set_actions(gdk::DragAction::MOVE);
            source.connect_prepare(glib::clone!(
                #[weak]
                expander,
                #[upgrade_or]
                None,
                move |_, _, _| {
                    rel_of(&expander).map(|r| gdk::ContentProvider::for_value(&r.to_value()))
                }
            ));
            expander.add_controller(source);

            let target = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
            let zone = |w: &gtk::TreeExpander, y: f64| {
                let h = w.height().max(1) as f64;
                if y < h * 0.28 {
                    "drop-before"
                } else if y > h * 0.72 {
                    "drop-after"
                } else {
                    "drop-into"
                }
            };
            let clear = |w: &gtk::TreeExpander| {
                if let Some(row) = w.parent() {
                    for c in ["drop-before", "drop-after", "drop-into"] {
                        row.remove_css_class(c);
                    }
                }
            };
            target.connect_motion(glib::clone!(
                #[weak]
                expander,
                #[upgrade_or]
                gdk::DragAction::empty(),
                move |_, _, y| {
                    clear(&expander);
                    if let Some(row) = expander.parent() {
                        row.add_css_class(zone(&expander, y));
                    }
                    gdk::DragAction::MOVE
                }
            ));
            target.connect_leave(glib::clone!(
                #[weak]
                expander,
                move |_| clear(&expander)
            ));
            target.connect_drop(glib::clone!(
                #[strong]
                weak,
                #[weak]
                expander,
                #[upgrade_or]
                false,
                move |_, value, _, y| {
                    clear(&expander);
                    let (Some(n), Some(on), Ok(rel)) =
                        (weak.upgrade(), rel_of(&expander), value.get::<String>())
                    else {
                        return false;
                    };
                    if on == rel {
                        return false;
                    }
                    n.drop_on(rel, &on, zone(&expander, y));
                    true
                }
            ));
            expander.add_controller(target);
            item.set_child(Some(&expander));
        });
        let tree = self.tree.clone();
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = item.item().and_downcast::<gtk::TreeListRow>();
            let expander = item.child().and_downcast::<gtk::TreeExpander>().unwrap();
            expander.set_list_row(row.as_ref());
            let rel = row_rel(row.and_then(|r| r.item())).unwrap_or_default();
            let parts = expander.child().unwrap();
            let image = parts.first_child().and_downcast::<gtk::Image>().unwrap();
            let icon = tree.borrow().icons.get(&rel).and_then(|k| icon_name(k));
            image.set_icon_name(Some(icon.unwrap_or("text-x-generic-symbolic")));
            let label = parts.last_child().and_downcast::<gtk::Label>().unwrap();
            label.set_text(store::name(&rel));
            expander.set_tooltip_text(Some(store::name(&rel)));
        });
        factory.connect_unbind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            if let Some(e) = item.child().and_downcast::<gtk::TreeExpander>() {
                e.set_list_row(None);
            }
        });
        factory
    }

    fn context_menu(&self, on: &gtk::TreeExpander, rel: &str, x: f64, y: f64) {
        let menu = gio::Menu::new();
        let item = |label: &str, action: &str| {
            let i = gio::MenuItem::new(Some(label), None);
            i.set_action_and_target_value(Some(action), Some(&rel.to_variant()));
            i
        };
        menu.append_item(&item("New subpage", "win.new-child"));
        menu.append_item(&item("Rename", "win.rename-page"));
        let edit = gio::Menu::new();
        edit.append_item(&item("Delete", "win.delete"));
        menu.append_section(None, &edit);
        let pop = gtk::PopoverMenu::from_model(Some(&menu));
        pop.set_has_arrow(false);
        // On the sidebar, not the row: a rescan (sync) recycles rows while the menu is open.
        let at = on
            .compute_point(&self.side, &gtk::graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
        pop.set_parent(&self.side);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(
            at.x() as i32,
            at.y() as i32,
            1,
            1,
        )));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    }

    fn drop_on(&self, rel: Rel, on: &str, zone: &str) {
        let parent = store::parent(on).to_string();
        let cmd = match zone {
            "drop-into" => Cmd::Move {
                rel,
                parent: on.into(),
                before: None,
            },
            "drop-before" => Cmd::Move {
                rel,
                parent,
                before: Some(on.into()),
            },
            _ => {
                let tree = self.tree.borrow();
                let kids = tree.kids(&parent);
                let next = kids
                    .iter()
                    .position(|k| k == on)
                    .and_then(|i| kids.get(i + 1))
                    .filter(|k| **k != rel)
                    .cloned();
                Cmd::Move {
                    rel,
                    parent,
                    before: next,
                }
            }
        };
        if let Cmd::Move { parent, .. } = &cmd
            && !parent.is_empty()
        {
            self.expanded.borrow_mut().insert(parent.clone());
        }
        self.save_now();
        self.send(cmd);
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmd.send(cmd);
    }

    fn action(&self, name: &str, arg: Option<String>) {
        let current = self.current.borrow().clone();
        match name {
            "new-page" => self.create(String::new()),
            "new-subpage" => self.create(current.unwrap_or_default()),
            "new-child" => self.create(arg.unwrap_or_default()),
            "delete" => {
                if let Some(rel) = arg.or(current) {
                    self.save_now();
                    self.send(Cmd::Delete(rel));
                }
            }
            "rename-page" => {
                let rel = arg.unwrap_or_default();
                if current.as_deref() == Some(rel.as_str()) {
                    self.action("rename", None);
                } else {
                    self.rename_after.set(true);
                    self.go(&rel, true, None);
                }
            }
            // The title is the name: renaming is editing it.
            "rename" => {
                let b = &self.editor.buffer;
                let (s, mut e) = (b.start_iter(), b.start_iter());
                e.forward_to_line_end();
                b.select_range(&s, &e);
                self.editor.vim_insert();
                self.editor.view.grab_focus();
            }
            "search" => {
                self.search.grab_focus();
                self.search.select_region(0, -1);
            }
            _ => {}
        }
    }

    fn create(&self, parent: Rel) {
        self.save_now();
        let date = glib::DateTime::now_local()
            .and_then(|d| d.format("%Y-%m-%d"))
            .map(|d| d.to_string())
            .unwrap_or_default();
        if !parent.is_empty() {
            self.expanded.borrow_mut().insert(parent.clone());
        }
        let raw = format!("---\ncreated: {date}\n---\n\n\n");
        self.send(Cmd::Create { parent, raw });
    }

    /// Opens a page: from the tree without moving focus, else into it.
    fn go(&self, rel: &str, focus: bool, find: Option<String>) {
        if self.current.borrow().as_deref() == Some(rel) && self.wanted.borrow().is_none() {
            if let Some(q) = find {
                self.editor.find(&q);
            }
            if focus {
                self.editor.view.grab_focus();
            }
            return;
        }
        self.save_now();
        self.wanted.replace(Some((rel.into(), focus, find)));
        self.send(Cmd::Load(rel.into()));
    }

    fn compose(&self) -> String {
        format!("{}{}", self.front.borrow(), self.editor.body())
    }

    fn schedule_save(self: &Rc<Self>) {
        if self.current.borrow().is_none() {
            return;
        }
        self.check_title();
        if let Some(t) = self.save_timer.take() {
            t.remove();
        }
        let weak = Rc::downgrade(self);
        self.save_timer.replace(Some(glib::timeout_add_local_once(
            Duration::from_millis(600),
            move || {
                if let Some(n) = weak.upgrade() {
                    n.save_timer.take();
                    n.save_now();
                }
            },
        )));
    }

    fn save_now(&self) {
        if let Some(t) = self.save_timer.take() {
            t.remove();
        }
        let Some(rel) = self.current.borrow().clone() else {
            return;
        };
        if !self.editor.buffer.is_modified() {
            return;
        }
        self.editor.buffer.set_modified(false);
        self.send(Cmd::Save {
            rel,
            name: store::file_name(&self.editor.title()),
            raw: self.compose(),
        });
    }

    /// Writes what is pending and waits for the disk, before the window goes.
    fn flush(&self) {
        self.save_now();
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Flush(tx));
        if rx.recv_timeout(Duration::from_secs(10)).is_err() {
            eprintln!("nexus-notes: the last save did not finish in time");
        }
        if let Some(rel) = self.current.borrow().as_ref() {
            write_state("notes-last", rel);
        }
    }

    fn watch(self: &Rc<Self>, rel: &str) {
        let file = gio::File::for_path(self.root.join(format!("{rel}.md")));
        let monitor = file
            .monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            .ok();
        if let Some(m) = &monitor {
            let weak = Rc::downgrade(self);
            m.connect_changed(move |_, _, _, _| {
                let Some(n) = weak.upgrade() else { return };
                if let Some(t) = n.reload_timer.take() {
                    t.remove();
                }
                let w = Rc::downgrade(&n);
                n.reload_timer.replace(Some(glib::timeout_add_local_once(
                    Duration::from_millis(300),
                    move || {
                        let Some(n) = w.upgrade() else { return };
                        n.reload_timer.take();
                        let current = n.current.borrow().clone();
                        if let Some(rel) = current
                            && !n.editor.buffer.is_modified()
                            && n.wanted.borrow().is_none()
                        {
                            n.send(Cmd::Load(rel));
                        }
                    },
                )));
            });
        }
        self.monitor.replace(monitor);
    }

    /// Watches every folder of the tree: a page added, removed or renamed
    /// by another program (sync) rescans the tree. Saves of known pages do not.
    fn watch_dirs(self: &Rc<Self>) {
        let dirs: Vec<Rel> = self.tree.borrow().children.keys().cloned().collect();
        let mut watched = self.dirs.borrow_mut();
        watched.retain(|d, _| dirs.contains(d));
        for d in dirs {
            if watched.contains_key(&d) {
                continue;
            }
            let path = if d.is_empty() {
                self.root.clone()
            } else {
                self.root.join(&d)
            };
            let Ok(m) = gio::File::for_path(&path)
                .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            else {
                continue;
            };
            let weak = Rc::downgrade(self);
            m.connect_changed(move |_, file, other, event| {
                use gio::FileMonitorEvent as E;
                let Some(n) = weak.upgrade() else { return };
                // What a path is to the tree: a hidden file, `.order`, or a page and whether it is known.
                let page = |f: &gio::File| -> Option<(bool, bool)> {
                    let path = f.path()?;
                    let name = path.file_name()?.to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        return (name == ".order").then_some((true, true));
                    }
                    let rel = path
                        .strip_prefix(&n.root)
                        .ok()?
                        .to_string_lossy()
                        .into_owned();
                    let rel = rel.strip_suffix(".md").unwrap_or(&rel);
                    Some((false, n.tree.borrow().contains(rel)))
                };
                let (a, b) = (page(file), other.and_then(page));
                let order = a.is_some_and(|x| x.0) || b.is_some_and(|x| x.0);
                let rescan = order
                    || match event {
                        E::Created | E::MovedIn => a.is_some_and(|x| !x.1),
                        E::Deleted | E::MovedOut => a.is_some_and(|x| x.1),
                        E::Renamed => a.is_some_and(|x| x.1) || b.is_some_and(|x| !x.1),
                        _ => false,
                    };
                if !rescan {
                    return;
                }
                if let Some(t) = n.scan_timer.take() {
                    t.remove();
                }
                let w = Rc::downgrade(&n);
                n.scan_timer.replace(Some(glib::timeout_add_local_once(
                    Duration::from_millis(400),
                    move || {
                        if let Some(n) = w.upgrade() {
                            n.scan_timer.take();
                            n.send(Cmd::Scan);
                        }
                    },
                )));
            });
            watched.insert(d, m);
        }
    }

    fn reply(self: &Rc<Self>, reply: Reply) {
        match reply {
            Reply::Tree(tree) => {
                *self.tree.borrow_mut() = tree;
                self.rebuild();
                self.watch_dirs();
                if !self.scanned.replace(true) {
                    let last = self.last.take().filter(|l| self.tree.borrow().contains(l));
                    let first = last.or_else(|| self.tree.borrow().first().cloned());
                    match first {
                        Some(rel) => self.go(&rel, true, None),
                        None => self.page.set_visible_child_name("empty"),
                    }
                } else if self.current.borrow().is_none() && self.wanted.borrow().is_none() {
                    let first = self.tree.borrow().first().cloned();
                    match first {
                        Some(rel) => self.go(&rel, false, None),
                        None => self.page.set_visible_child_name("empty"),
                    }
                }
            }
            Reply::Loaded { rel, raw } => {
                let wanted = self.wanted.borrow().clone();
                let (front, body) = doc::split_front_matter(&raw);
                match wanted {
                    Some((w, focus, find)) if w == rel => {
                        self.wanted.replace(None);
                        self.front.replace(front.to_string());
                        self.current.replace(Some(rel.clone()));
                        let fresh =
                            body.trim().is_empty() && store::name(&rel).starts_with("Untitled");
                        self.editor
                            // A new page's title is empty: what is typed names it.
                            .load(if fresh { "" } else { store::name(&rel) }, body, fresh);
                        self.page.set_visible_child_name("page");
                        self.update_crumbs();
                        self.select_current();
                        self.watch(&rel);
                        if let Some(q) = find {
                            self.editor.find(&q);
                        }
                        if focus || fresh {
                            self.editor.view.grab_focus();
                        }
                        if self.rename_after.take() {
                            self.action("rename", None);
                        }
                    }
                    // Changed on disk while shown and not edited: show the new text.
                    None if self.current.borrow().as_deref() == Some(&rel)
                        && !self.editor.buffer.is_modified()
                        && raw != self.compose() =>
                    {
                        self.front.replace(front.to_string());
                        self.editor.reload(body);
                        self.update_crumbs();
                    }
                    _ => {}
                }
            }
            Reply::Missing(rel) => {
                // The page shown went away (sync, another program): nothing
                // unsaved is lost, as edits would have written it again.
                if self.wanted.borrow().is_none()
                    && self.current.borrow().as_deref() == Some(&rel)
                    && !self.editor.buffer.is_modified()
                {
                    self.current.replace(None);
                    self.monitor.replace(None);
                    self.page.set_visible_child_name("blank");
                    self.show_toast(
                        &format!(
                            "“{}” was moved or deleted by another program",
                            store::name(&rel)
                        ),
                        None,
                        6,
                    );
                    self.send(Cmd::Scan);
                }
                if self.wanted.borrow().as_ref().is_some_and(|w| w.0 == rel) {
                    self.wanted.replace(None);
                    self.show_toast(
                        &format!("“{}” is no longer there", store::name(&rel)),
                        None,
                        5,
                    );
                    self.send(Cmd::Scan);
                }
            }
            Reply::Created(rel) => self.go(&rel, true, None),
            Reply::Renamed { from, to } => {
                let moved = |r: &str| -> Option<String> {
                    if r == from {
                        Some(to.clone())
                    } else {
                        r.strip_prefix(&format!("{from}/"))
                            .map(|rest| format!("{to}/{rest}"))
                    }
                };
                let cur = self.current.borrow().clone();
                if let Some(new) = cur.as_deref().and_then(moved) {
                    self.current.replace(Some(new.clone()));
                    self.watch(&new);
                    self.update_crumbs();
                }
                let w = self.wanted.borrow().clone();
                if let Some((r, f, q)) = w
                    && let Some(new) = moved(&r)
                {
                    self.wanted.replace(Some((new, f, q)));
                }
                let exp: HashSet<Rel> = self
                    .expanded
                    .borrow()
                    .iter()
                    .map(|r| moved(r).unwrap_or_else(|| r.clone()))
                    .collect();
                self.expanded.replace(exp);
            }
            Reply::Deleted { rel, trash } => {
                let gone = self
                    .current
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| *c == rel || c.starts_with(&format!("{rel}/")));
                if gone {
                    self.current.replace(None);
                    self.monitor.replace(None);
                    self.page.set_visible_child_name("blank");
                }
                let weak = Rc::downgrade(self);
                self.show_toast(
                    &format!("Deleted “{}”", store::name(&rel)),
                    Some(Box::new(move || {
                        if let Some(n) = weak.upgrade() {
                            n.send(Cmd::Restore {
                                trash: trash.clone(),
                            });
                        }
                    })),
                    8,
                );
            }
            Reply::Conflict { rel, copy } => self.show_toast(
                &format!(
                    "“{}” was changed by another program. Its version is kept as “{}”.",
                    store::name(&rel),
                    store::name(&copy)
                ),
                None,
                12,
            ),
            Reply::NameTaken { rel, name } => {
                if self.current.borrow().as_deref() == Some(&rel) {
                    self.title_note.set_text(&taken(&name, store::name(&rel)));
                    self.title_note.set_visible(true);
                }
            }
            Reply::Trash(items) => self.show_trash(items),
            Reply::Error(e) => {
                if self.current.borrow().is_some() && e.starts_with("Could not save") {
                    self.editor.buffer.set_modified(true);
                }
                self.show_toast(&e, None, 10);
            }
        }
    }

    fn update_crumbs(&self) {
        let cur = self.current.borrow().clone().unwrap_or_default();
        let icon = self
            .tree
            .borrow()
            .icons
            .get(&cur)
            .and_then(|k| icon_name(k));
        match icon {
            Some(i) => self.icon_button.set_icon_name(i),
            None => self.icon_button.set_label("Add icon"),
        }
        self.check_title();
        let parents: Vec<&str> = cur.split('/').collect();
        let shown = parents[..parents.len().saturating_sub(1)].join("  /  ");
        self.crumbs.set_text(&shown);
    }

    /// Fills the tree again, keeping what was open.
    fn rebuild(&self) {
        let mut exp = self.expanded.borrow_mut();
        for i in 0..self.model.n_items() {
            if let Some(row) = self.model.row(i)
                && let Some(rel) = row_rel(row.item())
            {
                if row.is_expanded() {
                    exp.insert(rel);
                } else {
                    exp.remove(&rel);
                }
            }
        }
        self.syncing.set(true);
        let items: Vec<gtk::StringObject> = self
            .tree
            .borrow()
            .kids("")
            .iter()
            .map(|r| gtk::StringObject::new(r))
            .collect();
        self.roots.splice(0, self.roots.n_items(), &items);
        let mut i = 0;
        while i < self.model.n_items() {
            if let Some(row) = self.model.row(i)
                && row_rel(row.item()).is_some_and(|r| exp.contains(&r))
            {
                row.set_expanded(true);
            }
            i += 1;
        }
        drop(exp);
        self.syncing.set(false);
        self.select_current();
    }

    fn select_current(&self) {
        let Some(cur) = self.current.borrow().clone() else {
            return;
        };
        self.syncing.set(true);
        let mut prefix = String::new();
        let parts: Vec<&str> = cur.split('/').collect();
        let find = |rel: &str| {
            (0..self.model.n_items()).find(|i| {
                self.model
                    .row(*i)
                    .is_some_and(|r| row_rel(r.item()).as_deref() == Some(rel))
            })
        };
        for p in &parts[..parts.len() - 1] {
            prefix = store::join(&prefix, p);
            if let Some(row) = find(&prefix).and_then(|i| self.model.row(i)) {
                row.set_expanded(true);
            }
        }
        match find(&cur) {
            Some(i) => self.selection.set_selected(i),
            None => {
                self.selection.unselect_all();
            }
        }
        self.syncing.set(false);
    }

    fn run_search(self: &Rc<Self>) {
        use std::sync::atomic::Ordering;
        let q = self.search.text().trim().to_string();
        let mine = self.search_generation.fetch_add(1, Ordering::Relaxed) + 1;
        if q.is_empty() {
            self.side.set_visible_child_name("tree");
            return;
        }
        // What was typed a moment ago is on disk for the search to read.
        self.save_now();
        let (root, generation) = (self.root.clone(), self.search_generation.clone());
        let (tx, rx) = async_channel::bounded(1);
        let flush = mpsc::channel();
        self.send(Cmd::Flush(flush.0));
        std::thread::spawn(move || {
            let _ = flush.1.recv_timeout(Duration::from_secs(5));
            let hits = store::search(&root, &q, &generation, mine);
            let _ = tx.send_blocking(hits);
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(hits) = rx.recv().await else { return };
            let Some(n) = weak.upgrade() else { return };
            if n.search_generation.load(Ordering::Relaxed) == mine {
                n.show_results(hits);
            }
        });
    }

    fn show_results(&self, hits: Vec<Hit>) {
        while let Some(row) = self.results.row_at_index(0) {
            self.results.remove(&row);
        }
        let q = self.search.text().trim().to_string();
        for h in &hits {
            let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let title = gtk::Label::builder()
                .label(store::name(&h.rel))
                .xalign(0.)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["result-title"])
                .build();
            b.append(&title);
            let parent = store::parent(&h.rel);
            if !parent.is_empty() {
                b.append(
                    &gtk::Label::builder()
                        .label(parent.replace('/', " / "))
                        .xalign(0.)
                        .ellipsize(gtk::pango::EllipsizeMode::Middle)
                        .css_classes(["result-path"])
                        .build(),
                );
            }
            if !h.snippet.is_empty() {
                let snippet = gtk::Label::builder()
                    .xalign(0.)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .lines(2)
                    .wrap(true)
                    .css_classes(["snippet"])
                    .build();
                snippet.set_markup(&highlight(&h.snippet, &q));
                b.append(&snippet);
            }
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&b));
            row.set_widget_name(&h.rel);
            self.results.append(&row);
        }
        if hits.is_empty() {
            let none = gtk::Label::builder()
                .label(format!("No page has “{q}”"))
                .xalign(0.)
                .wrap(true)
                .css_classes(["side-note"])
                .build();
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&none));
            row.set_activatable(false);
            row.set_selectable(false);
            self.results.append(&row);
        }
        self.side.set_visible_child_name("results");
    }

    /// Says, under the title, when its name is taken by another page here.
    fn check_title(&self) {
        let Some(rel) = self.current.borrow().clone() else {
            return;
        };
        let name = store::file_name(&self.editor.title());
        let other = store::join(store::parent(&rel), &name);
        let clash =
            !name.is_empty() && name != store::name(&rel) && self.tree.borrow().contains(&other);
        if clash {
            self.title_note.set_text(&taken(&name, store::name(&rel)));
        }
        self.title_note.set_visible(clash);
    }

    fn icon_picker(self: &Rc<Self>) -> gtk::Popover {
        let weak = Rc::downgrade(self);
        let grid = gtk::FlowBox::builder()
            .max_children_per_line(5)
            .min_children_per_line(5)
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["icon-grid"])
            .build();
        let pop = gtk::Popover::new();
        for (key, icon) in ICONS {
            let b = gtk::Button::from_icon_name(icon);
            b.set_tooltip_text(Some(key));
            b.connect_clicked(glib::clone!(
                #[strong]
                weak,
                #[weak]
                pop,
                move |_| {
                    pop.popdown();
                    if let Some(n) = weak.upgrade() {
                        n.set_icon(Some(key));
                    }
                }
            ));
            grid.insert(&b, -1);
        }
        let remove = gtk::Button::with_label("Remove icon");
        remove.add_css_class("flat");
        remove.connect_clicked(glib::clone!(
            #[strong]
            weak,
            #[weak]
            pop,
            move |_| {
                pop.popdown();
                if let Some(n) = weak.upgrade() {
                    n.set_icon(None);
                }
            }
        ));
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        b.append(&grid);
        b.append(&remove);
        pop.set_child(Some(&b));
        pop.add_css_class("icons");
        pop
    }

    /// Sets the page's icon in its front matter, and saves.
    fn set_icon(&self, key: Option<&str>) {
        let Some(rel) = self.current.borrow().clone() else {
            return;
        };
        let front = doc::set_front(&self.front.borrow(), "icon", key);
        self.front.replace(front);
        {
            let mut tree = self.tree.borrow_mut();
            match key {
                Some(k) => tree.icons.insert(rel, k.to_string()),
                None => tree.icons.remove(&rel),
            };
        }
        self.editor.buffer.set_modified(true);
        self.save_now();
        self.update_crumbs();
        self.rebuild();
    }

    fn trash_view(self: &Rc<Self>) -> gtk::Box {
        let weak = Rc::downgrade(self);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        head.add_css_class("trash-head");
        let back = gtk::Button::builder().css_classes(["flat"]).build();
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        inner.append(&gtk::Image::from_icon_name("go-previous-symbolic"));
        inner.append(&gtk::Label::new(Some("Pages")));
        back.set_child(Some(&inner));
        back.set_hexpand(true);
        back.set_halign(gtk::Align::Start);
        back.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(n) = weak.upgrade() {
                    n.side.set_visible_child_name("tree");
                }
            }
        ));
        head.append(&back);
        // Emptying asks first: it cannot be undone.
        let confirm = gtk::Box::new(gtk::Orientation::Vertical, 8);
        confirm.add_css_class("confirm");
        confirm.append(
            &gtk::Label::builder()
                .label("Delete every page in the Trash for good?")
                .wrap(true)
                .max_width_chars(28)
                .build(),
        );
        let really = gtk::Button::with_label("Empty Trash");
        really.add_css_class("destructive-action");
        confirm.append(&really);
        let pop = gtk::Popover::builder().child(&confirm).build();
        really.connect_clicked(glib::clone!(
            #[strong]
            weak,
            #[weak]
            pop,
            move |_| {
                pop.popdown();
                if let Some(n) = weak.upgrade() {
                    n.send(Cmd::Purge(None));
                }
            }
        ));
        let empty = gtk::MenuButton::builder()
            .child(&gtk::Label::new(Some("Empty Trash")))
            .popover(&pop)
            .build();
        head.append(&empty);
        self.trash.add_css_class("trash");
        self.trash.set_selection_mode(gtk::SelectionMode::None);
        let none = gtk::Label::builder()
            .label("The Trash is empty. Deleted pages stay here for 30 days.")
            .wrap(true)
            .valign(gtk::Align::Start)
            .xalign(0.)
            .css_classes(["side-note"])
            .build();
        self.trash.set_placeholder(Some(&none));
        let scroll = gtk::ScrolledWindow::builder()
            .child(&self.trash)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let view = gtk::Box::new(gtk::Orientation::Vertical, 0);
        view.append(&head);
        view.append(&scroll);
        view
    }

    fn show_trash(self: &Rc<Self>, items: Vec<store::Trashed>) {
        while let Some(row) = self.trash.row_at_index(0) {
            self.trash.remove(&row);
        }
        let now = glib::real_time();
        for t in items {
            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.set_hexpand(true);
            text.append(
                &gtk::Label::builder()
                    .label(store::name(&t.rel))
                    .xalign(0.)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["result-title"])
                    .build(),
            );
            let days = (now - t.when) / (24 * 3600 * 1_000_000);
            let from = match store::parent(&t.rel) {
                "" => String::new(),
                p => format!("In {} · ", p.replace('/', " / ")),
            };
            let when = match days {
                0 => "deleted today".to_string(),
                1 => "deleted yesterday".to_string(),
                d => format!("deleted {d} days ago"),
            };
            text.append(
                &gtk::Label::builder()
                    .label(format!("{from}{when}"))
                    .xalign(0.)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["result-path"])
                    .build(),
            );
            let restore = gtk::Button::from_icon_name("edit-undo-symbolic");
            restore.set_tooltip_text(Some("Restore"));
            restore.add_css_class("flat");
            let forget = gtk::Button::from_icon_name("edit-delete-symbolic");
            forget.set_tooltip_text(Some("Delete Permanently"));
            forget.add_css_class("flat");
            for (button, purge) in [(&restore, false), (&forget, true)] {
                let weak = Rc::downgrade(self);
                let dir = t.dir.clone();
                button.connect_clicked(move |_| {
                    if let Some(n) = weak.upgrade() {
                        n.send(if purge {
                            Cmd::Purge(Some(dir.clone()))
                        } else {
                            Cmd::Restore { trash: dir.clone() }
                        });
                    }
                });
            }
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            row.append(&text);
            row.append(&restore);
            row.append(&forget);
            self.trash.append(&row);
        }
    }

    fn show_toast(&self, text: &str, undo: Option<Box<dyn Fn()>>, secs: u64) {
        let t = &self.toast;
        t.label.set_text(text);
        t.button.set_visible(undo.is_some());
        t.undo.replace(undo);
        t.revealer.set_reveal_child(true);
        if let Some(old) = t.timer.take() {
            old.remove();
        }
        let revealer = t.revealer.downgrade();
        let timer = t.timer.clone();
        t.timer.replace(Some(glib::timeout_add_local_once(
            Duration::from_secs(secs),
            move || {
                timer.take();
                if let Some(r) = revealer.upgrade() {
                    r.set_reveal_child(false);
                }
            },
        )));
    }
    fn hide_toast(&self) {
        if let Some(old) = self.toast.timer.take() {
            old.remove();
        }
        self.toast.revealer.set_reveal_child(false);
    }
}

fn taken(name: &str, kept: &str) -> String {
    format!(
        "A page named “{name}” is already here. Choose another title: the file keeps the name “{kept}” until then."
    )
}

fn row_rel(item: Option<glib::Object>) -> Option<Rel> {
    let item = item?;
    let item = match item.downcast::<gtk::TreeListRow>() {
        Ok(row) => row.item()?,
        Err(item) => item,
    };
    Some(item.downcast::<gtk::StringObject>().ok()?.string().into())
}

/// The snippet as Pango markup, the words searched for in bold.
fn highlight(text: &str, query: &str) -> String {
    let lower = text.to_lowercase();
    let mut marks = vec![false; text.len()];
    for w in query.to_lowercase().split_whitespace() {
        let mut from = 0;
        while let Some(i) = lower[from..].find(w) {
            let s = from + i;
            // Lowercasing can change lengths; only mark where bytes line up.
            if lower.len() == text.len() {
                marks[s..s + w.len()].iter_mut().for_each(|m| *m = true);
            }
            from = s + w.len().max(1);
        }
    }
    let mut out = String::new();
    let mut bold = false;
    for (i, c) in text.char_indices() {
        if marks[i] != bold {
            out.push_str(if marks[i] { "<b>" } else { "</b>" });
            bold = marks[i];
        }
        out.push_str(&glib::markup_escape_text(&c.to_string()));
    }
    if bold {
        out.push_str("</b>");
    }
    out
}
