use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::net::UnixListener,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn activation_does_not_wait_for_daemon() {
    let root = std::env::temp_dir().join(format!("nexus-startup-{}", std::process::id()));
    fs::create_dir_all(root.join("nexus")).unwrap();
    // A listening but unresponsive daemon models slow D-Bus startup. Activation
    // must go straight to the resident UI, without requesting daemon status.
    let daemon = UnixListener::bind(root.join("nexus/daemon.sock")).unwrap();
    daemon.set_nonblocking(true).unwrap();
    let ui = UnixListener::bind(root.join("nexus/ui.sock")).unwrap();
    ui.set_nonblocking(true).unwrap();
    for page in ["shell", "launcher", "wifi", "close"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nexus"))
            .arg(page)
            .env("XDG_RUNTIME_DIR", &root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(
            status.is_some_and(|status| status.success()),
            "{page} waited for the daemon"
        );
        let (stream, _) = ui.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut message = String::new();
        BufReader::new(stream).read_line(&mut message).unwrap();
        assert_eq!(message, format!("{page}\n"));
    }
    assert_eq!(
        daemon.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    fs::remove_dir_all(root).unwrap();
}
