//! A file browser window: places, a toolbar with the path, and the folder as
//! a grid or a list. In chooser mode the same window is the portal's dialog.
use crate::{
    listing,
    ops::{self, Loc},
    thumbs,
};
use gtk::{gdk, gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Instant,
};

/// Results of a search everywhere, at most.
const RESULTS: usize = 2000;
/// Portal filter: a name and (0 glob | 1 mime type, pattern) rules.
pub type Filter = (String, Vec<(u32, String)>);
type Act = fn(&Rc<Browser>);
type Done = Box<dyn FnOnce(Option<(Vec<PathBuf>, Option<usize>)>)>;

/// What the FileChooser portal asked for.
pub struct Chooser {
    pub title: String,
    pub accept: String,
    pub save: bool,
    pub multiple: bool,
    pub directory: bool,
    pub name: String,
    pub filters: Vec<Filter>,
    pub filter: Option<usize>,
    /// "wayland:<exported handle>" of the requesting window.
    pub parent: String,
    pub done: RefCell<Option<Done>>,
}
/// View preferences, shared by the windows of the session.
#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Name,
    Size,
    Modified,
}
thread_local! {
    static GRID: Cell<bool> = const { Cell::new(true) };
    /// Index into `TILES`: Ctrl+scroll grows and shrinks the grid.
    static ZOOM: Cell<usize> = const { Cell::new(1) };
    static HIDDEN: Cell<bool> = const { Cell::new(false) };
    static SORT: Cell<Sort> = const { Cell::new(Sort::Name) };
    /// Where the last file dialog finished.
    static LAST: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// What the filter lets through.
#[derive(Default)]
struct Shown {
    hidden: Cell<bool>,
    query: RefCell<String>,
    dirs_only: bool,
    rules: RefCell<Vec<(u32, String)>>,
}
impl Shown {
    fn visible(&self, info: &gio::FileInfo) -> bool {
        if !self.hidden.get() && (info.is_hidden() || info.is_backup()) {
            return false;
        }
        let query = self.query.borrow();
        if !query.is_empty() && !info.display_name().to_lowercase().contains(query.as_str()) {
            return false;
        }
        if info.file_type() == gio::FileType::Directory {
            return true;
        }
        let rules = self.rules.borrow();
        !self.dirs_only
            && (rules.is_empty()
                || rules.iter().any(|(kind, pattern)| match kind {
                    0 => glob(pattern, &info.display_name()),
                    _ => info
                        .attribute_string("standard::fast-content-type")
                        .is_some_and(|t| gio::content_type_is_mime_type(&t, pattern)),
                }))
    }
}
fn glob(pattern: &str, name: &str) -> bool {
    let (Ok(p), Ok(n)) = (
        std::ffi::CString::new(pattern),
        std::ffi::CString::new(name),
    ) else {
        return false;
    };
    // SAFETY: both are NUL-terminated strings.
    unsafe { libc::fnmatch(p.as_ptr(), n.as_ptr(), 0) == 0 }
}
fn is_dir(info: &gio::FileInfo) -> bool {
    info.file_type() == gio::FileType::Directory
}
pub fn loc_of(info: &gio::FileInfo) -> Option<Loc> {
    match info.attribute_string(ops::PHONE_PATH) {
        Some(p) => Some(Loc::Phone(p.into())),
        None => Some(Loc::Local(
            info.attribute_object("standard::file")
                .and_downcast::<gio::File>()?
                .path()?,
        )),
    }
}

/// The widgets of one recycled list or grid cell.
struct Cell_ {
    tile: gtk::Box,
    icon: gtk::Image,
    picture: gtk::Picture,
    name: gtk::Label,
    meta: Option<(gtk::Label, gtk::Label)>,
    thumb: RefCell<Option<Arc<AtomicBool>>>,
    hover: RefCell<Option<glib::SourceId>>,
}

pub struct Browser {
    app: gtk::Application,
    pub window: gtk::ApplicationWindow,
    loc: RefCell<Loc>,
    back: RefCell<Vec<Loc>>,
    forward: RefCell<Vec<Loc>>,
    /// The folder shown, and the results of a search everywhere.
    store: gio::ListStore,
    results: gio::ListStore,
    loading: Cell<bool>,
    /// Counts navigations, so that a late listing does not land in another folder.
    generation: Cell<u64>,
    watch: RefCell<Option<gio::FileMonitor>>,
    refreshing: Cell<bool>,
    everywhere: gtk::ToggleButton,
    /// The running search: its cancel flag and its `fd` process.
    searching: RefCell<Option<(Arc<AtomicBool>, Arc<AtomicU32>)>>,
    phone_root: RefCell<Option<String>>,
    phone_name: RefCell<Option<String>>,
    shown: Rc<Shown>,
    filter: gtk::CustomFilter,
    filtered: gtk::FilterListModel,
    sorter: gtk::MultiSorter,
    model: gtk::SortListModel,
    selection: gtk::SelectionModel,
    scroller: gtk::ScrolledWindow,
    band: gtk::DrawingArea,
    /// The rubber band being dragged, in the band's coordinates.
    band_rect: Rc<Cell<Option<[f64; 4]>>>,
    list: gtk::ListView,
    grid: gtk::GridView,
    path: gtk::Entry,
    search: gtk::SearchEntry,
    places: gtk::ListBox,
    place_locs: RefCell<Vec<Option<Loc>>>,
    status: gtk::Label,
    busy: RefCell<Option<String>>,
    menu: gtk::PopoverMenu,
    /// Names to select once they are listed.
    reveal: RefCell<Vec<String>>,
    chooser: Option<Rc<Chooser>>,
    name: gtk::Entry,
    filters: gtk::DropDown,
    monitor: gio::VolumeMonitor,
    handlers: RefCell<Vec<glib::SignalHandlerId>>,
}
impl Drop for Browser {
    fn drop(&mut self) {
        for id in self.handlers.take() {
            self.monitor.disconnect(id);
        }
    }
}

fn icon_button(icon: &str, tip: &str, action: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("flat");
    b.set_tooltip_text(Some(tip));
    b.set_action_name(Some(action));
    b
}
fn meta_label() -> gtk::Label {
    let l = gtk::Label::new(None);
    l.add_css_class("file-meta");
    l
}
fn date(info: &gio::FileInfo) -> String {
    let Some(t) = info
        .modification_date_time()
        .and_then(|t| t.to_local().ok())
    else {
        return String::new();
    };
    let now = glib::DateTime::now_local().ok();
    let format = if now.as_ref().is_some_and(|n| n.ymd() == t.ymd()) {
        "Today %H:%M"
    } else if now.is_some_and(|n| n.year() == t.year()) {
        "%e %b %H:%M"
    } else {
        "%e %b %Y"
    };
    t.format(format).map(Into::into).unwrap_or_default()
}

impl Browser {
    /// Opens a window on `dir`, selecting `select` once listed.
    pub fn open(
        app: &gtk::Application,
        dir: &Loc,
        select: &[String],
        chooser: Option<Chooser>,
    ) -> Rc<Self> {
        let b = Rc::new_cyclic(|weak: &Weak<Self>| Self::build(app, weak, chooser));
        b.connect(&Rc::downgrade(&b));
        // The window owns its browser; everything else holds it weakly.
        let strong = RefCell::new(Some(b.clone()));
        b.window.connect_close_request(move |_| {
            if let Some(b) = strong.take()
                && let Some(c) = &b.chooser
                && let Some(done) = c.done.take()
            {
                done(None);
            }
            glib::Propagation::Proceed
        });
        b.reveal.replace(select.to_vec());
        b.go(dir.clone(), false);
        b.rebuild_places();
        b.window.present();
        b
    }

    fn build(app: &gtk::Application, weak: &Weak<Self>, chooser: Option<Chooser>) -> Self {
        let chooser = chooser.map(Rc::new);
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title(chooser.as_ref().map_or("Files", |c| c.title.as_str()))
            .default_width(if chooser.is_some() { 900 } else { 1040 })
            .default_height(if chooser.is_some() { 580 } else { 660 })
            .decorated(false)
            .build();
        window.add_css_class("nexus");
        window.add_css_class("files");
        let shown = Rc::new(Shown {
            hidden: Cell::new(HIDDEN.get()),
            dirs_only: chooser.as_ref().is_some_and(|c| c.directory),
            rules: RefCell::new(
                chooser
                    .as_ref()
                    .and_then(|c| c.filters.get(c.filter.unwrap_or(0)))
                    .map(|f| f.1.clone())
                    .unwrap_or_default(),
            ),
            ..Default::default()
        });
        let s = shown.clone();
        let filter = gtk::CustomFilter::new(move |o| s.visible(o.downcast_ref().unwrap()));
        let store = gio::ListStore::new::<gio::FileInfo>();
        let results = gio::ListStore::new::<gio::FileInfo>();
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        let sorter = gtk::MultiSorter::new();
        sorter.append(gtk::CustomSorter::new(|a, b| {
            let dir = |o: &glib::Object| is_dir(o.downcast_ref().unwrap());
            dir(b).cmp(&dir(a)).into()
        }));
        sorter.append(key_sorter(SORT.get()));
        let model = gtk::SortListModel::new(Some(filtered.clone()), Some(sorter.clone()));
        let selection: gtk::SelectionModel = if chooser.as_ref().is_some_and(|c| !c.multiple) {
            let s = gtk::SingleSelection::new(Some(model.clone()));
            s.set_autoselect(false);
            s.set_can_unselect(true);
            s.upcast()
        } else {
            gtk::MultiSelection::new(Some(model.clone())).upcast()
        };
        let list = gtk::ListView::new(Some(selection.clone()), Some(factory(weak, false)));
        let grid = gtk::GridView::new(Some(selection.clone()), Some(factory(weak, true)));
        grid.set_max_columns(32);
        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .build();
        scroller.set_child(Some(if GRID.get() {
            grid.upcast_ref::<gtk::Widget>()
        } else {
            list.upcast_ref()
        }));

        // Toolbar: history, the editable path, the filter and the view.
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        toolbar.add_css_class("files-header");
        toolbar.append(&icon_button(
            "go-previous-symbolic",
            "Back (Alt+←)",
            "win.back",
        ));
        toolbar.append(&icon_button(
            "go-next-symbolic",
            "Forward (Alt+→)",
            "win.forward",
        ));
        toolbar.append(&icon_button(
            "go-up-symbolic",
            "Parent folder (Alt+↑)",
            "win.up",
        ));
        let path = gtk::Entry::new();
        path.add_css_class("path");
        path.set_hexpand(true);
        toolbar.append(&path);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Filter"));
        search.set_width_chars(14);
        search.set_key_capture_widget(Some(&window));
        toolbar.append(&search);
        let everywhere = gtk::ToggleButton::new();
        everywhere.set_icon_name("web-browser-symbolic");
        everywhere.set_tooltip_text(Some("Search everywhere, not only in this folder"));
        everywhere.set_action_name(Some("win.everywhere"));
        toolbar.append(&everywhere);
        let view_toggle = gtk::ToggleButton::new();
        view_toggle.set_icon_name("view-grid-symbolic");
        view_toggle.add_css_class("flat");
        view_toggle.set_tooltip_text(Some("Grid (Ctrl+G)"));
        view_toggle.set_action_name(Some("win.grid"));
        toolbar.append(&view_toggle);
        let view_menu = gio::Menu::new();
        let sorting = gio::Menu::new();
        sorting.append(Some("By name"), Some("win.sort::name"));
        sorting.append(Some("By size"), Some("win.sort::size"));
        sorting.append(Some("By date modified"), Some("win.sort::modified"));
        view_menu.append_section(None, &sorting);
        let more = gio::Menu::new();
        more.append(Some("Show hidden files"), Some("win.hidden"));
        if chooser.is_none() {
            more.append(Some("New window"), Some("win.new-window"));
        }
        view_menu.append_section(None, &more);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&view_menu)
            .build();
        menu_button.add_css_class("flat");
        toolbar.append(&menu_button);
        if chooser.is_none() {
            let close = icon_button("window-close-symbolic", "Close (Ctrl+W)", "window.close");
            toolbar.append(&close);
        }

        let mut child = toolbar.first_child();
        while let Some(c) = child {
            c.set_valign(gtk::Align::Center);
            child = c.next_sibling();
        }
        let places = gtk::ListBox::new();
        places.add_css_class("files-sidebar");
        let sidebar = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(190)
            .child(&places)
            .build();
        sidebar.add_css_class("files-sidebar");

        let status = gtk::Label::new(None);
        status.set_xalign(0.);
        status.set_hexpand(true);
        status.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        status.add_css_class("file-meta");
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        footer.add_css_class("footer");
        let name = gtk::Entry::new();
        let filters = gtk::DropDown::from_strings(&[]);
        if let Some(c) = &chooser {
            if c.save {
                name.set_text(&c.name);
                name.set_hexpand(true);
                name.set_placeholder_text(Some("Name"));
                footer.append(&name);
                status.set_hexpand(false);
                status.set_max_width_chars(30);
            }
            footer.append(&status);
            if !c.filters.is_empty() {
                let names: Vec<&str> = c.filters.iter().map(|f| f.0.as_str()).collect();
                filters.set_model(Some(&gtk::StringList::new(&names)));
                filters.set_selected(c.filter.unwrap_or(0) as u32);
                footer.append(&filters);
            }
            let cancel = gtk::Button::with_label("Cancel");
            cancel.set_action_name(Some("window.close"));
            footer.append(&cancel);
            let accept = gtk::Button::with_mnemonic(&c.accept);
            accept.add_css_class("suggested-action");
            accept.set_action_name(Some("win.accept"));
            footer.append(&accept);
        } else {
            footer.append(&status);
        }

        let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
        main.append(&toolbar);
        // The rubber band is drawn over the view, which never targets it.
        let band = gtk::DrawingArea::new();
        band.add_css_class("band");
        band.set_can_target(false);
        let band_rect: Rc<Cell<Option<[f64; 4]>>> = Rc::default();
        let r = band_rect.clone();
        band.set_draw_func(move |area, cr, _, _| {
            let Some([x, y, w, h]) = r.get() else { return };
            let c = area.color();
            let (red, green, blue) = (
                f64::from(c.red()),
                f64::from(c.green()),
                f64::from(c.blue()),
            );
            cr.rectangle(x + 0.5, y + 0.5, w, h);
            cr.set_source_rgba(red, green, blue, 0.15);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(red, green, blue, 0.9);
            cr.set_line_width(1.);
            let _ = cr.stroke();
        });
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scroller));
        overlay.add_overlay(&band);
        main.append(&overlay);
        main.append(&footer);
        let menu = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        main.append(&menu);
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&sidebar);
        body.append(&main);
        window.set_child(Some(&body));
        Self {
            app: app.clone(),
            window,
            loc: RefCell::new(Loc::Local(glib::home_dir())),
            back: RefCell::default(),
            forward: RefCell::default(),
            store,
            results,
            loading: Cell::new(false),
            generation: Cell::new(0),
            watch: RefCell::default(),
            refreshing: Cell::new(false),
            everywhere,
            searching: RefCell::default(),
            phone_root: RefCell::default(),
            phone_name: RefCell::default(),
            shown,
            filter,
            filtered,
            sorter,
            model,
            selection,
            scroller,
            band,
            band_rect,
            list,
            grid,
            path,
            search,
            places,
            place_locs: RefCell::default(),
            status,
            busy: RefCell::default(),
            menu,
            reveal: RefCell::default(),
            chooser,
            name,
            filters,
            monitor: gio::VolumeMonitor::get(),
            handlers: RefCell::default(),
        }
    }

    /// Signals and actions; they hold the browser weakly.
    fn connect(&self, weak: &Weak<Self>) {
        let w = weak.clone();
        let on = move |f: fn(&Rc<Self>)| {
            let w = w.clone();
            move || {
                if let Some(b) = w.upgrade() {
                    f(&b)
                }
            }
        };
        let plain: [(&str, Act); 22] = [
            ("back", |b| b.history(true)),
            ("forward", |b| b.history(false)),
            ("up", Self::up),
            ("location", |b| {
                b.path.grab_focus();
                b.path.select_region(0, -1);
            }),
            ("new-folder", Self::new_folder),
            ("new-window", |b| {
                Self::open(&b.app, &b.loc.borrow(), &[], None);
            }),
            ("open", |b| b.open_selected()),
            ("copy", |b| b.to_clipboard(false)),
            ("cut", |b| b.to_clipboard(true)),
            ("paste", Self::paste),
            ("rename", Self::rename),
            ("trash", Self::trash),
            ("delete", Self::delete),
            ("restore", |b| {
                let paths = b.selected_local();
                b.background("Restoring", move |_| ops::restore(paths));
            }),
            ("empty-trash", |b| {
                b.selection.select_all();
                b.delete();
            }),
            ("terminal", Self::terminal),
            ("reveal", |b| {
                if let [Loc::Local(path)] = &b.selected_locs()[..]
                    && let Some(parent) = path.parent()
                {
                    b.reveal.replace(vec![Loc::Local(path.clone()).name()]);
                    b.go(Loc::Local(parent.to_path_buf()), true);
                }
            }),
            ("compress", Self::compress),
            ("send-to-phone", Self::send_to_phone),
            ("accept", Self::accept),
            ("escape", |b| {
                if !b.search.text().is_empty() {
                    b.search.set_text("");
                } else if b.chooser.is_some() {
                    b.window.close();
                } else {
                    b.selection.unselect_all();
                }
            }),
            ("focus-view", |b| b.focus_view()),
        ];
        for (name, f) in plain {
            let action = gio::SimpleAction::new(name, None);
            let run = on(f);
            action.connect_activate(move |_, _| run());
            self.window.add_action(&action);
        }
        let w = weak.clone();
        let hidden =
            gio::SimpleAction::new_stateful("hidden", None, &self.shown.hidden.get().to_variant());
        hidden.connect_change_state(move |a, v| {
            let (Some(b), Some(on)) = (w.upgrade(), v.and_then(|v| v.get::<bool>())) else {
                return;
            };
            a.set_state(&on.to_variant());
            HIDDEN.set(on);
            b.shown.hidden.set(on);
            b.filter.changed(gtk::FilterChange::Different);
        });
        let w = weak.clone();
        let grid = gio::SimpleAction::new_stateful("grid", None, &GRID.get().to_variant());
        grid.connect_change_state(move |a, v| {
            let (Some(b), Some(on)) = (w.upgrade(), v.and_then(|v| v.get::<bool>())) else {
                return;
            };
            a.set_state(&on.to_variant());
            GRID.set(on);
            b.set_grid(on);
        });
        let w = weak.clone();
        let sort = gio::SimpleAction::new_stateful(
            "sort",
            Some(glib::VariantTy::STRING),
            &"name".to_variant(),
        );
        sort.connect_change_state(move |a, v| {
            let (Some(b), Some(key)) = (w.upgrade(), v.and_then(|v| v.str().map(String::from)))
            else {
                return;
            };
            a.set_state(&key.to_variant());
            let by = match key.as_str() {
                "size" => Sort::Size,
                "modified" => Sort::Modified,
                _ => Sort::Name,
            };
            SORT.set(by);
            b.sorter.remove(1);
            b.sorter.append(key_sorter(by));
        });
        sort.set_state(
            &match SORT.get() {
                Sort::Name => "name",
                Sort::Size => "size",
                Sort::Modified => "modified",
            }
            .to_variant(),
        );
        let w = weak.clone();
        let open_with = gio::SimpleAction::new("open-with", Some(glib::VariantTy::STRING));
        open_with.connect_activate(move |_, v| {
            let (Some(b), Some(id)) = (w.upgrade(), v.and_then(|v| v.str().map(String::from)))
            else {
                return;
            };
            let files: Vec<gio::File> =
                b.selected_local().iter().map(gio::File::for_path).collect();
            if let Some(app) = gio::DesktopAppInfo::new(&id)
                && let Err(e) = app.launch(
                    &files,
                    Some(&WidgetExt::display(&b.window).app_launch_context()),
                )
            {
                b.error(&e.to_string());
            }
        });
        let w = weak.clone();
        let extract = gio::SimpleAction::new("extract", Some(glib::VariantTy::BOOLEAN));
        extract.connect_activate(move |_, v| {
            let (Some(b), Some(folder)) = (w.upgrade(), v.and_then(|v| v.get::<bool>())) else {
                return;
            };
            b.extract(b.selected_local(), folder);
        });
        let w = weak.clone();
        let everywhere = gio::SimpleAction::new_stateful("everywhere", None, &false.to_variant());
        everywhere.connect_change_state(move |a, v| {
            let (Some(b), Some(on)) = (w.upgrade(), v.and_then(|v| v.get::<bool>())) else {
                return;
            };
            a.set_state(&on.to_variant());
            b.search
                .set_placeholder_text(Some(if on { "Search everywhere" } else { "Filter" }));
            b.search_everywhere();
        });
        self.window.add_action(&everywhere);
        for a in [&hidden, &grid, &sort] {
            self.window.add_action(a);
        }
        self.window.add_action(&open_with);
        self.window.add_action(&extract);

        // Keys that belong to the view, not to the entries.
        let keys = gtk::ShortcutController::new();
        for (trigger, action) in [
            ("<Control>c", "win.copy"),
            ("<Control>x", "win.cut"),
            ("<Control>v", "win.paste"),
            ("Delete", "win.trash"),
            ("KP_Delete", "win.trash"),
            ("<Shift>Delete", "win.delete"),
            ("F2", "win.rename"),
            ("BackSpace", "win.up"),
        ] {
            keys.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(gtk::NamedAction::new(action)),
            ));
        }
        self.scroller.add_controller(keys);
        let escape = gtk::ShortcutController::new();
        escape.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::NamedAction::new("win.escape")),
        ));
        self.window.add_controller(escape);

        // Mouse back and forward buttons.
        let buttons = gtk::GestureClick::new();
        buttons.set_button(0);
        let w = weak.clone();
        buttons.connect_pressed(move |g, _, _, _| {
            if let Some(b) = w.upgrade() {
                match g.current_button() {
                    8 => b.history(true),
                    9 => b.history(false),
                    _ => {}
                }
            }
        });
        self.window.add_controller(buttons);

        // A click outside the items clears the selection; dragging from there
        // draws the rubber band.
        for view in [
            self.list.upcast_ref::<gtk::Widget>(),
            self.grid.upcast_ref(),
        ] {
            let click = gtk::GestureClick::new();
            click.set_propagation_phase(gtk::PropagationPhase::Capture);
            let w = weak.clone();
            click.connect_pressed(move |g, _, x, y| {
                let (Some(b), Some(view)) = (w.upgrade(), g.widget()) else {
                    return;
                };
                let modifiers = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
                if g.current_event_state().intersects(modifiers) {
                    return;
                }
                let mut picked = view.pick(x, y, gtk::PickFlags::DEFAULT);
                while let Some(p) = picked.filter(|p| *p != view) {
                    if matches!(p.css_name().as_str(), "row" | "child") {
                        return;
                    }
                    picked = p.parent();
                }
                b.selection.unselect_all();
            });
            view.add_controller(click);
            if self.chooser.as_ref().is_none_or(|c| c.multiple) {
                rubber_band(weak, view);
            }
        }

        // Right click on empty space: the folder's menu.
        let empty = gtk::GestureClick::new();
        empty.set_button(3);
        let w = weak.clone();
        empty.connect_pressed(move |g, _, x, y| {
            if let Some(b) = w.upgrade() {
                g.set_state(gtk::EventSequenceState::Claimed);
                b.selection.unselect_all();
                b.popup(b.scroller.upcast_ref(), x, y);
            }
        });
        self.scroller.add_controller(empty);

        // Ctrl+scroll: bigger or smaller tiles.
        let zoom = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        zoom.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = weak.clone();
        zoom.connect_scroll(move |c, _, dy| {
            if !c
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK)
            {
                return glib::Propagation::Proceed;
            }
            let level = ZOOM
                .get()
                .saturating_add_signed(if dy < 0. { 1 } else { -1 })
                .min(TILES.len() - 1);
            if level != ZOOM.get() {
                ZOOM.set(level);
                if let Some(b) = w.upgrade() {
                    // New cells at the new size; the visible ones are rebuilt.
                    b.grid.set_factory(Some(&factory(&w, true)));
                }
            }
            glib::Propagation::Stop
        });
        self.scroller.add_controller(zoom);

        // Files dropped from other applications or windows are copied here.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let w = weak.clone();
        drop.connect_drop(move |_, value, _, _| {
            let (Some(b), Ok(list)) = (w.upgrade(), value.get::<gdk::FileList>()) else {
                return false;
            };
            let here = b.loc.borrow().clone();
            let items: Vec<Loc> = list
                .files()
                .iter()
                .filter_map(|f| f.path())
                .filter(|p| Loc::Local(p.parent().unwrap_or(p).to_path_buf()) != here)
                .map(Loc::Local)
                .collect();
            if !items.is_empty() {
                b.transfer(items, false);
            }
            true
        });
        self.scroller.add_controller(drop);
        // Or on a place in the sidebar: the phone, a drive, a bookmark.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let w = weak.clone();
        let place = move |y: f64| {
            let b = w.upgrade()?;
            let row = b.places.row_at_y(y as i32)?;
            let loc = b
                .place_locs
                .borrow()
                .get(row.index() as usize)
                .cloned()
                .flatten()?;
            Some((b, row, loc))
        };
        let p = place.clone();
        drop.connect_motion(move |t, _, y| match p(y) {
            Some((b, row, _)) => {
                b.places.drag_highlight_row(&row);
                gdk::DragAction::COPY
            }
            None => {
                if let Some(list) = t.widget().and_downcast::<gtk::ListBox>() {
                    list.drag_unhighlight_row();
                }
                gdk::DragAction::empty()
            }
        });
        drop.connect_leave(|t| {
            if let Some(list) = t.widget().and_downcast::<gtk::ListBox>() {
                list.drag_unhighlight_row();
            }
        });
        drop.connect_drop(move |_, value, _, y| {
            let (Some((b, _, dest)), Ok(list)) = (place(y), value.get::<gdk::FileList>()) else {
                return false;
            };
            b.places.drag_unhighlight_row();
            let items = list
                .files()
                .iter()
                .filter_map(|f| f.path())
                .map(Loc::Local)
                .collect();
            b.copy_to(items, dest);
            true
        });
        self.places.add_controller(drop);

        let w = weak.clone();
        let activate = move |pos: u32| {
            if let Some(b) = w.upgrade() {
                b.activate(pos);
            }
        };
        let a = activate.clone();
        self.list.connect_activate(move |_, pos| a(pos));
        self.grid.connect_activate(move |_, pos| activate(pos));

        let w = weak.clone();
        self.path.connect_activate(move |entry| {
            if let Some(b) = w.upgrade() {
                b.go_to_text(&entry.text());
            }
        });
        let w = weak.clone();
        self.search.connect_search_changed(move |s| {
            if let Some(b) = w.upgrade() {
                b.shown.query.replace(s.text().to_lowercase());
                b.filter.changed(gtk::FilterChange::Different);
                b.search_everywhere();
                if b.model.n_items() > 0 {
                    b.selection.select_item(0, true);
                }
            }
        });
        let w = weak.clone();
        self.search.connect_activate(move |_| {
            if let Some(b) = w.upgrade() {
                b.open_selected();
            }
        });
        let w = weak.clone();
        self.search.connect_stop_search(move |_| {
            if let Some(b) = w.upgrade() {
                WidgetExt::activate_action(&b.window, "win.escape", None).ok();
            }
        });
        let w = weak.clone();
        self.places.connect_row_activated(move |_, row| {
            let Some(b) = w.upgrade() else { return };
            let loc = b
                .place_locs
                .borrow()
                .get(row.index() as usize)
                .cloned()
                .flatten();
            if let Some(loc) = loc {
                b.go(loc, true);
            }
        });
        let w = weak.clone();
        self.model.connect_items_changed(move |_, _, _, _| {
            if let Some(b) = w.upgrade() {
                b.reveal_pending();
                b.update_status();
            }
        });
        let w = weak.clone();
        self.selection.connect_selection_changed(move |_, _, _| {
            if let Some(b) = w.upgrade() {
                b.update_status();
                b.selection_to_name();
                // The keyboard is on a folder: it may be opened next.
                if let [info] = &b.selected()[..]
                    && is_dir(info)
                    && let Some(loc) = loc_of(info)
                {
                    listing::prefetch(loc, GRID.get());
                }
            }
        });
        let w = weak.clone();
        self.filters.connect_selected_notify(move |d| {
            let Some(b) = w.upgrade() else { return };
            let rules = b
                .chooser
                .as_ref()
                .and_then(|c| c.filters.get(d.selected() as usize));
            b.shown
                .rules
                .replace(rules.map(|f| f.1.clone()).unwrap_or_default());
            b.filter.changed(gtk::FilterChange::Different);
        });
        let mut handlers = self.handlers.borrow_mut();
        for signal in ["mount-added", "mount-removed", "mount-changed"] {
            let w = weak.clone();
            handlers.push(self.monitor.connect_local(signal, false, move |_| {
                if let Some(b) = w.upgrade() {
                    b.rebuild_places();
                }
                None
            }));
        }
        if let Some(c) = &self.chooser {
            if c.save {
                self.name.connect_activate(|e| {
                    e.activate_action("win.accept", None).ok();
                });
                // Select the name without its extension, ready to type over.
                let stem = c.name.rfind('.').filter(|&i| i > 0).unwrap_or(c.name.len());
                let (name, stem) = (self.name.clone(), c.name[..stem].chars().count() as i32);
                self.window.connect_map(move |_| {
                    name.grab_focus();
                    name.select_region(0, stem);
                });
            }
            if let Some(handle) = c.parent.strip_prefix("wayland:").map(String::from) {
                self.window.connect_realize(move |w| set_parent(w, &handle));
            }
        }
        // A connected phone shows among the places; a dialog that opens
        // files offers its files too.
        if self
            .chooser
            .as_ref()
            .is_none_or(|c| !c.save && !c.directory)
        {
            let w = weak.clone();
            let poll = move || {
                let w = w.clone();
                glib::spawn_future_local(async move {
                    let name = background(ops::phone_name).await;
                    if let Some(b) = w.upgrade()
                        && *b.phone_name.borrow() != name
                    {
                        b.phone_name.replace(name);
                        b.rebuild_places();
                    }
                });
            };
            poll();
            let w = weak.clone();
            glib::timeout_add_seconds_local(5, move || {
                if w.upgrade().is_none() {
                    return glib::ControlFlow::Break;
                }
                poll();
                glib::ControlFlow::Continue
            });
        }
    }

    fn rebuild_places(&self) {
        while let Some(row) = self.places.row_at_index(0) {
            self.places.remove(&row);
        }
        let mut locs = Vec::new();
        let section = |title: &str, locs: &mut Vec<Option<Loc>>| {
            let l = gtk::Label::new(Some(title));
            l.set_xalign(0.);
            l.add_css_class("sidebar-section");
            let row = gtk::ListBoxRow::builder()
                .child(&l)
                .activatable(false)
                .selectable(false)
                .build();
            self.places.append(&row);
            locs.push(None);
        };
        let add = |name: &str, icon: &str, loc: Loc, locs: &mut Vec<Option<Loc>>| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let image = gtk::Image::from_icon_name(icon);
            image.add_css_class("place-icon");
            image.set_pixel_size(14);
            row.append(&image);
            let l = gtk::Label::new(Some(name));
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            row.append(&l);
            self.places.append(&row);
            locs.push(Some(loc));
        };
        section("Places", &mut locs);
        let home = glib::home_dir();
        add(
            "Home",
            "user-home-symbolic",
            Loc::Local(home.clone()),
            &mut locs,
        );
        use glib::UserDirectory::*;
        for (dir, name, icon) in [
            (Desktop, "Desktop", "user-desktop-symbolic"),
            (Documents, "Documents", "folder-documents-symbolic"),
            (Downloads, "Downloads", "folder-download-symbolic"),
            (Pictures, "Pictures", "folder-pictures-symbolic"),
            (Music, "Music", "folder-music-symbolic"),
            (Videos, "Videos", "folder-videos-symbolic"),
        ] {
            if let Some(p) = glib::user_special_dir(dir).filter(|p| *p != home && p.is_dir()) {
                add(name, icon, Loc::Local(p), &mut locs);
            }
        }
        if self.chooser.is_none() {
            add(
                "Trash",
                "user-trash-symbolic",
                Loc::Local(ops::trash_dir()),
                &mut locs,
            );
        }
        let bookmarks = std::fs::read_to_string(glib::user_config_dir().join("gtk-3.0/bookmarks"))
            .unwrap_or_default();
        let bookmarks: Vec<(PathBuf, String)> = bookmarks
            .lines()
            .filter_map(|line| {
                let (uri, label) = line.split_once(' ').unwrap_or((line, ""));
                let path = glib::filename_from_uri(uri).ok()?.0;
                let label = if label.is_empty() {
                    Loc::Local(path.clone()).name()
                } else {
                    label.into()
                };
                Some((path, label))
            })
            .collect();
        if !bookmarks.is_empty() {
            section("Bookmarks", &mut locs);
            for (path, label) in bookmarks {
                add(&label, "folder-symbolic", Loc::Local(path), &mut locs);
            }
        }
        section("Devices", &mut locs);
        add(
            "File System",
            "drive-harddisk-symbolic",
            Loc::Local("/".into()),
            &mut locs,
        );
        for mount in self.monitor.mounts() {
            if let Some(path) = mount.root().path() {
                add(
                    &mount.name(),
                    "drive-removable-media-symbolic",
                    Loc::Local(path),
                    &mut locs,
                );
            }
        }
        if let Some(name) = self.phone_name.borrow().as_deref() {
            add(name, "phone-symbolic", Loc::Phone(String::new()), &mut locs);
        }
        self.place_locs.replace(locs);
        self.highlight_place();
    }
    fn highlight_place(&self) {
        let loc = self.loc.borrow();
        let root = self.phone_root.borrow();
        let index = self
            .place_locs
            .borrow()
            .iter()
            .position(|l| match (l, &*loc) {
                (Some(Loc::Phone(p)), Loc::Phone(here)) => {
                    p.is_empty() && Some(here) == root.as_ref()
                }
                (Some(l), here) => l == here,
                _ => false,
            });
        match index.and_then(|i| self.places.row_at_index(i as i32)) {
            Some(row) => self.places.select_row(Some(&row)),
            None => self.places.unselect_all(),
        }
    }

    fn go(self: &Rc<Self>, loc: Loc, record: bool) {
        let old = self.loc.replace(loc.clone());
        if record && old != loc {
            self.back.borrow_mut().push(old);
            self.forward.borrow_mut().clear();
        }
        self.search.set_text("");
        self.stop_search();
        self.filtered.set_model(Some(&self.store));
        self.busy.take();
        self.watch.take();
        self.path.set_text(&match &loc {
            Loc::Local(p) => p.to_string_lossy().into_owned(),
            Loc::Phone(p) => format!("phone:{p}"),
        });
        // A known folder shows at once; the fresh listing then reconciles it.
        self.loading.set(true);
        match listing::cached(&loc) {
            Some((listing, _)) => self.store.splice(0, self.store.n_items(), &listing),
            None => self.store.remove_all(),
        }
        self.load(true);
        if let Loc::Local(p) = &loc
            && let Ok(watch) = gio::File::for_path(p)
                .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
        {
            let w = Rc::downgrade(self);
            watch.connect_changed(move |_, _, _, _| {
                if let Some(b) = w.upgrade() {
                    b.refresh();
                }
            });
            self.watch.replace(Some(watch));
        }
        self.set_title();
        self.enable("back", !self.back.borrow().is_empty());
        self.enable("forward", !self.forward.borrow().is_empty());
        self.highlight_place();
        self.update_status();
        self.focus_view();
    }
    fn set_title(&self) {
        if self.chooser.is_some() {
            return;
        }
        let title = match &*self.loc.borrow() {
            Loc::Phone(p) if p.is_empty() || Some(p) == self.phone_root.borrow().as_ref() => {
                self.phone_name.borrow().clone().unwrap_or("Phone".into())
            }
            l => l.name(),
        };
        self.window.set_title(Some(&title));
    }
    /// Lists the current folder; into an empty view as entries come, or
    /// reconciled with what it shows.
    fn load(self: &Rc<Self>, navigated: bool) {
        let generation = self.generation.get() + u64::from(navigated);
        self.generation.set(generation);
        let streamed = self.store.n_items() == 0;
        self.loading.set(true);
        let (w, loc) = (Rc::downgrade(self), self.loc.borrow().clone());
        let current = move || w.upgrade().filter(|b| b.generation.get() == generation);
        glib::spawn_future_local(async move {
            let c = current.clone();
            let listed = listing::list(loc.clone(), move |batch| {
                if let (true, Some(b)) = (streamed, c()) {
                    b.store.extend_from_slice(batch);
                }
            })
            .await;
            let Some(b) = current() else { return };
            b.loading.set(false);
            match listed {
                Ok((canonical, listing)) => {
                    if !streamed {
                        listing::reconcile(&b.store, &listing);
                    }
                    if let (Loc::Phone(asked), Loc::Phone(root)) = (&loc, &canonical)
                        && asked.is_empty()
                    {
                        b.phone_root.replace(Some(root.clone()));
                        b.loc.replace(canonical.clone());
                        b.path.set_text(&format!("phone:{root}"));
                        b.highlight_place();
                        b.set_title();
                    }
                    b.reveal_pending();
                }
                Err(e) => b.error(&e),
            }
            b.update_status();
        });
    }
    /// Lists the folder again, once for a burst of changes.
    fn refresh(self: &Rc<Self>) {
        if self.refreshing.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
            if let Some(b) = w.upgrade() {
                b.refreshing.set(false);
                b.load(false);
            }
        });
    }
    fn history(self: &Rc<Self>, back: bool) {
        let (from, to) = if back {
            (&self.back, &self.forward)
        } else {
            (&self.forward, &self.back)
        };
        let Some(loc) = from.borrow_mut().pop() else {
            return;
        };
        to.borrow_mut().push(self.loc.borrow().clone());
        self.go(loc, false);
    }
    fn up(self: &Rc<Self>) {
        let loc = self.loc.borrow().clone();
        let parent = match &loc {
            Loc::Local(p) => p.parent().map(|p| Loc::Local(p.to_path_buf())),
            Loc::Phone(p) if Some(p) != self.phone_root.borrow().as_ref() && !p.is_empty() => p
                .rsplit_once('/')
                .map(|(parent, _)| Loc::Phone(parent.into())),
            Loc::Phone(_) => None,
        };
        if let Some(parent) = parent {
            self.reveal.replace(vec![loc.name()]);
            self.go(parent, true);
        }
    }
    fn go_to_text(self: &Rc<Self>, text: &str) {
        let text = text.trim();
        if let Some(p) = text.strip_prefix("phone:") {
            return self.go(Loc::Phone(p.trim_end_matches('/').into()), true);
        }
        let path = if let Some(rest) = text.strip_prefix('~') {
            glib::home_dir().join(rest.trim_start_matches('/'))
        } else if text.contains("://") {
            gio::File::for_uri(text).path().unwrap_or_default()
        } else {
            match &*self.loc.borrow() {
                Loc::Local(here) => here.join(text),
                Loc::Phone(_) => PathBuf::from(text),
            }
        };
        if path.is_dir() {
            self.go(Loc::Local(path), true);
        } else if let (true, Some(parent)) = (path.exists(), path.parent()) {
            self.reveal.replace(vec![Loc::Local(path.clone()).name()]);
            self.go(Loc::Local(parent.to_path_buf()), true);
        } else {
            self.error(&format!("{} does not exist", path.display()));
        }
    }
    /// With "Everywhere" on, the filter searches the whole computer by name;
    /// results stream in from a background `fd`.
    fn search_everywhere(self: &Rc<Self>) {
        self.stop_search();
        self.results.remove_all();
        let query = self.search.text().trim().to_owned();
        if !self.everywhere.is_active() || query.is_empty() {
            self.filtered.set_model(Some(&self.store));
            return self.update_status();
        }
        self.filtered.set_model(Some(&self.results));
        let cancel = Arc::new(AtomicBool::new(false));
        let pid = Arc::new(AtomicU32::new(0));
        self.searching.replace(Some((cancel.clone(), pid.clone())));
        let (tx, rx) = async_channel::unbounded();
        let c = cancel.clone();
        std::thread::spawn(move || {
            ops::search(&query, RESULTS, &c, &pid, &|batch| {
                tx.send_blocking(batch).is_ok()
            });
        });
        self.busy.replace(Some("Searching everywhere…".into()));
        self.update_status();
        let w = Rc::downgrade(self);
        let current = move || {
            w.upgrade().filter(|b| {
                b.searching
                    .borrow()
                    .as_ref()
                    .is_some_and(|s| Arc::ptr_eq(&s.0, &cancel))
            })
        };
        glib::spawn_future_local(async move {
            while let Ok(batch) = rx.recv().await {
                let Some(b) = current() else { return };
                let infos: Vec<gio::FileInfo> = batch.iter().map(ops::found_info).collect();
                b.results.extend_from_slice(&infos);
            }
            if let Some(b) = current() {
                b.busy.take();
                b.update_status();
            }
        });
    }
    fn stop_search(&self) {
        let Some((cancel, pid)) = self.searching.take() else {
            return;
        };
        cancel.store(true, Ordering::Relaxed);
        let pid = pid.load(Ordering::Relaxed);
        if pid != 0 {
            // SAFETY: plain kill(2); the process is not reaped before its reader stops.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        self.busy.take();
    }
    fn searching_everywhere(&self) -> bool {
        self.filtered.model().is_some_and(|m| m == self.results)
    }
    fn set_grid(&self, on: bool) {
        let show: &gtk::Widget = if on {
            self.grid.upcast_ref()
        } else {
            self.list.upcast_ref()
        };
        self.scroller.set_child(Some(show));
        self.focus_view();
    }
    fn view(&self) -> gtk::Widget {
        self.scroller
            .child()
            .unwrap_or_else(|| self.list.clone().upcast())
    }
    fn focus_view(&self) {
        if !self.chooser.as_ref().is_some_and(|c| c.save) {
            self.view().grab_focus();
        }
    }
    fn enable(&self, action: &str, on: bool) {
        if let Some(a) = self
            .window
            .lookup_action(action)
            .and_downcast::<gio::SimpleAction>()
        {
            a.set_enabled(on);
        }
    }
    fn info(&self, pos: u32) -> Option<gio::FileInfo> {
        self.model.item(pos).and_downcast()
    }
    fn selected(&self) -> Vec<gio::FileInfo> {
        let set = self.selection.selection();
        (0..set.size() as u32)
            .filter_map(|i| self.info(set.nth(i)))
            .collect()
    }
    fn selected_locs(&self) -> Vec<Loc> {
        self.selected().iter().filter_map(loc_of).collect()
    }
    fn selected_local(&self) -> Vec<PathBuf> {
        self.selected_locs()
            .into_iter()
            .filter_map(|l| match l {
                Loc::Local(p) => Some(p),
                Loc::Phone(_) => None,
            })
            .collect()
    }
    fn local_dir(&self) -> Option<PathBuf> {
        match &*self.loc.borrow() {
            Loc::Local(p) => Some(p.clone()),
            Loc::Phone(_) => None,
        }
    }
    fn in_trash(&self) -> bool {
        self.local_dir()
            .is_some_and(|p| p.starts_with(ops::trash_dir()))
    }
    fn reveal_pending(&self) {
        if self.reveal.borrow().is_empty() {
            return;
        }
        let mut first = None;
        for pos in 0..self.model.n_items() {
            let Some(info) = self.info(pos) else { continue };
            let name = info.name().to_string_lossy().into_owned();
            let mut reveal = self.reveal.borrow_mut();
            if let Some(i) = reveal.iter().position(|n| *n == name) {
                reveal.swap_remove(i);
                self.selection.select_item(pos, first.is_none());
                first.get_or_insert(pos);
            }
        }
        if let Some(pos) = first {
            self.view()
                .activate_action("list.scroll-to-item", Some(&pos.to_variant()))
                .ok();
        }
        if !self.loading.get() {
            self.reveal.borrow_mut().clear();
        }
    }
    fn update_status(&self) {
        if let Some(busy) = &*self.busy.borrow() {
            self.status.set_text(busy);
            return;
        }
        self.status.remove_css_class("error");
        let n = self.model.n_items();
        let selected = self.selection.selection().size();
        let mut text = match n {
            _ if self.searching_everywhere() => format!(
                "{n} result{}{}",
                if n == 1 { "" } else { "s" },
                if n as usize >= RESULTS {
                    " (first ones only)"
                } else {
                    ""
                }
            ),
            0 if self.loading.get() => "Loading…".to_owned(),
            1 => "1 item".to_owned(),
            n => format!("{n} items"),
        };
        if selected > 0 {
            let size: i64 = self
                .selected()
                .iter()
                .filter(|i| !is_dir(i))
                .map(|i| i.size())
                .sum();
            text += &format!(", {selected} selected");
            if size > 0 {
                text += &format!(" ({})", glib::format_size(size as u64));
            }
        }
        self.status.set_text(&text);
    }
    fn error(&self, message: &str) {
        self.status.set_text(message);
        self.status.add_css_class("error");
    }
    /// In a save dialog, picking a file proposes its name.
    fn selection_to_name(&self) {
        if self.chooser.as_ref().is_some_and(|c| c.save)
            && let [info] = &self.selected()[..]
            && !is_dir(info)
        {
            self.name.set_text(&info.display_name());
        }
    }

    fn activate(self: &Rc<Self>, pos: u32) {
        // Enter after typing a filter: the keyboard cursor stayed behind, the
        // selection follows the filter.
        if !self.selection.is_selected(pos) && !self.selection.selection().is_empty() {
            return self.open_selected();
        }
        let Some(info) = self.info(pos) else { return };
        let Some(loc) = loc_of(&info) else { return };
        if is_dir(&info) {
            return self.go(loc, true);
        }
        if self.chooser.is_some() {
            self.selection.select_item(pos, true);
            return self.accept();
        }
        self.launch(loc, &info);
    }
    fn open_selected(self: &Rc<Self>) {
        let selected = self.selected();
        if let Some(dir) = selected.iter().find(|i| is_dir(i)).and_then(loc_of) {
            return self.go(dir, true);
        }
        if self.chooser.is_some() {
            return self.accept();
        }
        for info in &selected {
            if let Some(loc) = loc_of(info) {
                self.launch(loc, info);
            }
        }
    }
    fn launch(self: &Rc<Self>, loc: Loc, info: &gio::FileInfo) {
        match loc {
            Loc::Local(path) if ops::archive(&info.name().to_string_lossy()).is_some() => {
                self.offer_extract(path);
            }
            Loc::Local(path) => self.launch_local(&path),
            Loc::Phone(remote) => {
                // A copy in the cache, for the default application.
                let local = phone_cache(&remote);
                let fresh = local.metadata().is_ok_and(|m| {
                    m.len() as i64 == info.size()
                        && m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            >= info.modification_date_time().map(|t| t.to_unix())
                });
                if fresh {
                    return self.launch_local(&local);
                }
                let target = local.clone();
                let w = Rc::downgrade(self);
                self.background_then(
                    &format!("Downloading {}", info.display_name()),
                    move |p| {
                        std::fs::create_dir_all(target.parent().unwrap())
                            .map_err(|e| e.to_string())?;
                        ops::download(&remote, &target, p)
                    },
                    move || {
                        if let Some(b) = w.upgrade() {
                            b.launch_local(&local);
                        }
                    },
                );
            }
        }
    }
    fn launch_local(&self, path: &Path) {
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
        let status = self.status.clone();
        launcher.launch(Some(&self.window), gio::Cancellable::NONE, move |r| {
            if let Err(e) = r
                && !e.matches(gtk::DialogError::Dismissed)
            {
                status.set_text(e.message());
                status.add_css_class("error");
            }
        });
    }
    fn offer_extract(self: &Rc<Self>, path: PathBuf) {
        let name = Loc::Local(path.clone()).name();
        let stem = ops::archive(&name).map_or(name.clone(), |a| a.0.to_owned());
        let dialog = gtk::AlertDialog::builder()
            .message(&name)
            .detail("This is an archive.")
            .buttons([
                "Cancel",
                "Open",
                "Extract Here",
                &format!("Extract to “{stem}/”"),
            ])
            .cancel_button(0)
            .default_button(3)
            .build();
        let w = Rc::downgrade(self);
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |r| {
            let Some(b) = w.upgrade() else { return };
            match r {
                Ok(1) => b.launch_local(&path),
                Ok(2) => b.extract(vec![path], false),
                Ok(3) => b.extract(vec![path], true),
                _ => {}
            }
        });
    }
    fn extract(self: &Rc<Self>, archives: Vec<PathBuf>, folder: bool) {
        let Some(dir) = self.local_dir() else { return };
        let label = format!(
            "Extracting {}",
            archives
                .first()
                .map(|p| Loc::Local(p.clone()).name())
                .unwrap_or_default()
        );
        self.background(&label, move |p| {
            for a in &archives {
                p(format!("Extracting {}", Loc::Local(a.clone()).name()));
                ops::extract(a, &dir, folder)?;
            }
            Ok(())
        });
    }

    fn popup(self: &Rc<Self>, from: &gtk::Widget, x: f64, y: f64) {
        let selected = self.selected();
        let locs: Vec<Loc> = selected.iter().filter_map(loc_of).collect();
        let local: Vec<&PathBuf> = locs
            .iter()
            .filter_map(|l| if let Loc::Local(p) = l { Some(p) } else { None })
            .collect();
        let phone = matches!(*self.loc.borrow(), Loc::Phone(_));
        let menu = gio::Menu::new();
        let section = |items: &[(&str, &str)]| {
            let s = gio::Menu::new();
            for (label, action) in items {
                s.append(Some(label), Some(action));
            }
            menu.append_section(None, &s);
            s
        };
        if self.chooser.is_some() {
            section(&[
                ("New Folder…", "win.new-folder"),
                ("Show Hidden Files", "win.hidden"),
            ]);
        } else if selected.is_empty() {
            section(&[("Paste", "win.paste"), ("New Folder…", "win.new-folder")]);
            if !phone {
                section(&[("Open Terminal Here", "win.terminal")]);
            }
            if self.in_trash() {
                section(&[("Empty Trash", "win.empty-trash")]);
            }
            section(&[("Show Hidden Files", "win.hidden")]);
        } else {
            let open = section(&[("Open", "win.open")]);
            if self.searching_everywhere() && selected.len() == 1 {
                open.append(Some("Open Containing Folder"), Some("win.reveal"));
            }
            if let ([path], [info]) = (&local[..], &selected[..])
                && let Some(mime) = info.attribute_string("standard::fast-content-type")
                && !is_dir(info)
            {
                let apps = gio::Menu::new();
                for app in gio::AppInfo::recommended_for_type(&mime) {
                    if let Some(id) = app.id() {
                        apps.append(Some(&app.name()), Some(&format!("win.open-with::{id}")));
                    }
                }
                if apps.n_items() > 0 {
                    open.append_submenu(Some("Open With"), &apps);
                }
                if let Some((stem, _)) =
                    ops::archive(&path.file_name().unwrap_or_default().to_string_lossy())
                {
                    open.append(Some("Extract Here"), Some("win.extract(false)"));
                    open.append(
                        Some(&format!("Extract to “{stem}/”")),
                        Some("win.extract(true)"),
                    );
                }
            }
            section(&[
                ("Cut", "win.cut"),
                ("Copy", "win.copy"),
                ("Paste", "win.paste"),
            ]);
            let edit = section(&[]);
            if selected.len() == 1 {
                edit.append(Some("Rename…"), Some("win.rename"));
            }
            if !phone {
                edit.append(Some("Compress…"), Some("win.compress"));
            }
            if let Some(name) = self.phone_name.borrow().as_deref()
                && !local.is_empty()
            {
                edit.append(Some(&format!("Send to {name}")), Some("win.send-to-phone"));
            }
            if self.in_trash() {
                edit.append(Some("Restore"), Some("win.restore"));
                edit.append(Some("Delete Permanently"), Some("win.delete"));
            } else if phone {
                edit.append(Some("Delete"), Some("win.delete"));
            } else {
                edit.append(Some("Move to Trash"), Some("win.trash"));
            }
        }
        let Some((x, y)) = from.translate_coordinates(&self.menu.parent().unwrap(), x, y) else {
            return;
        };
        self.menu.set_menu_model(Some(&menu));
        self.menu
            .set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        self.menu.popup();
    }

    fn to_clipboard(&self, cut: bool) {
        let locs = self.selected_locs();
        if locs.is_empty() {
            return;
        }
        let uris: Vec<String> = locs.iter().map(Loc::uri).collect();
        let text: Vec<String> = locs
            .iter()
            .map(|l| match l {
                Loc::Local(p) => p.to_string_lossy().into_owned(),
                Loc::Phone(p) => p.clone(),
            })
            .collect();
        let bytes = |s: String| glib::Bytes::from_owned(s.into_bytes());
        let provider = gdk::ContentProvider::new_union(&[
            // What GNOME and Xfce file managers exchange, with cut or copy.
            gdk::ContentProvider::for_bytes(
                "x-special/gnome-copied-files",
                &bytes(format!(
                    "{}\n{}",
                    if cut { "cut" } else { "copy" },
                    uris.join("\n")
                )),
            ),
            gdk::ContentProvider::for_bytes("text/uri-list", &bytes(uris.join("\r\n") + "\r\n")),
            gdk::ContentProvider::for_bytes("text/plain;charset=utf-8", &bytes(text.join("\n"))),
        ]);
        let _ = self.window.clipboard().set_content(Some(&provider));
    }
    /// Copies the selection into the phone's Download folder.
    fn send_to_phone(self: &Rc<Self>) {
        let items = self.selected_local().into_iter().map(Loc::Local).collect();
        self.copy_to(items, Loc::Phone(String::new()));
    }
    /// Copies `items` into `dest`; the phone's place stands for its Download folder.
    fn copy_to(self: &Rc<Self>, items: Vec<Loc>, dest: Loc) {
        if items.is_empty() {
            return;
        }
        self.background("Copying", move |p| {
            let dest = match dest {
                Loc::Phone(d) if d.is_empty() => {
                    Loc::Phone(format!("{}/Download", ops::phone_ls("")?.0))
                }
                d => d,
            };
            ops::transfer(items, false, dest, p)
        });
    }
    fn paste(self: &Rc<Self>) {
        let clipboard = self.window.clipboard();
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            const GNOME: &str = "x-special/gnome-copied-files";
            let Ok((stream, mime)) = clipboard
                .read_future(&[GNOME, "text/uri-list"], glib::Priority::DEFAULT)
                .await
            else {
                return;
            };
            let out = gio::MemoryOutputStream::new_resizable();
            let flags = gio::OutputStreamSpliceFlags::CLOSE_SOURCE
                | gio::OutputStreamSpliceFlags::CLOSE_TARGET;
            if out
                .splice_future(&stream, flags, glib::Priority::DEFAULT)
                .await
                .is_err()
            {
                return;
            }
            let bytes = out.steal_as_bytes();
            let text = String::from_utf8_lossy(&bytes);
            let mut lines = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .peekable();
            let cut = mime == GNOME && lines.next() == Some("cut");
            let items: Vec<Loc> = lines.filter_map(Loc::from_uri).collect();
            if let Some(b) = w.upgrade()
                && !items.is_empty()
            {
                b.transfer(items, cut);
            }
        });
    }
    fn transfer(self: &Rc<Self>, items: Vec<Loc>, cut: bool) {
        let dest = self.loc.borrow().clone();
        let reveal = items.iter().map(Loc::name).collect();
        self.reveal.replace(reveal);
        let label = if cut { "Moving" } else { "Copying" };
        self.background(label, move |p| ops::transfer(items, cut, dest, p));
    }
    fn trash(self: &Rc<Self>) {
        if self.in_trash() || matches!(*self.loc.borrow(), Loc::Phone(_)) {
            return self.delete();
        }
        let paths = self.selected_local();
        if !paths.is_empty() {
            self.background("Moving to the trash", move |_| ops::trash(paths));
        }
    }
    fn delete(self: &Rc<Self>) {
        let locs = self.selected_locs();
        if locs.is_empty() {
            return;
        }
        let message = match &locs[..] {
            [one] => format!("Delete “{}” permanently?", one.name()),
            many => format!("Delete {} items permanently?", many.len()),
        };
        let dialog = gtk::AlertDialog::builder()
            .message(&message)
            .detail("They cannot be restored.")
            .buttons(["Cancel", "Delete"])
            .cancel_button(0)
            .default_button(0)
            .build();
        let w = Rc::downgrade(self);
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |r| {
            if let (Some(b), Ok(1)) = (w.upgrade(), r) {
                b.background("Deleting", move |_| ops::delete(locs));
            }
        });
    }
    fn rename(self: &Rc<Self>) {
        let [info] = &self.selected()[..] else { return };
        let Some(loc) = loc_of(info) else { return };
        let old = info.display_name().to_string();
        let w = Rc::downgrade(self);
        ask(
            &self.window,
            "Rename",
            &old.clone(),
            "Rename",
            move |name| {
                let Some(b) = w.upgrade() else { return };
                if name == old || name.contains('/') {
                    return;
                }
                b.reveal.replace(vec![name.clone()]);
                match loc.clone() {
                    Loc::Local(p) => {
                        let to = p.with_file_name(&name);
                        b.background("Renaming", move |_| {
                            if to.symlink_metadata().is_ok() {
                                return Err(format!("{name} already exists"));
                            }
                            std::fs::rename(&p, &to).map_err(|e| e.to_string())
                        });
                    }
                    Loc::Phone(p) => {
                        let to = format!("{}/{name}", p.rsplit_once('/').map_or("", |s| s.0));
                        b.background("Renaming", move |_| {
                            ops::phone("rename", serde_json::json!({ "path": p, "to": to }))
                                .map(drop)
                        });
                    }
                }
            },
        );
    }
    fn new_folder(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        ask(
            &self.window,
            "New Folder",
            "New folder",
            "Create",
            move |name| {
                let Some(b) = w.upgrade() else { return };
                if name.is_empty() || name.contains('/') {
                    return;
                }
                b.reveal.replace(vec![name.clone()]);
                let target = b.loc.borrow().child(&name);
                b.background("Creating the folder", move |_| match target {
                    Loc::Local(p) => std::fs::create_dir(p).map_err(|e| e.to_string()),
                    Loc::Phone(p) => {
                        ops::phone("mkdir", serde_json::json!({ "path": p })).map(drop)
                    }
                });
            },
        );
    }
    fn compress(self: &Rc<Self>) {
        let (Some(dir), paths) = (self.local_dir(), self.selected_local()) else {
            return;
        };
        let names: Vec<String> = paths.iter().map(|p| Loc::Local(p.clone()).name()).collect();
        let Some(first) = names.first() else { return };
        let suggested = if names.len() == 1 {
            format!("{first}.zip")
        } else {
            "Archive.zip".into()
        };
        let w = Rc::downgrade(self);
        ask(
            &self.window,
            "Compress (.zip, .tar.zst, .tar.gz, .7z…)",
            &suggested,
            "Compress",
            move |out| {
                let Some(b) = w.upgrade() else { return };
                if out.is_empty() || out.contains('/') {
                    return;
                }
                b.reveal.replace(vec![out.clone()]);
                let (dir, names) = (dir.clone(), names.clone());
                b.background(&format!("Compressing to {out}"), move |_| {
                    ops::compress(&dir, names, &out)
                });
            },
        );
    }
    fn terminal(self: &Rc<Self>) {
        let Some(dir) = self.local_dir() else { return };
        let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::NONE);
        launcher.set_cwd(&dir);
        if let Err(e) = launcher.spawn(&[std::ffi::OsStr::new("xdg-terminal-exec")]) {
            self.error(&format!("Could not open a terminal: {e}"));
        }
    }

    /// Runs `job` on a thread, its progress in the status line; long jobs end
    /// with a notification.
    fn background(
        self: &Rc<Self>,
        label: &str,
        job: impl FnOnce(ops::Progress) -> Result<(), String> + Send + 'static,
    ) {
        self.background_then(label, job, || {});
    }
    fn background_then(
        self: &Rc<Self>,
        label: &str,
        job: impl FnOnce(ops::Progress) -> Result<(), String> + Send + 'static,
        then: impl FnOnce() + 'static,
    ) {
        enum Msg {
            Progress(String),
            Done(Result<(), String>),
        }
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            let progress = |s: String| {
                let _ = tx.send_blocking(Msg::Progress(s));
            };
            let result = job(&progress);
            let _ = tx.send_blocking(Msg::Done(result));
        });
        self.busy.replace(Some(format!("{label}…")));
        self.update_status();
        let (w, label, start) = (Rc::downgrade(self), label.to_owned(), Instant::now());
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                let Some(b) = w.upgrade() else { return };
                match msg {
                    Msg::Progress(text) => {
                        b.busy.replace(Some(text));
                        b.update_status();
                    }
                    Msg::Done(result) => {
                        b.busy.take();
                        b.update_status();
                        b.refresh();
                        let long = start.elapsed().as_secs() >= 3;
                        match result {
                            Ok(()) => {
                                if long && !b.window.is_active() {
                                    let n = gio::Notification::new(&format!("{label}: done"));
                                    b.app.send_notification(None, &n);
                                }
                                then();
                            }
                            Err(e) => {
                                b.error(&e);
                                if long {
                                    let n = gio::Notification::new(&format!("{label} failed"));
                                    n.set_body(Some(&e));
                                    b.app.send_notification(None, &n);
                                }
                            }
                        }
                        return;
                    }
                }
            }
        });
    }

    fn accept(self: &Rc<Self>) {
        let Some(c) = self.chooser.clone() else {
            return;
        };
        let selected = self.selected();
        let files: Vec<PathBuf> = if c.save {
            let Some(dir) = self.local_dir() else { return };
            let name = self.name.text();
            if name.is_empty() {
                return;
            }
            let target = dir.join(name.as_str());
            if target.is_dir() {
                self.name.set_text("");
                return self.go(Loc::Local(target), true);
            }
            if target.exists() {
                let dialog = gtk::AlertDialog::builder()
                    .message(format!("Replace “{name}”?"))
                    .detail("A file with this name already exists.")
                    .buttons(["Cancel", "Replace"])
                    .cancel_button(0)
                    .default_button(1)
                    .build();
                let w = Rc::downgrade(self);
                dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |r| {
                    if let (Some(b), Ok(1)) = (w.upgrade(), r) {
                        b.finish(vec![target]);
                    }
                });
                return;
            }
            vec![target]
        } else if c.directory {
            let Some(dir) = self.local_dir() else { return };
            let dirs: Vec<PathBuf> = selected
                .iter()
                .filter(|i| is_dir(i))
                .filter_map(loc_of)
                .filter_map(|l| match l {
                    Loc::Local(p) => Some(p),
                    Loc::Phone(_) => None,
                })
                .collect();
            if dirs.is_empty() { vec![dir] } else { dirs }
        } else {
            if let [info] = &selected[..]
                && is_dir(info)
                && let Some(loc) = loc_of(info)
            {
                return self.go(loc, true);
            }
            let locs: Vec<Loc> = selected
                .iter()
                .filter(|i| !is_dir(i))
                .filter_map(loc_of)
                .collect();
            // The phone's files are handed over as fresh copies in the cache.
            let mut remote = Vec::new();
            let files: Vec<PathBuf> = locs
                .into_iter()
                .map(|l| match l {
                    Loc::Local(p) => p,
                    Loc::Phone(r) => {
                        let local = phone_cache(&r);
                        remote.push((r, local.clone()));
                        local
                    }
                })
                .collect();
            if files.is_empty() {
                return;
            }
            if !remote.is_empty() {
                let w = Rc::downgrade(self);
                self.background_then(
                    "Downloading from the phone",
                    move |p| {
                        for (r, local) in &remote {
                            std::fs::create_dir_all(local.parent().unwrap())
                                .map_err(|e| e.to_string())?;
                            ops::download(r, local, p)?;
                        }
                        Ok(())
                    },
                    move || {
                        if let Some(b) = w.upgrade() {
                            b.finish(files);
                        }
                    },
                );
                return;
            }
            files
        };
        self.finish(files);
    }
    fn finish(&self, files: Vec<PathBuf>) {
        let Some(c) = &self.chooser else { return };
        if let Some(dir) = self.local_dir() {
            LAST.set(Some(dir));
        }
        let filter = (!c.filters.is_empty()).then(|| self.filters.selected() as usize);
        if let Some(done) = c.done.take() {
            done(Some((files, filter)));
        }
        self.window.close();
    }
}
/// Where a copy of the phone's file `remote` goes.
fn phone_cache(remote: &str) -> PathBuf {
    glib::user_cache_dir()
        .join("nexus/phone")
        .join(remote.trim_start_matches('/'))
}
/// Where a file dialog without a folder of its own opens.
pub fn last_folder() -> Option<PathBuf> {
    LAST.with_borrow(|l| l.clone()).filter(|p| p.is_dir())
}

fn key_sorter(by: Sort) -> gtk::Sorter {
    let info = |v: &[glib::Value]| v[0].get::<gio::FileInfo>().unwrap();
    match by {
        Sort::Name => {
            let s = gtk::StringSorter::new(Some(gtk::ClosureExpression::with_callback(
                std::iter::empty::<gtk::Expression>(),
                move |v| info(v).display_name().to_string(),
            )));
            s.set_collation(gtk::Collation::Filename);
            s.upcast()
        }
        Sort::Size | Sort::Modified => {
            let s = gtk::NumericSorter::new(Some(gtk::ClosureExpression::with_callback(
                std::iter::empty::<gtk::Expression>(),
                move |v| {
                    let i = info(v);
                    if by == Sort::Size {
                        i.size()
                    } else {
                        i.modification_date_time().map_or(0, |t| t.to_unix())
                    }
                },
            )));
            s.set_sort_order(gtk::SortType::Descending);
            s.upcast()
        }
    }
}

/// Recycled cells of the list (icon, name, size, date) or the grid (icon or
/// thumbnail over the name), with right click and dragging out.
/// Grid tile sizes; the largest is the thumbnails' own, so they never upscale.
const TILES: [i32; 5] = [48, 64, 88, 112, 128];

/// Whether `(x, y)` in `view` falls on an item rather than empty space.
fn on_item(view: &gtk::Widget, x: f64, y: f64) -> bool {
    let mut picked = view.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(p) = picked.filter(|p| p != view) {
        if matches!(p.css_name().as_str(), "row" | "child") {
            return true;
        }
        picked = p.parent();
    }
    false
}
/// Dragging from empty space selects the items the band covers as it moves,
/// not once it is let go as GTK's own band does. Ctrl adds to the selection.
// ponytail: only the items on screen are hit; add auto-scroll and a model-wide
// hit test if dragging past the edge matters.
fn rubber_band(weak: &Weak<Browser>, view: &gtk::Widget) {
    let drag = gtk::GestureDrag::new();
    // Where the drag started, in content coordinates, and the selection then.
    let start: Rc<Cell<Option<(f64, f64)>>> = Rc::default();
    let base: Rc<RefCell<gtk::Bitset>> = Rc::new(RefCell::new(gtk::Bitset::new_empty()));
    let (w, st, bs) = (weak.clone(), start.clone(), base.clone());
    drag.connect_drag_begin(move |g, x, y| {
        let (Some(b), Some(view)) = (w.upgrade(), g.widget()) else {
            return;
        };
        if on_item(&view, x, y) {
            g.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        let v = b.scroller.vadjustment().value();
        st.set(Some((x, y + v)));
        let keep = g
            .current_event_state()
            .contains(gdk::ModifierType::CONTROL_MASK);
        *bs.borrow_mut() = if keep {
            b.selection.selection()
        } else {
            gtk::Bitset::new_empty()
        };
    });
    let (w, st, bs) = (weak.clone(), start.clone(), base.clone());
    drag.connect_drag_update(move |g, dx, dy| {
        let (Some(b), Some(view), Some((x0, y0))) = (w.upgrade(), g.widget(), st.get()) else {
            return;
        };
        let Some((sx, sy)) = g.start_point() else {
            return;
        };
        g.set_state(gtk::EventSequenceState::Claimed);
        let v = b.scroller.vadjustment().value();
        // The band in the view's coordinates, its start following the scroll.
        let (x1, y1) = (sx + dx, sy + dy);
        let (top, left) = ((y0 - v).min(y1), x0.min(x1));
        let (bottom, right) = ((y0 - v).max(y1), x0.max(x1));
        let hits = bs.borrow().copy();
        let mut child = view.first_child();
        while let Some(c) = child {
            if let (Some(bounds), Some(root)) = (c.compute_bounds(&view), c.first_child()) {
                let overlaps = bounds.x() < right as f32
                    && bounds.x() + bounds.width() > left as f32
                    && bounds.y() < bottom as f32
                    && bounds.y() + bounds.height() > top as f32;
                // SAFETY: set in bind, always a u32.
                if let (true, Some(pos)) = (overlaps, unsafe { root.data::<u32>("position") }) {
                    hits.add(unsafe { *pos.as_ref() });
                }
            }
            child = c.next_sibling();
        }
        let n = b.selection.n_items();
        b.selection
            .set_selection(&hits, &gtk::Bitset::new_range(0, n));
        if let Some((ox, oy)) = view.translate_coordinates(&b.band, left, top) {
            b.band_rect.set(Some([ox, oy, right - left, bottom - top]));
            b.band.queue_draw();
        }
    });
    let (w, st) = (weak.clone(), start);
    drag.connect_drag_end(move |_, _, _| {
        st.set(None);
        if let Some(b) = w.upgrade() {
            b.band_rect.set(None);
            b.band.queue_draw();
        }
    });
    view.add_controller(drag);
}

fn factory(weak: &Weak<Browser>, grid: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let w = weak.clone();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let icon = gtk::Image::new();
        let name = gtk::Label::new(None);
        // A rounded tile: the symbolic icon in the theme's colours, or the thumbnail.
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_visible(false);
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 0);
        tile.add_css_class("tile");
        tile.set_overflow(gtk::Overflow::Hidden);
        tile.set_halign(gtk::Align::Center);
        tile.set_valign(gtk::Align::Center);
        icon.set_vexpand(true);
        picture.set_vexpand(true);
        tile.append(&icon);
        tile.append(&picture);
        let root = if grid {
            let size = TILES[ZOOM.get()];
            let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
            // Tiles line up along the top, whether the name takes one line or two.
            b.set_valign(gtk::Align::Start);
            b.set_size_request(size + 32, -1);
            tile.set_size_request(size, size);
            icon.set_pixel_size(size * 7 / 16);
            name.set_yalign(0.);
            name.set_wrap(true);
            name.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            name.set_lines(2);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            name.set_justify(gtk::Justification::Center);
            name.set_max_width_chars((size + 32) / 9);
            b.append(&tile);
            b.append(&name);
            b
        } else {
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            tile.set_size_request(26, 26);
            icon.set_pixel_size(14);
            name.set_xalign(0.);
            name.set_hexpand(true);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            b.append(&tile);
            b.append(&name);
            b
        };
        root.add_css_class(if grid { "grid-cell" } else { "list-cell" });
        let meta = (!grid).then(|| {
            let (size, date) = (meta_label(), meta_label());
            size.set_xalign(1.);
            size.set_width_chars(10);
            date.set_xalign(0.);
            date.set_width_chars(16);
            date.set_max_width_chars(28);
            date.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            root.append(&size);
            root.append(&date);
            (size, date)
        });
        // SAFETY: the only data under this key, always of this type.
        unsafe {
            item.set_data(
                "cell",
                Rc::new(Cell_ {
                    tile,
                    icon,
                    picture,
                    name,
                    meta,
                    thumb: RefCell::default(),
                    hover: RefCell::default(),
                }),
            )
        };
        let menu = gtk::GestureClick::new();
        menu.set_button(3);
        let (w2, it) = (w.clone(), item.downgrade());
        menu.connect_pressed(move |g, _, x, y| {
            let (Some(b), Some(item)) = (w2.upgrade(), it.upgrade()) else {
                return;
            };
            g.set_state(gtk::EventSequenceState::Claimed);
            let pos = item.position();
            if !b.selection.is_selected(pos) {
                b.selection.select_item(pos, true);
            }
            if let Some(widget) = g.widget() {
                b.popup(&widget, x, y);
            }
        });
        root.add_controller(menu);
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::COPY);
        let (w2, it) = (w.clone(), item.downgrade());
        drag.connect_prepare(move |_, _, _| {
            let (b, item) = (w2.upgrade()?, it.upgrade()?);
            if !b.selection.is_selected(item.position()) {
                b.selection.select_item(item.position(), true);
            }
            let files: Vec<gio::File> =
                b.selected_local().iter().map(gio::File::for_path).collect();
            (!files.is_empty()).then(|| {
                gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value())
            })
        });
        root.add_controller(drag);
        // Resting on a folder lists it ahead, for an instant opening.
        let hover = gtk::EventControllerMotion::new();
        let it = item.downgrade();
        hover.connect_enter(move |_, _, _| {
            let Some(item) = it.upgrade() else { return };
            let Some(info) = item.item().and_downcast::<gio::FileInfo>().filter(is_dir) else {
                return;
            };
            // SAFETY: set above, with this type.
            let cell = unsafe { item.data::<Rc<Cell_>>("cell").unwrap().as_ref().clone() };
            let c = cell.clone();
            let id =
                glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
                    c.hover.take();
                    if let Some(loc) = loc_of(&info) {
                        listing::prefetch(loc, GRID.get());
                    }
                });
            if let Some(old) = cell.hover.replace(Some(id)) {
                old.remove();
            }
        });
        let it = item.downgrade();
        hover.connect_leave(move |_| {
            let Some(item) = it.upgrade() else { return };
            // SAFETY: set above, with this type.
            let cell = unsafe { item.data::<Rc<Cell_>>("cell").unwrap().as_ref().clone() };
            // Still pending: the timeout empties the slot when it runs.
            if let Some(id) = cell.hover.take() {
                id.remove();
            }
        });
        root.add_controller(hover);
        item.set_child(Some(&root));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(info) = item.item().and_downcast::<gio::FileInfo>() else {
            return;
        };
        // SAFETY: set in setup, with this type.
        let cell = unsafe { item.data::<Rc<Cell_>>("cell").unwrap().as_ref().clone() };
        cell.name.set_text(&info.display_name());
        match info.symbolic_icon() {
            Some(icon) => cell.icon.set_from_gicon(&icon),
            None => cell.icon.set_icon_name(Some("text-x-generic-symbolic")),
        }
        cell.icon.set_visible(true);
        cell.picture.set_visible(false);
        cell.picture.set_paintable(None::<&gdk::Paintable>);
        if is_dir(&info) {
            cell.tile.add_css_class("folder");
        } else {
            cell.tile.remove_css_class("folder");
        }
        if let Some(root) = item.child() {
            // SAFETY: the only data under this key, always a u32.
            unsafe { root.set_data("position", item.position()) };
        }
        // Search results say where they are.
        let parent = info.attribute_string(ops::SEARCH_PARENT);
        if let Some(root) = item.child() {
            root.set_tooltip_text(parent.as_deref());
        }
        if let Some((size, date_label)) = &cell.meta {
            size.set_text(&if is_dir(&info) {
                String::new()
            } else {
                glib::format_size(info.size() as u64).into()
            });
            date_label.set_text(&parent.map_or_else(|| date(&info), String::from));
            return;
        }
        if let Some((key, source)) = thumbs::source(&info) {
            cell.thumb
                .replace(thumbs::load(key, source, Some((&cell.icon, &cell.picture))));
        }
    });
    factory.connect_unbind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        // SAFETY: set in setup, with this type.
        let cell = unsafe { item.data::<Rc<Cell_>>("cell").unwrap().as_ref().clone() };
        if let Some(flag) = cell.thumb.take() {
            flag.store(true, Ordering::Relaxed);
        }
    });
    factory
}

/// A one-line question in a small modal window.
fn ask(
    parent: &gtk::ApplicationWindow,
    title: &str,
    text: &str,
    accept: &str,
    done: impl Fn(String) + 'static,
) {
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .resizable(false)
        .default_width(380)
        .decorated(false)
        .build();
    dialog.add_css_class("nexus");
    dialog.add_css_class("files");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.add_css_class("prompt");
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("row-title");
    heading.set_xalign(0.);
    body.append(&heading);
    let entry = gtk::Entry::new();
    entry.set_text(text);
    body.append(&entry);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    cancel.set_action_name(Some("window.close"));
    let ok = gtk::Button::with_label(accept);
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    body.append(&buttons);
    dialog.set_child(Some(&body));
    let keys = gtk::ShortcutController::new();
    keys.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::NamedAction::new("window.close")),
    ));
    dialog.add_controller(keys);
    let (d, e) = (dialog.clone(), entry.clone());
    ok.connect_clicked(move |_| {
        done(e.text().trim().to_owned());
        d.close();
    });
    entry.connect_activate(move |_| ok.emit_clicked());
    let stem = text.rfind('.').filter(|&i| i > 0).unwrap_or(text.len());
    let stem = text[..stem].chars().count() as i32;
    dialog.connect_map(move |_| {
        entry.grab_focus();
        entry.select_region(0, stem);
    });
    dialog.present();
}

/// Runs a blocking call on a thread, for the main loop to await.
pub async fn background<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(f());
    });
    rx.recv().await.expect("the worker thread panicked")
}

/// Makes a portal dialog the child of the application window that asked,
/// through the handle it exported (xdg-foreign); compositors then float and
/// center it over that window.
fn set_parent(window: &gtk::ApplicationWindow, handle: &str) {
    unsafe extern "C" {
        fn gdk_wayland_toplevel_set_transient_for_exported(
            toplevel: *mut gdk::ffi::GdkToplevel,
            handle: *const std::ffi::c_char,
        ) -> glib::ffi::gboolean;
    }
    let (Some(surface), Ok(handle)) = (window.surface(), std::ffi::CString::new(handle)) else {
        return;
    };
    if surface.type_().name() == "GdkWaylandToplevel" {
        // SAFETY: a Wayland toplevel surface, and a NUL-terminated handle.
        unsafe {
            gdk_wayland_toplevel_set_transient_for_exported(
                surface.as_ptr().cast(),
                handle.as_ptr(),
            );
        }
    }
}
