//! A real `nexus-vaultd`, spoken to over its socket: one client it must turn
//! away, and one it must serve, which has to be a binary called `nexus`
//! beside it. That one is a copy of this test binary running `session` below.
use nexus_control::vault::{self, VaultFile};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    thread::sleep,
    time::Duration,
};

fn call(req: Value) -> anyhow::Result<Value> {
    vault::request(&req, false)
}
fn ok(req: Value) -> Value {
    let what = req["op"].clone();
    call(req).unwrap_or_else(|e| panic!("{what}: {e}"))
}
fn refused(req: Value) -> String {
    let what = req["op"].clone();
    match call(req) {
        Err(e) => e.to_string(),
        Ok(v) => panic!("{what} should have failed: {v}"),
    }
}

/// Runs inside the copy named `nexus`; `round_trip_and_peer_rejection` starts it.
#[test]
#[ignore = "started by round_trip_and_peer_rejection"]
fn session() {
    let first = ok(json!({"op":"status"}));
    assert_eq!(
        first,
        json!({"exists":false,"unlocked":false,"count":0,"damaged":0})
    );
    assert_eq!(
        refused(json!({"op":"unlock","password":"anything at all"})),
        "No vault here yet"
    );
    assert_eq!(
        refused(json!({"op":"create","password":"short"})),
        "Use at least 8 characters"
    );
    ok(json!({"op":"create","password":"the master password"}));
    assert_eq!(ok(json!({"op":"status"}))["unlocked"], true);

    let id = ok(
        json!({"op":"save","title":"Example","urls":["https://example.com/login"],
        "username":"ada","password":"s3cret","notes":"nb"}),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let raw = fs::read_to_string(vault::path()).unwrap();
    for leak in ["s3cret", "ada", "Example", "the master password"] {
        assert!(!raw.contains(leak), "{leak} is in the file in the clear");
    }
    assert_eq!(
        fs::metadata(vault::path()).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let list = ok(json!({"op":"list"}));
    assert_eq!(list[0]["username"], "ada");
    assert!(list[0].get("password").is_none(), "{list}");

    let got = ok(json!({"op":"get","id":id}));
    assert_eq!(got["password"], "s3cret");
    assert_eq!(got["id"], id.as_str());

    let matched = ok(json!({"op":"match","url":"https://www.example.com/"}));
    assert_eq!(matched.as_array().unwrap().len(), 1);
    assert!(matched[0].get("password").is_none());
    assert_eq!(
        ok(json!({"op":"match","url":"https://example.com.evil.net/"})),
        json!([])
    );
    assert_eq!(
        ok(json!({"op":"match","url":"http://example.com/"})),
        json!([]),
        "http must not see an https-only entry"
    );

    assert_eq!(
        ok(json!({"op":"fill","id":id,"url":"https://example.com/"}))["password"],
        "s3cret"
    );
    assert_eq!(
        refused(json!({"op":"fill","id":id,"url":"https://evil.net/"})),
        "This entry is not for that site"
    );

    let generated = ok(json!({"op":"generate","length":32,"symbols":false}));
    let p = generated["password"].as_str().unwrap();
    assert_eq!(p.len(), 32);
    assert!(p.chars().all(|c| c.is_ascii_alphanumeric()));

    ok(
        json!({"op":"save","id":id,"title":"Example","urls":["https://example.com/login"],
        "username":"ada","password":"changed","notes":""}),
    );
    let got = ok(json!({"op":"get","id":id}));
    assert_eq!(got["password"], "changed");
    assert_eq!(got["history"][0]["password"], "s3cret");

    // A different vault's entries come in, re-encrypted, under its own password.
    let (mut other, other_key) = VaultFile::create("the other password").unwrap();
    let mut e = vault::Entry::default();
    e.title = "Elsewhere".into();
    e.password = "from the phone".into();
    let their_id = other.save(&other_key, None, e, "phone").unwrap();
    let file = serde_json::to_value(&other).unwrap();
    assert_eq!(
        refused(json!({"op":"merge_other","vault":file,"password":"wrong"})),
        "Wrong password"
    );
    // That failure counts: the next attempt waits, whoever makes it.
    assert!(
        refused(json!({"op":"merge_other","vault":file,"password":"the other password"}))
            .starts_with("Too many attempts")
    );
    sleep(Duration::from_millis(1100));
    let merged = ok(json!({"op":"merge_other","vault":file,"password":"the other password"}));
    assert_eq!(merged, json!({"imported":1,"skipped":0}));
    assert_eq!(
        ok(json!({"op":"get","id":their_id}))["password"],
        "from the phone"
    );

    // Locked, nothing but the password opens it; unknown peers and ops are refused.
    ok(json!({"op":"lock"}));
    assert_eq!(ok(json!({"op":"status"}))["unlocked"], false);
    assert_eq!(refused(json!({"op":"list"})), "The vault is locked");
    assert_eq!(refused(json!({"op":"get","id":id})), "The vault is locked");
    assert_eq!(
        refused(json!({"op":"fill","id":id,"url":"https://example.com/"})),
        "The vault is locked"
    );
    assert_eq!(refused(json!({"op":"unknown"})), "Unknown request");

    assert_eq!(
        refused(json!({"op":"unlock","password":"not the master password"})),
        "Wrong password"
    );
    assert!(
        refused(json!({"op":"unlock","password":"the master password"}))
            .starts_with("Too many attempts")
    );
    sleep(Duration::from_millis(1100));
    ok(json!({"op":"unlock","password":"the master password"}));
    assert_eq!(ok(json!({"op":"status"}))["count"], 2);

    assert_eq!(
        refused(json!({"op":"change_password","old":"wrong","new":"another password"})),
        "Wrong password"
    );
    sleep(Duration::from_millis(2100));
    ok(json!({"op":"change_password","old":"the master password","new":"another password"}));
    ok(json!({"op":"lock"}));
    sleep(Duration::from_millis(1100));
    assert_eq!(
        refused(json!({"op":"unlock","password":"the master password"})),
        "Wrong password"
    );
    sleep(Duration::from_millis(1100));
    ok(json!({"op":"unlock","password":"another password"}));
    ok(json!({"op":"delete","id":id}));
    let list = ok(json!({"op":"list"}));
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["title"], "Elsewhere");
    ok(json!({"op":"lock"}));
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn folder(p: &Path) -> PathBuf {
    fs::create_dir_all(p).unwrap();
    p.to_owned()
}

#[test]
fn round_trip_and_peer_rejection() {
    let base = std::env::temp_dir().join(format!("nexus-vault-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let bin = folder(&base.join("bin"));
    let run = folder(&base.join("run"));
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
    let data = folder(&base.join("data"));
    let state = folder(&base.join("state"));
    fs::copy(env!("CARGO_BIN_EXE_nexus-vaultd"), bin.join("nexus-vaultd")).unwrap();
    fs::copy(std::env::current_exe().unwrap(), bin.join("nexus")).unwrap();
    let env = |c: &mut Command| {
        c.env("XDG_RUNTIME_DIR", &run)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_STATE_HOME", &state);
    };
    let mut daemon = Command::new(bin.join("nexus-vaultd"));
    env(&mut daemon);
    let _daemon = Daemon(daemon.spawn().unwrap());
    let socket = run.join("nexus/vault.sock");
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        sleep(Duration::from_millis(50));
    }

    // This binary is not an installed Nexus one: it gets no answer at all.
    let mut stranger = UnixStream::connect(&socket).unwrap();
    stranger
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(stranger, r#"{{"op":"status"}}"#).unwrap();
    let mut line = String::new();
    let n = BufReader::new(&stranger).read_line(&mut line).unwrap_or(0);
    assert_eq!((n, line.as_str()), (0, ""), "a stranger got an answer");

    let mut client = Command::new(bin.join("nexus"));
    env(&mut client);
    let status = client
        .args([
            "--exact",
            "session",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "the session in the `nexus` copy failed");
    let _ = fs::remove_dir_all(&base);
}
