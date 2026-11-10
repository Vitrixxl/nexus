//! The password vault (`VAULT-SPEC.md`): crypto, the file, merging, URL
//! matching and the generator. Merging needs no key, so `nexusd` can sync the
//! file; only `nexus-vaultd` ever holds a [`VaultKey`].
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;
use std::{collections::BTreeMap, fs, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};
use unicode_normalization::UnicodeNormalization;
use zeroize::{Zeroize, Zeroizing};

pub const ITERS: u32 = 600_000;
const MIN_ITERS: u32 = 100_000;
/// A synced file must not be able to hang an unlock for ever.
const MAX_ITERS: u32 = 10_000_000;
const HISTORY: usize = 5;
pub const WRONG_PASSWORD: &str = "Wrong password";

/// The vault key, or a key derived from a password. Deliberately not `Debug`.
#[derive(Clone)]
pub struct VaultKey(Zeroizing<[u8; 32]>);
impl VaultKey {
    /// Best effort: what is locked stays out of swap.
    pub fn pin(&self) {
        // SAFETY: the pointer and length describe this key's own bytes.
        unsafe { libc::mlock(self.0.as_ptr().cast(), 32) };
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Kdf {
    pub alg: String,
    pub iters: u32,
    pub salt: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct WrappedKey {
    pub updated: i64,
    pub nonce: String,
    pub ct: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Stored {
    pub updated: i64,
    pub deleted: bool,
    pub device: String,
    pub nonce: String,
    pub ct: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}
impl Stored {
    fn rank(&self) -> (i64, &str, &str) {
        (self.updated, &self.device, &self.ct)
    }
}
/// `vault.json`. Unknown fields survive a rewrite.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct VaultFile {
    pub v: u32,
    pub id: String,
    pub kdf: Kdf,
    pub key: WrappedKey,
    pub entries: BTreeMap<String, Stored>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Past {
    pub password: String,
    pub changed: i64,
}
/// An entry's plaintext. Wiped when dropped; deliberately not `Debug`.
#[derive(Default, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub history: Vec<Past>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.title.zeroize();
        self.urls.iter_mut().for_each(Zeroize::zeroize);
        self.username.zeroize();
        self.password.zeroize();
        self.notes.zeroize();
        self.history.iter_mut().for_each(|p| p.password.zeroize());
    }
}

/// Wipes every string in a JSON value that held secrets.
pub fn scrub(value: &mut Value) {
    match value {
        Value::String(s) => s.zeroize(),
        Value::Array(a) => a.iter_mut().for_each(scrub),
        Value::Object(o) => o.values_mut().for_each(scrub),
        _ => {}
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the system's random source failed");
    bytes
}
pub fn new_id() -> String {
    random::<16>().iter().map(|b| format!("{b:02x}")).collect()
}
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
pub fn b64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                B64[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}
pub fn unb64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, group) in bytes.chunks(4).enumerate() {
        let padding = group.iter().rev().take_while(|b| **b == b'=').count();
        if padding > 2 || (padding > 0 && i + 1 != bytes.len() / 4) {
            return None;
        }
        let mut n = 0u32;
        for b in &group[..4 - padding] {
            n = n << 6 | B64.iter().position(|c| c == b)? as u32;
        }
        n <<= 6 * padding;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - padding]);
    }
    Some(out)
}

fn cipher(key: &VaultKey) -> Aes256Gcm {
    Aes256Gcm::new((&*key.0).into())
}
/// A fresh nonce and the ciphertext with its tag, in base64.
fn seal(key: &VaultKey, aad: &str, plain: &[u8]) -> (String, String) {
    let nonce = random::<12>();
    let ct = cipher(key)
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: aad.as_bytes(),
            },
        )
        .expect("AES-GCM has no failure for an input of this size");
    (b64(&nonce), b64(&ct))
}
fn open(key: &VaultKey, nonce: &str, ct: &str, aad: &str) -> Option<Zeroizing<Vec<u8>>> {
    let nonce: [u8; 12] = unb64(nonce)?.try_into().ok()?;
    let plain = cipher(key)
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &unb64(ct)?,
                aad: aad.as_bytes(),
            },
        )
        .ok()?;
    Some(Zeroizing::new(plain))
}

fn key_aad(id: &str) -> String {
    format!("nexus-vault-key-v1|{id}")
}
fn entry_aad(vault: &str, id: &str, updated: i64, deleted: bool) -> String {
    let deleted = u8::from(deleted);
    format!("nexus-vault-entry-v1|{vault}|{id}|{updated}|{deleted}")
}

impl Kdf {
    fn fresh(iters: u32) -> Self {
        Self {
            alg: "pbkdf2-sha256".into(),
            iters,
            salt: b64(&random::<16>()),
            extra: Map::new(),
        }
    }
    /// The key-encryption key. The caller maps a failed unwrap to one message.
    fn derive(&self, password: &str) -> Result<VaultKey> {
        if self.alg != "pbkdf2-sha256" {
            bail!("This vault uses an unknown key derivation");
        }
        if !(MIN_ITERS..=MAX_ITERS).contains(&self.iters) {
            bail!("This vault's key settings are unsafe, so it was not unlocked");
        }
        let salt = unb64(&self.salt).context("This vault's key settings are damaged")?;
        let password = Zeroizing::new(password.nfc().collect::<String>());
        let mut key = Zeroizing::new([0; 32]);
        pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, self.iters, &mut *key);
        Ok(VaultKey(key))
    }
}

/// `max(now, previous + 1)`, what the merge relies on.
fn next_stamp(previous: i64) -> i64 {
    crate::phone::now_ms().max(previous + 1)
}

impl VaultFile {
    pub fn create(password: &str) -> Result<(Self, VaultKey)> {
        Self::create_with(password, ITERS)
    }
    fn create_with(password: &str, iters: u32) -> Result<(Self, VaultKey)> {
        let vk = VaultKey(Zeroizing::new(random::<32>()));
        let mut file = Self {
            v: 1,
            id: new_id(),
            kdf: Kdf::fresh(iters),
            key: WrappedKey {
                updated: 0,
                nonce: String::new(),
                ct: String::new(),
                extra: Map::new(),
            },
            entries: BTreeMap::new(),
            extra: Map::new(),
        };
        file.wrap(&vk, password, iters)?;
        Ok((file, vk))
    }
    fn wrap(&mut self, vk: &VaultKey, password: &str, iters: u32) -> Result<()> {
        let kdf = Kdf::fresh(iters);
        let kek = kdf.derive(password)?;
        let (nonce, ct) = seal(&kek, &key_aad(&self.id), &*vk.0);
        self.key = WrappedKey {
            updated: next_stamp(self.key.updated),
            nonce,
            ct,
            extra: std::mem::take(&mut self.key.extra),
        };
        self.kdf = Kdf {
            extra: std::mem::take(&mut self.kdf.extra),
            ..kdf
        };
        Ok(())
    }
    /// The vault key. A wrong password and a damaged key blob read the same.
    pub fn unlock(&self, password: &str) -> Result<VaultKey> {
        let kek = self.kdf.derive(password)?;
        let plain = open(&kek, &self.key.nonce, &self.key.ct, &key_aad(&self.id))
            .context(WRONG_PASSWORD)?;
        let key = <[u8; 32]>::try_from(plain.as_slice()).context(WRONG_PASSWORD)?;
        Ok(VaultKey(Zeroizing::new(key)))
    }
    /// A new master password; the vault key and so every entry stay as they are.
    pub fn change_password(&mut self, old: &str, new: &str) -> Result<VaultKey> {
        let vk = self.unlock(old)?;
        self.wrap(&vk, new, self.kdf.iters.max(ITERS))?;
        Ok(vk)
    }

    fn plaintext(&self, vk: &VaultKey, id: &str) -> Option<Zeroizing<Vec<u8>>> {
        let s = self.entries.get(id)?;
        let aad = entry_aad(&self.id, id, s.updated, s.deleted);
        open(vk, &s.nonce, &s.ct, &aad)
    }
    /// A live entry, if it decrypts.
    pub fn entry(&self, vk: &VaultKey, id: &str) -> Option<Entry> {
        if self.entries.get(id)?.deleted {
            return None;
        }
        serde_json::from_slice(&self.plaintext(vk, id)?).ok()
    }
    /// The live entries with their ids and times, and how many would not decrypt.
    pub fn entries(&self, vk: &VaultKey) -> (Vec<(String, i64, Entry)>, usize) {
        let mut damaged = 0;
        let mut list = Vec::new();
        for (id, s) in &self.entries {
            if s.deleted {
                continue;
            }
            match self.entry(vk, id) {
                Some(entry) => list.push((id.clone(), s.updated, entry)),
                None => damaged += 1,
            }
        }
        (list, damaged)
    }
    pub fn live(&self) -> usize {
        self.entries.values().filter(|s| !s.deleted).count()
    }
    fn put(&mut self, vk: &VaultKey, id: &str, deleted: bool, plain: &[u8], device: &str) {
        let updated = next_stamp(self.entries.get(id).map_or(0, |s| s.updated));
        let aad = entry_aad(&self.id, id, updated, deleted);
        let (nonce, ct) = seal(vk, &aad, plain);
        let extra = self
            .entries
            .get_mut(id)
            .map(|s| std::mem::take(&mut s.extra))
            .unwrap_or_default();
        self.entries.insert(
            id.to_owned(),
            Stored {
                updated,
                deleted,
                device: device.to_owned(),
                nonce,
                ct,
                extra,
            },
        );
    }
    /// Adds or replaces an entry; a replaced password goes into its history.
    pub fn save(
        &mut self,
        vk: &VaultKey,
        id: Option<&str>,
        mut entry: Entry,
        device: &str,
    ) -> Result<String> {
        let id = match id {
            Some(id) if self.entries.contains_key(id) => id.to_owned(),
            Some(_) => bail!("That entry no longer exists"),
            None => new_id(),
        };
        if let Some(old) = self.entry(vk, &id) {
            entry.history.clone_from(&old.history);
            entry.extra.clone_from(&old.extra);
            if !old.password.is_empty() && old.password != entry.password {
                entry.history.insert(
                    0,
                    Past {
                        password: old.password.clone(),
                        changed: crate::phone::now_ms(),
                    },
                );
                entry.history.truncate(HISTORY);
            }
        } else if self.entries.contains_key(&id) && !self.entries[&id].deleted {
            bail!("That entry is damaged and cannot be changed");
        }
        let plain = Zeroizing::new(serde_json::to_vec(&entry)?);
        self.put(vk, &id, false, &plain, device);
        Ok(id)
    }
    pub fn delete(&mut self, vk: &VaultKey, id: &str, device: &str) -> Result<()> {
        if !self.entries.contains_key(id) {
            bail!("That entry no longer exists");
        }
        self.put(vk, id, true, b"{}", device);
        Ok(())
    }

    /// Takes in what `other` has that is newer, and whether anything changed.
    pub fn merge(&mut self, other: &Self) -> Result<bool> {
        if self.id != other.id {
            bail!("That is a different vault");
        }
        let mut changed = false;
        for (id, theirs) in &other.entries {
            if self
                .entries
                .get(id)
                .is_none_or(|ours| theirs.rank() > ours.rank())
            {
                self.entries.insert(id.clone(), theirs.clone());
                changed = true;
            }
        }
        if (other.key.updated, &other.key.ct) > (self.key.updated, &self.key.ct) {
            self.key = other.key.clone();
            self.kdf = other.kdf.clone();
            changed = true;
        }
        Ok(changed)
    }
    /// Re-encrypts the entries of a different vault into this one: how many
    /// came in, and how many would not decrypt.
    pub fn absorb(
        &mut self,
        vk: &VaultKey,
        other: &Self,
        password: &str,
    ) -> Result<(usize, usize)> {
        if self.id == other.id {
            bail!("That is this vault, not a different one");
        }
        let theirs = other.unlock(password)?;
        let (mut imported, mut skipped) = (0, 0);
        for (id, s) in other.entries.iter().filter(|(_, s)| !s.deleted) {
            let Some(plain) = other.plaintext(&theirs, id) else {
                skipped += 1;
                continue;
            };
            if self.entries.get(id).is_none_or(|o| s.updated > o.updated) {
                let (nonce, ct) = seal(vk, &entry_aad(&self.id, id, s.updated, false), &plain);
                self.entries.insert(
                    id.clone(),
                    Stored {
                        nonce,
                        ct,
                        ..s.clone()
                    },
                );
                imported += 1;
            }
        }
        Ok((imported, skipped))
    }
    /// A file from elsewhere: nothing about it is trusted but its shape.
    pub fn parse(value: Value) -> Result<Self> {
        let file: Self = serde_json::from_value(value).context("Not a vault file")?;
        if file.v != 1 || !valid_id(&file.id) || !file.entries.keys().all(|id| valid_id(id)) {
            bail!("Not a vault file");
        }
        Ok(file)
    }
}

pub fn path() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("nexus/vault.json")
}
pub fn load() -> Result<Option<VaultFile>> {
    match fs::read(path()) {
        Ok(bytes) => Ok(Some(VaultFile::parse(
            serde_json::from_slice(&bytes).context("The vault file is damaged")?,
        )?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn write(file: &VaultFile) -> Result<()> {
    let path = path();
    let part = path.with_extension("json.part");
    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&part)?;
    f.write_all(&serde_json::to_vec(file)?)?;
    f.sync_all()?;
    fs::rename(&part, &path)?;
    Ok(())
}
/// Read, change, write, under the advisory lock. `f` returns the file to write,
/// or `None` to leave it as it is, and what to hand back.
pub fn update<R>(f: impl FnOnce(Option<VaultFile>) -> Result<(Option<VaultFile>, R)>) -> Result<R> {
    let path = path();
    fs::create_dir_all(path.parent().context("No data folder")?)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path.with_extension("json.lock"))?;
    lock.lock_exclusive()?;
    let (new, out) = f(load()?)?;
    if let Some(new) = new {
        write(&new)?;
    }
    Ok(out)
}

/// The vault daemon's reply, through a fresh connection. `spawn` starts the
/// daemon when nothing answers; without it, no daemon reads as an error.
pub fn request(req: &Value, spawn: bool) -> Result<Value> {
    use std::os::unix::net::UnixStream;
    let socket = crate::runtime()?.join("vault.sock");
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(_) if spawn => start_daemon(&socket)?,
        Err(_) => bail!("The vault daemon is not running"),
    };
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    line.zeroize();
    let mut reply = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(&stream), &mut reply)?;
    let mut value: Value =
        serde_json::from_str(&reply).context("The vault daemon did not answer")?;
    reply.zeroize();
    let out = if value["ok"] == true {
        Ok(value["data"].take())
    } else {
        Err(anyhow::anyhow!(
            "{}",
            value["error"].as_str().unwrap_or("The vault daemon failed")
        ))
    };
    scrub(&mut value);
    out
}
fn start_daemon(socket: &std::path::Path) -> Result<std::os::unix::net::UnixStream> {
    let mut child =
        std::process::Command::new(std::env::current_exe()?.with_file_name("nexus-vaultd"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Could not start the vault daemon")?;
    // Not left a zombie when it exits, idle, before this process.
    std::thread::spawn(move || child.wait());
    for _ in 0..100 {
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) {
            return Ok(stream);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    bail!("Could not start the vault daemon")
}

/// Common two-label public suffixes, where the site is the third label from
/// the end, and shared hosts whose subdomains are different sites.
const SUFFIXES: &[&str] = &[
    "ac.uk",
    "co.uk",
    "gov.uk",
    "ltd.uk",
    "me.uk",
    "net.uk",
    "org.uk",
    "plc.uk",
    "sch.uk",
    "ac.jp",
    "co.jp",
    "go.jp",
    "ne.jp",
    "or.jp",
    "co.kr",
    "or.kr",
    "com.au",
    "edu.au",
    "gov.au",
    "net.au",
    "org.au",
    "co.nz",
    "net.nz",
    "org.nz",
    "ac.nz",
    "co.za",
    "org.za",
    "co.in",
    "net.in",
    "org.in",
    "gov.in",
    "ac.in",
    "com.br",
    "net.br",
    "org.br",
    "gov.br",
    "com.cn",
    "net.cn",
    "org.cn",
    "edu.cn",
    "gov.cn",
    "com.hk",
    "com.mx",
    "com.tw",
    "com.sg",
    "com.tr",
    "com.ar",
    "com.pl",
    "com.ru",
    "com.ua",
    "co.il",
    "co.id",
    "co.th",
    "github.io",
    "gitlab.io",
    "vercel.app",
    "netlify.app",
    "pages.dev",
    "web.app",
    "workers.dev",
    "firebaseapp.com",
    "herokuapp.com",
    "blogspot.com",
    "azurewebsites.net",
    "cloudfront.net",
];
struct Site {
    https: bool,
    domain: String,
}
fn site(url: &str) -> Option<Site> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let https = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return None,
    };
    // `\` is a path separator to browsers: `https://evil.com\@bank.com/`.
    let authority = rest.split(['/', '?', '#', '\\']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = if let Some(v6) = host.strip_prefix('[') {
        v6.split(']').next()?.to_ascii_lowercase()
    } else {
        host.split(':').next()?.to_ascii_lowercase()
    };
    let host = host.strip_suffix('.').unwrap_or(&host);
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':') || b >= 0x80)
    {
        return None;
    }
    let domain = if host.parse::<std::net::IpAddr>().is_ok() || host == "localhost" {
        host.to_owned()
    } else {
        let labels: Vec<&str> = host.split('.').collect();
        let take = if labels.len() >= 3
            && SUFFIXES.contains(&labels[labels.len() - 2..].join(".").as_str())
        {
            3
        } else {
            2.min(labels.len())
        };
        labels[labels.len() - take..].join(".")
    };
    Some(Site { https, domain })
}
/// Whether a page may use a credential saved for `entry`: the same site, and
/// never an https-only credential on an http page.
pub fn url_matches(entry: &str, page: &str) -> bool {
    let (Some(e), Some(p)) = (site(entry), site(page)) else {
        return false;
    };
    e.domain == p.domain && (p.https || !e.https)
}
pub fn entry_matches(urls: &[String], page: &str) -> bool {
    urls.iter().any(|u| url_matches(u, page))
}

/// A uniform value below `n` (rejection sampling).
fn below(n: usize) -> usize {
    let n = n as u16;
    let zone = 256 - 256 % n;
    loop {
        let [byte] = random::<1>();
        if u16::from(byte) < zone {
            return usize::from(u16::from(byte) % n);
        }
    }
}
const LOWER: &str = "abcdefghijklmnopqrstuvwxyz";
const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGIT: &str = "0123456789";
const SYMBOL: &str = "!@#$%^&*()-_=+[]{};:,.?/";
pub const MIN_LENGTH: usize = 8;
pub const MAX_LENGTH: usize = 128;
/// A password with one of each class at least. Whole candidates are redrawn
/// until one qualifies, so every valid password stays equally likely.
pub fn generate(length: usize, symbols: bool) -> String {
    let length = length.clamp(MIN_LENGTH, MAX_LENGTH);
    let classes: &[&str] = if symbols {
        &[LOWER, UPPER, DIGIT, SYMBOL]
    } else {
        &[LOWER, UPPER, DIGIT]
    };
    let alphabet: Vec<char> = classes.iter().flat_map(|c| c.chars()).collect();
    loop {
        let candidate: String = (0..length)
            .map(|_| alphabet[below(alphabet.len())])
            .collect();
        if classes
            .iter()
            .all(|class| candidate.chars().any(|c| class.contains(c)))
        {
            return candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vector() -> (Value, VaultFile) {
        let v: Value = serde_json::from_str(include_str!("../tests/vault-vector.json")).unwrap();
        let file = VaultFile::parse(v["vault"].clone()).unwrap();
        (v, file)
    }
    const ENTRY: &str = "ffeeddccbbaa99887766554433221100";
    const TOMB: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn vector_decrypts_and_tombstone_authenticates() {
        let (v, file) = vector();
        let vk = file.unlock(v["_password"].as_str().unwrap()).unwrap();
        assert_eq!(
            hex(&*vk.0),
            v["_vault_key_hex"].as_str().unwrap(),
            "vault key"
        );
        let plain = file.plaintext(&vk, ENTRY).unwrap();
        assert_eq!(
            std::str::from_utf8(&plain).unwrap(),
            v["_entry_plaintext"].as_str().unwrap()
        );
        assert_eq!(&*file.plaintext(&vk, TOMB).unwrap(), b"{}");
        assert_eq!(file.entry(&vk, ENTRY).unwrap().password, "p@ss-é-1");
        assert!(
            file.entry(&vk, TOMB).is_none(),
            "a tombstone is not an entry"
        );
        let (list, damaged) = file.entries(&vk);
        assert_eq!((list.len(), damaged), (1, 0));
    }
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    #[test]
    fn nfc_password_also_unlocks_the_vector() {
        let (_, file) = vector();
        // Decomposed é: normalised to the same bytes.
        assert!(file.unlock("correct horse e\u{301} battery").is_ok());
    }
    #[test]
    fn tampering_id_updated_or_deleted_fails() {
        let (v, file) = vector();
        let vk = file.unlock(v["_password"].as_str().unwrap()).unwrap();
        let s = &file.entries[ENTRY];
        let ok = |vault: &str, id: &str, updated: i64, deleted: bool| {
            open(
                &vk,
                &s.nonce,
                &s.ct,
                &entry_aad(vault, id, updated, deleted),
            )
            .is_some()
        };
        assert!(ok(&file.id, ENTRY, s.updated, false));
        assert!(!ok(&file.id, TOMB, s.updated, false), "other entry id");
        assert!(!ok(&file.id, ENTRY, s.updated + 1, false), "other time");
        assert!(!ok(&file.id, ENTRY, s.updated, true), "deleted flag");
        assert!(!ok(TOMB, ENTRY, s.updated, false), "other vault id");
    }
    #[test]
    fn tampering_the_stored_fields_fails() {
        let (v, file) = vector();
        let vk = file.unlock(v["_password"].as_str().unwrap()).unwrap();
        let mut bad = file.clone();
        bad.entries.get_mut(ENTRY).unwrap().updated += 1;
        assert!(bad.entry(&vk, ENTRY).is_none());
        let mut bad = file.clone();
        bad.entries.get_mut(ENTRY).unwrap().deleted = true;
        assert!(bad.plaintext(&vk, ENTRY).is_none());
        let mut bad = file.clone();
        let flipped: String = bad.entries[ENTRY].ct.replacen('V', "W", 1);
        bad.entries.get_mut(ENTRY).unwrap().ct = flipped;
        assert!(bad.entry(&vk, ENTRY).is_none());
        // The tombstone cannot be turned back into a live entry either.
        let mut bad = file.clone();
        bad.entries.get_mut(TOMB).unwrap().deleted = false;
        assert!(bad.plaintext(&vk, TOMB).is_none());
        // Nor does a wrapped key move to another vault id.
        let mut bad = file.clone();
        bad.id = TOMB.into();
        assert!(bad.unlock(v["_password"].as_str().unwrap()).is_err());
    }
    #[test]
    fn wrong_password_says_so_and_only_so() {
        let (_, file) = vector();
        let e = file.unlock("not it").err().unwrap();
        assert_eq!(e.to_string(), "Wrong password");
        let mut damaged = file.clone();
        damaged.key.ct = "AAAA".into();
        let e = damaged.unlock("not it").err().unwrap();
        assert_eq!(e.to_string(), "Wrong password");
        let mut weak = file.clone();
        weak.kdf.iters = 99_999;
        assert!(weak.unlock("whatever").err().unwrap().to_string() != "Wrong password");
    }
    fn fresh() -> (VaultFile, VaultKey) {
        // Below the 600000 of a real vault: these run often.
        VaultFile::create_with("hunter22", 100_000).unwrap()
    }
    fn entry(password: &str) -> Entry {
        let mut e = Entry::default();
        e.title = "Example".into();
        e.urls = vec!["https://example.com".into()];
        e.username = "ada".into();
        e.password = password.into();
        e
    }
    #[test]
    fn create_save_get_round_trip() {
        let (mut f, vk) = fresh();
        let id = f.save(&vk, None, entry("one"), "pc").unwrap();
        assert_eq!(f.entry(&vk, &id).unwrap().password, "one");
        assert_eq!(f.live(), 1);
        // Same password, another write: a fresh nonce.
        let nonce = f.entries[&id].nonce.clone();
        f.save(&vk, Some(&id), entry("one"), "pc").unwrap();
        assert_ne!(f.entries[&id].nonce, nonce);
    }
    #[test]
    fn history_keeps_the_last_five_replaced() {
        let (mut f, vk) = fresh();
        let id = f.save(&vk, None, entry("p0"), "pc").unwrap();
        for i in 1..8 {
            f.save(&vk, Some(&id), entry(&format!("p{i}")), "pc")
                .unwrap();
        }
        let got = f.entry(&vk, &id).unwrap();
        let old: Vec<&str> = got.history.iter().map(|p| p.password.as_str()).collect();
        assert_eq!(old, ["p6", "p5", "p4", "p3", "p2"]);
    }
    #[test]
    fn updated_is_max_of_now_and_previous_plus_one() {
        let (mut f, vk) = fresh();
        let id = f.save(&vk, None, entry("x"), "pc").unwrap();
        let far = crate::phone::now_ms() + 1_000_000;
        // Rewrap the entry at a future time, as another device's clock would.
        f.put(&vk, &id, false, b"{}", "other");
        f.entries.get_mut(&id).unwrap().updated = far;
        let aad = entry_aad(&f.id, &id, far, false);
        let (nonce, ct) = seal(&vk, &aad, b"{}");
        let s = f.entries.get_mut(&id).unwrap();
        (s.nonce, s.ct) = (nonce, ct);
        f.save(&vk, Some(&id), entry("y"), "pc").unwrap();
        assert_eq!(f.entries[&id].updated, far + 1);
    }
    #[test]
    fn merge_newer_tuple_wins_and_tombstones_stay() {
        let (mut a, vk) = fresh();
        let id = a.save(&vk, None, entry("base"), "pc").unwrap();
        let mut b = a.clone();
        b.save(&vk, Some(&id), entry("newer"), "phone").unwrap();
        assert!(a.merge(&b).unwrap());
        assert_eq!(a.entry(&vk, &id).unwrap().password, "newer");
        // Older copy changes nothing.
        let older = a.clone();
        assert!(!a.merge(&older).unwrap());
        // A deletion syncs, and is not undone by an older write.
        let before = a.clone();
        a.delete(&vk, &id, "pc").unwrap();
        let mut c = before.clone();
        c.merge(&a).unwrap();
        assert!(c.entries[&id].deleted);
        assert!(c.entry(&vk, &id).is_none());
        assert!(!c.merge(&before).unwrap());
        assert!(
            c.entries[&id].deleted,
            "older write can't revive a tombstone"
        );
        // Equal times: the device name, then the ciphertext, settle it the same
        // way on both sides.
        let mut x = before.clone();
        let mut y = before.clone();
        x.put(&vk, &id, false, b"{}", "aaa");
        y.put(&vk, &id, false, b"{}", "bbb");
        let t = x.entries[&id].updated.max(y.entries[&id].updated) + 5;
        x.entries.get_mut(&id).unwrap().updated = t;
        y.entries.get_mut(&id).unwrap().updated = t;
        let (mut p, q) = (x.clone(), y.clone());
        p.merge(&q).unwrap();
        let mut r = y.clone();
        r.merge(&x).unwrap();
        assert_eq!(p.entries[&id].device, "bbb");
        assert_eq!(r.entries[&id].device, "bbb");
    }
    #[test]
    fn merge_refuses_a_different_vault() {
        let (mut a, _) = fresh();
        let (b, _) = fresh();
        assert!(a.merge(&b).is_err());
    }
    #[test]
    fn key_wrap_and_kdf_move_together_and_entries_survive() {
        let (mut a, vk) = fresh();
        let id = a.save(&vk, None, entry("secret"), "pc").unwrap();
        let mut other = a.clone();
        std::thread::sleep(std::time::Duration::from_millis(3));
        other.change_password("hunter22", "correct horse").unwrap();
        assert!(a.merge(&other).unwrap());
        assert_eq!(a.kdf.salt, other.kdf.salt);
        assert_eq!(a.key.ct, other.key.ct);
        let vk2 = a.unlock("correct horse").unwrap();
        assert!(a.unlock("hunter22").is_err());
        assert_eq!(a.entry(&vk2, &id).unwrap().password, "secret");
        // And not backwards.
        let mut stale = a.clone();
        stale.key.updated -= 1;
        stale.key.ct = "AAAA".into();
        assert!(!a.merge(&stale).unwrap());
    }
    #[test]
    fn absorb_reencrypts_a_different_vault() {
        let (mut a, vk) = fresh();
        let (mut b, vkb) = fresh();
        let id = b.save(&vkb, None, entry("theirs"), "phone").unwrap();
        let gone = b.save(&vkb, None, entry("dead"), "phone").unwrap();
        b.delete(&vkb, &gone, "phone").unwrap();
        assert!(a.absorb(&vk, &b, "nope").is_err());
        assert_eq!(a.absorb(&vk, &b, "hunter22").unwrap(), (1, 0));
        let got = a.entry(&vk, &id).unwrap();
        assert_eq!(got.password, "theirs");
        assert!(!a.entries.contains_key(&gone), "tombstones stay behind");
        assert_eq!(a.id.len(), 32);
        assert!(a.absorb(&vk, &a.clone(), "hunter22").is_err());
    }
    #[test]
    fn unknown_fields_are_kept_and_shape_is_checked() {
        let (v, file) = vector();
        let mut raw = v["vault"].clone();
        raw["future"] = json!(7);
        raw["entries"][ENTRY]["hint"] = json!("x");
        let parsed = VaultFile::parse(raw).unwrap();
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back["future"], 7);
        assert_eq!(back["entries"][ENTRY]["hint"], "x");
        let mut bad = v["vault"].clone();
        bad["v"] = json!(2);
        assert!(VaultFile::parse(bad).is_err());
        let mut bad = v["vault"].clone();
        bad["entries"]["../evil"] = bad["entries"][ENTRY].clone();
        assert!(VaultFile::parse(bad).is_err());
        assert_eq!(file.live(), 1);
    }
    #[test]
    fn base64_round_trip() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len as u8).map(|i| i.wrapping_mul(37)).collect();
            assert_eq!(unb64(&b64(&bytes)).unwrap(), bytes);
        }
        assert_eq!(b64(b"any carnal pleas"), "YW55IGNhcm5hbCBwbGVhcw==");
        assert!(unb64("not base64").is_none());
        assert!(unb64("QQ=A").is_none());
        assert!(unb64("=").is_none());
    }
    #[test]
    fn urls_match_by_site() {
        let m = url_matches;
        assert!(m("https://example.com/login", "https://example.com/other"));
        assert!(m("https://example.com", "https://a.example.com/x"));
        assert!(m("https://a.example.com", "https://b.example.com"));
        assert!(m("https://EXAMPLE.com", "HTTPS://example.com"));
        assert!(m("https://example.com:8443", "https://example.com"));
        assert!(m("https://u:p@example.com/", "https://example.com"));
        assert!(m("https://bbc.co.uk", "https://www.bbc.co.uk/news"));
        assert!(m("http://example.com", "http://example.com"));
        assert!(m("http://example.com", "https://example.com"));
        // Lookalikes.
        assert!(!m("https://example.com", "https://example.com.evil.net"));
        assert!(!m("https://example.com", "https://evil-example.com"));
        assert!(!m("https://example.com", "https://exampIe.com"));
        assert!(
            !m("https://example.com", "https://exаmple.com"),
            "Cyrillic а"
        );
        assert!(!m(
            "https://example.com",
            "https://notexample.com/example.com"
        ));
        assert!(!m("https://example.com", "https://evil.net/?u=example.com"));
        assert!(!m("https://example.com", "https://evil.net#example.com"));
        assert!(!m("https://example.com", "https://example.com@evil.net/"));
        assert!(!m("https://example.com", "https://evil.net\\@example.com/"));
        assert!(!m("https://bank.co.uk", "https://other.co.uk"));
        assert!(!m("https://alice.github.io", "https://bob.github.io"));
        // http vs https.
        assert!(!m("https://example.com", "http://example.com"));
        assert!(m("https://example.com", "https://example.com"));
        // Only http(s).
        assert!(!m("https://example.com", "ftp://example.com"));
        assert!(!m("ftp://example.com", "https://example.com"));
        assert!(!m("androidapp://com.example", "https://example.com"));
        assert!(!m("https://example.com", "javascript://example.com"));
        assert!(!m("example.com", "https://example.com"));
        // IPs and localhost match on the whole host.
        assert!(m("http://192.168.1.2:80/", "http://192.168.1.2"));
        assert!(!m("http://192.168.1.2", "http://192.168.1.3"));
        assert!(m("http://[::1]:3000/", "http://[::1]/x"));
        assert!(!m("http://[::1]", "http://[::2]"));
        assert!(m("http://localhost:3000", "http://localhost:8080"));
        assert!(!m("http://localhost", "http://foo.localhost"));
    }
    #[test]
    fn entries_match_through_any_url() {
        let urls = vec![
            "androidapp://com.x".to_owned(),
            "https://a.com".to_owned(),
            "http://b.com".to_owned(),
        ];
        assert!(entry_matches(&urls, "https://a.com/x"));
        assert!(entry_matches(&urls, "http://b.com/x"));
        assert!(!entry_matches(&urls, "https://c.com"));
    }
    #[test]
    fn generator_composition() {
        for len in [8, 20, 64, 128] {
            for symbols in [true, false] {
                let p = generate(len, symbols);
                assert_eq!(p.chars().count(), len);
                assert!(p.chars().any(|c| LOWER.contains(c)), "{p}");
                assert!(p.chars().any(|c| UPPER.contains(c)), "{p}");
                assert!(p.chars().any(|c| DIGIT.contains(c)), "{p}");
                assert_eq!(p.chars().any(|c| SYMBOL.contains(c)), symbols, "{p}");
            }
        }
        assert_eq!(generate(0, true).chars().count(), MIN_LENGTH);
        assert_eq!(generate(9999, true).chars().count(), MAX_LENGTH);
    }
    #[test]
    fn generator_is_not_biased() {
        let n = 86;
        let draws = 200_000;
        let mut counts = vec![0usize; n];
        for _ in 0..draws {
            counts[below(n)] += 1;
        }
        let expected = draws as f64 / n as f64;
        // Five sigma either way: a biased `below` lands far outside.
        let sigma = expected.sqrt();
        for (i, count) in counts.iter().enumerate() {
            assert!(
                (*count as f64 - expected).abs() < 5.0 * sigma,
                "symbol {i} drawn {count}, expected {expected}"
            );
        }
        // Rejection never reaches past the zone.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            seen.insert(below(3));
        }
        assert_eq!(seen.len(), 3);
    }
}
