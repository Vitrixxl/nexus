//! File operations, run on a background thread: copies and moves (local or to
//! and from the phone), deletion, trash, archives and the phone's IPC.
use gtk::{gio, glib, prelude::*};
pub use nexus_control::phone_client::*;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::HashSet,
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    time::{Duration, Instant},
};

/// Where a file lives: on this computer, or on the paired phone.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Loc {
    Local(PathBuf),
    Phone(String),
}
/// FileInfo attribute holding a phone file's path.
pub const PHONE_PATH: &str = "nexus::phone-path";
/// Clipboard URIs of phone files.
pub const PHONE_SCHEME: &str = "nexus-phone://";
impl Loc {
    pub fn name(&self) -> String {
        match self {
            Loc::Local(p) => p
                .file_name()
                .map_or("/".into(), |n| n.to_string_lossy().into()),
            Loc::Phone(p) => p.rsplit('/').next().unwrap_or_default().to_string(),
        }
    }
    pub fn child(&self, name: &str) -> Loc {
        match self {
            Loc::Local(p) => Loc::Local(p.join(name)),
            Loc::Phone(p) => Loc::Phone(format!("{}/{name}", p.trim_end_matches('/'))),
        }
    }
    pub fn uri(&self) -> String {
        match self {
            Loc::Local(p) => {
                glib::filename_to_uri(p, None).map_or_else(|_| String::new(), Into::into)
            }
            Loc::Phone(p) => format!(
                "{PHONE_SCHEME}{}",
                glib::Uri::escape_string(p, Some("/"), false)
            ),
        }
    }
    pub fn from_uri(uri: &str) -> Option<Loc> {
        match uri.strip_prefix(PHONE_SCHEME) {
            Some(p) => Some(Loc::Phone(
                glib::Uri::unescape_string(p, None::<&str>)?.into(),
            )),
            None => glib::filename_from_uri(uri)
                .ok()
                .map(|(p, _)| Loc::Local(p)),
        }
    }
}

pub fn phone_info(dir: &str, e: &Value) -> gio::FileInfo {
    let name = e["name"].as_str().unwrap_or_default();
    let info = file_info(
        name,
        e["dir"].as_bool().unwrap_or(false),
        e["size"].as_i64().unwrap_or(0),
        e["mtime"].as_i64().unwrap_or(0),
    );
    info.set_attribute_string(PHONE_PATH, &format!("{}/{name}", dir.trim_end_matches('/')));
    info
}
/// A file's entry built from what is known of it, without touching the disk.
fn file_info(name: &str, is_dir: bool, size: i64, mtime: i64) -> gio::FileInfo {
    let info = gio::FileInfo::new();
    info.set_name(name);
    info.set_display_name(name);
    info.set_is_hidden(name.starts_with('.'));
    info.set_file_type(if is_dir {
        gio::FileType::Directory
    } else {
        gio::FileType::Regular
    });
    info.set_size(size);
    if let Ok(t) = glib::DateTime::from_unix_local(mtime) {
        info.set_modification_date_time(&t);
    }
    let mime = if is_dir {
        "inode/directory".into()
    } else {
        gio::content_type_guess(Some(name), None).0
    };
    info.set_attribute_string("standard::fast-content-type", &mime);
    info.set_symbolic_icon(&gio::content_type_get_symbolic_icon(&mime));
    info
}

/// A search result.
pub struct Found {
    path: PathBuf,
    dir: bool,
    size: i64,
    mtime: i64,
}
pub fn found_info(f: &Found) -> gio::FileInfo {
    let name = f.path.file_name().unwrap_or_default().to_string_lossy();
    let info = file_info(&name, f.dir, f.size, f.mtime);
    info.set_attribute_object("standard::file", &gio::File::for_path(&f.path));
    if let Some(parent) = f.path.parent() {
        info.set_attribute_string(SEARCH_PARENT, &parent.to_string_lossy());
    }
    info
}
/// FileInfo attribute: the folder of a search result.
pub const SEARCH_PARENT: &str = "nexus::parent";
/// Finds up to `max` files and folders whose name contains `query`, in the
/// home folder first, then in the rest of the system, with `fd`; `send`
/// receives them in batches and returns false to stop.
pub fn search(
    query: &str,
    max: usize,
    cancel: &AtomicBool,
    pid: &AtomicU32,
    send: &dyn Fn(Vec<Found>) -> bool,
) {
    let home = glib::home_dir();
    let mut roots = vec![home.clone()];
    for top in fs::read_dir("/")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
    {
        if ["/proc", "/sys", "/dev", "/run"]
            .iter()
            .any(|s| top == Path::new(s))
        {
            continue;
        }
        if home.starts_with(&top) {
            let siblings = fs::read_dir(&top)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path());
            roots.extend(siblings.filter(|s| !home.starts_with(s)));
        } else {
            roots.push(top);
        }
    }
    let (mut batch, found, sent) = (Vec::new(), Cell::new(0), Cell::new(Instant::now()));
    let push = |path: PathBuf, batch: &mut Vec<Found>| {
        let Ok(meta) = path.symlink_metadata() else {
            return true;
        };
        batch.push(Found {
            dir: meta.is_dir(),
            size: meta.len() as i64,
            mtime: meta.mtime(),
            path,
        });
        found.set(found.get() + 1);
        if batch.len() >= 128 || sent.get().elapsed() > Duration::from_millis(100) {
            sent.set(Instant::now());
            return send(std::mem::take(batch));
        }
        true
    };
    let fd = glib::find_program_in_path("fd");
    for root in roots {
        if cancel.load(Ordering::Relaxed) || found.get() >= max {
            break;
        }
        let Some(fd) = &fd else {
            // No fd: a plain walk, slower but the same results.
            let mut stack = vec![root];
            let needle = query.to_lowercase();
            while let Some(dir) = stack.pop() {
                for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    if cancel.load(Ordering::Relaxed) || found.get() >= max {
                        continue;
                    }
                    if entry.file_type().is_ok_and(|t| t.is_dir())
                        && name != ".git"
                        && name != "node_modules"
                    {
                        stack.push(entry.path());
                    }
                    if name.contains(&needle) && !push(entry.path(), &mut batch) {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
            continue;
        };
        let mut cmd = Command::new(fd);
        cmd.args([
            "--hidden",
            "--fixed-strings",
            "--ignore-case",
            "--absolute-path",
            "--color",
            "never",
        ])
        .args(["--type", "f", "--type", "d", "--type", "l"])
        .args([
            "--exclude",
            ".git",
            "--exclude",
            "node_modules",
            "--max-results",
        ])
        .arg((max - found.get()).to_string());
        let Ok(mut child) = cmd
            .arg("--")
            .arg(query)
            .arg(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        pid.store(child.id(), Ordering::Relaxed);
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if cancel.load(Ordering::Relaxed) || !push(PathBuf::from(line), &mut batch) {
                    cancel.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        pid.store(0, Ordering::Relaxed);
        let _ = child.kill();
        let _ = child.wait();
    }
    if !batch.is_empty() && !cancel.load(Ordering::Relaxed) {
        send(batch);
    }
}

/// `name`, or `name (2)`, `name (3)`… before its extension, the first `taken` refuses.
pub fn unique(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = split_extension(name);
    std::iter::once(name.to_owned())
        .chain((2..).map(|n| format!("{stem} ({n}){ext}")))
        .find(|n| !taken(n))
        .unwrap()
}
fn split_extension(name: &str) -> (&str, &str) {
    let lower = name.to_lowercase();
    let at = [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"]
        .iter()
        .find(|e| lower.ends_with(*e) && lower.len() > e.len())
        .map(|e| name.len() - e.len())
        .or_else(|| name.rfind('.').filter(|&i| i > 0));
    at.map_or((name, ""), |i| name.split_at(i))
}

/// Copies or moves `items` into the directory `dest`.
pub fn transfer(items: Vec<Loc>, cut: bool, dest: Loc, progress: Progress) -> Result<(), String> {
    let mut taken: HashSet<String> = match &dest {
        Loc::Phone(dir) => phone_ls(dir)?
            .1
            .iter()
            .filter_map(|e| e["name"].as_str().map(String::from))
            .collect(),
        Loc::Local(_) => HashSet::new(),
    };
    for (i, src) in items.iter().enumerate() {
        let name = src.name();
        progress(format!(
            "{} {name} ({}/{})",
            if cut { "Moving" } else { "Copying" },
            i + 1,
            items.len()
        ));
        if cut && dest.child(&name) == *src {
            continue;
        }
        let target = dest.child(&unique(&name, |n| match &dest {
            Loc::Local(d) => d.join(n).symlink_metadata().is_ok(),
            Loc::Phone(_) => taken.contains(n),
        }));
        taken.insert(target.name());
        match (src, &target) {
            (Loc::Local(s), Loc::Local(t)) => {
                let mut cmd = Command::new(if cut { "mv" } else { "cp" });
                if !cut {
                    cmd.args(["-a", "--reflink=auto"]);
                }
                run(cmd.arg("-T").arg("--").arg(s).arg(t))?;
            }
            (Loc::Phone(s), Loc::Phone(t)) if cut => {
                phone("rename", json!({ "path": s, "to": t }))?;
            }
            (Loc::Phone(_), Loc::Phone(_)) => {
                return Err("Copies within the phone are not supported".into());
            }
            (Loc::Phone(s), Loc::Local(t)) => {
                download_tree(s, t, progress)?;
                if cut {
                    phone("delete", json!({ "path": s }))?;
                }
            }
            (Loc::Local(s), Loc::Phone(t)) => {
                upload_tree(s, t, progress)?;
                if cut {
                    remove_local(s)?;
                }
            }
        }
    }
    Ok(())
}
fn remove_local(path: &Path) -> Result<(), String> {
    if path.is_dir() && !path.is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|e| format!("{}: {e}", path.display()))
}
pub fn trash_dir() -> PathBuf {
    glib::user_data_dir().join("Trash/files")
}
pub fn trash(paths: Vec<PathBuf>) -> Result<(), String> {
    for p in paths {
        gio::File::for_path(&p)
            .trash(gio::Cancellable::NONE)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
/// Deletes for good, with the trash's records of trashed files.
pub fn delete(items: Vec<Loc>) -> Result<(), String> {
    for item in items {
        match item {
            Loc::Local(p) => {
                remove_local(&p)?;
                if p.parent() == Some(&trash_dir()) {
                    let _ = fs::remove_file(trash_info(&p));
                }
            }
            Loc::Phone(p) => {
                phone("delete", json!({ "path": p }))?;
            }
        }
    }
    Ok(())
}
fn trash_info(file: &Path) -> PathBuf {
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    trash_dir()
        .with_file_name("info")
        .join(format!("{name}.trashinfo"))
}
/// Puts trashed files back where they were deleted from.
pub fn restore(paths: Vec<PathBuf>) -> Result<(), String> {
    for p in paths {
        let info = fs::read_to_string(trash_info(&p)).map_err(|e| e.to_string())?;
        let original = info
            .lines()
            .find_map(|l| l.strip_prefix("Path="))
            .and_then(|o| glib::Uri::unescape_string(o, None::<&str>))
            .ok_or("No original location is recorded")?;
        let original = trash_dir().parent().unwrap().join(original.as_str());
        if original.symlink_metadata().is_ok() {
            return Err(format!("{} already exists", original.display()));
        }
        if let Some(parent) = original.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        run(Command::new("mv")
            .arg("-T")
            .arg("--")
            .arg(&p)
            .arg(&original))?;
        let _ = fs::remove_file(trash_info(&p));
    }
    Ok(())
}
fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{}: {e}", cmd.get_program().to_string_lossy()))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(err.lines().last().unwrap_or("Failed").trim().to_owned())
}

/// Archives bsdtar (or 7z, unrar, unzip) unpacks.
const ARCHIVES: [&str; 31] = [
    ".tar.gz",
    ".tar.bz2",
    ".tar.xz",
    ".tar.zst",
    ".tar.lz4",
    ".tar.lzma",
    ".tar.lz",
    ".tar.z",
    ".tgz",
    ".tbz",
    ".tbz2",
    ".txz",
    ".tzst",
    ".tlz",
    ".tar",
    ".zip",
    ".jar",
    ".apk",
    ".xpi",
    ".cbz",
    ".cbr",
    ".7z",
    ".rar",
    ".iso",
    ".cpio",
    ".deb",
    ".rpm",
    ".cab",
    ".xar",
    ".lha",
    ".lzh",
];
/// Single compressed files and the tool that decompresses each to stdout.
const STREAMS: [(&str, &str); 7] = [
    (".gz", "gzip"),
    (".bz2", "bzip2"),
    (".xz", "xz"),
    (".lzma", "xz"),
    (".zst", "zstd"),
    (".lz4", "lz4"),
    (".z", "gzip"),
];
/// What an archive named `name` extracts to, and the tool when it is a single
/// compressed file.
pub fn archive(name: &str) -> Option<(&str, Option<&'static str>)> {
    let lower = name.to_lowercase();
    let strip = |ext: &str| (lower.len() > ext.len()).then(|| &name[..name.len() - ext.len()]);
    if let Some(ext) = ARCHIVES.iter().find(|e| lower.ends_with(*e)) {
        return Some((strip(ext)?, None));
    }
    let (ext, tool) = STREAMS.iter().find(|(e, _)| lower.ends_with(e))?;
    Some((strip(ext)?, Some(*tool)))
}
/// Extracts into `dir`, or into a new folder there named after the archive.
pub fn extract(file: &Path, dir: &Path, folder: bool) -> Result<(), String> {
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    let (stem, stream) = archive(&name).ok_or("Not an archive")?;
    let free = |n: &str| dir.join(n).symlink_metadata().is_ok();
    if let Some(tool) = stream {
        let out = dir.join(unique(stem, free));
        let target = fs::File::create(&out).map_err(|e| e.to_string())?;
        let ok = Command::new(tool)
            .args(["-dc", "--"])
            .arg(file)
            .stdout(target)
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            let _ = fs::remove_file(&out);
            return Err(format!("{tool} could not decompress {name}"));
        }
        return Ok(());
    }
    let dest = if folder {
        let d = dir.join(unique(stem, free));
        fs::create_dir(&d).map_err(|e| e.to_string())?;
        d
    } else {
        dir.to_path_buf()
    };
    let lower = name.to_lowercase();
    let mut tried = Vec::new();
    let mut last = String::from("No extraction tool is installed (install libarchive)");
    for tool in ["bsdtar", "7z", "unrar", "unzip"] {
        if glib::find_program_in_path(tool).is_none()
            || tool == "unrar" && !lower.ends_with(".rar")
            || tool == "unzip" && !lower.ends_with(".zip")
        {
            continue;
        }
        let mut cmd = Command::new(tool);
        match tool {
            "bsdtar" => cmd.arg("-xf").arg(file).arg("-C").arg(&dest),
            "7z" => cmd
                .arg("x")
                .arg("-y")
                .arg(format!("-o{}", dest.display()))
                .arg(file),
            "unrar" => cmd
                .args(["x", "-o+"])
                .arg(file)
                .arg(format!("{}/", dest.display())),
            _ => cmd.arg("-o").arg(file).arg("-d").arg(&dest),
        };
        tried.push(tool);
        match run(&mut cmd) {
            Ok(()) => return Ok(()),
            Err(e) => last = format!("{tool}: {e}"),
        }
    }
    if folder {
        let _ = fs::remove_dir(&dest);
    }
    Err(last)
}
/// Packs `names`, in `dir`, into `dir/out`; the extension picks the format.
pub fn compress(dir: &Path, names: Vec<String>, out: &str) -> Result<(), String> {
    run(Command::new("bsdtar")
        .arg("-a")
        .arg("-cf")
        .arg(dir.join(out))
        .arg("-C")
        .arg(dir)
        .arg("--")
        .args(names))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names() {
        let taken = ["a.txt", "a (2).txt", "x.tar.gz", "dir"];
        let free = |n: &str| taken.contains(&n);
        assert_eq!(unique("b.txt", free), "b.txt");
        assert_eq!(unique("a.txt", free), "a (3).txt");
        assert_eq!(unique("x.tar.gz", free), "x (2).tar.gz");
        assert_eq!(unique("dir", free), "dir (2)");
        assert_eq!(unique(".bashrc", |n| n == ".bashrc"), ".bashrc (2)");
        assert_eq!(archive("Photos.TAR.GZ"), Some(("Photos", None)));
        assert_eq!(archive("log.zst"), Some(("log", Some("zstd"))));
        assert_eq!(archive("a.7z"), Some(("a", None)));
        assert_eq!(archive("notes.txt"), None);
        assert_eq!(archive(".zip"), None);
        let phone = Loc::Phone("/storage/emulated/0/My Photos".into());
        assert_eq!(Loc::from_uri(&phone.uri()), Some(phone.clone()));
        assert_eq!(phone.child("a.jpg").name(), "a.jpg");
    }
}
