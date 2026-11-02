//! Grid thumbnails. Read from the freedesktop cache, or made by a few worker
//! threads when missing (images in-process with gdk-pixbuf, others by the
//! installed thumbnailers) and written back to the cache. Only cells on screen
//! ask for one, and a cell leaving the screen drops its request.
use crate::ops;
use gtk::{gdk, gdk_pixbuf::Pixbuf, gio, glib, prelude::*};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// The cache's "normal" size.
const SIZE: i32 = 128;
const WORKERS: usize = 3;
/// Decoded thumbnails kept for scrolling back, about 64 KiB each.
const KEEP: usize = 300;
/// Larger images are not worth decoding for a thumbnail.
const MAX_IMAGE: u64 = 80 << 20;

pub enum Source {
    Local {
        path: PathBuf,
        mime: String,
        mtime: i64,
        cached: Option<PathBuf>,
    },
    Phone {
        path: String,
        mtime: i64,
    },
}
/// Decoded pixels: they cross threads, a texture does not need to.
pub struct Pixels {
    bytes: glib::Bytes,
    width: i32,
    height: i32,
    stride: usize,
    alpha: bool,
}
struct Job {
    source: Source,
    cancelled: Arc<AtomicBool>,
    reply: async_channel::Sender<Option<Pixels>>,
}
struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
}
thread_local! {
    static TEXTURES: RefCell<HashMap<String, gdk::Texture>> = RefCell::default();
}

/// What a grid cell can show for `info`, if anything beyond its icon.
pub fn source(info: &gio::FileInfo) -> Option<(String, Source)> {
    let mtime = info.modification_date_time().map_or(0, |t| t.to_unix());
    let mime = info.attribute_string("standard::fast-content-type")?;
    if let Some(path) = info.attribute_string(ops::PHONE_PATH) {
        return (mime.starts_with("image/") || mime.starts_with("video/")).then(|| {
            let path = path.to_string();
            (
                format!("phone:{path}:{mtime}"),
                Source::Phone { path, mtime },
            )
        });
    }
    let path = info
        .attribute_object("standard::file")
        .and_downcast::<gio::File>()?
        .path()?;
    let cached = info
        .attribute_byte_string("thumbnail::path")
        .filter(|_| {
            info.boolean("thumbnail::is-valid") || !info.has_attribute("thumbnail::is-valid")
        })
        .map(|p| PathBuf::from(p.as_str()));
    if cached.is_none() && (!can_make(&mime) || info.size() as u64 > MAX_IMAGE && pixbuf(&mime)) {
        return None;
    }
    let key = format!("{}:{mtime}", path.display());
    Some((
        key,
        Source::Local {
            path,
            mime: mime.into(),
            mtime,
            cached,
        },
    ))
}
/// Sets `image` to the thumbnail of `key` now if it is at hand, or later; the
/// returned flag cancels it.
/// Without widgets, it only warms the cache for a folder about to open.
pub fn load(
    key: String,
    source: Source,
    widgets: Option<(&gtk::Image, &gtk::Picture)>,
) -> Option<Arc<AtomicBool>> {
    if let Some(texture) = TEXTURES.with_borrow(|t| t.get(&key).cloned()) {
        if let Some((icon, picture)) = widgets {
            show(icon, picture, &texture);
        }
        return None;
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let (reply, receive) = async_channel::bounded(1);
    let q = queue();
    q.jobs.lock().unwrap().push_back(Job {
        source,
        cancelled: cancelled.clone(),
        reply,
    });
    q.ready.notify_one();
    let (widgets, flag) = (
        widgets.map(|(i, p)| (i.clone(), p.clone())),
        cancelled.clone(),
    );
    glib::spawn_future_local(async move {
        let Ok(Some(px)) = receive.recv().await else {
            return;
        };
        let texture: gdk::Texture = gdk::MemoryTexture::new(
            px.width,
            px.height,
            if px.alpha {
                gdk::MemoryFormat::R8g8b8a8
            } else {
                gdk::MemoryFormat::R8g8b8
            },
            &px.bytes,
            px.stride,
        )
        .upcast();
        TEXTURES.with_borrow_mut(|t| {
            // ponytail: wholesale reset instead of LRU; fine at this size.
            if t.len() >= KEEP {
                t.clear();
            }
            t.insert(key, texture.clone());
        });
        // The cell may show another file by now.
        if let Some((icon, picture)) = widgets
            && !flag.load(Ordering::Relaxed)
        {
            show(&icon, &picture, &texture);
        }
    });
    Some(cancelled)
}
fn show(icon: &gtk::Image, picture: &gtk::Picture, texture: &gdk::Texture) {
    picture.set_paintable(Some(texture));
    picture.set_visible(true);
    icon.set_visible(false);
}

fn queue() -> &'static Queue {
    static QUEUE: OnceLock<Arc<Queue>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let q = Arc::new(Queue {
            jobs: Mutex::default(),
            ready: Condvar::new(),
        });
        for _ in 0..WORKERS {
            let q = q.clone();
            std::thread::spawn(move || {
                loop {
                    let job = {
                        let mut jobs = q.jobs.lock().unwrap();
                        loop {
                            // Newest first: the cells the user is looking at.
                            match jobs.pop_back() {
                                Some(job) => break job,
                                None => jobs = q.ready.wait(jobs).unwrap(),
                            }
                        }
                    };
                    if !job.cancelled.load(Ordering::Relaxed) {
                        let _ = job.reply.send_blocking(make(&job.source));
                    }
                }
            });
        }
        q
    })
}

fn failed() -> &'static Mutex<HashSet<PathBuf>> {
    static FAILED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    FAILED.get_or_init(Mutex::default)
}
fn make(source: &Source) -> Option<Pixels> {
    match source {
        Source::Local {
            path,
            mime,
            mtime,
            cached,
        } => {
            if let Some(px) = cached.as_deref().and_then(decode) {
                return Some(px);
            }
            if failed().lock().unwrap().contains(path) {
                return None;
            }
            let made = generate(path, mime, *mtime);
            if made.is_none() {
                failed().lock().unwrap().insert(path.clone());
            }
            made
        }
        Source::Phone { path, mtime } => {
            let file = glib::user_cache_dir()
                .join("nexus/phone-thumbs")
                .join(format!(
                    "{}.jpg",
                    glib::compute_checksum_for_string(
                        glib::ChecksumType::Md5,
                        format!("{path}:{mtime}")
                    )?
                ));
            if !file.exists() {
                let data =
                    ops::phone("thumb", serde_json::json!({ "path": path, "size": SIZE })).ok()?;
                fs::create_dir_all(file.parent()?).ok()?;
                fs::write(&file, glib::base64_decode(data["b64"].as_str()?)).ok()?;
            }
            decode(&file)
        }
    }
}
fn decode(file: &Path) -> Option<Pixels> {
    let pixbuf = Pixbuf::from_file_at_scale(file, SIZE, SIZE, true).ok()?;
    Some(Pixels {
        bytes: pixbuf.read_pixel_bytes(),
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: pixbuf.rowstride() as usize,
        alpha: pixbuf.has_alpha(),
    })
}
/// Makes and caches the thumbnail of a local file, as the freedesktop
/// thumbnail specification describes.
fn generate(path: &Path, mime: &str, mtime: i64) -> Option<Pixels> {
    let cache = glib::user_cache_dir().join("thumbnails/normal");
    if path.starts_with(glib::user_cache_dir().join("thumbnails")) {
        return None;
    }
    let uri = glib::filename_to_uri(path, None).ok()?;
    let md5 = glib::compute_checksum_for_string(glib::ChecksumType::Md5, uri.as_str())?;
    let tmp = cache.join(format!(".{md5}.{}.png", std::process::id()));
    fs::create_dir_all(&cache).ok()?;
    let _ = fs::set_permissions(&cache, fs::Permissions::from_mode(0o700));
    let pixbuf = if pixbuf(mime) {
        Pixbuf::from_file_at_scale(path, SIZE, SIZE, true)
            .ok()
            .and_then(|p| p.apply_embedded_orientation())
    } else {
        None
    };
    let pixbuf = match pixbuf {
        Some(p) => p,
        None => {
            run_thumbnailer(path, &uri, mime, &tmp)?;
            let p = Pixbuf::from_file_at_scale(&tmp, SIZE, SIZE, true).ok();
            let _ = fs::remove_file(&tmp);
            p?
        }
    };
    let mtime = mtime.to_string();
    pixbuf
        .savev(
            &tmp,
            "png",
            &[
                ("tEXt::Thumb::URI", uri.as_str()),
                ("tEXt::Thumb::MTime", &mtime),
                ("tEXt::Software", "Nexus Files"),
            ],
        )
        .ok()?;
    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    fs::rename(&tmp, cache.join(format!("{md5}.png"))).ok()?;
    Some(Pixels {
        bytes: pixbuf.read_pixel_bytes(),
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: pixbuf.rowstride() as usize,
        alpha: pixbuf.has_alpha(),
    })
}

/// Whether gdk-pixbuf reads `mime` itself.
fn pixbuf(mime: &str) -> bool {
    static MIMES: OnceLock<HashSet<String>> = OnceLock::new();
    MIMES
        .get_or_init(|| {
            Pixbuf::formats()
                .iter()
                .flat_map(|f| f.mime_types())
                .map(String::from)
                .collect()
        })
        .contains(mime)
}
fn can_make(mime: &str) -> bool {
    pixbuf(mime) || thumbnailers().iter().any(|t| t.0.iter().any(|m| m == mime))
}
/// Mime types and command of each installed thumbnailer.
fn thumbnailers() -> &'static Vec<(Vec<String>, Vec<String>)> {
    static ALL: OnceLock<Vec<(Vec<String>, Vec<String>)>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut all = Vec::new();
        let dirs = std::iter::once(glib::user_data_dir()).chain(glib::system_data_dirs());
        for entry in dirs
            .filter_map(|d| fs::read_dir(d.join("thumbnailers")).ok())
            .flatten()
            .flatten()
        {
            let file = glib::KeyFile::new();
            if file
                .load_from_file(entry.path(), glib::KeyFileFlags::NONE)
                .is_err()
            {
                continue;
            }
            let get = |key| file.string("Thumbnailer Entry", key).ok();
            let (Some(exec), Some(mimes)) = (get("Exec"), get("MimeType")) else {
                continue;
            };
            if get("TryExec").is_some_and(|t| glib::find_program_in_path(t.as_str()).is_none()) {
                continue;
            }
            let Ok(argv) = glib::shell_parse_argv(exec.as_str()) else {
                continue;
            };
            let mimes = mimes.split(';').filter(|m| !m.is_empty()).map(String::from);
            all.push((
                mimes.collect(),
                argv.iter().map(|a| a.to_string_lossy().into()).collect(),
            ));
        }
        // Poppler ships no thumbnailer entry, but its tools make one.
        if !all
            .iter()
            .any(|t: &(Vec<String>, _)| t.0.iter().any(|m| m == "application/pdf"))
            && glib::find_program_in_path("pdftocairo").is_some()
        {
            let argv = "pdftocairo -png -singlefile -f 1 -l 1 -scale-to %s %i %o";
            all.push((
                vec!["application/pdf".into()],
                argv.split(' ').map(String::from).collect(),
            ));
        }
        all
    })
}
fn run_thumbnailer(path: &Path, uri: &str, mime: &str, out: &Path) -> Option<()> {
    let (_, argv) = thumbnailers()
        .iter()
        .find(|t| t.0.iter().any(|m| m == mime))?;
    let size = SIZE.to_string();
    // pdftocairo appends the extension itself.
    let out_arg = if argv[0] == "pdftocairo" {
        out.with_extension("")
    } else {
        out.to_path_buf()
    };
    let args: Vec<std::ffi::OsString> = argv
        .iter()
        .map(|a| match a.as_str() {
            "%i" => path.into(),
            "%u" => uri.into(),
            "%o" => out_arg.clone().into(),
            "%s" => size.as_str().into(),
            a => a.replace("%%", "%").into(),
        })
        .collect();
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) => return (status.success() && out.exists()).then_some(()),
            None if start.elapsed() > Duration::from_secs(10) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(out);
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
