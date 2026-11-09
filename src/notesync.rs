//! Nexus Notes, kept in step with the phone over its link. Both sides hold the
//! whole notes folder: `A.md` pages, `A/` subpages, a `.order` per folder. A
//! sync compares each file's SHA-256 here, on the phone, and at the last sync
//! with that phone (its base); a side that still has the base's version takes
//! the other's. Nothing is lost:
//!
//! - both changed a page: ours keeps its name, the phone's is kept beside it
//!   as "Page (changed on phone DATE)" on both sides;
//! - deleted on one side, edited on the other: the edit comes back;
//! - deleted on one side, untouched on the other: it goes to `.trash` there;
//! - a page that moved or was renamed whole (same content, new path) moves on
//!   the other side rather than coming back as a second copy;
//! - both changed a `.order`: the two orders merge.
//!
//! Files travel in chunks into a hidden staging folder, then replace their
//! target by a rename, and only if the target is still what the plan saw: an
//! edit made meanwhile waits for the next sync. Paths from the phone are
//! checked: relative, inside the folder, no hidden or `..` part, `.md` pages
//! and `.order` files alone. Symbolic links are never followed.
use anyhow::{Context, Result, bail};
use gtk::glib;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// What one message carries of a file.
pub const CHUNK: usize = 256 << 10;
/// More notes than this and nothing syncs: a manifest must fit one message.
// ponytail: one message per manifest; page it if anyone keeps more notes.
pub const MAX_FILES: usize = 5000;
const MAX_PATH: usize = 512;
/// A `.order` is merged in memory.
const MAX_ORDER: usize = 1 << 20;
/// Where transfers land before they replace their target.
const STAGING: &str = ".nexus-sync";

/// Relative path → SHA-256 of its content, in hexadecimal.
pub type Manifest = BTreeMap<String, String>;

/// The notes folder of `nexus-notes`.
pub fn notes_dir() -> PathBuf {
    std::env::var_os("NEXUS_NOTES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            glib::user_special_dir(glib::UserDirectory::Documents)
                .unwrap_or_else(|| glib::home_dir().join("Documents"))
                .join("Notes")
        })
}

/// A path the sync may touch: `a/b.md` or `a/.order`, nothing hidden, no `..`.
pub fn valid(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_PATH {
        return false;
    }
    let clean = |s: &str| !s.is_empty() && !s.chars().any(|c| c == '\\' || c.is_control());
    let mut parts: Vec<&str> = path.split('/').collect();
    let last = parts.pop().unwrap_or_default();
    parts.iter().all(|d| clean(d) && !d.starts_with('.'))
        && clean(last)
        && (last == ".order" || (last.len() > 3 && last.ends_with(".md") && !last.starts_with('.')))
}

fn hex(c: glib::Checksum) -> String {
    c.string().unwrap_or_default().to_string()
}
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut c = glib::Checksum::new(glib::ChecksumType::Sha256).unwrap();
    c.update(bytes);
    hex(c)
}
fn hash_file(path: &Path) -> Result<String> {
    let mut c = glib::Checksum::new(glib::ChecksumType::Sha256).unwrap();
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0; 64 << 10];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(c));
        }
        c.update(&buf[..n]);
    }
}

/// One side of a sync: the desktop's folder, or the phone through the link.
pub trait Side {
    /// Up to `len` bytes of `path` from `offset`; fewer at its end.
    fn read(&mut self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>>;
    /// Stages the bytes of `path` at `offset`; offset 0 starts afresh.
    fn write(&mut self, path: &str, offset: u64, bytes: &[u8]) -> Result<()>;
    /// Puts what was staged for `path` in place, if it hashes to `hash` and
    /// `path` still holds `expect` (`None`: if there is nothing there).
    fn commit(&mut self, path: &str, hash: &str, expect: Option<&str>) -> Result<()>;
    /// Moves `path` into `.trash/<stamp>/`, if it still holds `expect`.
    fn trash(&mut self, path: &str, expect: &str, stamp: &str) -> Result<()>;
    /// Renames `from` to the free `to`, if `from` still holds `expect`.
    fn rename(&mut self, from: &str, to: &str, expect: &str) -> Result<()>;
}

/// The notes folder on this side.
pub struct Dir {
    pub root: PathBuf,
    /// The hash of each file by its modification time and size, so that a
    /// manifest reads only what changed.
    cache: HashMap<String, (i64, i64, u64, String)>,
}
impl Dir {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: HashMap::new(),
        }
    }
    /// Where `rel` lives, refusing a path that would leave the folder.
    fn path(&self, rel: &str) -> Result<PathBuf> {
        if !valid(rel) {
            bail!("Not a notes path: {rel:?}");
        }
        let mut p = self.root.clone();
        for part in rel.split('/') {
            p.push(part);
            if fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
                bail!("{rel} goes through a symbolic link");
            }
        }
        Ok(p)
    }
    fn current(&self, path: &Path) -> Result<Option<String>> {
        match fs::symlink_metadata(path) {
            Ok(m) if m.is_file() => Ok(Some(hash_file(path)?)),
            Ok(_) => bail!("{} is not a file", path.display()),
            Err(_) => Ok(None),
        }
    }
    fn staged(&self, rel: &str) -> PathBuf {
        self.root
            .join(STAGING)
            .join(&hash_bytes(rel.as_bytes())[..32])
    }
    /// Every page and `.order`, hashed. Fails rather than miss some: a file
    /// left out would look deleted.
    pub fn manifest(&mut self) -> Result<Manifest> {
        let mut out = Manifest::new();
        let mut stack = vec![(self.root.clone(), String::new())];
        while let Some((dir, rel)) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if rel.is_empty() && e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(e).with_context(|| format!("Can't read {}", dir.display())),
            };
            for e in entries {
                let e = e?;
                let name = e.file_name().to_string_lossy().into_owned();
                let path = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                let kind = e.file_type()?;
                if kind.is_dir() && !name.starts_with('.') {
                    stack.push((e.path(), path));
                } else if kind.is_file() && valid(&path) {
                    let m = e.metadata()?;
                    let key = (m.mtime(), m.mtime_nsec(), m.len());
                    let hash = match self.cache.get(&path) {
                        Some((s, n, l, h)) if (*s, *n, *l) == key => h.clone(),
                        _ => hash_file(&e.path())?,
                    };
                    self.cache
                        .insert(path.clone(), (key.0, key.1, key.2, hash.clone()));
                    out.insert(path, hash);
                    if out.len() > MAX_FILES {
                        bail!("Over {MAX_FILES} notes: too many to sync");
                    }
                }
            }
        }
        self.cache.retain(|k, _| out.contains_key(k));
        Ok(out)
    }
    /// Removes the folders `path` leaves empty, up to the root.
    fn prune(&self, path: &Path) {
        let mut dir = path.parent();
        while let Some(d) = dir.filter(|d| *d != self.root && d.starts_with(&self.root)) {
            if fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
}
impl Side for Dir {
    fn read(&mut self, rel: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut f = fs::File::open(self.path(rel)?)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::with_capacity(len.min(CHUNK));
        f.take(len.min(CHUNK) as u64).read_to_end(&mut buf)?;
        Ok(buf)
    }
    fn write(&mut self, rel: &str, offset: u64, bytes: &[u8]) -> Result<()> {
        self.path(rel)?;
        let part = self.staged(rel);
        fs::create_dir_all(part.parent().unwrap())?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(offset == 0)
            .open(&part)?;
        if f.metadata()?.len() != offset {
            bail!("Chunk out of order for {rel}");
        }
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(bytes)?;
        Ok(())
    }
    fn commit(&mut self, rel: &str, hash: &str, expect: Option<&str>) -> Result<()> {
        let target = self.path(rel)?;
        let part = self.staged(rel);
        let result = (|| {
            if hash_file(&part)? != hash {
                bail!("{rel} changed while it was copied");
            }
            if self.current(&target)?.as_deref() != expect {
                bail!("{rel} changed meanwhile");
            }
            fs::File::open(&part)?.sync_all()?;
            fs::create_dir_all(target.parent().unwrap())?;
            fs::rename(&part, &target)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&part);
        }
        let _ = fs::remove_dir(part.parent().unwrap());
        result
    }
    fn trash(&mut self, rel: &str, expect: &str, stamp: &str) -> Result<()> {
        let path = self.path(rel)?;
        if self.current(&path)?.as_deref() != Some(expect) {
            bail!("{rel} changed meanwhile");
        }
        if !stamp.bytes().all(|b| b.is_ascii_digit()) || stamp.is_empty() {
            bail!("Bad trash stamp");
        }
        let to = self.root.join(".trash").join(stamp).join(rel);
        fs::create_dir_all(to.parent().unwrap())?;
        fs::rename(&path, &to)?;
        self.prune(&path);
        Ok(())
    }
    fn rename(&mut self, from: &str, to: &str, expect: &str) -> Result<()> {
        let (src, dst) = (self.path(from)?, self.path(to)?);
        if self.current(&src)?.as_deref() != Some(expect) {
            bail!("{from} changed meanwhile");
        }
        if fs::symlink_metadata(&dst).is_ok() {
            bail!("{to} is taken");
        }
        fs::create_dir_all(dst.parent().unwrap())?;
        fs::rename(&src, &dst)?;
        self.prune(&src);
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub enum Act {
    /// Desktop → phone; the phone's file must still be as planned.
    Push(String),
    Pull(String),
    /// Deleted on the phone: to the desktop's trash.
    TrashHere(String),
    TrashThere(String),
    /// Moved on the phone from the first path to the second: the same here.
    MoveHere(String, String),
    MoveThere(String, String),
    /// Both changed: keep both.
    Conflict(String),
}

/// What to do so that both sides match, given each side's files and the base.
pub fn plan(base: &Manifest, here: &Manifest, there: &Manifest) -> Vec<Act> {
    let paths: BTreeSet<&String> = base.keys().chain(here.keys()).chain(there.keys()).collect();
    let mut acts = Vec::new();
    for path in paths {
        let (b, d, p) = (base.get(path), here.get(path), there.get(path));
        if d == p {
            continue;
        }
        let p_ = path.clone();
        acts.push(match (b, d, p) {
            (None, Some(_), None) => Act::Push(p_),
            (None, None, Some(_)) => Act::Pull(p_),
            (Some(b), d, None) if d == Some(b) => Act::TrashHere(p_),
            (Some(b), d, Some(_)) if d == Some(b) => Act::Pull(p_),
            (Some(b), None, p) if p == Some(b) => Act::TrashThere(p_),
            (Some(b), Some(_), p) if p == Some(b) => Act::Push(p_),
            // Deleted on one side, edited on the other: the edit wins.
            (Some(_), None, Some(_)) => Act::Pull(p_),
            (Some(_), Some(_), None) => Act::Push(p_),
            _ => Act::Conflict(p_),
        });
    }
    // A deletion, and a new file on the same side with the content the deleted
    // one had: a move.
    let mut used = vec![false; acts.len()];
    for i in 0..acts.len() {
        let (gone, deleted_there) = match &acts[i] {
            Act::TrashHere(g) => (g.clone(), true),
            Act::TrashThere(g) => (g.clone(), false),
            _ => continue,
        };
        let (side, other) = if deleted_there {
            (there, here)
        } else {
            (here, there)
        };
        let found = (0..acts.len()).find(|&j| {
            let n = match &acts[j] {
                Act::Pull(n) if deleted_there => n,
                Act::Push(n) if !deleted_there => n,
                _ => return false,
            };
            !used[j]
                && !base.contains_key(n)
                && !other.contains_key(n)
                && side.get(n) == base.get(&gone)
        });
        if let Some(j) = found {
            used[j] = true;
            let (Act::Pull(n) | Act::Push(n)) = &acts[j] else {
                unreachable!()
            };
            acts[i] = if deleted_there {
                Act::MoveHere(gone, n.clone())
            } else {
                Act::MoveThere(gone, n.clone())
            };
        }
    }
    let mut acts: Vec<Act> = acts
        .into_iter()
        .zip(used)
        .filter_map(|(a, used)| (!used).then_some(a))
        .collect();
    // Moves first: their targets are free now, and may not be later.
    acts.sort_by_key(|a| !matches!(a, Act::MoveHere(..) | Act::MoveThere(..)));
    acts
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// The copies kept beside pages both sides changed.
    pub conflicts: Vec<String>,
    pub changed: usize,
    /// What could not be done this time; the next sync tries again.
    pub errors: Vec<String>,
}

fn copy(
    from: &mut dyn Side,
    to: &mut dyn Side,
    src: &str,
    dst: &str,
    hash: &str,
    expect: Option<&str>,
) -> Result<()> {
    let mut offset = 0;
    loop {
        let chunk = from.read(src, offset, CHUNK)?;
        to.write(dst, offset, &chunk)?;
        offset += chunk.len() as u64;
        if chunk.len() < CHUNK {
            break;
        }
    }
    to.commit(dst, hash, expect)
}

fn read_all(side: &mut dyn Side, path: &str) -> Result<Vec<u8>> {
    let bytes = side.read(path, 0, MAX_ORDER)?;
    if bytes.len() >= MAX_ORDER {
        bail!("{path} is too large to merge");
    }
    Ok(bytes)
}

/// `dir/Page (changed on phone WHEN).md`, or with ` 2`… if that is taken.
fn copy_name(path: &str, when: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let stem = path.strip_suffix(".md").unwrap_or(path);
    (1..)
        .map(|i| match i {
            1 => format!("{stem} (changed on phone {when}).md"),
            i => format!("{stem} (changed on phone {when}) {i}.md"),
        })
        .find(|c| !taken(c))
        .unwrap()
}

/// Brings `here` (the desktop) and `there` (the phone) to the same notes.
/// `base` is what they last agreed on, updated as each step succeeds. `when`
/// dates conflict copies; `stamp` names this sync's trash folder.
pub fn sync(
    here: &mut dyn Side,
    there: &mut dyn Side,
    dm: &Manifest,
    mut pm: Manifest,
    base: &mut Manifest,
    when: &str,
    stamp: &str,
) -> Outcome {
    pm.retain(|k, _| valid(k));
    let mut out = Outcome::default();
    for (path, hash) in dm {
        if pm.get(path) == Some(hash) {
            base.insert(path.clone(), hash.clone());
        }
    }
    base.retain(|k, _| dm.contains_key(k) || pm.contains_key(k));
    let mut made: BTreeSet<String> = BTreeSet::new();
    for act in plan(base, dm, &pm) {
        let h = |m: &Manifest, p: &str| m.get(p).cloned().unwrap_or_default();
        let r: Result<()> = match &act {
            Act::Push(p) => copy(here, there, p, p, &h(dm, p), pm.get(p).map(|s| s.as_str()))
                .map(|()| drop(base.insert(p.clone(), h(dm, p)))),
            Act::Pull(p) => copy(there, here, p, p, &h(&pm, p), dm.get(p).map(|s| s.as_str()))
                .map(|()| drop(base.insert(p.clone(), h(&pm, p)))),
            Act::TrashHere(p) => here
                .trash(p, &h(dm, p), stamp)
                .map(|()| drop(base.remove(p))),
            Act::TrashThere(p) => there
                .trash(p, &h(&pm, p), stamp)
                .map(|()| drop(base.remove(p))),
            Act::MoveHere(from, to) => here.rename(from, to, &h(dm, from)).map(|()| {
                base.remove(from);
                base.insert(to.clone(), h(&pm, to));
            }),
            Act::MoveThere(from, to) => there.rename(from, to, &h(&pm, from)).map(|()| {
                base.remove(from);
                base.insert(to.clone(), h(dm, to));
            }),
            Act::Conflict(p) if p.ends_with(".order") => (|| {
                let ours = read_all(here, p)?;
                let theirs = read_all(there, p)?;
                let ours = String::from_utf8_lossy(&ours);
                let mut lines: Vec<&str> = ours.lines().filter(|l| !l.is_empty()).collect();
                let theirs = String::from_utf8_lossy(&theirs);
                for l in theirs.lines() {
                    if !l.is_empty() && !lines.contains(&l) {
                        lines.push(l);
                    }
                }
                let merged = lines.join("\n") + "\n";
                let hash = hash_bytes(merged.as_bytes());
                here.write(p, 0, merged.as_bytes())?;
                here.commit(p, &hash, dm.get(p).map(|s| s.as_str()))?;
                there.write(p, 0, merged.as_bytes())?;
                there.commit(p, &hash, pm.get(p).map(|s| s.as_str()))?;
                base.insert(p.clone(), hash);
                Ok(())
            })(),
            Act::Conflict(p) => (|| {
                let taken = |c: &str| {
                    let dir = format!("{}/", c.strip_suffix(".md").unwrap_or(c));
                    made.contains(c)
                        || [dm, &pm].iter().any(|m| {
                            m.contains_key(c)
                                || m.range(dir.clone()..)
                                    .next()
                                    .is_some_and(|(k, _)| k.starts_with(&dir))
                        })
                };
                let copy_path = copy_name(p, when, &taken);
                made.insert(copy_path.clone());
                let (ours, theirs) = (h(dm, p), h(&pm, p));
                // The phone keeps its version under the new name, then each
                // side gets the version it lacks.
                there.rename(p, &copy_path, &theirs)?;
                copy(here, there, p, p, &ours, None)?;
                base.insert(p.clone(), ours);
                copy(there, here, &copy_path, &copy_path, &theirs, None)?;
                base.insert(copy_path.clone(), theirs);
                out.conflicts.push(copy_path);
                Ok(())
            })(),
        };
        match r {
            Ok(()) => out.changed += 1,
            Err(e) => out.errors.push(format!("{e:#}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nexus-notesync-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn put(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    fn get(root: &Path, rel: &str) -> Option<String> {
        fs::read_to_string(root.join(rel)).ok()
    }
    /// The desktop and a phone standing in for the real one, with the base.
    struct Pair {
        d: Dir,
        p: Dir,
        base: Manifest,
        tag: PathBuf,
    }
    impl Pair {
        fn new(tag: &str) -> Self {
            let t = temp(tag);
            Self {
                d: Dir::new(t.join("desktop")),
                p: Dir::new(t.join("phone")),
                base: Manifest::new(),
                tag: t,
            }
        }
        fn sync(&mut self) -> Outcome {
            let dm = self.d.manifest().unwrap();
            let pm = self.p.manifest().unwrap();
            let out = sync(
                &mut self.d,
                &mut self.p,
                &dm,
                pm,
                &mut self.base,
                "2026-11-07 10.00",
                "1",
            );
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            // Synced, the two sides hold the same files and the base is them.
            let (dm, pm) = (self.d.manifest().unwrap(), self.p.manifest().unwrap());
            assert_eq!(dm, pm);
            assert_eq!(dm, self.base);
            out
        }
        fn trashed(&self, side: &str) -> Vec<String> {
            let mut d = Dir::new(self.tag.join(side).join(".trash/1"));
            d.manifest().unwrap().into_keys().collect()
        }
    }
    impl Drop for Pair {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.tag);
        }
    }

    #[test]
    fn paths_from_the_phone_stay_inside() {
        for ok in ["A.md", "A/b.md", ".order", "A/.order", "Ünï cödé.md"] {
            assert!(valid(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/x.md",
            "../x.md",
            "A/../../x.md",
            "./x.md",
            "A//b.md",
            ".trash/x.md",
            ".x.md",
            "A/.hidden/b.md",
            "x.txt",
            ".md",
            "a\\b.md",
            "a\0.md",
            "A/..",
            "..",
        ] {
            assert!(!valid(bad), "{bad:?}");
        }
        assert!(!valid(&format!("{}.md", "a".repeat(MAX_PATH))));
        let pair = Pair::new("traversal");
        let mut d = Dir::new(pair.tag.join("desktop"));
        assert!(d.write("../escape.md", 0, b"x").is_err());
        assert!(d.trash("../../etc/passwd.md", "", "1").is_err());
        assert!(d.rename("A.md", "../B.md", "").is_err());
        assert!(d.read("../x.md", 0, 10).is_err());
        // A symbolic link out of the folder is neither listed nor written through.
        put(&pair.tag, "outside/secret.md", "no");
        fs::create_dir_all(&d.root).unwrap();
        std::os::unix::fs::symlink(pair.tag.join("outside"), d.root.join("link")).unwrap();
        assert!(d.manifest().unwrap().is_empty());
        assert!(d.write("link/secret.md", 0, b"x").is_err());
        assert_eq!(get(&pair.tag, "outside/secret.md").as_deref(), Some("no"));
        // A phone listing a bad path gets nothing done with it.
        let mut pm = Manifest::new();
        pm.insert("../evil.md".into(), hash_bytes(b"x"));
        let dm = d.manifest().unwrap();
        let mut p = Dir::new(pair.tag.join("phone"));
        let out = sync(&mut d, &mut p, &dm, pm, &mut Manifest::new(), "w", "1");
        assert_eq!(out.changed, 0);
        assert!(!pair.tag.join("evil.md").exists());
    }

    #[test]
    fn first_sync_merges_and_keeps_both_of_a_clash() {
        let mut s = Pair::new("first");
        put(&s.d.root, "Desk.md", "d");
        put(&s.d.root, "Same.md", "same");
        put(&s.d.root, "Clash.md", "desktop's");
        put(&s.p.root, "Phone/Sub.md", "p");
        put(&s.p.root, "Same.md", "same");
        put(&s.p.root, "Clash.md", "phone's");
        let out = s.sync();
        let copy = "Clash (changed on phone 2026-11-07 10.00).md";
        assert_eq!(out.conflicts, [copy]);
        for root in [&s.d.root, &s.p.root] {
            assert_eq!(get(root, "Desk.md").as_deref(), Some("d"));
            assert_eq!(get(root, "Phone/Sub.md").as_deref(), Some("p"));
            assert_eq!(get(root, "Clash.md").as_deref(), Some("desktop's"));
            assert_eq!(get(root, copy).as_deref(), Some("phone's"));
        }
        // Nothing is left to do.
        assert_eq!(s.sync().changed, 0);
    }

    #[test]
    fn both_changed_keeps_both() {
        let mut s = Pair::new("both");
        put(&s.d.root, "Plan.md", "v1");
        s.sync();
        put(&s.d.root, "Plan.md", "desktop edit");
        put(&s.p.root, "Plan.md", "phone edit");
        let out = s.sync();
        assert_eq!(
            out.conflicts,
            ["Plan (changed on phone 2026-11-07 10.00).md"]
        );
        for root in [&s.d.root, &s.p.root] {
            assert_eq!(get(root, "Plan.md").as_deref(), Some("desktop edit"));
            assert_eq!(get(root, &out.conflicts[0]).as_deref(), Some("phone edit"));
        }
        // Again on the same page the same minute: a second name.
        put(&s.d.root, "Plan.md", "d2");
        put(&s.p.root, "Plan.md", "p2");
        assert_eq!(
            s.sync().conflicts,
            ["Plan (changed on phone 2026-11-07 10.00) 2.md"]
        );
    }

    #[test]
    fn one_side_changed_wins_and_deletions_go_to_trash() {
        let mut s = Pair::new("oneside");
        put(&s.d.root, "A.md", "a");
        put(&s.d.root, "B.md", "b");
        put(&s.d.root, "Dir/C.md", "c");
        s.sync();
        put(&s.p.root, "A.md", "a from phone");
        fs::remove_file(s.d.root.join("B.md")).unwrap();
        fs::remove_file(s.p.root.join("Dir/C.md")).unwrap();
        fs::remove_dir(s.p.root.join("Dir")).unwrap();
        let out = s.sync();
        assert!(out.conflicts.is_empty());
        assert_eq!(get(&s.d.root, "A.md").as_deref(), Some("a from phone"));
        // Never a hard delete: each side's trash holds what the other deleted,
        // and an emptied folder goes.
        assert_eq!(s.trashed("phone"), ["B.md"]);
        assert_eq!(s.trashed("desktop"), ["Dir/C.md"]);
        assert!(!s.d.root.join("Dir").exists());
    }

    #[test]
    fn deleted_on_one_side_edited_on_the_other_comes_back() {
        let mut s = Pair::new("delvsedit");
        put(&s.d.root, "Keep.md", "v1");
        put(&s.d.root, "Back.md", "v1");
        s.sync();
        fs::remove_file(s.d.root.join("Keep.md")).unwrap();
        put(&s.p.root, "Keep.md", "edited on phone");
        put(&s.d.root, "Back.md", "edited here");
        fs::remove_file(s.p.root.join("Back.md")).unwrap();
        s.sync();
        assert_eq!(
            get(&s.d.root, "Keep.md").as_deref(),
            Some("edited on phone")
        );
        assert_eq!(get(&s.p.root, "Back.md").as_deref(), Some("edited here"));
        assert!(s.trashed("phone").is_empty() && s.trashed("desktop").is_empty());
    }

    #[test]
    fn renames_and_moves_do_not_duplicate() {
        let mut s = Pair::new("rename");
        put(&s.d.root, "Old.md", "page");
        put(&s.d.root, "Folder.md", "folder page");
        put(&s.d.root, "Folder/Kid.md", "kid");
        put(&s.d.root, "Folder/.order", "Kid\n");
        s.sync();
        // Renamed on the phone; a page with its subpages moved on the desktop.
        fs::rename(s.p.root.join("Old.md"), s.p.root.join("New.md")).unwrap();
        fs::create_dir_all(s.d.root.join("Parent")).unwrap();
        fs::rename(
            s.d.root.join("Folder.md"),
            s.d.root.join("Parent/Folder.md"),
        )
        .unwrap();
        fs::rename(s.d.root.join("Folder"), s.d.root.join("Parent/Folder")).unwrap();
        let dm = s.d.manifest().unwrap();
        let pm = s.p.manifest().unwrap();
        let acts = plan(&s.base, &dm, &pm);
        assert!(acts.contains(&Act::MoveHere("Old.md".into(), "New.md".into())));
        assert!(acts.contains(&Act::MoveThere(
            "Folder/Kid.md".into(),
            "Parent/Folder/Kid.md".into()
        )));
        assert!(
            acts.iter()
                .all(|a| matches!(a, Act::MoveHere(..) | Act::MoveThere(..)))
        );
        s.sync();
        assert!(s.trashed("phone").is_empty() && s.trashed("desktop").is_empty());
        assert_eq!(
            get(&s.p.root, "Parent/Folder/Kid.md").as_deref(),
            Some("kid")
        );
        assert!(!s.p.root.join("Folder").exists());
        // Renamed and edited: the old one goes to the trash, no duplicate in the tree.
        fs::rename(s.p.root.join("New.md"), s.p.root.join("Newer.md")).unwrap();
        put(&s.p.root, "Newer.md", "page, edited");
        s.sync();
        assert_eq!(s.trashed("desktop"), ["New.md"]);
    }

    #[test]
    fn orders_merge() {
        let mut s = Pair::new("order");
        put(&s.d.root, ".order", "A\nB\n");
        s.sync();
        put(&s.d.root, ".order", "B\nA\nC\n");
        put(&s.p.root, ".order", "A\nB\nD\n");
        assert!(s.sync().conflicts.is_empty());
        assert_eq!(get(&s.p.root, ".order").as_deref(), Some("B\nA\nC\nD\n"));
    }

    #[test]
    fn large_files_travel_in_chunks_and_nothing_overwrites_a_fresh_edit() {
        let mut s = Pair::new("chunks");
        // Two chunks and a bit, with a boundary on an exact chunk too.
        let big: String = (0..CHUNK * 2 + 77)
            .map(|i| (b'a' + (i % 26) as u8) as char)
            .collect();
        let exact = "x".repeat(CHUNK);
        put(&s.p.root, "Big.md", &big);
        put(&s.p.root, "Exact.md", &exact);
        put(&s.p.root, "Empty.md", "");
        s.sync();
        assert_eq!(get(&s.d.root, "Big.md").as_deref(), Some(big.as_str()));
        assert_eq!(get(&s.d.root, "Exact.md").as_deref(), Some(exact.as_str()));
        assert_eq!(get(&s.d.root, "Empty.md").as_deref(), Some(""));
        // No staging left behind.
        assert!(!s.d.root.join(STAGING).exists());
        // A commit whose target changed since the plan does nothing.
        s.d.write("Big.md", 0, b"stale").unwrap();
        put(&s.d.root, "Big.md", "edited meanwhile");
        let stale = hash_bytes(b"stale");
        assert!(
            s.d.commit("Big.md", &stale, Some(&hash_bytes(big.as_bytes())))
                .is_err()
        );
        assert_eq!(
            get(&s.d.root, "Big.md").as_deref(),
            Some("edited meanwhile")
        );
        // Nor does one whose content is not what was announced, or out of order.
        s.d.write("New.md", 0, b"abc").unwrap();
        assert!(s.d.commit("New.md", &hash_bytes(b"abd"), None).is_err());
        assert!(!s.d.root.join("New.md").exists());
        assert!(s.d.write("New.md", 5, b"x").is_err());
    }
}
