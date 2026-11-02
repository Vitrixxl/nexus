//! The FileChooser portal backend (`org.freedesktop.impl.portal.FileChooser`):
//! xdg-desktop-portal forwards applications' open and save dialogs here, and
//! they open as Nexus Files windows in chooser mode.
use crate::{
    browser::{self, Browser, Chooser, Filter},
    ops::{self, Loc},
};
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::PathBuf,
    rc::{Rc, Weak},
};

pub const NAME: &str = "org.freedesktop.impl.portal.desktop.nexus";
pub const CHOOSER: &str = "org.freedesktop.impl.portal.FileChooser";
const REQUEST: &str = "org.freedesktop.impl.portal.Request";
pub const XML: &str = r#"<node>
<interface name="org.freedesktop.impl.portal.FileChooser">
<method name="OpenFile"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
<method name="SaveFile"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
<method name="SaveFiles"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
</interface>
<interface name="org.freedesktop.impl.portal.Request"><method name="Close"/></interface>
</node>"#;

/// A path option, sent as a NUL-terminated byte string.
fn path(options: &glib::VariantDict, key: &str) -> Option<PathBuf> {
    let mut bytes = options.lookup::<Vec<u8>>(key).ok().flatten()?;
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    (!bytes.is_empty()).then(|| OsString::from_vec(bytes).into())
}

pub fn open(
    app: &gtk::Application,
    connection: &gio::DBusConnection,
    method: &str,
    params: &glib::Variant,
    invocation: gio::DBusMethodInvocation,
) {
    let arg = |i| params.child_value(i).str().unwrap_or_default().to_owned();
    let (handle, parent, title) = (arg(0), arg(2), arg(3));
    let options = glib::VariantDict::new(Some(&params.child_value(4)));
    let flag = |key| options.lookup::<bool>(key).ok().flatten().unwrap_or(false);
    let mut filters: Vec<Filter> = options.lookup("filters").ok().flatten().unwrap_or_default();
    let current: Option<Filter> = options.lookup("current_filter").ok().flatten();
    let filter = current.map(|c| match filters.iter().position(|f| f.0 == c.0) {
        Some(i) => i,
        None => {
            filters.push(c);
            filters.len() - 1
        }
    });
    let current_file = path(&options, "current_file");
    let folder = path(&options, "current_folder")
        .or_else(|| {
            current_file
                .as_ref()
                .and_then(|f| f.parent().map(Into::into))
        })
        .filter(|p| p.is_dir())
        .or_else(browser::last_folder)
        .unwrap_or_else(|| match method {
            "OpenFile" => glib::home_dir(),
            _ => glib::user_special_dir(glib::UserDirectory::Downloads)
                .unwrap_or_else(glib::home_dir),
        });
    let name = options
        .lookup::<String>("current_name")
        .ok()
        .flatten()
        .or_else(|| {
            current_file
                .as_ref()
                .and_then(|f| f.file_name())
                .map(|n| n.to_string_lossy().into())
        })
        .unwrap_or_default();
    // SaveFiles picks a folder for files the application names.
    let names: Vec<String> = options
        .lookup::<Vec<Vec<u8>>>("files")
        .ok()
        .flatten()
        .unwrap_or_default()
        .into_iter()
        .map(|mut n| {
            n.retain(|b| *b != 0);
            String::from_utf8_lossy(&n).into_owned()
        })
        .collect();
    let save = method == "SaveFile";
    let directory = flag("directory") || method == "SaveFiles";
    let accept = options
        .lookup::<String>("accept_label")
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            (if save {
                "_Save"
            } else if directory {
                "_Select"
            } else {
                "_Open"
            })
            .into()
        });

    // Close() from the frontend: the application gave up on the dialog.
    let window: Rc<RefCell<Weak<Browser>>> = Rc::default();
    let Some(request) = gio::DBusNodeInfo::for_xml(XML)
        .ok()
        .and_then(|n| n.lookup_interface(REQUEST))
    else {
        return;
    };
    let w = window.clone();
    let registration = connection
        .register_object(&handle, &request)
        .method_call(move |_, _, _, _, _, _, invocation| {
            if let Some(b) = w.borrow().upgrade() {
                b.window.close();
            }
            invocation.return_value(None);
        })
        .build()
        .ok();
    let registration = Cell::new(registration);
    let (connection, reply_filters, many) =
        (connection.clone(), filters.clone(), method == "SaveFiles");
    let done = move |result: Option<(Vec<PathBuf>, Option<usize>)>| {
        if let Some(id) = registration.take() {
            let _ = connection.unregister_object(id);
        }
        let results = glib::VariantDict::new(None);
        let code = match result {
            Some((mut files, filter)) => {
                if many && let Some(dir) = files.pop() {
                    files = names
                        .iter()
                        .map(|n| dir.join(ops::unique(n, |c| dir.join(c).exists())))
                        .collect();
                }
                let uris: Vec<String> = files
                    .iter()
                    .filter_map(|f| glib::filename_to_uri(f, None).ok().map(Into::into))
                    .collect();
                results.insert_value("uris", &uris.to_variant());
                if let Some(f) = filter.and_then(|i| reply_filters.get(i)) {
                    results.insert_value("current_filter", &f.to_variant());
                }
                0u32
            }
            None => 1,
        };
        invocation.return_value(Some(&glib::Variant::tuple_from_iter([
            code.to_variant(),
            results.end(),
        ])));
    };
    let chooser = Chooser {
        title,
        accept,
        save,
        multiple: flag("multiple"),
        directory,
        name,
        filters,
        filter,
        parent,
        done: RefCell::new(Some(Box::new(done))),
    };
    let b = Browser::open(app, &Loc::Local(folder), &[], Some(chooser));
    window.replace(Rc::downgrade(&b));
}
