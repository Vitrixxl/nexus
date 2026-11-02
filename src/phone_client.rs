//! The phone, from a client of the daemon: requests and chunked transfers,
//! for `nexus-files` and `nexus phone`.
use crate::{Request, request};
use gtk::glib;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

/// The IPC's 1 MiB line limit, with room for base64.
pub const CHUNK: usize = 512 * 1024;
pub type Progress<'a> = &'a dyn Fn(String);

/// A blocking call to the phone, through the daemon.
pub fn phone(op: &str, args: Value) -> Result<Value, String> {
    let reply = request(&Request::Phone {
        op: op.into(),
        args,
    })
    .map_err(|e| e.to_string())?;
    if reply.ok {
        Ok(reply.data.unwrap_or_default())
    } else {
        Err(reply.error.unwrap_or_else(|| "The phone refused".into()))
    }
}
/// The connected phone's name.
pub fn phone_name() -> Option<String> {
    request(&Request::Status).ok()?.state?.phone.name
}
/// Entries of a phone directory, with its canonical path.
pub fn phone_ls(path: &str) -> Result<(String, Vec<Value>), String> {
    let data = phone("ls", json!({ "path": path }))?;
    let entries = data["entries"].as_array().cloned().unwrap_or_default();
    Ok((data["path"].as_str().unwrap_or(path).to_owned(), entries))
}
pub fn download_tree(remote: &str, local: &Path, progress: Progress) -> Result<(), String> {
    // Only a directory lists.
    let Ok((path, entries)) = phone_ls(remote) else {
        return download(remote, local, progress);
    };
    fs::create_dir_all(local).map_err(|e| e.to_string())?;
    for e in entries {
        let name = e["name"].as_str().unwrap_or_default();
        let child = format!("{}/{name}", path.trim_end_matches('/'));
        if e["dir"].as_bool().unwrap_or(false) {
            download_tree(&child, &local.join(name), progress)?;
        } else {
            download(&child, &local.join(name), progress)?;
        }
    }
    Ok(())
}
/// Reads a phone file in chunks into `local`, which appears complete or not at all.
pub fn download(remote: &str, local: &Path, progress: Progress) -> Result<(), String> {
    let part = local.with_file_name(format!(
        ".{}.part",
        local.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut file = fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut offset = 0u64;
    let result = (|| loop {
        let r = phone(
            "read",
            json!({ "path": remote, "offset": offset, "len": CHUNK }),
        )?;
        let bytes = glib::base64_decode(r["b64"].as_str().unwrap_or_default());
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        offset += bytes.len() as u64;
        let size = r["size"].as_u64().unwrap_or(0);
        if bytes.is_empty() || offset >= size {
            return Ok(());
        }
        progress(format!(
            "Downloading {} ({}%)",
            local.file_name().unwrap_or_default().to_string_lossy(),
            offset * 100 / size.max(1)
        ));
    })();
    match result {
        Ok(()) => fs::rename(&part, local).map_err(|e| e.to_string()),
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}
pub fn upload_tree(local: &Path, remote: &str, progress: Progress) -> Result<(), String> {
    if local.is_dir() {
        phone("mkdir", json!({ "path": remote }))?;
        for entry in fs::read_dir(local).map_err(|e| e.to_string())?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            upload_tree(&entry.path(), &format!("{remote}/{name}"), progress)?;
        }
        return Ok(());
    }
    let mut file = fs::File::open(local).map_err(|e| e.to_string())?;
    let size = file.metadata().map_or(0, |m| m.len()).max(1);
    let mut buf = vec![0; CHUNK * 3 / 4];
    let mut offset = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 && offset > 0 {
            return Ok(());
        }
        let b64 = glib::base64_encode(&buf[..n]);
        phone(
            "write",
            json!({ "path": remote, "offset": offset, "b64": b64.as_str() }),
        )?;
        offset += n as u64;
        if n == 0 {
            return Ok(());
        }
        progress(format!(
            "Sending {} ({}%)",
            local.file_name().unwrap_or_default().to_string_lossy(),
            offset * 100 / size
        ));
    }
}
