//! Package transactions without a terminal: the manager runs under a pty that
//! outlives each request, because sudo's timestamp belongs to a terminal and to
//! the session on it, so one password carries from one request to the next for
//! as long as sudo itself allows. The password is written to the pty only when
//! the prompt is up, nothing was printed since, echo is verified off and a process
//! holding root rights, which a build script cannot fake, is on the terminal.
//! Afterwards it is kept solely to blank it out of what is captured, for that
//! request at most, never to answer another prompt.
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// What sudo prints when it wants the password, as `SUDO_PROMPT`.
const SUDO_MARKER: &str = "[nexus-sudo-password]";
/// A secret or an idle session never lasts longer than this after its last use.
pub const KEEP: Duration = Duration::from_secs(15 * 60);
/// A silent, unfinished line this old is taken for a question nothing recognised.
const STALL: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(20)
};
/// A commit silent this long may be stuck on a scriptlet: Cancel comes back, and
/// only ever sends the interrupt, which pacman itself ignores while committing.
const STUCK: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(5 * 60)
};
/// Runs one line at a time, all in one terminal session, and answers on fd 3
/// with the exit status. It survives a cancel, and so does sudo's timestamp: the
/// traps only outlive the signals here, a subshell and what it runs get the
/// default action back. `exec`, so that the status is the manager's own even when
/// it takes its time over a signal. The bare `echo` ends a line left unfinished,
/// so that it cannot run into the next request. The lines are built by `quote`.
const LEADER: &str = r#"trap : HUP INT QUIT TERM
while :; do
    IFS= read -r line <&3; s=$?
    [ "$s" -gt 128 ] && continue
    [ "$s" -eq 0 ] || exit 0
    ( eval "exec $line" ) 3<&-
    s=$?
    echo
    echo "$s" >&3
done"#;

/// The stand-in for pacman, yay and sudo in the lab: one explicit variable with
/// an absolute path, read nowhere else. When it is set, nothing real is run. Only
/// tests and `--features lab` builds look: it skips the root check and receives
/// the password, so a variable leaked into a session must do nothing.
pub fn fake_manager() -> Option<String> {
    if !cfg!(any(test, feature = "lab")) {
        return None;
    }
    std::env::var("NEXUS_PACKAGES_FAKE")
        .ok()
        .filter(|path| path.starts_with('/'))
}

/// A secret overwritten when it is dropped. No `Debug`, so that it cannot be logged.
pub struct Secret(Vec<u8>);
impl Secret {
    pub fn new(bytes: &[u8]) -> Self {
        // Room for the newline, so that adding it does not leave a copy behind.
        let mut kept = Vec::with_capacity(bytes.len() + 1);
        kept.extend_from_slice(bytes);
        Self(kept)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        crate::lock::pam::wipe(&mut self.0);
    }
}
/// What is known of a running transaction.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A finished line of output, without colours or the password.
    Line(String),
    /// A line the manager keeps overwriting, such as a download's progress bar.
    Transient(String),
    /// sudo wants the password; `rejected` when it just refused the last one.
    Password { rejected: bool },
    /// The manager asks something and waits.
    Question(String),
    /// Whether stopping now would leave the system half changed.
    Committing(bool),
    /// `(done, total)` of the manager's own count.
    Progress(u32, u32),
    /// Over: the exit status, or None when it was stopped.
    Done(Option<i32>),
}
/// A package name as pacman allows it. Names go through as arguments of their
/// own, but one that starts with a dash would still be read as an option.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._+-".contains(c))
}
/// The commands this page can run. There is no path to a partial upgrade.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    UpdateAll,
    /// The same without the AUR, for when it cannot be asked what would be built:
    /// yay could then build what nobody reviewed.
    UpdateOfficial,
    /// From a named repository, so that what was reviewed is what gets built
    /// when the AUR and a repository share a name.
    Install {
        repo: String,
        name: String,
    },
    Remove(String),
}
impl Change {
    /// The command for `manager`, `yay` or `pacman`; an error for a bad name.
    pub fn command(&self, manager: &str) -> Result<Vec<String>, String> {
        let refused = |name: &str| Err(format!("Refused the package name “{name}”."));
        let manager = if *self == Change::UpdateOfficial {
            "pacman"
        } else {
            manager
        };
        let mut args: Vec<String> = match self {
            Change::UpdateAll | Change::UpdateOfficial => vec!["-Syu".into()],
            Change::Install { repo, name } => {
                if !valid_name(name) {
                    return refused(name);
                }
                if !valid_name(repo) {
                    return refused(repo);
                }
                vec!["-S".into(), format!("{repo}/{name}")]
            }
            Change::Remove(name) => {
                if !valid_name(name) {
                    return refused(name);
                }
                vec!["-Rns".into(), name.clone()]
            }
        };
        args.extend(["--noconfirm", "--color", "never"].map(String::from));
        if manager == "yay" {
            args.extend(
                [
                    "--sudoloop",
                    "--answerclean",
                    "None",
                    "--answerdiff",
                    "None",
                    "--answeredit",
                    "None",
                    "--answerupgrade",
                    "None",
                ]
                .map(String::from),
            );
        }
        let mut command = match fake_manager() {
            Some(fake) => vec![fake],
            None if manager == "yay" => vec!["yay".into()],
            None => vec!["sudo".into(), "pacman".into()],
        };
        command.append(&mut args);
        Ok(command)
    }
}
/// One word for the session's shell.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}
/// Every occurrence of the secret out of `text`.
pub fn scrub(text: &str, secret: &[u8]) -> String {
    // Lossy as the output is, so that a password not in UTF-8 still matches.
    let secret = String::from_utf8_lossy(secret);
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(&*secret, "•••")
}
/// Colours, cursor moves and what a carriage return overwrites out of a line.
pub fn clean(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.next_if_eq(&'[').is_some() {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\r' if chars.peek().is_some() => out.clear(),
            '\r' => {}
            '\u{8}' => {
                out.pop();
            }
            c if c.is_control() && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}
/// A question the manager waits on, from the line it has not finished.
fn question(tail: &str) -> bool {
    let tail = tail.trim_end();
    tail.ends_with("[Y/n]")
        || tail.ends_with("[y/N]")
        || tail.ends_with("(default=1):")
        || tail.contains("Enter a selection")
        || tail.contains("Enter a number")
}
/// The manager's own count, `(3/12) upgrading foot`.
fn progress(line: &str) -> Option<(u32, u32)> {
    let (count, _) = line.trim_start().strip_prefix('(')?.split_once(')')?;
    let (done, total) = count.trim().split_once('/')?;
    Some((done.trim().parse().ok()?, total.trim().parse().ok()?))
}
/// `Some(true)` once pacman starts changing the system: interrupting it from
/// there stops between two packages. `Some(false)` when that is over and an AUR
/// build starts, which can be dropped at any time.
fn phase(line: &str) -> Option<bool> {
    let line = line.trim_start();
    if line.starts_with(":: Processing package changes")
        || line.starts_with(":: Running pre-transaction hooks")
    {
        Some(true)
    } else if line.starts_with("==> Making package") {
        Some(false)
    } else {
        None
    }
}
/// What best explains a failure: the last words, from the first error among them
/// on, as what follows an error details it. An older error goes on top.
pub fn failure_text<'a>(lines: impl DoubleEndedIterator<Item = &'a String>) -> String {
    const SHOWN: usize = 8;
    let is_error = |line: &str| {
        let line = line.trim_start();
        ["error:", "==> ERROR", "-> error", "fatal:", "sudo:"]
            .iter()
            .any(|start| line.starts_with(start))
    };
    let lines: Vec<&str> = lines.map(String::as_str).collect();
    let (older, last) = lines.split_at(lines.len().saturating_sub(SHOWN));
    let mut shown = match last.iter().position(|line| is_error(line)) {
        Some(first) => last[first..].to_vec(),
        None => last.to_vec(),
    };
    if !last.iter().any(|line| is_error(line))
        && let Some(error) = older.iter().rev().find(|line| is_error(line))
    {
        shown.insert(0, error);
    }
    shown.join("\n")
}
/// Whether the screen is locked: `nexus lock` or hyprlock runs.
// ponytail: same as the private check in src/phone.rs, share them once one file owns it.
pub fn screen_locked() -> bool {
    std::fs::read_dir("/proc").is_ok_and(|procs| {
        procs.flatten().any(|p| {
            // `cmdline` can wait on a stuck process, `comm` cannot: only the two
            // names that can be a locker get that far.
            std::fs::read(p.path().join("comm"))
                .is_ok_and(|name| matches!(name.trim_ascii_end(), b"hyprlock" | b"nexus"))
                && std::fs::read(p.path().join("cmdline")).is_ok_and(|c| is_locker(&c))
        })
    })
}
fn is_locker(cmdline: &[u8]) -> bool {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let name = args[0].rsplit(|b| *b == b'/').next().unwrap_or_default();
    name == b"hyprlock"
        || (name == b"nexus"
            && args.get(1) == Some(&&b"lock"[..])
            && !args.contains(&&b"--preview"[..]))
}
/// Sends what Ctrl+C or Ctrl+\\ would, the only signals the kernel allows through
/// a pty. A plain `kill` from here cannot reach what runs as root; this can, and
/// sudo passes it on.
fn interrupt(master: &File, signal: libc::c_int) {
    // SAFETY: a valid descriptor and an integer argument.
    unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSIG, signal) };
}
/// Whether this `/proc/<pid>/stat` and `status` describe sudo in that session,
/// holding root in one of its ids. A setuid sudo does, even while it reads the
/// password as the user. Nothing unprivileged can: the name can be chosen, the ids
/// cannot. `fakeroot` only lies to its own children, not through `/proc`.
fn is_sudo(stat: &str, status: &str, session: i32) -> bool {
    let Some((head, tail)) = stat.rsplit_once(") ") else {
        return false;
    };
    // After the name: state, parent, group, session.
    let in_session = tail.split_whitespace().nth(3) == Some(session.to_string().as_str());
    let root = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .is_some_and(|ids| ids.split_whitespace().any(|id| id == "0"));
    in_session && root && head.ends_with("(sudo")
}
// ponytail: tells a real sudo is on the terminal, not that it alone reads it. A build could start sudo and race it for the input; it runs with the owner's rights and root scriptlets anyway.
fn sudo_on_terminal(session: i32) -> bool {
    std::fs::read_dir("/proc").is_ok_and(|procs| {
        procs.flatten().any(|p| {
            let read = |file| std::fs::read_to_string(p.path().join(file));
            matches!((read("stat"), read("status")), (Ok(stat), Ok(status)) if is_sudo(&stat, &status, session))
        })
    })
}
fn echo_off(master: &File) -> bool {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr fills the structure when it returns 0.
    unsafe {
        libc::tcgetattr(master.as_raw_fd(), termios.as_mut_ptr()) == 0
            && termios.assume_init().c_lflag & libc::ECHO == 0
    }
}
/// A prompt killed before it could restore the terminal must not leave echo off,
/// or the check before writing the password would prove nothing the next time.
fn restore_echo(master: &File) {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: the structure is only read after tcgetattr filled it.
    unsafe {
        if libc::tcgetattr(master.as_raw_fd(), termios.as_mut_ptr()) == 0 {
            let mut termios = termios.assume_init();
            termios.c_lflag |= libc::ECHO;
            libc::tcsetattr(master.as_raw_fd(), libc::TCSANOW, &termios);
        }
    }
}
/// Every attempt of the running request, as a mistyped one is close to the real
/// thing, with the time of the last.
type Kept = Arc<Mutex<(Vec<Secret>, Instant)>>;
/// What is needed to tell whether a prompt on the terminal is sudo's.
struct Asking {
    /// A prompt was recognised and nothing has been printed since.
    open: AtomicBool,
    session: i32,
    /// Tests and the lab's stand-in, which has no root to show. See `fake_manager`.
    any_program: bool,
}
impl Asking {
    fn is_sudo(&self, master: &File) -> bool {
        echo_off(master) && (self.any_program || sudo_on_terminal(self.session))
    }
}
/// The pty and the shell that keeps its session alive.
pub struct Session {
    master: Arc<File>,
    leader: Option<Child>,
    commands: UnixStream,
    secret: Kept,
    asking: Arc<Asking>,
    /// The request running, 0 when none is.
    current: Arc<AtomicU64>,
    requests: u64,
    cancelled: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    committing: Arc<AtomicBool>,
    events: async_channel::Sender<Event>,
    pub receiver: async_channel::Receiver<Event>,
}
impl Session {
    pub fn start() -> Result<Self, String> {
        Self::start_trusting(cfg!(test) || fake_manager().is_some())
    }
    fn start_trusting(any_program: bool) -> Result<Self, String> {
        let error = |what: &str| format!("{what}: {}", std::io::Error::last_os_error());
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: valid pointers; the rest may be null.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } != 0
        {
            return Err(error("Could not open a terminal for the manager"));
        }
        // SAFETY: openpty just gave these out, and nothing else owns them.
        let (master, slave) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: valid descriptors, a valid winsize, a flag taking no pointer.
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size);
        }
        let (commands, theirs) =
            UnixStream::pair().map_err(|e| format!("Could not talk to the manager: {e}"))?;
        let stdio = |fd: &OwnedFd| fd.try_clone().map(Stdio::from);
        let theirs_fd = theirs.as_raw_fd();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", LEADER])
            .env("SUDO_PROMPT", SUDO_MARKER)
            // English, for the lines read here, without breaking builds that want UTF-8.
            .env("LC_ALL", "C.UTF-8")
            .env_remove("LANGUAGE")
            .env_remove("SUDO_ASKPASS")
            .env("TERM", "dumb")
            .env("PAGER", "cat")
            .stdin(stdio(&slave).map_err(|e| e.to_string())?)
            .stdout(stdio(&slave).map_err(|e| e.to_string())?)
            .stderr(stdio(&slave).map_err(|e| e.to_string())?);
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                // Whatever started this shell may have ignored them, as nohup does,
                // and an ignored signal stays ignored through exec and through trap.
                for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
                    libc::signal(signal, libc::SIG_DFL);
                }
                // dup2 onto itself would leave close-on-exec set.
                let fd3_ok = if theirs_fd == 3 {
                    libc::fcntl(3, libc::F_SETFD, 0) == 0
                } else {
                    libc::dup2(theirs_fd, 3) >= 0
                };
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 || !fd3_ok {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let leader = command
            .spawn()
            .map_err(|e| format!("Could not start the session: {e}"))?;
        drop((slave, theirs));
        let session = leader.id() as i32;
        // Bounded: a manager outrunning the page waits for it instead of filling memory.
        let (events, receiver) = async_channel::bounded(1024);
        let this = Self {
            master: Arc::new(master),
            leader: Some(leader),
            commands,
            secret: Arc::new(Mutex::new((vec![], Instant::now()))),
            asking: Arc::new(Asking {
                open: AtomicBool::new(false),
                session,
                any_program,
            }),
            current: Arc::default(),
            requests: 0,
            cancelled: Arc::default(),
            closed: Arc::default(),
            committing: Arc::default(),
            events,
            receiver,
        };
        let (master, secret, asking, closed, committing, events) = (
            this.master.clone(),
            this.secret.clone(),
            this.asking.clone(),
            this.closed.clone(),
            this.committing.clone(),
            this.events.clone(),
        );
        std::thread::spawn(move || {
            read_output(&master, &secret, &asking, &closed, &committing, &events);
        });
        Ok(this)
    }
    /// Whether the session can take another request.
    pub fn alive(&mut self) -> bool {
        self.leader
            .as_mut()
            .is_some_and(|leader| matches!(leader.try_wait(), Ok(None)))
    }
    /// Starts `command`; `Done` follows when it ends.
    pub fn run(&mut self, command: &[String]) -> Result<(), String> {
        restore_echo(&self.master);
        self.cancelled.store(false, Ordering::Relaxed);
        self.committing.store(false, Ordering::Relaxed);
        self.requests += 1;
        self.current.store(self.requests, Ordering::Relaxed);
        let line = command
            .iter()
            .map(|w| quote(w))
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(self.commands, "{line}")
            .map_err(|e| format!("The manager's session is gone: {e}"))?;
        let reader = self
            .commands
            .try_clone()
            .map_err(|e| format!("The manager's session is gone: {e}"))?;
        let (events, secret, current, cancelled, committing) = (
            self.events.clone(),
            self.secret.clone(),
            self.current.clone(),
            self.cancelled.clone(),
            self.committing.clone(),
        );
        std::thread::spawn(move || {
            let mut status = String::new();
            let read = BufReader::new(reader).read_line(&mut status);
            // What it printed last is still on its way through the pty.
            // ponytail: a fixed wait, a marker line in the pty if output ever arrives late.
            std::thread::sleep(Duration::from_millis(120));
            current.store(0, Ordering::Relaxed);
            committing.store(false, Ordering::Relaxed);
            secret.lock().unwrap().0.clear();
            let code = match read {
                Ok(n) if n > 0 => status.trim().parse().ok(),
                _ => None,
            };
            // Stopped, unless it was too late and it got there all the same.
            let code = code.filter(|c| *c == 0 || !cancelled.load(Ordering::Relaxed));
            let _ = events.send_blocking(Event::Done(code));
        });
        Ok(())
    }
    /// Writes the password to the pty, once, if sudo's prompt is still the last
    /// thing printed and echo is off, which sudo sets before printing it. It is
    /// kept before that, in case the terminal repeats it anyway, and only to blank
    /// it out of the output, until the request ends, `KEEP` passes or the screen
    /// locks.
    pub fn answer(&self, mut password: Secret) -> Result<(), String> {
        if !self.asking.open.swap(false, Ordering::SeqCst) || !self.asking.is_sudo(&self.master) {
            return Err(
                "Not sent: sudo is no longer waiting on this prompt. Cancel and try again.".into(),
            );
        }
        let mut kept = self.secret.lock().unwrap();
        password.0.push(b'\n');
        let written = self.write(&password.0);
        password.0.pop();
        kept.0.push(password);
        kept.1 = Instant::now();
        written
    }
    /// Wipes the password now.
    pub fn forget(&self) {
        self.secret.lock().unwrap().0.clear();
    }
    /// An answer to a [`Event::Question`].
    pub fn reply(&self, text: &str) -> Result<(), String> {
        self.write(format!("{text}\n").as_bytes())
    }
    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        (&*self.master)
            .write_all(bytes)
            .map_err(|e| format!("Could not write to the manager's terminal: {e}"))
    }
    /// Stops the running request, harder if it does not listen. The session stays,
    /// and with it sudo's timestamp, unless it takes the last signal to get there;
    /// dropping the dead session then hangs up on whatever root left behind.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.forget();
        interrupt(&self.master, libc::SIGINT);
        let (master, current, committing) = (
            self.master.clone(),
            self.current.clone(),
            self.committing.clone(),
        );
        let request = current.load(Ordering::Relaxed);
        let group = self.leader.as_ref().map_or(0, |leader| leader.id() as i32);
        std::thread::spawn(move || {
            // The harder signals would break a commit, which may have just begun.
            let running = || {
                request != 0
                    && current.load(Ordering::Relaxed) == request
                    && !committing.load(Ordering::Relaxed)
            };
            std::thread::sleep(Duration::from_secs(2));
            if running() {
                interrupt(&master, libc::SIGQUIT);
            }
            std::thread::sleep(Duration::from_secs(3));
            if running() && group > 0 {
                // SAFETY: the group of the unreaped session leader.
                unsafe { libc::kill(-group, libc::SIGKILL) };
            }
        });
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        self.forget();
        let _ = self.commands.shutdown(std::net::Shutdown::Both);
        // Past this point, leaving it to finish is the only way not to break the
        // system: it has no terminal to wait on and its session ends with it.
        let committing = self.committing.load(Ordering::Relaxed);
        if !committing {
            interrupt(&self.master, libc::SIGINT);
        }
        let (master, leader) = (self.master.clone(), self.leader.take());
        std::thread::spawn(move || {
            if !committing {
                std::thread::sleep(Duration::from_secs(3));
            }
            // Closing the last handle hangs up on whatever is still on the terminal.
            drop(master);
            if let Some(mut leader) = leader {
                if !committing {
                    // SAFETY: the group of the unreaped session leader.
                    unsafe { libc::kill(-(leader.id() as i32), libc::SIGKILL) };
                }
                let _ = leader.wait();
            }
        });
    }
}
/// Turns the terminal's bytes into events until it has no one left on it.
fn read_output(
    master: &File,
    secret: &Kept,
    asking: &Asking,
    closed: &AtomicBool,
    committing: &AtomicBool,
    events: &async_channel::Sender<Event>,
) {
    const MAX_LINE: usize = 64 * 1024;
    let shown = |text: &str| {
        let kept = secret.lock().unwrap();
        kept.0
            .iter()
            .fold(text.to_string(), |text, s| scrub(&text, &s.0))
    };
    let send = |event| {
        let _ = events.send_blocking(event);
    };
    let mut pending = Vec::<u8>::new();
    let mut buffer = [0u8; 4096];
    let mut asked = false;
    let mut refused = false;
    let mut stuck_said = false;
    let mut last_output = Instant::now();
    let mut poll = libc::pollfd {
        fd: master.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    while !closed.load(Ordering::Relaxed) {
        {
            let mut kept = secret.lock().unwrap();
            if kept.1.elapsed() > KEEP {
                kept.0.clear();
            }
        }
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut poll, 1, 500) } <= 0 {
            if !asked && !pending.is_empty() && last_output.elapsed() > STALL {
                let text = clean(&String::from_utf8_lossy(&pending));
                // Percentages are a slow download, not a question.
                if !text.trim().is_empty() && !text.contains('%') {
                    asked = true;
                    send(Event::Question(shown(&text)));
                }
            }
            if !stuck_said && committing.load(Ordering::Relaxed) && last_output.elapsed() > STUCK {
                stuck_said = true;
                send(Event::Committing(false));
            }
            continue;
        }
        let n = match (&*master).read(&mut buffer) {
            Ok(n) if n > 0 => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            _ => return,
        };
        last_output = Instant::now();
        // Not stuck after all: stopping it would be as unsafe as it was.
        if std::mem::take(&mut stuck_said) && committing.load(Ordering::Relaxed) {
            send(Event::Committing(true));
        }
        asking.open.store(false, Ordering::SeqCst);
        pending.extend_from_slice(&buffer[..n]);
        while let Some(at) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=at).collect();
            // Read as printed, or a mistyped `a` hides the commit from the words below;
            // only what `shown` returns leaves this function.
            let line = clean(&String::from_utf8_lossy(&line[..line.len() - 1]));
            refused |= line.contains("Sorry, try again") || line.contains("incorrect password");
            asked = false;
            if let Some((done, total)) = progress(&line) {
                send(Event::Progress(done, total));
            }
            if let Some(now) = phase(&line) {
                committing.store(now, Ordering::Relaxed);
                send(Event::Committing(now));
            }
            if !line.trim().is_empty() {
                send(Event::Line(shown(&line)));
            }
        }
        // A bare carriage return ends what a progress bar will overwrite. One at
        // the very end may still be the start of a line ending.
        let end = pending.len().saturating_sub(1);
        if let Some(at) = pending[..end].iter().rposition(|b| *b == b'\r') {
            let bar: Vec<u8> = pending.drain(..=at).collect();
            let text = clean(&String::from_utf8_lossy(&bar));
            if !text.trim().is_empty() {
                if let Some((done, total)) = progress(&text) {
                    send(Event::Progress(done, total));
                }
                send(Event::Transient(shown(text.trim())));
            }
            asked = false;
        }
        let text = clean(&String::from_utf8_lossy(&pending));
        let lower = text.to_lowercase();
        // PAM may word it its own way unless sudo was told to override that. A
        // prompt from anything but sudo stays a plain question, with no password.
        if (text.contains(SUDO_MARKER)
            || (lower.trim_end().ends_with(':') && lower.contains("password")))
            && asking.is_sudo(master)
        {
            pending.clear();
            asking.open.store(true, Ordering::SeqCst);
            send(Event::Password { rejected: refused });
            refused = false;
            asked = true;
        } else if !asked && (question(&text) || text.contains(SUDO_MARKER)) {
            asked = true;
            send(Event::Question(shown(&text)));
        } else if pending.len() > MAX_LINE {
            send(Event::Line(shown(&text)));
            pending.clear();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// Behaves as `sudo yay` does on the pty: asks with echo off, refuses, counts.
    const SCRIPT: &str = r#"#!/bin/sh
password() {
    stty -echo
    printf '%s' "$SUDO_PROMPT"
    IFS= read -r answer
    stty echo
    printf '\n'
}
case "$1" in
fail) echo 'error: failed to commit transaction (conflicting files)'; exit 3 ;;
hang) echo started; exec sleep 300 ;;
commit) echo ':: Processing package changes...'; exec sleep 300 ;;
committed) echo ':: Processing package changes...'; exit 0 ;;
late) trap 'echo too late; exit 0' INT; echo started; while :; do sleep 0.1; done ;;
gone) password; exit 0 ;;
talks) stty -echo; printf '%s' "$SUDO_PROMPT"; sleep 0.3; echo more; IFS= read -r answer; stty echo; exit 0 ;;
stubborn) trap '' HUP INT QUIT TERM; echo started; while :; do sleep 1; done ;;
ask) printf 'Proceed with installation? [Y/n] '; IFS= read -r a; echo "said $a"; exit 0 ;;
silent) printf 'Which mirror do you like best? '; IFS= read -r a; echo "said $a"; exit 0 ;;
bars) printf 'mesa 10%%\r'; sleep 0.2; printf 'mesa 50%%\r'; sleep 0.2; printf 'mesa 100%%\n'; exit 0 ;;
echoing) printf '%s' "$SUDO_PROMPT"; IFS= read -r answer; echo "got $answer"; exit 0 ;;
prompt) password; exit 0 ;;
pam) stty -echo; printf 'Password for root: '; IFS= read -r answer; stty echo; printf '\n'; exit 0 ;;
same) tty; ps -o sid= -p $$; exit 0 ;;
typo)
    password
    while [ "$answer" != "right" ]; do
        echo "Sorry, try again."
        password
    done
    printf '(1/2) upgrading %s\r' "$answer"
    sleep 0.1
    printf '(1/2) upgrading %s\r' "$answer"
    sleep 0.1
    echo ':: Processing package changes...'
    trap 'exit 9' QUIT
    trap '' INT
    i=0
    while [ $i -lt 65 ]; do sleep 0.1; i=$((i + 1)); done
    echo 'finished'
    exit 0
    ;;
*)
    password
    while [ "$answer" != "right" ]; do
        echo "Sorry, try again."
        password
    done
    echo "the password was $answer here"
    echo ':: Processing package changes...'
    echo '(1/2) upgrading foot'
    printf '\033[32m(2/2) upgrading mesa\033[0m\n'
    echo '==> Making package: foo'
    exit 0
    ;;
esac
"#;
    fn fake() -> &'static std::path::Path {
        static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
        PATH.get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let path = std::env::current_exe()
                .unwrap()
                .with_file_name("nexus-fake-manager");
            std::fs::write(&path, SCRIPT).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        })
    }
    fn next(session: &Session) -> Event {
        let (tx, rx) = std::sync::mpsc::channel();
        let receiver = session.receiver.clone();
        std::thread::spawn(move || {
            let _ = tx.send(receiver.recv_blocking());
        });
        rx.recv_timeout(Duration::from_secs(15))
            .expect("the session stopped talking")
            .unwrap()
    }
    fn until(session: &Session, wanted: impl Fn(&Event) -> bool) -> (Vec<String>, Event) {
        let mut lines = vec![];
        loop {
            match next(session) {
                e if wanted(&e) => return (lines, e),
                Event::Line(line) => lines.push(line),
                _ => {}
            }
        }
    }
    fn done(session: &Session) -> (Vec<String>, Event) {
        until(session, |e| matches!(e, Event::Done(_)))
    }
    fn asked(session: &Session) -> Event {
        until(session, |e| matches!(e, Event::Password { .. })).1
    }
    fn run(session: &mut Session, arg: &str) {
        // Through sh: another test forking while the script is written would make
        // running it directly fail as "text file busy".
        let word = fake().to_string_lossy().into_owned();
        session.run(&["/bin/sh".into(), word, arg.into()]).unwrap();
    }
    #[test]
    fn prompt_wrong_password_progress_and_success() {
        let mut session = Session::start().unwrap();
        run(&mut session, "update");
        assert_eq!(asked(&session), Event::Password { rejected: false });
        session.answer(Secret::new(b"wrong")).unwrap();
        let (lines, again) = until(&session, |e| matches!(e, Event::Password { .. }));
        assert_eq!(again, Event::Password { rejected: true });
        assert!(lines.iter().all(|l| !l.contains("wrong")), "{lines:?}");
        session.forget();
        session.answer(Secret::new(b"right")).unwrap();
        let mut events = vec![];
        loop {
            let event = next(&session);
            let over = matches!(event, Event::Done(_));
            events.push(event);
            if over {
                break;
            }
        }
        assert!(events.contains(&Event::Committing(true)));
        assert!(events.contains(&Event::Committing(false)));
        assert!(events.contains(&Event::Progress(2, 2)));
        assert_eq!(events.last(), Some(&Event::Done(Some(0))));
        let text = events
            .iter()
            .filter_map(|e| match e {
                Event::Line(line) => Some(line.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Echo stayed off, and what the manager repeated is blanked.
        assert!(!text.contains("right"), "{text}");
        assert!(text.contains("the password was ••• here"), "{text}");
        assert!(text.contains("(2/2) upgrading mesa") && !text.contains('\x1b'));
        // Nothing is kept once it is over.
        assert!(session.secret.lock().unwrap().0.is_empty());
    }
    #[test]
    fn the_password_is_never_written_while_echo_is_on() {
        let mut session = Session::start().unwrap();
        run(&mut session, "echoing");
        // Not even asked for: it is only a question.
        until(&session, |e| matches!(e, Event::Question(_)));
        assert!(session.answer(Secret::new(b"hunter2")).is_err());
        assert!(session.secret.lock().unwrap().0.is_empty());
        session.reply("nothing").unwrap();
        let (lines, _) = done(&session);
        assert!(lines.iter().all(|l| !l.contains("hunter2")), "{lines:?}");
    }
    /// What a build script could do: copy sudo's prompt with echo off.
    #[test]
    fn a_prompt_copied_by_anything_but_sudo_gets_no_password() {
        let mut session = Session::start_trusting(false).unwrap();
        run(&mut session, "prompt");
        until(&session, |e| matches!(e, Event::Question(_)));
        assert!(session.answer(Secret::new(b"hunter2")).is_err());
        assert!(session.secret.lock().unwrap().0.is_empty());
        session.cancel();
        done(&session);
    }
    #[test]
    fn only_a_process_with_root_ids_in_the_session_counts_as_sudo() {
        let stat = |name: &str, session: i32| format!("7 ({name}) S 1 7 {session} 0 0");
        let root = "Name:\tsudo\nUid:\t1000\t1000\t0\t1000\n";
        let user = "Name:\tsudo\nUid:\t1000\t1000\t1000\t1000\n";
        assert!(is_sudo(&stat("sudo", 42), root, 42));
        assert!(!is_sudo(&stat("sudo", 42), user, 42));
        assert!(!is_sudo(&stat("sudo", 43), root, 42));
        assert!(!is_sudo(&stat("pacman", 42), root, 42));
        assert!(!is_sudo(&stat("sudo) S 1 7 42 (x", 41), user, 42));
        // This test itself is no root process, whatever it calls itself.
        assert!(!sudo_on_terminal(std::process::id() as i32));
    }
    /// A mistyped password can be a letter of the very words read here: the commit
    /// must still be seen, and Cancel must not break it. Takes past the last signal.
    #[test]
    fn a_short_password_hides_nothing_from_the_state_and_shows_nowhere() {
        let mut session = Session::start().unwrap();
        run(&mut session, "typo");
        asked(&session);
        session.answer(Secret::new(b"a")).unwrap();
        assert_eq!(asked(&session), Event::Password { rejected: true });
        session.answer(Secret::new(b"1")).unwrap();
        assert_eq!(asked(&session), Event::Password { rejected: true });
        session.answer(Secret::new(b"right")).unwrap();
        let mut events = vec![];
        loop {
            let event = next(&session);
            let commit = matches!(&event, Event::Line(l) if l.contains("Processing"));
            events.push(event);
            if commit {
                break;
            }
        }
        assert!(events.contains(&Event::Committing(true)));
        assert!(session.committing.load(Ordering::Relaxed));
        assert!(events.contains(&Event::Progress(1, 2)));
        let shown: Vec<&String> = events
            .iter()
            .filter_map(|e| match e {
                Event::Line(text) | Event::Transient(text) => Some(text),
                _ => None,
            })
            .collect();
        for text in shown {
            assert!(
                !text.contains(['a', '1']) && !text.contains("right"),
                "{text}"
            );
        }
        session.cancel();
        let (lines, over) = done(&session);
        assert_eq!(over, Event::Done(Some(0)));
        assert!(lines.iter().any(|l| l == "finished"), "{lines:?}");
        assert!(session.alive());
    }
    #[test]
    fn the_password_goes_once_and_not_after_more_output() {
        let mut session = Session::start().unwrap();
        run(&mut session, "gone");
        asked(&session);
        session.answer(Secret::new(b"one")).unwrap();
        assert!(session.answer(Secret::new(b"two")).is_err());
        done(&session);
        run(&mut session, "talks");
        asked(&session);
        until(&session, |e| matches!(e, Event::Line(l) if l == "more"));
        assert!(session.answer(Secret::new(b"hunter2")).is_err());
        session.cancel();
        done(&session);
    }
    #[test]
    fn a_silent_commit_gets_cancel_back_with_the_interrupt_only() {
        let mut session = Session::start().unwrap();
        run(&mut session, "commit");
        until(&session, |e| *e == Event::Committing(true));
        until(&session, |e| *e == Event::Committing(false));
        assert!(session.committing.load(Ordering::Relaxed));
        session.cancel();
        assert_eq!(done(&session).1, Event::Done(None));
        assert!(session.alive());
    }
    #[test]
    fn finishing_despite_a_cancel_is_a_success() {
        let mut session = Session::start().unwrap();
        run(&mut session, "late");
        until(&session, |e| matches!(e, Event::Line(l) if l == "started"));
        session.cancel();
        assert_eq!(done(&session).1, Event::Done(Some(0)));
    }
    #[test]
    fn a_prompt_in_pam_wording_is_recognised() {
        let mut session = Session::start().unwrap();
        run(&mut session, "pam");
        assert_eq!(asked(&session), Event::Password { rejected: false });
    }
    /// sudo's timestamp is keyed on the terminal and its session: what a second
    /// request needs to see for the first one's password to carry.
    #[test]
    fn requests_share_the_terminal_and_session_sudo_keys_on() {
        let mut session = Session::start().unwrap();
        run(&mut session, "same");
        let (first, _) = done(&session);
        // A cancelled request must not cost the password either.
        run(&mut session, "hang");
        until(&session, |e| matches!(e, Event::Line(l) if l == "started"));
        session.cancel();
        assert_eq!(done(&session).1, Event::Done(None));
        run(&mut session, "same");
        let (second, _) = done(&session);
        assert!(first[0].starts_with("/dev/pts/"), "{first:?}");
        assert_eq!(first, second);
        assert_eq!(
            first[1].trim(),
            session.leader.as_ref().unwrap().id().to_string()
        );
    }
    #[test]
    fn failures_keep_their_status_and_text() {
        let mut session = Session::start().unwrap();
        run(&mut session, "fail");
        let (lines, over) = done(&session);
        assert_eq!(over, Event::Done(Some(3)));
        assert!(lines[0].contains("failed to commit transaction"));
    }
    #[test]
    fn questions_are_surfaced_and_answered() {
        let mut session = Session::start().unwrap();
        run(&mut session, "ask");
        let (_, question) = until(&session, |e| matches!(e, Event::Question(_)));
        assert_eq!(
            question,
            Event::Question("Proceed with installation? [Y/n] ".into())
        );
        session.reply("y").unwrap();
        let (lines, over) = done(&session);
        assert_eq!(over, Event::Done(Some(0)));
        assert!(lines.iter().any(|l| l.ends_with("said y")), "{lines:?}");
    }
    #[test]
    fn an_unknown_question_surfaces_instead_of_hanging() {
        let mut session = Session::start().unwrap();
        run(&mut session, "silent");
        let (_, question) = until(&session, |e| matches!(e, Event::Question(_)));
        assert_eq!(
            question,
            Event::Question("Which mirror do you like best? ".into())
        );
        session.reply("this").unwrap();
        assert_eq!(done(&session).1, Event::Done(Some(0)));
    }
    #[test]
    fn progress_bars_are_transient() {
        let mut session = Session::start().unwrap();
        run(&mut session, "bars");
        let mut events = vec![];
        loop {
            let event = next(&session);
            if matches!(event, Event::Done(_)) {
                break;
            }
            events.push(event);
        }
        assert!(events.contains(&Event::Transient("mesa 10%".into())));
        assert!(events.contains(&Event::Line("mesa 100%".into())));
    }
    #[test]
    fn cancelling_at_the_prompt_leaves_echo_usable() {
        let mut session = Session::start().unwrap();
        run(&mut session, "prompt");
        asked(&session);
        session.cancel();
        assert_eq!(done(&session).1, Event::Done(None));
        run(&mut session, "ask");
        until(&session, |e| matches!(e, Event::Question(_)));
        assert!(!echo_off(&session.master));
    }
    #[test]
    fn cancelling_gets_past_what_ignores_it_and_leaves_nothing() {
        let mut session = Session::start().unwrap();
        run(&mut session, "stubborn");
        until(&session, |e| matches!(e, Event::Line(l) if l == "started"));
        let group = session.leader.as_ref().unwrap().id() as i32;
        session.cancel();
        assert_eq!(done(&session).1, Event::Done(None));
        // That took the last signal, which the session does not survive.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!session.alive());
        drop(session);
        // SAFETY: signal 0 only asks whether the group still exists.
        assert_ne!(unsafe { libc::kill(-group, 0) }, 0);
    }
    #[test]
    fn dropping_the_session_ends_it() {
        let mut session = Session::start().unwrap();
        run(&mut session, "hang");
        until(&session, |e| matches!(e, Event::Line(l) if l == "started"));
        let group = session.leader.as_ref().unwrap().id() as i32;
        drop(session);
        for _ in 0..50 {
            // SAFETY: signal 0 only asks whether the group still exists.
            if unsafe { libc::kill(-group, 0) } != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the session outlived its owner");
    }
    /// An idle session once held a commit that finished: nothing to leave running,
    /// and no zombie left behind.
    #[test]
    fn a_session_is_reaped_after_a_finished_commit() {
        let mut session = Session::start().unwrap();
        run(&mut session, "committed");
        assert_eq!(done(&session).1, Event::Done(Some(0)));
        assert!(!session.committing.load(Ordering::Relaxed));
        let group = session.leader.as_ref().unwrap().id() as i32;
        drop(session);
        for _ in 0..50 {
            // SAFETY: signal 0 only asks whether the group still exists, as it does
            // for as long as its leader is a zombie.
            if unsafe { libc::kill(-group, 0) } != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the session's leader was not reaped");
    }
    #[test]
    fn names_are_validated() {
        for good in ["foot", "lib32-mesa", "gtk4+extra", "python3.12", "a@b_c"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "-Rdd",
            "--config=/x",
            ".hidden",
            "a b",
            "a;b",
            "a$(b)",
            "a/b",
            "a\nb",
            "é",
        ] {
            assert!(!valid_name(bad), "{bad}");
            let install = |repo: &str, name: &str| Change::Install {
                repo: repo.into(),
                name: name.into(),
            };
            assert!(install("extra", bad).command("pacman").is_err());
            assert!(install(bad, "foot").command("yay").is_err());
            assert!(Change::Remove(bad.into()).command("yay").is_err());
        }
    }
    #[test]
    fn commands_only_upgrade_fully_and_never_ask() {
        let update = Change::UpdateAll.command("yay").unwrap();
        assert_eq!(update[..2], ["yay", "-Syu"]);
        for flag in ["--noconfirm", "--sudoloop", "--answerdiff"] {
            assert!(update.iter().any(|a| a == flag), "{flag}");
        }
        let install = Change::Install {
            repo: "extra".into(),
            name: "foot".into(),
        };
        let command = install.command("pacman").unwrap();
        assert_eq!(command[..4], ["sudo", "pacman", "-S", "extra/foot"]);
        assert_eq!(
            Change::Remove("foot".into()).command("pacman").unwrap()[..4],
            ["sudo", "pacman", "-Rns", "foot"]
        );
        // Whatever the manager: yay could build what was not reviewed.
        let official = Change::UpdateOfficial.command("yay").unwrap();
        assert_eq!(official[..3], ["sudo", "pacman", "-Syu"]);
        assert!(!official.iter().any(|a| a == "--sudoloop"));
    }
    #[test]
    fn the_password_is_scrubbed() {
        assert_eq!(scrub("a hunter2 b hunter2", b"hunter2"), "a ••• b •••");
        assert_eq!(scrub("nothing", b""), "nothing");
        assert_eq!(scrub("nothing", b"hunter2"), "nothing");
        assert_eq!(scrub("a mot\u{fffd} b", b"mot\xff"), "a ••• b");
    }
    #[test]
    fn lines_lose_colours_and_overwritten_text() {
        assert_eq!(clean("\x1b[1;32mok\x1b[0m"), "ok");
        assert_eq!(clean("10%\r50%\r100%"), "100%");
        assert_eq!(clean("done\r"), "done");
    }
    #[test]
    fn shell_words_are_quoted() {
        assert_eq!(quote("a'b c"), r"'a'\''b c'");
    }
    #[test]
    fn failures_show_the_errors_or_the_last_words() {
        let lines: Vec<String> = ["resolving", "error: conflict", "more", "sudo: 3 tries"]
            .map(String::from)
            .into();
        assert_eq!(
            failure_text(lines.iter()),
            "error: conflict\nmore\nsudo: 3 tries"
        );
        let plain: Vec<String> = ["one", "two"].map(String::from).into();
        assert_eq!(failure_text(plain.iter()), "one\ntwo");
    }
    #[test]
    fn lockers_are_recognised() {
        assert!(is_locker(b"/usr/bin/nexus\0lock\0"));
        assert!(is_locker(b"hyprlock\0"));
        assert!(!is_locker(b"nexus\0lock\0--preview\0"));
        assert!(!is_locker(b"nexus\0shell\0"));
    }
}
