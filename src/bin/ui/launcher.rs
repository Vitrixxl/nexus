use super::super::{hbox, label, vbox};
use gtk::{glib, prelude::*};
use nexus_control::{
    Request,
    launcher::{self, Application, Catalogue},
    request,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

pub struct Launcher {
    pub widget: gtk::Box,
    pub search: gtk::Entry,
    update: Rc<dyn Fn(Catalogue)>,
}
impl Launcher {
    pub fn update(&self, catalogue: Catalogue) {
        (self.update)(catalogue);
    }
    pub fn new(catalogue: Catalogue, close: Rc<dyn Fn()>) -> Self {
        let root = vbox(0);
        root.add_css_class("launcher");
        root.set_width_request(640);
        let search_row = hbox(14);
        search_row.add_css_class("search-row");
        let icon = gtk::Image::from_icon_name("system-search-symbolic");
        icon.set_pixel_size(20);
        search_row.append(&icon);
        let search = gtk::Entry::new();
        search.set_placeholder_text(Some("Search"));
        search.set_hexpand(true);
        search.add_css_class("launcher-search");
        search_row.append(&search);
        root.append(&search_row);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let results = gtk::ListBox::new();
        results.add_css_class("app-results");
        results.set_selection_mode(gtk::SelectionMode::Single);
        results.set_activate_on_single_click(true);
        root.append(&results);
        let empty = vbox(8);
        empty.add_css_class("empty-results");
        let empty_title = label("No matches", "row-title");
        let empty_hint = label("Try another application name.", "muted");
        empty.append(&empty_title);
        empty.append(&empty_hint);
        empty.set_visible(false);
        root.append(&empty);
        let error = label("", "error");
        error.set_visible(false);
        root.append(&error);
        // Where the pointer was last seen over the results. Only moving from there
        // moves the selection: not resting where the launcher opens, nor rows
        // changing under a still pointer.
        let pointer = Rc::new(Cell::new(None::<(f64, f64)>));
        let motion = gtk::EventControllerMotion::new();
        let (seen, list) = (pointer.clone(), results.clone());
        motion.connect_motion(move |motion, _, y| {
            // On the surface: the panel slides as it opens, moving the list under a
            // still pointer, and the compositor reports that same position again.
            let Some(at) = motion.current_event().and_then(|event| event.position()) else {
                return;
            };
            if seen.replace(Some(at)).is_some_and(|was| was != at)
                && let Some(row) = list.row_at_y(y as i32)
            {
                list.select_row(Some(&row));
            }
        });
        results.add_controller(motion);
        // Opening again on an empty search changes no text, so refreshes nothing.
        let on_close = error.clone();
        root.connect_unmap(move |_| {
            on_close.set_visible(false);
            pointer.set(None);
        });
        let catalogue = Rc::new(RefCell::new(catalogue));
        // Rows and their icon paintables are prepared once while the shell starts.
        // Opening or searching only reparents the already cached widgets.
        let rows = Rc::new(RefCell::new(
            catalogue
                .borrow()
                .applications
                .iter()
                .map(app_row)
                .collect::<Vec<_>>(),
        ));
        let indices = Rc::new(RefCell::new(Vec::<usize>::new()));
        let refresh: Rc<dyn Fn(&str)> = {
            let results = results.clone();
            let catalogue = catalogue.clone();
            let rows = rows.clone();
            let indices = indices.clone();
            let empty = empty.clone();
            let error = error.clone();
            Rc::new(move |query| {
                error.set_visible(false);
                let cache = catalogue.borrow();
                let ranked = launcher::ranked(&cache.applications, &cache.history, query);
                // Cached rows keep their selected flag when removed; GtkListBox then refuses
                // to select them again, leaving stale highlights and no selected row for Enter.
                results.unselect_all();
                while let Some(child) = results.first_child() {
                    results.remove(&child);
                }
                empty.set_visible(ranked.is_empty());
                let loading = cache.revision == 0;
                empty_title.set_text(if loading {
                    "Loading applications…"
                } else {
                    "No matches"
                });
                empty_hint.set_visible(!loading);
                for i in &ranked {
                    results.append(&rows.borrow()[*i]);
                }
                *indices.borrow_mut() = ranked;
                if let Some(row) = results.row_at_index(0) {
                    results.select_row(Some(&row));
                }
            })
        };
        refresh("");
        let r = refresh.clone();
        search.connect_changed(move |entry| r(entry.text().as_str()));
        let cache2 = catalogue.clone();
        let rows2 = rows.clone();
        let refresh2 = refresh.clone();
        let entry = search.clone();
        let indices2 = indices.clone();
        let update: Rc<dyn Fn(Catalogue)> = Rc::new(move |new| {
            let changed = cache2.borrow().applications != new.applications;
            if changed {
                *rows2.borrow_mut() = new.applications.iter().map(app_row).collect();
            }
            *cache2.borrow_mut() = new;
            indices2.borrow_mut().clear();
            refresh2(entry.text().as_str());
        });
        let pending = Rc::new(Cell::new(false));
        let cache2 = catalogue.clone();
        let indices2 = indices.clone();
        let error2 = error.clone();
        results.connect_row_activated(move |_, row| {
            if pending.replace(true) {
                return;
            }
            let Some(index) = indices2.borrow().get(row.index() as usize).copied() else {
                pending.set(false);
                return;
            };
            let id = cache2.borrow().applications[index].id.clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = request(&Request::Launch { id })
                    .map_err(|e| e.to_string())
                    .and_then(|r| {
                        if r.ok {
                            Ok(())
                        } else {
                            Err(r
                                .error
                                .unwrap_or_else(|| "Could not open application".into()))
                        }
                    });
                let _ = tx.send_blocking(result);
            });
            let close = close.clone();
            let error = error2.clone();
            let pending = pending.clone();
            glib::spawn_future_local(async move {
                if let Ok(result) = rx.recv().await {
                    match result {
                        Ok(()) => close(),
                        Err(e) => {
                            error.set_text(&e);
                            error.set_visible(true);
                        }
                    }
                }
                pending.set(false);
            });
        });
        let list = results.clone();
        search.connect_activate(move |_| {
            if let Some(row) = list.selected_row() {
                list.emit_by_name::<()>("row-activated", &[&row]);
            }
        });
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let list = results.clone();
        key.connect_key_pressed(move |_, key, _, modifiers| {
            if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter) {
                if let Some(row) = list.selected_row() {
                    list.emit_by_name::<()>("row-activated", &[&row]);
                }
                return glib::Propagation::Stop;
            }
            let ctrl = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let down = key == gtk::gdk::Key::Down
                || (ctrl && matches!(key, gtk::gdk::Key::n | gtk::gdk::Key::N));
            let up = key == gtk::gdk::Key::Up
                || (ctrl && matches!(key, gtk::gdk::Key::p | gtk::gdk::Key::P));
            if down || up {
                let current = list.selected_row().map(|r| r.index()).unwrap_or(0);
                let next = if down {
                    current + 1
                } else {
                    (current - 1).max(0)
                };
                if let Some(row) = list.row_at_index(next) {
                    list.select_row(Some(&row));
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        root.add_controller(key);
        Self {
            widget: root,
            search,
            update,
        }
    }
}
fn app_row(app: &Application) -> gtk::ListBoxRow {
    const ICON: i32 = 32;
    let row = gtk::ListBoxRow::new();
    let b = hbox(14);
    b.add_css_class("app-row");
    let display = gtk::gdk::Display::default().unwrap();
    match app
        .icon
        .as_deref()
        .and_then(|i| super::dock::app_image(&display, i, ICON, 2))
    {
        Some(icon) => b.append(&icon),
        None => b.append(&super::dock::letter(&app.name, "app-letter")),
    }
    let text = vbox(2);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let name = label(&app.name, "app-name");
    name.set_wrap(false);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    // A tiny natural width lets long text ellipsize instead of widening the panel.
    name.set_max_width_chars(1);
    text.append(&name);
    if !app.description.is_empty() {
        let description = label(&app.description, "app-description");
        description.set_wrap(false);
        description.set_ellipsize(gtk::pango::EllipsizeMode::End);
        description.set_max_width_chars(1);
        text.append(&description);
    }
    b.append(&text);
    row.set_child(Some(&b));
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    #[ignore = "requires an isolated display: xvfb-run cargo test --bin nexus keyboard_navigation -- --ignored"]
    fn keyboard_navigation_and_enter_launch_selected_application() {
        // A private mock daemon prevents this UI test from launching real apps.
        let dir = std::env::temp_dir().join(format!("nexus-ui-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("nexus")).unwrap();
        // Run this ignored test alone, before GTK starts any worker threads.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        gtk::init().unwrap();
        let socket = std::os::unix::net::UnixListener::bind(dir.join("nexus/daemon.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = socket.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert!(
                matches!(serde_json::from_str::<Request>(&line).unwrap(), Request::Launch { id } if id == "probe-b.desktop")
            );
            stream.write_all(b"{\"ok\":true}\n").unwrap();
        });
        let closed = Rc::new(Cell::new(false));
        let closed2 = closed.clone();
        let catalogue = Catalogue {
            applications: ["a", "b"]
                .iter()
                .map(|suffix| Application {
                    id: format!("probe-{suffix}.desktop"),
                    name: format!("Probe {suffix}"),
                    description: String::new(),
                    keywords: String::new(),
                    icon: None,
                })
                .collect(),
            ..Default::default()
        };
        let launcher = Launcher::new(catalogue, Rc::new(move || closed2.set(true)));
        let list = launcher
            .widget
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::ListBox>()
            .unwrap();
        let key = launcher
            .widget
            .observe_controllers()
            .item(0)
            .unwrap()
            .downcast::<gtk::EventControllerKey>()
            .unwrap();
        assert_eq!(key.propagation_phase(), gtk::PropagationPhase::Capture);
        let press = |value: gtk::gdk::Key, mods: gtk::gdk::ModifierType| {
            assert!(key.emit_by_name::<bool>("key-pressed", &[&value, &0u32, &mods]));
        };
        let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
        assert_eq!(list.selected_row().unwrap().index(), 0);
        // Searching reuses cached rows; the first result must still be selectable.
        launcher.search.set_text("Probe");
        launcher.search.set_text("");
        assert_eq!(list.selected_row().unwrap().index(), 0);
        let mut row = list.first_child();
        let mut selected = 0;
        while let Some(r) = row {
            selected += usize::from(r.downcast_ref::<gtk::ListBoxRow>().unwrap().is_selected());
            row = r.next_sibling();
        }
        assert_eq!(selected, 1, "only one row may be highlighted");
        press(gtk::gdk::Key::n, ctrl);
        assert_eq!(list.selected_row().unwrap().index(), 1);
        press(gtk::gdk::Key::p, ctrl);
        assert_eq!(list.selected_row().unwrap().index(), 0);
        press(gtk::gdk::Key::p, ctrl);
        assert_eq!(list.selected_row().unwrap().index(), 0);
        press(gtk::gdk::Key::Down, gtk::gdk::ModifierType::empty());
        assert_eq!(list.selected_row().unwrap().index(), 1);
        press(gtk::gdk::Key::Return, gtk::gdk::ModifierType::empty());
        for _ in 0..200 {
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
            if closed.get() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(closed.get(), "successful launch must close the panel");
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
