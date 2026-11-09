//! The notes on disk: `<root>/A.md` is a page, `<root>/A/` holds its subpages.
//! One worker thread does every write, in order, so that a save, a rename and
//! the next load never cross. It remembers what it last read or wrote of each
//! file: a file that differs from that was changed by something else, and is
//! kept beside ours rather than overwritten.
use gtk::glib;
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};

/// Pages are relative paths without `.md`, `/`-separated; "" is the root.
pub type Rel = String;

pub fn parent(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(p, _)| p)
}
pub fn name(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, n)| n)
}
pub fn join(parent: &str, name: &str) -> Rel {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}/{name}")
    }
}
/// A title as a file name: no separators, no hidden files.
pub fn file_name(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    s.trim()
        .trim_start_matches('.')
        .trim()
        .chars()
        .take(120)
        .collect()
}

#[derive(Default, Clone)]
pub struct Tree {
    pub children: HashMap<Rel, Vec<Rel>>,
    /// The icons pages chose, from their front matter.
    pub icons: HashMap<Rel, String>,
}
impl Tree {
    pub fn kids(&self, rel: &str) -> &[Rel] {
        self.children.get(rel).map_or(&[], |v| v)
    }
    pub fn first(&self) -> Option<&Rel> {
        self.kids("").first()
    }
    pub fn contains(&self, rel: &str) -> bool {
        self.kids(parent(rel)).iter().any(|r| r == rel)
    }
}

pub enum Cmd {
    Scan,
    Load(Rel),
    /// Writes `raw`, renaming the page to `name` first when that is free.
    Save {
        rel: Rel,
        name: String,
        raw: String,
    },
    Create {
        parent: Rel,
        raw: String,
    },
    Delete(Rel),
    Restore {
        trash: PathBuf,
    },
    ListTrash,
    /// Deletes one item of the trash for good, or all of it.
    Purge(Option<PathBuf>),
    Move {
        rel: Rel,
        parent: Rel,
        before: Option<Rel>,
    },
    Flush(mpsc::Sender<()>),
}

pub enum Reply {
    Tree(Tree),
    Loaded {
        rel: Rel,
        raw: String,
    },
    Missing(Rel),
    Created(Rel),
    Renamed {
        from: Rel,
        to: Rel,
    },
    Deleted {
        rel: Rel,
        trash: PathBuf,
    },
    /// A title's name is taken by a sibling: the file keeps its name.
    NameTaken {
        rel: Rel,
        name: String,
    },
    Trash(Vec<Trashed>),
    /// The other version of a page changed elsewhere, kept as this page.
    Conflict {
        rel: Rel,
        copy: Rel,
    },
    Error(String),
}

/// A page in the trash.
pub struct Trashed {
    pub dir: PathBuf,
    /// Where it was.
    pub rel: Rel,
    /// When, in microseconds since the epoch.
    pub when: i64,
}

/// How long the trash keeps a page.
const KEEP_US: i64 = 30 * 24 * 3600 * 1_000_000;

pub struct Store {
    pub root: PathBuf,
    known: HashMap<PathBuf, Vec<u8>>,
    /// Renames done before the window learnt of them.
    aliases: Vec<(Rel, Rel)>,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            known: HashMap::new(),
            aliases: Vec::new(),
        }
    }
    fn file(&self, rel: &str) -> PathBuf {
        self.root.join(format!("{rel}.md"))
    }
    fn dir(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        }
    }
    fn exists(&self, rel: &str) -> bool {
        self.file(rel).exists() || self.dir(rel).is_dir()
    }
    fn resolve(&self, rel: &str) -> Rel {
        let mut rel = rel.to_string();
        for _ in 0..8 {
            let Some((from, to)) = self
                .aliases
                .iter()
                .rev()
                .find(|(f, _)| rel == *f || rel.starts_with(&format!("{f}/")))
            else {
                break;
            };
            rel = format!("{to}{}", &rel[from.len()..]);
        }
        rel
    }
    fn unalias(&mut self, rel: &str) {
        self.aliases
            .retain(|(f, _)| f != rel && !f.starts_with(&format!("{rel}/")));
    }

    pub fn scan(&self) -> Tree {
        let mut tree = Tree::default();
        let _ = fs::create_dir_all(&self.root);
        self.scan_dir("", &mut tree);
        tree
    }
    fn scan_dir(&self, rel: &str, tree: &mut Tree) {
        let Ok(entries) = fs::read_dir(self.dir(rel)) else {
            return;
        };
        let mut names: Vec<String> = Vec::new();
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with('.') {
                continue;
            }
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            let stem = if is_dir {
                n
            } else if let Some(s) = n.strip_suffix(".md") {
                s.to_string()
            } else {
                continue;
            };
            if !is_dir && let Some(icon) = icon_of(&e.path()) {
                tree.icons.insert(join(rel, &stem), icon);
            }
            names.push(stem);
        }
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        let order = self.order(rel);
        names.sort_by_key(|n| order.iter().position(|o| o == n).unwrap_or(usize::MAX));
        let kids: Vec<Rel> = names.iter().map(|n| join(rel, n)).collect();
        for k in &kids {
            if self.dir(k).is_dir() {
                self.scan_dir(k, tree);
            }
        }
        tree.children.insert(rel.into(), kids);
    }
    fn trash(&self) -> PathBuf {
        self.root.join(".trash")
    }

    /// The trash, newest first.
    pub fn list_trash(&self) -> Vec<Trashed> {
        let mut out: Vec<Trashed> = fs::read_dir(self.trash())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let dir = e.path();
                let when: i64 = e.file_name().to_str()?.parse().ok()?;
                let rel = fs::read_to_string(dir.join(".origin"))
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        // From before origins were kept: the page's own name.
                        fs::read_dir(&dir).ok()?.flatten().find_map(|f| {
                            let n = f.file_name().to_string_lossy().into_owned();
                            (!n.starts_with('.'))
                                .then(|| n.strip_suffix(".md").unwrap_or(&n).to_string())
                        })
                    })?;
                Some(Trashed { dir, rel, when })
            })
            .collect();
        out.sort_by_key(|t| -t.when);
        out
    }

    /// Deletes what the trash has kept for more than 30 days.
    pub fn purge_old(&self, now: i64) {
        for t in self.list_trash() {
            if now - t.when > KEEP_US {
                let _ = fs::remove_dir_all(&t.dir);
            }
        }
    }

    fn order(&self, rel: &str) -> Vec<String> {
        fs::read_to_string(self.dir(rel).join(".order"))
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default()
    }
    fn set_order(&self, rel: &str, names: &[String]) -> std::io::Result<()> {
        write_atomic(
            &self.dir(rel).join(".order"),
            (names.join("\n") + "\n").as_bytes(),
        )
    }
    /// Keeps a `.order` that exists in step with a rename.
    fn rename_in_order(&self, parent: &str, from: &str, to: &str) {
        let mut order = self.order(parent);
        if let Some(o) = order.iter_mut().find(|o| *o == from) {
            *o = to.into();
            let _ = self.set_order(parent, &order);
        }
    }

    pub fn load(&mut self, rel: &str) -> Option<String> {
        let path = self.file(rel);
        match fs::read(&path) {
            Ok(bytes) => {
                let raw = String::from_utf8_lossy(&bytes).into_owned();
                self.known.insert(path, bytes);
                Some(raw)
            }
            // A folder without its page: the page is empty until written.
            Err(_) if self.dir(rel).is_dir() => {
                self.known.remove(&path);
                Some(String::new())
            }
            Err(_) => None,
        }
    }

    /// Writes a page, keeping what someone else wrote there as a sibling.
    pub fn save(&mut self, rel: &str, raw: &str) -> std::io::Result<Option<Rel>> {
        let path = self.file(rel);
        let disk = fs::read(&path).ok();
        if disk.as_deref() == Some(raw.as_bytes()) {
            self.known.insert(path, raw.as_bytes().to_vec());
            return Ok(None);
        }
        let mut copy = None;
        if let Some(disk) = disk
            && self.known.get(&path) != Some(&disk)
        {
            let stamp = glib::DateTime::now_local()
                .and_then(|d| d.format("%Y-%m-%d %H.%M.%S"))
                .map(|s| s.to_string())
                .unwrap_or_default();
            let base = format!("{} (changed elsewhere {stamp})", name(rel));
            let other = self.free(parent(rel), &base);
            write_atomic(&self.file(&other), &disk)?;
            copy = Some(other);
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        write_atomic(&path, raw.as_bytes())?;
        self.known.insert(path, raw.as_bytes().to_vec());
        Ok(copy)
    }

    /// A free page name in `parent`: `base`, else `base 2`, `base 3`...
    pub fn free(&self, parent: &str, base: &str) -> Rel {
        (1..)
            .map(|i| {
                join(
                    parent,
                    &if i == 1 {
                        base.into()
                    } else {
                        format!("{base} {i}")
                    },
                )
            })
            .find(|r| !self.exists(r))
            .unwrap()
    }

    pub fn create(&mut self, parent: &str, raw: &str) -> std::io::Result<Rel> {
        let rel = self.free(parent, "Untitled");
        self.unalias(&rel);
        self.save(&rel, raw)?;
        Ok(rel)
    }

    /// Moves a page and its subpages; the target must be free.
    pub fn rename(&mut self, rel: &str, to: &str) -> std::io::Result<()> {
        if rel == to {
            return Ok(());
        }
        if self.exists(to) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("A page named “{}” is already there", name(to)),
            ));
        }
        if let Some(dir) = self.file(to).parent() {
            fs::create_dir_all(dir)?;
        }
        let (file, dir) = (self.file(rel), self.dir(rel));
        if file.exists() {
            fs::rename(&file, self.file(to))?;
        }
        if dir.is_dir() {
            fs::rename(&dir, self.dir(to))?;
        }
        let moved: Vec<_> = self
            .known
            .keys()
            .filter(|k| **k == file || k.starts_with(&dir))
            .cloned()
            .collect();
        for k in moved {
            let v = self.known.remove(&k).unwrap();
            let new = if k == file {
                self.file(to)
            } else {
                self.dir(to).join(k.strip_prefix(&dir).unwrap())
            };
            self.known.insert(new, v);
        }
        if parent(rel) == parent(to) {
            self.rename_in_order(parent(rel), name(rel), name(to));
        }
        self.unalias(to);
        self.aliases.push((rel.into(), to.into()));
        Ok(())
    }

    /// Moves a page into `.trash`, from where [`Store::restore`] brings it back.
    pub fn delete(&mut self, rel: &str) -> std::io::Result<PathBuf> {
        let stamp = glib::real_time();
        let trash = self.trash().join(stamp.to_string());
        fs::create_dir_all(&trash)?;
        fs::write(trash.join(".origin"), rel)?;
        let (file, dir) = (self.file(rel), self.dir(rel));
        if file.exists() {
            fs::rename(&file, trash.join(format!("{}.md", name(rel))))?;
        }
        if dir.is_dir() {
            fs::rename(&dir, trash.join(name(rel)))?;
        }
        self.known.retain(|k, _| *k != file && !k.starts_with(&dir));
        Ok(trash)
    }
    /// Puts a page back where it was, under a free name if that one is taken.
    pub fn restore(&mut self, trash: &Path) -> std::io::Result<Rel> {
        let item = self
            .list_trash()
            .into_iter()
            .find(|t| t.dir == trash)
            .ok_or_else(|| std::io::Error::other("That page is no longer in the trash"))?;
        let rel = self.free(parent(&item.rel), name(&item.rel));
        fs::create_dir_all(self.dir(parent(&rel)))?;
        let file = trash.join(format!("{}.md", name(&item.rel)));
        if file.exists() {
            fs::rename(&file, self.file(&rel))?;
        }
        let dir = trash.join(name(&item.rel));
        if dir.is_dir() {
            fs::rename(&dir, self.dir(&rel))?;
        }
        fs::remove_dir_all(trash)?;
        self.unalias(&rel);
        Ok(rel)
    }

    /// Puts `rel` under `to_parent`, before `before` or last. Returns where it went.
    pub fn place(
        &mut self,
        rel: &str,
        to_parent: &str,
        before: Option<&str>,
    ) -> std::io::Result<Rel> {
        if to_parent == rel || to_parent.starts_with(&format!("{rel}/")) {
            return Err(std::io::Error::other("A page cannot go inside itself"));
        }
        let to = join(to_parent, name(rel));
        if parent(rel) != to_parent {
            self.rename(rel, &to)?;
        }
        let mut names: Vec<String> = self
            .scan_kids(to_parent)
            .into_iter()
            .filter(|n| n != name(rel))
            .collect();
        let at = before
            .and_then(|b| names.iter().position(|n| n == name(b)))
            .unwrap_or(names.len());
        names.insert(at, name(rel).into());
        self.set_order(to_parent, &names)?;
        Ok(to)
    }
    fn scan_kids(&self, rel: &str) -> Vec<String> {
        let mut tree = Tree::default();
        self.scan_dir(rel, &mut tree);
        tree.kids(rel).iter().map(|r| name(r).to_string()).collect()
    }

    pub fn run(mut self, rx: mpsc::Receiver<Cmd>, tx: async_channel::Sender<Reply>) {
        let send = |r: Reply| {
            let _ = tx.send_blocking(r);
        };
        let err = |e: std::io::Error| Reply::Error(e.to_string());
        while let Ok(cmd) = rx.recv() {
            let tree = match cmd {
                Cmd::Scan => {
                    self.purge_old(glib::real_time());
                    true
                }
                Cmd::Flush(done) => {
                    let _ = done.send(());
                    false
                }
                Cmd::Load(rel) => {
                    let rel = self.resolve(&rel);
                    send(match self.load(&rel) {
                        Some(raw) => Reply::Loaded { rel, raw },
                        None => Reply::Missing(rel),
                    });
                    false
                }
                Cmd::Save {
                    rel,
                    name: title,
                    raw,
                } => {
                    let mut rel = self.resolve(&rel);
                    let mut tree = false;
                    if !title.is_empty() && title != name(&rel) {
                        let to = join(parent(&rel), &title);
                        // A taken name keeps the old one, and the window says so.
                        if self.exists(&to) {
                            send(Reply::NameTaken {
                                rel: rel.clone(),
                                name: title.clone(),
                            });
                        } else {
                            match self.rename(&rel, &to) {
                                Ok(()) => {
                                    send(Reply::Renamed {
                                        from: rel.clone(),
                                        to: to.clone(),
                                    });
                                    rel = to;
                                    tree = true;
                                }
                                Err(e) => send(err(e)),
                            }
                        }
                    }
                    match self.save(&rel, &raw) {
                        Ok(Some(copy)) => {
                            send(Reply::Conflict { rel, copy });
                            tree = true;
                        }
                        Ok(None) => {}
                        Err(e) => send(Reply::Error(format!(
                            "Could not save “{}”: {e}",
                            name(&rel)
                        ))),
                    }
                    tree
                }
                Cmd::Create { parent, raw } => {
                    let parent = self.resolve(&parent);
                    match self.create(&parent, &raw) {
                        Ok(rel) => send(Reply::Created(rel)),
                        Err(e) => send(err(e)),
                    }
                    true
                }
                Cmd::Delete(rel) => {
                    let rel = self.resolve(&rel);
                    match self.delete(&rel) {
                        Ok(trash) => send(Reply::Deleted { rel, trash }),
                        Err(e) => send(err(e)),
                    }
                    true
                }
                Cmd::Restore { trash } => {
                    if let Err(e) = self.restore(&trash) {
                        send(err(e));
                    }
                    send(Reply::Trash(self.list_trash()));
                    true
                }
                Cmd::ListTrash => {
                    send(Reply::Trash(self.list_trash()));
                    false
                }
                Cmd::Purge(which) => {
                    let dirs: Vec<PathBuf> = match which {
                        Some(d) => vec![d],
                        None => self.list_trash().into_iter().map(|t| t.dir).collect(),
                    };
                    for d in dirs.iter().filter(|d| d.starts_with(self.trash())) {
                        if let Err(e) = fs::remove_dir_all(d) {
                            send(err(e));
                        }
                    }
                    send(Reply::Trash(self.list_trash()));
                    false
                }
                Cmd::Move {
                    rel,
                    parent: p,
                    before,
                } => {
                    let rel = self.resolve(&rel);
                    let p = self.resolve(&p);
                    match self.place(&rel, &p, before.as_deref()) {
                        Ok(to) if to != rel => send(Reply::Renamed { from: rel, to }),
                        Ok(_) => {}
                        Err(e) => send(err(e)),
                    }
                    true
                }
            };
            if tree {
                send(Reply::Tree(self.scan()));
            }
        }
    }
}

/// The icon in a page's front matter, from its first bytes only.
fn icon_of(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut head = [0u8; 512];
    let n = fs::File::open(path).ok()?.read(&mut head).ok()?;
    let head = String::from_utf8_lossy(&head[..n]);
    let fm = head.strip_prefix("---\n")?;
    fm.lines()
        .take_while(|l| !matches!(l.trim_end(), "---" | "..."))
        .find_map(|l| l.strip_prefix("icon:"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Writes beside the file, flushes, then renames over it: a reader sees the
/// old file or the new one, never half of one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub struct Hit {
    pub rel: Rel,
    /// The line that matched, or the start of the page for a title match.
    pub snippet: String,
}

/// Every page whose title or text holds all the words of `query`, titles
/// first. Gives up as soon as `generation` moves on.
pub fn search(root: &Path, query: &str, generation: &Arc<AtomicU64>, mine: u64) -> Vec<Hit> {
    // ponytail: reads every file per query; an index if thousands of large pages get slow.
    let words: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    let mut titles = Vec::new();
    let mut texts = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        if generation.load(Ordering::Relaxed) != mine {
            return Vec::new();
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with('.') {
                continue;
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push((e.path(), join(&rel, &n)));
                continue;
            }
            let Some(stem) = n.strip_suffix(".md") else {
                continue;
            };
            let page = join(&rel, stem);
            let text = fs::read(e.path())
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let body = crate::doc::split_front_matter(&text).1;
            let (title, lower) = (stem.to_lowercase(), body.to_lowercase());
            if !words.iter().all(|w| title.contains(w) || lower.contains(w)) {
                continue;
            }
            let line = body
                .lines()
                .find(|l| l.to_lowercase().contains(&words[0]))
                .or_else(|| body.lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("");
            let shown = crate::doc::readable(line);
            let snippet = shown.trim().chars().take(160).collect();
            let hit = Hit { rel: page, snippet };
            if words.iter().all(|w| title.contains(w)) {
                titles.push(hit);
            } else {
                texts.push(hit);
            }
        }
    }
    titles.sort_by_key(|h| h.rel.to_lowercase());
    texts.sort_by_key(|h| h.rel.to_lowercase());
    titles.extend(texts);
    titles.truncate(200);
    titles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nexus-notes-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn atomic_save_replaces_and_leaves_nothing_behind() {
        let root = temp("atomic");
        let mut s = Store::new(root.clone());
        let rel = s.create("", "one\n").unwrap();
        assert_eq!(rel, "Untitled");
        assert_eq!(s.save(&rel, "two\n").unwrap(), None);
        assert_eq!(
            fs::read_to_string(root.join("Untitled.md")).unwrap(),
            "two\n"
        );
        let names: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, ["Untitled.md"]);
        // A write that fails leaves the old file and no temporary.
        let missing = root.join("no-such-dir").join("x.md");
        assert!(write_atomic(&missing, b"x").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_changed_elsewhere_is_kept_beside_ours() {
        let root = temp("conflict");
        let mut s = Store::new(root.clone());
        fs::write(root.join("Plan.md"), "mine\n").unwrap();
        assert_eq!(s.load("Plan").as_deref(), Some("mine\n"));
        fs::write(root.join("Plan.md"), "theirs\n").unwrap();
        let copy = s.save("Plan", "mine, edited\n").unwrap().expect("a copy");
        assert!(copy.starts_with("Plan (changed elsewhere "));
        assert_eq!(
            fs::read_to_string(root.join("Plan.md")).unwrap(),
            "mine, edited\n"
        );
        assert_eq!(fs::read_to_string(s.file(&copy)).unwrap(), "theirs\n");
        // Our own write is known: the next save is no conflict.
        assert_eq!(s.save("Plan", "again\n").unwrap(), None);
        // Neither is a file that appeared where we only knew a folder.
        fs::create_dir(root.join("Dir")).unwrap();
        assert_eq!(s.load("Dir").as_deref(), Some(""));
        fs::write(root.join("Dir.md"), "new elsewhere\n").unwrap();
        assert!(s.save("Dir", "ours\n").unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trash_restores_beside_a_taken_name_and_forgets_after_30_days() {
        let root = temp("trash");
        let mut s = Store::new(root.clone());
        s.save("A/B", "---\nicon: star\n---\nb").unwrap();
        assert_eq!(s.scan().icons.get("A/B").map(String::as_str), Some("star"));
        let trash = s.delete("A/B").unwrap();
        let listed = s.list_trash();
        assert_eq!((listed.len(), listed[0].rel.as_str()), (1, "A/B"));
        s.save("A/B", "new").unwrap();
        assert_eq!(s.restore(&trash).unwrap(), "A/B 2");
        assert!(s.list_trash().is_empty());
        let old = s.delete("A/B 2").unwrap();
        s.purge_old(glib::real_time());
        assert_eq!(s.list_trash().len(), 1);
        s.purge_old(glib::real_time() + KEEP_US + 1);
        assert!(s.list_trash().is_empty() && !old.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tree_rename_move_delete_restore() {
        let root = temp("tree");
        let mut s = Store::new(root.clone());
        for (rel, raw) in [("B", "b"), ("a", "a"), ("a/child", "c")] {
            s.save(rel, raw).unwrap();
        }
        let t = s.scan();
        assert_eq!(t.kids(""), ["a", "B"]);
        assert_eq!(t.kids("a"), ["a/child"]);
        s.load("a/child");
        s.rename("a", "Alpha").unwrap();
        assert_eq!(s.resolve("a/child"), "Alpha/child");
        assert!(s.rename("B", "Alpha").is_err());
        // Known contents follow a rename: saving the child is no conflict.
        assert_eq!(s.save("Alpha/child", "c2").unwrap(), None);
        assert_eq!(s.place("B", "", Some("Alpha")).unwrap(), "B");
        assert_eq!(s.scan().kids(""), ["B", "Alpha"]);
        assert_eq!(s.place("B", "Alpha", None).unwrap(), "Alpha/B");
        assert!(s.place("Alpha", "Alpha/B", None).is_err());
        assert_eq!(s.scan().kids("Alpha"), ["Alpha/child", "Alpha/B"]);
        let trash = s.delete("Alpha").unwrap();
        assert!(s.scan().kids("").is_empty());
        assert_eq!(s.restore(&trash).unwrap(), "Alpha");
        assert_eq!(
            fs::read_to_string(root.join("Alpha/child.md")).unwrap(),
            "c2"
        );
        let g = Arc::new(AtomicU64::new(1));
        let hits = search(&root, "C2", &g, 1);
        assert_eq!(
            hits.iter().map(|h| h.rel.as_str()).collect::<Vec<_>>(),
            ["Alpha/child"]
        );
        assert_eq!(file_name(" ../a/b "), "-a-b");
        fs::remove_dir_all(root).unwrap();
    }
}
