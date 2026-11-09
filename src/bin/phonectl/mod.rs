//! `nexus phone …`: the paired phone from a terminal. Relative phone paths
//! start at its storage root (/storage/emulated/0).
use anyhow::{Result, bail};
use gtk::glib;
use nexus_control::{Request, ensure_daemon, phone_client::*, request};
use serde_json::json;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub const USAGE: &str = "nexus phone status
nexus phone ls [PATH]
nexus phone push [-m] LOCAL... [DIR]   send files or folders (default Download), -m moves them
nexus phone pull [-m] PATH... [DIR]    fetch files or folders (default .), -m moves them
nexus phone mv FROM TO
nexus phone rm PATH...
nexus phone mkdir PATH
nexus phone sms NUMBER TEXT...
nexus phone disconnect

The screen (JSON on stdout; x, y in screen pixels, as in `tree`'s boxes):
nexus phone tree                       what is on screen: windows, then texts, boxes, actions
nexus phone shot [FILE.jpg]            a screenshot; prints where, and the screen's size
nexus phone tap X Y
nexus phone hold X Y
nexus phone swipe X Y X2 Y2 [MS]
nexus phone type TEXT...               into the focused field
nexus phone key KEY                    back home recents notifications quick lock wake backspace enter
nexus phone apps
nexus phone open PACKAGE
nexus phone mirror                     a window on the live screen";

fn call(op: &str, args: serde_json::Value) -> Result<serde_json::Value> {
    phone(op, args).map_err(anyhow::Error::msg)
}
fn number(arg: &str) -> Result<f64> {
    match arg.parse() {
        Ok(n) if n >= 0.0 => Ok(n),
        _ => bail!("Not a position: {arg}"),
    }
}
/// A phone path: relative ones start at the storage root.
fn remote(path: &str, root: &mut Option<String>) -> Result<String> {
    if path.starts_with('/') {
        return Ok(path.trim_end_matches('/').to_owned());
    }
    if root.is_none() {
        *root = Some(phone_ls("").map_err(anyhow::Error::msg)?.0);
    }
    let base = root.as_deref().unwrap_or_default();
    let rest = path.trim_start_matches("./").trim_end_matches('/');
    Ok(if rest.is_empty() || rest == "." {
        base.to_owned()
    } else {
        format!("{base}/{rest}")
    })
}
fn progress(line: String) {
    eprint!("\r\x1b[K{line}");
    let _ = std::io::stderr().flush();
}
/// Splits `[-m] SOURCES... [DEST]`: with a single source, DEST is the default.
fn sources(args: &[String]) -> Result<(bool, Vec<String>, Option<String>)> {
    let moving = args.first().is_some_and(|a| a == "-m" || a == "--move");
    let mut rest = args[usize::from(moving)..].to_vec();
    if rest.is_empty() {
        bail!("Nothing to copy\n\n{USAGE}");
    }
    let dest = if rest.len() > 1 { rest.pop() } else { None };
    Ok((moving, rest, dest))
}

pub fn run(args: &[String]) -> Result<()> {
    ensure_daemon()?;
    let mut root = None;
    let cmd = args.first().map(String::as_str).unwrap_or_default();
    let rest = &args[args.len().min(1)..];
    match cmd {
        "status" => {
            let phone = request(&Request::Status)?
                .state
                .map(|s| s.phone)
                .unwrap_or_default();
            let Some(name) = phone.name else {
                bail!(
                    "No phone is connected. Pair one with: {}",
                    phone.pair_url.unwrap_or_default()
                );
            };
            println!("{name}");
            if let Some(info) = phone.info {
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
        }
        "ls" => {
            let path = remote(rest.first().map_or("", String::as_str), &mut root)?;
            let (_, entries) = phone_ls(&path).map_err(anyhow::Error::msg)?;
            let mut out = String::new();
            for e in entries {
                let name = e["name"].as_str().unwrap_or_default();
                let size = match e["dir"] == true {
                    true => String::new(),
                    false => glib::format_size(e["size"].as_u64().unwrap_or(0)).to_string(),
                };
                let slash = if e["dir"] == true { "/" } else { "" };
                out += &format!("{size:>10}  {name}{slash}\n");
            }
            // Quietly, into a closed pipe (`| head`).
            let _ = std::io::stdout().write_all(out.as_bytes());
        }
        "push" => {
            let (moving, srcs, dest) = sources(rest)?;
            let dir = remote(dest.as_deref().unwrap_or("Download"), &mut root)?;
            for src in srcs {
                let local = Path::new(&src);
                let Some(name) = local
                    .canonicalize()?
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                else {
                    bail!("Cannot send {src}");
                };
                upload_tree(local, &format!("{dir}/{name}"), &progress)
                    .map_err(anyhow::Error::msg)?;
                if moving {
                    if local.is_dir() {
                        std::fs::remove_dir_all(local)?
                    } else {
                        std::fs::remove_file(local)?
                    }
                }
                eprintln!("\r\x1b[K{src} → {dir}/{name}");
            }
        }
        "pull" => {
            let (moving, srcs, dest) = sources(rest)?;
            let dir = PathBuf::from(dest.unwrap_or_else(|| ".".into()));
            for src in srcs {
                let path = remote(&src, &mut root)?;
                let name = path.rsplit('/').next().unwrap_or_default();
                let local = dir.join(name);
                download_tree(&path, &local, &progress).map_err(anyhow::Error::msg)?;
                if moving {
                    call("delete", json!({ "path": path }))?;
                }
                eprintln!("\r\x1b[K{path} → {}", local.display());
            }
        }
        "mv" if rest.len() == 2 => {
            let (from, to) = (remote(&rest[0], &mut root)?, remote(&rest[1], &mut root)?);
            call("rename", json!({ "path": from, "to": to }))?;
        }
        "rm" if !rest.is_empty() => {
            for p in rest {
                call("delete", json!({ "path": remote(p, &mut root)? }))?;
            }
        }
        "mkdir" if rest.len() == 1 => {
            call("mkdir", json!({ "path": remote(&rest[0], &mut root)? }))?;
        }
        "sms" if rest.len() >= 2 => {
            call(
                "sms_send",
                json!({ "to": rest[0], "body": rest[1..].join(" ") }),
            )?;
        }
        "disconnect" => {
            call("disconnect", serde_json::Value::Null)?;
        }
        "tree" | "apps" => println!("{}", call(&format!("screen_{cmd}"), json!({}))?),
        "shot" => {
            let r = call("screen_shot", json!({}))?;
            let path = match rest.first() {
                Some(file) => PathBuf::from(file),
                None => glib::user_cache_dir().join("nexus/phone-shot.jpg"),
            };
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(
                &path,
                glib::base64_decode(r["b64"].as_str().unwrap_or_default()),
            )?;
            println!("{}", json!({ "path": path, "w": r["w"], "h": r["h"] }));
        }
        "tap" | "hold" if rest.len() == 2 => {
            let (x, y) = (number(&rest[0])?, number(&rest[1])?);
            call(&format!("screen_{cmd}"), json!({ "x": x, "y": y }))?;
        }
        "swipe" if rest.len() == 4 || rest.len() == 5 => {
            let mut p = rest.iter().map(|a| number(a)).collect::<Result<Vec<_>>>()?;
            p.resize(5, 300.0);
            call(
                "screen_swipe",
                json!({ "x": p[0], "y": p[1], "x2": p[2], "y2": p[3], "ms": p[4] as u64 }),
            )?;
        }
        "type" if !rest.is_empty() => {
            call("screen_text", json!({ "text": rest.join(" ") }))?;
        }
        "key" if rest.len() == 1 => {
            call("screen_key", json!({ "key": rest[0] }))?;
        }
        "open" if rest.len() == 1 => {
            call("screen_open", json!({ "package": rest[0] }))?;
        }
        _ => bail!("Usage:\n{USAGE}"),
    }
    Ok(())
}
