//! Folder listings, enumerated off the main thread (GIO's asynchronous
//! enumeration, or `ls` over the phone's IPC) and kept in a small in-memory
//! cache: visited and hovered folders open at once, then a fresh listing
//! reconciles them.
use crate::{
    browser::background,
    ops::{self, Loc},
    thumbs,
};
use gtk::{gio, glib, prelude::*};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

pub const ATTRIBUTES: &str = "standard::name,standard::display-name,standard::type,standard::size,standard::symbolic-icon,standard::fast-content-type,standard::is-hidden,standard::is-backup,time::modified,thumbnail::path,thumbnail::is-valid";
/// Folders kept; larger listings than `BIG` are not kept at all.
const KEEP: usize = 64;
const BIG: usize = 20_000;
/// A hovered folder listed this recently is not listed again.
const FRESH: Duration = Duration::from_secs(5);
/// Thumbnails warmed for a prefetched folder: about a screenful.
const WARM: usize = 40;

pub type Listing = Rc<Vec<gio::FileInfo>>;
thread_local! {
    /// Most recent first.
    static CACHE: RefCell<VecDeque<(Loc, Listing, Instant)>> = RefCell::default();
    static PENDING: RefCell<HashSet<Loc>> = RefCell::default();
}

pub fn cached(loc: &Loc) -> Option<(Listing, Instant)> {
    CACHE.with_borrow_mut(|c| {
        let i = c.iter().position(|e| e.0 == *loc)?;
        let entry = c.remove(i)?;
        let found = (entry.1.clone(), entry.2);
        c.push_front(entry);
        Some(found)
    })
}
fn keep(loc: Loc, listing: Listing) {
    CACHE.with_borrow_mut(|c| {
        c.retain(|e| e.0 != loc);
        if listing.len() <= BIG {
            c.push_front((loc, listing, Instant::now()));
            c.truncate(KEEP);
        }
    });
}

/// Lists `loc`, handing partial results to `batch` as they come, and keeps the
/// result. Returns the folder's canonical location (the phone's storage root
/// for `Phone("")`) with its entries.
pub async fn list(loc: Loc, batch: impl Fn(&[gio::FileInfo])) -> Result<(Loc, Listing), String> {
    let (canonical, entries) = match &loc {
        Loc::Local(path) => {
            let dir = gio::File::for_path(path);
            let e = dir
                .enumerate_children_future(
                    ATTRIBUTES,
                    gio::FileQueryInfoFlags::NONE,
                    glib::Priority::DEFAULT,
                )
                .await
                .map_err(|e| e.message().to_owned())?;
            let mut all = Vec::new();
            loop {
                let chunk = e
                    .next_files_future(1024, glib::Priority::DEFAULT)
                    .await
                    .map_err(|e| e.message().to_owned())?;
                if chunk.is_empty() {
                    break;
                }
                for info in &chunk {
                    info.set_attribute_object("standard::file", &dir.child(info.name()));
                }
                batch(&chunk);
                all.extend(chunk);
            }
            (loc.clone(), all)
        }
        Loc::Phone(path) => {
            let path = path.clone();
            let (canonical, entries) = background(move || ops::phone_ls(&path)).await?;
            let infos: Vec<_> = entries
                .iter()
                .map(|e| ops::phone_info(&canonical, e))
                .collect();
            batch(&infos);
            (Loc::Phone(canonical), infos)
        }
    };
    let listing = Rc::new(entries);
    if canonical != loc {
        keep(loc, listing.clone());
    }
    keep(canonical.clone(), listing.clone());
    Ok((canonical, listing))
}

/// Lists a folder the user is about to open, and makes the thumbnails of its
/// first screenful.
pub fn prefetch(loc: Loc, thumbnails: bool) {
    if cached(&loc).is_some_and(|c| c.1.elapsed() < FRESH)
        || !PENDING.with_borrow_mut(|p| p.insert(loc.clone()))
    {
        return;
    }
    glib::spawn_future_local(async move {
        let listed = list(loc.clone(), |_| {}).await;
        PENDING.with_borrow_mut(|p| p.remove(&loc));
        let Ok((_, listing)) = listed else { return };
        if !thumbnails {
            return;
        }
        // The view's default order: folders first, then by name.
        let mut files: Vec<&gio::FileInfo> = listing
            .iter()
            .filter(|i| i.file_type() != gio::FileType::Directory && !i.is_hidden())
            .collect();
        files.sort_by_cached_key(|i| i.display_name().to_lowercase());
        let folders = listing
            .iter()
            .filter(|i| i.file_type() == gio::FileType::Directory)
            .count();
        for info in files.into_iter().take(WARM.saturating_sub(folders)) {
            if let Some((key, source)) = thumbs::source(info) {
                thumbs::load(key, source, None);
            }
        }
    });
}

/// Brings `store` in line with `fresh` with as few changes as possible, so that
/// selection and scrolling survive a refresh.
pub fn reconcile(store: &gio::ListStore, fresh: &[gio::FileInfo]) {
    let mut by_name: HashMap<PathBuf, &gio::FileInfo> =
        fresh.iter().map(|i| (i.name(), i)).collect();
    let same = |a: &gio::FileInfo, b: &gio::FileInfo| {
        a.size() == b.size()
            && a.file_type() == b.file_type()
            && a.modification_date_time() == b.modification_date_time()
    };
    let mut changes = Vec::new();
    for pos in 0..store.n_items() {
        let Some(old) = store.item(pos).and_downcast::<gio::FileInfo>() else {
            continue;
        };
        match by_name.remove(&old.name()) {
            None => changes.push((pos, None)),
            Some(new) if !same(&old, new) => changes.push((pos, Some(new.clone()))),
            Some(_) => {}
        }
    }
    // A different folder altogether: replace it in one go.
    if changes.len() + by_name.len() > 256 {
        store.splice(0, store.n_items(), fresh);
        return;
    }
    for (pos, new) in changes.into_iter().rev() {
        match new {
            Some(new) => store.splice(pos, 1, &[new]),
            None => store.remove(pos),
        }
    }
    let added: Vec<gio::FileInfo> = by_name.into_values().cloned().collect();
    store.extend_from_slice(&added);
}
