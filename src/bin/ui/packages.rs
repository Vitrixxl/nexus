//! Packages: what can be updated, found or removed, through pacman and yay. A
//! change runs here too, in a view of its own: what it does, its progress and
//! output, sudo's password prompt and the manager's questions, then the outcome.
use super::super::{hbox, label, spinner, vbox};
use gtk::{glib, glib::translate::ToGlibPtr, prelude::*};
use nexus_control::launcher::score;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    process::{Command, Stdio},
    rc::Rc,
    time::Instant,
};
#[path = "packages_tx.rs"]
mod tx;
use tx::{Change, Event, Secret, Session};

#[derive(Clone, Debug, PartialEq)]
struct Package {
    repo: String,
    name: String,
    version: String,
    description: String,
    installed: bool,
}
fn have(tool: &str) -> bool {
    Command::new("which")
        .arg(tool)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
/// yay when it is there, for the AUR too.
fn manager() -> &'static str {
    if tx::fake_manager().is_some() || have("yay") {
        "yay"
    } else {
        "pacman"
    }
}
fn run(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new(tx::fake_manager().as_deref().unwrap_or(program))
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Could not run {program}: {e}"))
}
fn output(program: &str, args: &[&str]) -> Result<String, String> {
    let out = run(program, args)?;
    // Nothing found and nothing to update both exit with an error.
    if out.status.success() || out.stderr.is_empty() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}
/// `repo/name version [flags]` then an indented description, as `-Ss` prints them.
fn parse_search(text: &str) -> Vec<Package> {
    let mut packages: Vec<Package> = vec![];
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            if let Some(last) = packages.last_mut() {
                last.description = line.trim().to_string();
            }
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(id), Some(version)) = (words.next(), words.next()) else {
            continue;
        };
        let Some((repo, name)) = id.split_once('/') else {
            continue;
        };
        packages.push(Package {
            repo: repo.into(),
            name: name.into(),
            version: version.into(),
            description: String::new(),
            installed: line.contains("[installed") || line.contains("(Installed"),
        });
    }
    packages
}
/// `name version` per line, as `-Qe` prints them; `-Qu` adds `-> new`.
fn parse_list(text: &str) -> Vec<Package> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            let version = words.next()?;
            let version = match (words.next(), words.next()) {
                (Some("->"), Some(new)) => format!("{version} → {new}"),
                _ => version.to_string(),
            };
            Some(Package {
                repo: String::new(),
                name: name.into(),
                version,
                description: String::new(),
                installed: true,
            })
        })
        .collect()
}
fn aur_updates() -> Result<Vec<Package>, String> {
    if manager() != "yay" {
        return Ok(vec![]);
    }
    output("yay", &["-Qua"]).map(|text| parse_list(&text))
}
/// With whether the AUR failed to answer. Official updates come from pacman, as
/// `yay -Qu` fails along with the AUR.
fn updates() -> Result<(Vec<Package>, bool), String> {
    let mut list = parse_list(&output("pacman", &["-Qu"])?);
    let aur = aur_updates().inspect_err(|e| eprintln!("Packages: the AUR: {e}"));
    let aur_failed = aur.is_err();
    for mut aur in aur.unwrap_or_default() {
        aur.repo = "aur".into();
        match list.iter_mut().find(|p| p.name == aur.name) {
            Some(known) => *known = aur,
            None => list.push(aur),
        }
    }
    Ok((list, aur_failed))
}
fn installed() -> Result<Vec<Package>, String> {
    output("pacman", &["-Qe"]).map(|text| parse_list(&text))
}
/// How old the package lists are: `-Qu` knows nothing newer than the last sync.
fn last_sync() -> Option<String> {
    let age = std::fs::metadata("/var/lib/pacman/sync/core.db")
        .and_then(|m| m.modified())
        .ok()?
        .elapsed()
        .ok()?
        .as_secs();
    Some(match age {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", age / 60),
        3600..86400 => format!("{} h ago", age / 3600),
        _ => format!("{} d ago", age / 86400),
    })
}
/// What `yay -Si` says a package needs to build and run, without versions.
fn dependencies(info: &str) -> Vec<String> {
    let mut wanted = false;
    let mut found = vec![];
    for line in info.lines() {
        let value = match line.split_once(':') {
            Some((key, value)) if !key.starts_with(char::is_whitespace) => {
                wanted = ["Depends On", "Make Deps", "Check Deps"].contains(&key.trim());
                value
            }
            // A long value carries on, indented, on the next line.
            _ => line,
        };
        if wanted {
            found.extend(
                value
                    .split_whitespace()
                    .filter_map(|dep| dep.split(['<', '>', '=']).next())
                    .filter(|dep| *dep != "None" && tx::valid_name(dep))
                    .map(String::from),
            );
        }
    }
    found
}
/// These AUR packages and the ones they would pull in: they all get built. An
/// error when that cannot be known, as yay would build what the review left out.
// ponytail: a dependency only an AUR package provides under another name stays unseen; yay then picks it with no review. Needs yay's own resolver to close.
fn with_aur_dependencies(
    roots: Vec<String>,
    query: impl Fn(&str, &[&str]) -> Result<String, String>,
) -> Result<Vec<String>, String> {
    const MAX: usize = 64;
    let is_missing = |dep: &str| -> Result<bool, String> {
        // `+` is common in a name and means something in a pattern.
        let pattern = format!("^{}$", dep.replace('+', r"\+"));
        Ok(!query("pacman", &["-T", dep])?.trim().is_empty()
            && query("pacman", &["-Ssq", &pattern])?.trim().is_empty())
    };
    let info = |name: &str| match query("yay", &["-Sia", name]) {
        Ok(text) if text.trim().is_empty() => Err(format!("The AUR said nothing about {name}.")),
        Ok(text) => Ok(Some(text)),
        // All it says of a name it does not have. An outage says more.
        Err(e) if e.lines().all(|line| line.contains("No AUR package found")) => Ok(None),
        Err(e) => Err(e),
    };
    let mut all = roots.clone();
    let mut todo = roots;
    while let Some(name) = todo.pop() {
        let Some(text) = info(&name)? else {
            return Err(format!("The AUR has no package {name}."));
        };
        for dep in dependencies(&text) {
            if !all.contains(&dep) && is_missing(&dep)? && info(&dep)?.is_some() {
                if all.len() >= MAX {
                    return Err(format!("They pull in more than {MAX} AUR packages."));
                }
                all.push(dep.clone());
                todo.push(dep);
            }
        }
    }
    Ok(all)
}
/// The build scripts of these AUR packages and of all they pull in, for reading
/// before they run.
fn review(names: Vec<String>) -> Result<Vec<(String, String)>, String> {
    let all = with_aur_dependencies(names, output).map_err(|e| {
        format!(
            "Could not work out everything to review, so nothing was changed: \
             check your connection and try again.\n{e}"
        )
    })?;
    pkgbuilds(&all)
}
fn pkgbuilds(names: &[String]) -> Result<Vec<(String, String)>, String> {
    names
        .iter()
        .map(|name| {
            if !tx::valid_name(name) {
                return Err(format!("Refused the package name “{name}”."));
            }
            let out = run("yay", &["-Gpa", name])?;
            let text = String::from_utf8_lossy(&out.stdout);
            // Without the AUR it fails with the reason on stdout, not a script.
            if !out.status.success() {
                return Err(format!(
                    "Could not get the build script of {name}.\n{}",
                    text.trim()
                ));
            }
            if text.trim().is_empty() {
                return Err(format!("The AUR has no build script for {name}."));
            }
            Ok((name.clone(), text.trim().to_string()))
        })
        .collect()
}
/// What a removal takes along, as pacman works it out without changing anything.
/// An error says what stands in the way.
fn removal(name: &str) -> Result<Vec<String>, String> {
    if !tx::valid_name(name) {
        return Err(format!("Refused the package name “{name}”."));
    }
    output("pacman", &["-Rnsp", "--print-format", "%n", name])
        .map_err(|e| {
            format!("Could not work out what this would remove, so nothing was changed.\n{e}")
        })
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|n| !n.is_empty() && *n != name)
                .map(String::from)
                .collect()
        })
}
/// The query as arguments. A word with a leading dash would be read as an option
/// (`-y` is a sync, for one), and no package name can start with it.
fn search_args(query: &str) -> Vec<&str> {
    let mut args = vec!["-Ss"];
    args.extend(query.split_whitespace().filter(|w| !w.starts_with('-')));
    args
}
fn search(query: &str) -> Result<Vec<Package>, String> {
    let args = search_args(query);
    if args.len() == 1 {
        return Ok(vec![]);
    }
    output(manager(), &args).map(|text| {
        let mut found = parse_search(&text);
        rank(&mut found, query);
        found.truncate(MAX_ROWS);
        found
    })
}
/// The likeliest first, which `-Ss` leaves to the bottom: the best name match,
/// then the official repositories, then the shortest name.
fn rank(found: &mut [Package], query: &str) {
    found.sort_by_key(|p| {
        let score = score(&p.name, &p.description, query).unwrap_or(0);
        (std::cmp::Reverse(score), p.repo == "aur", p.name.len())
    });
}
/// More rows than anyone reads, and than a list should hold.
const MAX_ROWS: usize = 200;
/// Enough to read back a failure; older output leaves the view.
const MAX_LOG_LINES: usize = 5000;
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}
#[derive(Clone, Copy, PartialEq)]
enum View {
    Updates,
    Search,
    Installed,
}
/// What was found out before a change starts.
enum Prepared {
    Go,
    /// The build scripts to read first, by package.
    Review(Vec<(String, String)>),
    /// What a removal takes along.
    Remove(Vec<String>),
}
/// How a change is called along the way.
#[derive(Default)]
struct Words {
    doing: String,
    review: String,
    confirm: &'static str,
    done: String,
    failed: String,
    stopped: &'static str,
}
fn words(change: &Change, count: usize) -> Words {
    match change {
        Change::UpdateAll => Words {
            doing: if count > 0 {
                format!("Updating {}", plural(count, "package"))
            } else {
                "Updating the system".into()
            },
            done: "System updated".into(),
            failed: "Update failed".into(),
            stopped: "Update cancelled",
            ..Words::default()
        },
        Change::Update(name) => Words {
            doing: format!("Updating {name}"),
            done: format!("{name} updated"),
            failed: format!("Could not update {name}"),
            stopped: "Update cancelled",
            ..Words::default()
        },
        Change::Install { name, .. } => Words {
            doing: format!("Installing {name}"),
            review: format!("Review {name}"),
            confirm: "Build and Install",
            done: format!("{name} installed"),
            failed: format!("Could not install {name}"),
            stopped: "Install cancelled",
        },
        Change::Remove(name) => Words {
            doing: format!("Removing {name}"),
            review: format!("Remove {name}?"),
            confirm: "Remove",
            done: format!("{name} removed"),
            failed: format!("Could not remove {name}"),
            stopped: "Removal cancelled",
        },
    }
}
/// The page a change runs in.
struct TxView {
    page: gtk::Box,
    title: gtk::Label,
    summary: gtk::Label,
    back: gtk::Button,
    cancel: gtk::Button,
    confirm: gtk::Button,
    /// Starts over from what the change needs first, review included.
    retry: gtk::Button,
    /// In place of Cancel while stopping would leave the system half changed.
    applying: gtk::Label,
    stage_row: gtk::Box,
    stage: gtk::Label,
    bar: gtk::ProgressBar,
    notice: gtk::Label,
    choose: gtk::DropDown,
    password_box: gtk::Box,
    password: gtk::PasswordEntry,
    password_error: gtk::Label,
    password_send: gtk::Button,
    question_box: gtk::Box,
    question: gtk::Label,
    answer: gtk::Entry,
    send: gtk::Button,
    yes: gtk::Button,
    no: gtk::Button,
    outcome: gtk::Label,
    /// Opens the output. A build script to review has no need of it.
    details: gtk::ToggleButton,
    log_scroll: gtk::ScrolledWindow,
    log: gtk::TextView,
    log_end: gtk::TextMark,
}
impl TxView {
    fn new() -> Self {
        let page = vbox(12);
        let head = hbox(8);
        let title = label("", "title");
        title.set_hexpand(true);
        head.append(&title);
        let applying = label("Applying, can't cancel", "muted");
        applying.set_wrap(false);
        applying.set_tooltip_text(Some("Stopping now would leave the system half changed."));
        head.append(&applying);
        let button = |text: &str| {
            let b = gtk::Button::with_label(text);
            b.set_valign(gtk::Align::Center);
            head.append(&b);
            b
        };
        let back = button("Back");
        let cancel = button("Cancel");
        let confirm = button("");
        let retry = button("Try Again");
        retry.add_css_class("suggested-action");
        page.append(&head);
        let summary = label("", "muted");
        summary.set_lines(2);
        summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
        page.append(&summary);
        let stage_row = hbox(8);
        let stage = label("", "");
        stage.set_wrap(false);
        stage.set_ellipsize(gtk::pango::EllipsizeMode::End);
        stage.set_hexpand(true);
        stage_row.append(&stage);
        page.append(&stage_row);
        let bar = gtk::ProgressBar::new();
        bar.add_css_class("package-progress");
        page.append(&bar);
        let notice = label("", "banner");
        page.append(&notice);
        let choose = gtk::DropDown::from_strings(&[]);
        choose.set_halign(gtk::Align::Start);
        page.append(&choose);

        let password_box = vbox(8);
        password_box.add_css_class("package-sheet");
        password_box.append(&label("Password required", "row-title"));
        let why = label("sudo asks for your password to change packages.", "muted");
        password_box.append(&why);
        let line = hbox(8);
        let password = gtk::PasswordEntry::new();
        password.set_hexpand(true);
        password.set_placeholder_text(Some("Password"));
        line.append(&password);
        let password_send = gtk::Button::with_label("Continue");
        password_send.add_css_class("suggested-action");
        line.append(&password_send);
        password_box.append(&line);
        let password_error = label("", "error");
        password_box.append(&password_error);
        page.append(&password_box);

        let question_box = vbox(8);
        question_box.add_css_class("package-sheet");
        question_box.append(&label("The package manager asks", "row-title"));
        let question = label("", "");
        question.set_selectable(true);
        question_box.append(&question);
        question_box.append(&label(
            "Do not type your password here: sudo always asks in its own box.",
            "muted",
        ));
        let line = hbox(8);
        let answer = gtk::Entry::new();
        answer.set_hexpand(true);
        answer.set_placeholder_text(Some("Answer"));
        line.append(&answer);
        let send = gtk::Button::with_label("Send");
        send.add_css_class("suggested-action");
        line.append(&send);
        let yes = gtk::Button::with_label("Yes");
        line.append(&yes);
        let no = gtk::Button::with_label("No");
        line.append(&no);
        question_box.append(&line);
        page.append(&question_box);

        let outcome = label("", "");
        outcome.set_selectable(true);
        // An error can hold a URL as long as the list of AUR packages.
        outcome.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        page.append(&outcome);

        let details = gtk::ToggleButton::with_label("Show details");
        details.add_css_class("flat");
        details.add_css_class("package-details");
        details.set_halign(gtk::Align::Start);
        page.append(&details);
        let log = gtk::TextView::new();
        log.add_css_class("package-log");
        log.set_editable(false);
        log.set_cursor_visible(false);
        log.set_wrap_mode(gtk::WrapMode::WordChar);
        log.set_left_margin(12);
        log.set_right_margin(12);
        log.set_top_margin(10);
        log.set_bottom_margin(10);
        let buffer = log.buffer();
        let log_end = buffer.create_mark(None, &buffer.end_iter(), false);
        // As tall as what it holds, up to the room there is.
        let log_scroll = gtk::ScrolledWindow::builder()
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&log)
            .build();
        log_scroll.add_css_class("package-log-frame");
        page.append(&log_scroll);
        let (scroll, shown, end) = (log_scroll.clone(), log.clone(), log_end.clone());
        details.connect_toggled(move |details| {
            let open = details.is_active();
            details.set_label(if open { "Hide details" } else { "Show details" });
            scroll.set_visible(open);
            if open {
                shown.scroll_mark_onscreen(&end);
            }
        });
        let this = Self {
            page,
            title,
            summary,
            back,
            cancel,
            confirm,
            retry,
            applying,
            stage_row,
            stage,
            bar,
            notice,
            choose,
            password_box,
            password,
            password_error,
            password_send,
            question_box,
            question,
            answer,
            send,
            yes,
            no,
            outcome,
            details,
            log_scroll,
            log,
            log_end,
        };
        // Both send what was typed, and neither is a button of the window.
        let send = this.password_send.clone();
        this.password.connect_activate(move |_| send.emit_clicked());
        let send = this.send.clone();
        this.answer.connect_activate(move |_| send.emit_clicked());
        this.clear();
        this
    }
    /// Back to nothing showing, with nothing left in the entries.
    fn clear(&self) {
        self.allow_cancel(true);
        for part in [
            self.summary.upcast_ref::<gtk::Widget>(),
            self.back.upcast_ref(),
            self.cancel.upcast_ref(),
            self.confirm.upcast_ref(),
            self.retry.upcast_ref(),
            self.stage_row.upcast_ref(),
            self.bar.upcast_ref(),
            self.notice.upcast_ref(),
            self.choose.upcast_ref(),
            self.outcome.upcast_ref(),
            self.details.upcast_ref(),
            self.log_scroll.upcast_ref(),
        ] {
            part.set_visible(false);
        }
        self.hide_password();
        self.question_box.set_visible(false);
        self.confirm.remove_css_class("suggested-action");
        self.confirm.remove_css_class("destructive-action");
        self.outcome.remove_css_class("banner");
        self.outcome.remove_css_class("package-done");
        self.confirm.set_sensitive(true);
        self.details.set_active(false);
        self.log_scroll.set_vexpand(false);
        self.bar.set_fraction(0.);
    }
    fn allow_cancel(&self, allowed: bool) {
        if !allowed && self.cancel.has_focus() {
            self.keep_focus();
        }
        self.cancel.set_sensitive(true);
        self.cancel.set_visible(allowed);
        self.applying.set_visible(!allowed);
    }
    /// With the entries gone focus must stay here, not fall back on the sidebar.
    fn keep_focus(&self) {
        self.page.set_focusable(true);
        self.page.grab_focus();
    }
    fn hide_password(&self) {
        if self.password_box.is_visible() {
            self.keep_focus();
        }
        self.password.set_text("");
        self.password_error.set_visible(false);
        self.password_box.set_visible(false);
    }
    /// The bar below already draws what a line's own `[###---] 40%` says.
    fn stage(&self, text: &str) {
        self.stage.set_text(text.split(" [").next().unwrap_or(text));
        self.stage_row.set_visible(true);
    }
    /// A build script, to read in all the room there is.
    fn show_script(&self, text: &str) {
        self.log.buffer().set_text(text);
        self.log_scroll.set_vexpand(true);
        self.log_scroll.set_visible(true);
        self.log_scroll.vadjustment().set_value(0.);
    }
    fn at_bottom(&self) -> bool {
        let scroll = self.log_scroll.vadjustment();
        scroll.value() + scroll.page_size() >= scroll.upper() - 40.
    }
    fn append(&self, line: &str, drop_oldest: bool) {
        let buffer = self.log.buffer();
        if buffer.char_count() > 0 {
            buffer.insert(&mut buffer.end_iter(), "\n");
        }
        buffer.insert(&mut buffer.end_iter(), line);
        if drop_oldest && let Some(mut second) = buffer.iter_at_line(1) {
            buffer.delete(&mut buffer.start_iter(), &mut second);
        }
    }
}
pub struct Packages {
    pub widget: gtk::Stack,
    list: gtk::ListBox,
    /// The list, or a message in its place.
    body: gtk::Stack,
    message_title: gtk::Label,
    message_text: gtk::Label,
    retry: gtk::Button,
    status: gtk::Label,
    loading: gtk::Spinner,
    /// The AUR could not be checked, and what that leaves out.
    aur_note: gtk::Label,
    update_all: gtk::Button,
    query: gtk::SearchEntry,
    view: Cell<View>,
    /// Bumped by every request, so that an older answer is dropped.
    generation: Cell<u64>,
    all_installed: RefCell<Vec<Package>>,
    update_names: RefCell<Vec<String>>,
    tx: TxView,
    /// Lives across requests: sudo's timestamp depends on it.
    session: RefCell<Option<Session>>,
    change: RefCell<Option<Change>>,
    words: RefCell<Words>,
    /// Bumped by leaving or finishing a preparation, whose answer is then dropped.
    preparing: Cell<u64>,
    running: Cell<bool>,
    committed: Cell<bool>,
    /// The manager gave a count, so the bar no longer pulses.
    counted: Cell<bool>,
    last_used: Cell<Instant>,
    checking_lock: Cell<bool>,
    output: RefCell<VecDeque<String>>,
    build_scripts: RefCell<Vec<(String, String)>>,
    this: std::rc::Weak<Self>,
}
impl Packages {
    pub fn new() -> Rc<Self> {
        let widget = gtk::Stack::new();
        widget.set_margin_top(30);
        widget.set_margin_bottom(24);
        widget.set_margin_start(32);
        widget.set_margin_end(32);
        let browse = vbox(12);
        let head = hbox(12);
        let title = label("Packages", "title");
        title.set_hexpand(true);
        head.append(&title);
        let update_all = gtk::Button::with_label("Update All");
        update_all.set_valign(gtk::Align::Center);
        update_all.set_visible(false);
        head.append(&update_all);
        browse.append(&head);
        let tabs = hbox(0);
        tabs.add_css_class("segmented");
        tabs.set_halign(gtk::Align::Start);
        let updates_tab = gtk::ToggleButton::with_label("Updates");
        let search_tab = gtk::ToggleButton::with_label("Search");
        let installed_tab = gtk::ToggleButton::with_label("Installed");
        search_tab.set_group(Some(&updates_tab));
        installed_tab.set_group(Some(&updates_tab));
        updates_tab.set_active(true);
        tabs.append(&updates_tab);
        tabs.append(&search_tab);
        tabs.append(&installed_tab);
        browse.append(&tabs);
        let query = gtk::SearchEntry::new();
        // A search per letter would be slow and useless.
        query.set_search_delay(250);
        query.set_visible(false);
        browse.append(&query);
        let status_row = hbox(8);
        let loading = spinner();
        loading.set_visible(false);
        status_row.append(&loading);
        let status = label("", "muted");
        status_row.append(&status);
        browse.append(&status_row);
        let aur_note = label(
            "Could not check the AUR, so its updates are not listed.",
            "banner",
        );
        aur_note.set_visible(false);
        browse.append(&aur_note);
        let list = gtk::ListBox::new();
        list.add_css_class("package-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();
        let message = vbox(6);
        message.set_valign(gtk::Align::Center);
        message.set_halign(gtk::Align::Center);
        message.add_css_class("package-message");
        let message_title = label("", "row-title");
        message_title.set_justify(gtk::Justification::Center);
        message_title.set_xalign(0.5);
        message.append(&message_title);
        let message_text = label("", "muted");
        message_text.set_justify(gtk::Justification::Center);
        message_text.set_xalign(0.5);
        message_text.set_selectable(true);
        message_text.set_max_width_chars(70);
        message.append(&message_text);
        let retry = gtk::Button::with_label("Try Again");
        retry.set_halign(gtk::Align::Center);
        retry.set_margin_top(8);
        message.append(&retry);
        let body = gtk::Stack::new();
        body.set_vexpand(true);
        body.add_named(&scroll, Some("list"));
        body.add_named(&message, Some("message"));
        browse.append(&body);
        widget.add_named(&browse, Some("browse"));
        let tx = TxView::new();
        widget.add_named(&tx.page, Some("tx"));
        let this = Rc::new_cyclic(|this| Self {
            widget,
            list,
            body,
            message_title,
            message_text,
            retry,
            status,
            loading,
            aur_note,
            update_all,
            query,
            view: Cell::new(View::Updates),
            generation: Cell::new(0),
            all_installed: RefCell::default(),
            update_names: RefCell::default(),
            tx,
            session: RefCell::default(),
            change: RefCell::default(),
            words: RefCell::default(),
            preparing: Cell::new(0),
            running: Cell::new(false),
            committed: Cell::new(false),
            counted: Cell::new(false),
            last_used: Cell::new(Instant::now()),
            checking_lock: Cell::new(false),
            output: RefCell::default(),
            build_scripts: RefCell::default(),
            this: this.clone(),
        });
        for (tab, view) in [
            (&updates_tab, View::Updates),
            (&search_tab, View::Search),
            (&installed_tab, View::Installed),
        ] {
            let weak = this.this.clone();
            tab.connect_toggled(move |tab| {
                if tab.is_active()
                    && let Some(s) = weak.upgrade()
                {
                    s.view.set(view);
                    s.query.set_text("");
                    s.clear_list();
                    s.refresh();
                    if view != View::Updates {
                        s.query.grab_focus();
                    }
                }
            });
        }
        let weak = this.this.clone();
        this.query.connect_search_changed(move |_| {
            if let Some(s) = weak.upgrade() {
                s.refresh();
            }
        });
        this.on_click(&this.retry, |s| s.refresh());
        this.on_click(&this.update_all, |s| s.begin(Change::UpdateAll));
        this.on_click(&this.tx.back, |s| {
            s.preparing.set(s.preparing.get() + 1);
            s.widget.set_visible_child_name("browse");
        });
        this.on_click(&this.tx.confirm, |s| s.start());
        this.on_click(&this.tx.retry, |s| {
            let change = s.change.borrow().clone();
            if let Some(change) = change {
                s.begin(change);
            }
        });
        this.on_click(&this.tx.cancel, |s| {
            if let Some(session) = s.session.borrow().as_ref() {
                session.cancel();
                s.tx.hide_password();
                s.tx.question_box.set_visible(false);
                s.tx.cancel.set_sensitive(false);
                s.tx.stage("Cancelling…");
            }
        });
        this.on_click(&this.tx.password_send, |s| s.send_password());
        this.on_click(&this.tx.send, |s| {
            let text = s.tx.answer.text().to_string();
            s.reply(&text);
        });
        this.on_click(&this.tx.yes, |s| s.reply("y"));
        this.on_click(&this.tx.no, |s| s.reply("n"));
        let weak = this.this.clone();
        this.tx.choose.connect_selected_notify(move |choose| {
            if let Some(s) = weak.upgrade()
                && let Some((_, text)) = s.build_scripts.borrow().get(choose.selected() as usize)
            {
                s.tx.show_script(text);
            }
        });
        // Only looked at once the page is, not at every login.
        let weak = this.this.clone();
        this.widget.connect_map(move |_| {
            if let Some(s) = weak.upgrade() {
                s.all_installed.borrow_mut().clear();
                s.refresh();
                // After whatever opened the page has put focus on its sidebar entry.
                let weak = s.this.clone();
                glib::idle_add_local_once(move || {
                    if let Some(s) = weak.upgrade() {
                        s.focus_prompt();
                    }
                });
            }
        });
        // Closing only hides the window, and the session lives on here. What is left
        // for the next opening: the transaction in view, no half-typed password.
        let weak = this.this.clone();
        this.widget.connect_realize(move |widget| {
            let Some(window) = widget.root().and_downcast::<gtk::Window>() else {
                return;
            };
            let weak = weak.clone();
            window.connect_close_request(move |_| {
                if let Some(s) = weak.upgrade().filter(|s| s.running.get()) {
                    s.tx.password.set_text("");
                    if let Some(pages) = s.widget.parent().and_downcast::<gtk::Stack>() {
                        pages.set_visible_child(&s.widget);
                    }
                }
                glib::Propagation::Proceed
            });
        });
        this
    }
    fn on_click(&self, button: &gtk::Button, act: impl Fn(&Self) + 'static) {
        let weak = self.this.clone();
        button.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                act(&s);
            }
        });
    }
    fn clear_list(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.body.set_visible_child_name("list");
    }
    fn message(&self, title: &str, text: &str, retry: bool) {
        self.clear_list();
        self.message_title.set_text(title);
        self.message_text.set_text(text);
        self.message_text.set_visible(!text.is_empty());
        self.retry.set_visible(retry);
        self.body.set_visible_child_name("message");
    }
    fn fetch(
        &self,
        status: &str,
        work: impl FnOnce() -> Result<(Vec<Package>, bool), String> + Send + 'static,
    ) {
        self.generation.set(self.generation.get() + 1);
        let generation = self.generation.get();
        self.status.set_text(status);
        self.loading.set_visible(true);
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(work());
        });
        let weak = self.this.clone();
        glib::spawn_future_local(async move {
            if let (Ok(result), Some(s)) = (rx.recv().await, weak.upgrade())
                && s.generation.get() == generation
            {
                s.show(result);
            }
        });
    }
    fn refresh(&self) {
        let view = self.view.get();
        self.query.set_visible(view != View::Updates);
        self.query.set_placeholder_text(Some(match view {
            View::Installed => "Filter installed packages",
            _ => "Search packages",
        }));
        self.update_all.set_visible(view == View::Updates);
        self.update_all.set_sensitive(false);
        self.aur_note.set_visible(false);
        let text = self.query.text().trim().to_string();
        match view {
            View::Updates => self.fetch("Checking for updates…", updates),
            View::Installed => {
                let unread = self.all_installed.borrow().is_empty();
                if unread {
                    self.fetch("Reading installed packages…", || {
                        installed().map(|list| (list, false))
                    });
                } else {
                    self.generation.set(self.generation.get() + 1);
                    self.show(Ok((self.all_installed.borrow().clone(), false)));
                }
            }
            View::Search if text.len() < 2 => {
                self.generation.set(self.generation.get() + 1);
                self.loading.set_visible(false);
                self.status.set_text("");
                self.message(
                    "Find a package",
                    if manager() == "yay" {
                        "Type at least two letters to search the official repositories and the AUR."
                    } else {
                        "Type at least two letters to search the official repositories."
                    },
                    false,
                );
            }
            View::Search => self.fetch("Searching…", move || {
                search(&text).map(|list| (list, false))
            }),
        }
    }
    /// The list, and whether the AUR failed to answer for it.
    fn show(&self, result: Result<(Vec<Package>, bool), String>) {
        self.loading.set_visible(false);
        self.clear_list();
        let view = self.view.get();
        let (packages, aur_failed) = match result {
            Ok(list) => list,
            Err(e) => {
                self.status.set_text("");
                let title = match view {
                    View::Updates => "Could not check for updates",
                    View::Search => "Could not search",
                    View::Installed => "Could not read installed packages",
                };
                self.message(title, &e, true);
                return;
            }
        };
        if view == View::Installed && self.all_installed.borrow().is_empty() {
            *self.all_installed.borrow_mut() = packages.clone();
        }
        // The installed list is filtered here: it is already all in memory.
        let filter = self.query.text().to_lowercase();
        let filter = filter.trim();
        let matching: Vec<&Package> = packages
            .iter()
            .filter(|p| view != View::Installed || p.name.contains(filter))
            .collect();
        let count = matching.len();
        self.aur_note.set_visible(aur_failed);
        let synced = last_sync()
            .map(|age| format!("Lists synced {age}"))
            .unwrap_or_default();
        self.status.set_text(&match (view, count) {
            (View::Updates, 0) => String::new(),
            (View::Updates, n) => format!("{} available. {synced}", plural(n, "update")),
            (View::Search, 0) | (View::Installed, 0) => String::new(),
            (View::Search, n) if n == MAX_ROWS => format!("The best {MAX_ROWS} results"),
            (View::Search, n) => plural(n, "result"),
            (View::Installed, n) if n > MAX_ROWS => {
                format!(
                    "{} packages. Showing {MAX_ROWS}: filter to find the others.",
                    n
                )
            }
            (View::Installed, n) => plural(n, "package"),
        });
        if view == View::Updates {
            *self.update_names.borrow_mut() = matching.iter().map(|p| p.name.clone()).collect();
            // The lists may be old, so there is always a way to sync them.
            self.update_all.set_sensitive(true);
            self.update_all.set_label(if count > 0 {
                "Update All"
            } else {
                "Sync and Update"
            });
            if count > 0 {
                self.update_all.add_css_class("suggested-action");
            } else {
                self.update_all.remove_css_class("suggested-action");
            }
        }
        match (view, count) {
            (View::Updates, 0) if aur_failed => {
                self.message("Official packages are up to date", &synced, false);
            }
            (View::Updates, 0) => self.message("Everything is up to date", &synced, false),
            (View::Search, 0) => self.message(
                "Nothing found",
                &format!("No package matches “{}”.", self.query.text().trim()),
                false,
            ),
            (View::Installed, 0) if !filter.is_empty() => self.message(
                "Nothing found",
                &format!("No installed package matches “{filter}”."),
                false,
            ),
            (View::Installed, 0) => self.message("No packages found", "", true),
            _ => {}
        }
        for package in matching.into_iter().take(MAX_ROWS) {
            self.list.append(&self.row(package));
        }
    }
    fn row(&self, package: &Package) -> gtk::Box {
        let row = hbox(12);
        row.add_css_class("package-row");
        let text = vbox(2);
        text.set_hexpand(true);
        let head = hbox(8);
        let name = label(&package.name, "row-title");
        name.set_wrap(false);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        head.append(&name);
        let aur = package.repo == "aur";
        let view = self.view.get();
        if aur {
            head.append(&label("AUR", "package-badge"));
        }
        if view == View::Search && package.installed {
            head.append(&label("Installed", "package-badge"));
        }
        text.append(&head);
        let origin = if package.repo.is_empty() || aur {
            package.version.clone()
        } else {
            format!("{}  ·  {}", package.repo, package.version)
        };
        let detail = label(&origin, "caption");
        detail.add_css_class("muted");
        detail.set_wrap(false);
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&detail);
        if !package.description.is_empty() {
            let about = label(&package.description, "caption");
            about.set_wrap(true);
            about.set_max_width_chars(1);
            about.set_lines(2);
            about.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.append(&about);
        }
        row.append(&text);
        let title = match (view, package.installed, aur) {
            (View::Updates, ..) => "Update",
            (_, true, _) => "Remove…",
            (_, false, true) => "Install…",
            (_, false, false) => "Install",
        };
        let b = gtk::Button::with_label(title);
        b.set_valign(gtk::Align::Center);
        let package = package.clone();
        self.on_click(&b, move |s| {
            let name = package.name.clone();
            s.begin(if view == View::Updates {
                Change::Update(name)
            } else if package.installed {
                Change::Remove(name)
            } else {
                Change::Install {
                    repo: package.repo.clone(),
                    name,
                }
            });
        });
        row.append(&b);
        row
    }
    /// A change from the start: nothing runs before what it needs is found and shown.
    fn begin(&self, change: Change) {
        match change.clone() {
            Change::Remove(name) => {
                self.prepare(change, move || removal(&name).map(Prepared::Remove));
            }
            Change::Install { repo, name } if repo == "aur" => {
                self.prepare(change, move || review(vec![name]).map(Prepared::Review));
            }
            _ => self.prepare(change, || Ok(Prepared::Go)),
        }
    }
    /// Opens the change's view and finds out what it needs before it can start.
    fn prepare(
        &self,
        change: Change,
        work: impl FnOnce() -> Result<Prepared, String> + Send + 'static,
    ) {
        let count = self.update_names.borrow().len();
        *self.words.borrow_mut() = words(&change, count);
        *self.change.borrow_mut() = Some(change);
        self.preparing.set(self.preparing.get() + 1);
        let preparing = self.preparing.get();
        self.tx.clear();
        self.tx.title.set_text(&self.words.borrow().doing);
        self.tx.stage("Getting ready…");
        self.tx.back.set_label("Back");
        self.tx.back.set_visible(true);
        self.widget.set_visible_child_name("tx");
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(work());
        });
        let weak = self.this.clone();
        glib::spawn_future_local(async move {
            let (Ok(result), Some(s)) = (rx.recv().await, weak.upgrade()) else {
                return;
            };
            if s.preparing.get() != preparing {
                return;
            }
            match result {
                Ok(Prepared::Go) => s.start(),
                Ok(Prepared::Review(scripts)) => s.show_review(scripts),
                Ok(Prepared::Remove(also)) => s.show_removal(&also),
                Err(e) => s.show_failure(&e),
            }
        });
    }
    /// AUR packages only: their build scripts run unreviewed once this goes ahead.
    fn show_review(&self, scripts: Vec<(String, String)>) {
        let words = self.words.borrow();
        self.tx.clear();
        self.tx.title.set_text(&words.review);
        let names: Vec<&str> = scripts.iter().map(|(name, _)| name.as_str()).collect();
        self.tx.notice.set_text(&format!(
            "{} from the AUR, where anyone can publish. The build script below runs on this \
             computer with your rights and its install steps run as root, and nothing will \
             stop to show it again. Go ahead only if you trust what you read.",
            if names.len() == 1 {
                format!("{} comes", names[0])
            } else {
                format!(
                    "{} come, dependencies included,",
                    plural(names.len(), "package")
                )
            },
        ));
        self.tx.notice.set_visible(true);
        self.tx
            .choose
            .set_model(Some(&gtk::StringList::new(&names)));
        self.tx.choose.set_visible(names.len() > 1);
        self.tx.choose.set_selected(0);
        self.tx.show_script(&scripts[0].1);
        *self.build_scripts.borrow_mut() = scripts;
        self.tx.back.set_label("Back");
        self.tx.back.set_visible(true);
        self.tx.confirm.set_label(words.confirm);
        self.tx.confirm.add_css_class("suggested-action");
        self.tx.confirm.set_visible(true);
        self.arm_confirm();
    }
    /// It appears where the button that led here was: a double click must not
    /// confirm it. A timer left by a page since closed cannot shorten the wait.
    fn arm_confirm(&self) {
        self.tx.confirm.set_sensitive(false);
        let (weak, preparing) = (self.this.clone(), self.preparing.get());
        glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
            if let Some(s) = weak.upgrade().filter(|s| s.preparing.get() == preparing) {
                s.tx.confirm.set_sensitive(true);
            }
        });
    }
    fn show_removal(&self, also: &[String]) {
        let words = self.words.borrow();
        self.tx.clear();
        self.tx.title.set_text(&words.review);
        self.tx.summary.set_text(&if also.is_empty() {
            "Nothing else depends on it, and no other package goes with it.".to_string()
        } else {
            format!(
                "Also removes {} that only it needed, listed below.",
                plural(also.len(), "package")
            )
        });
        self.tx.summary.set_visible(true);
        if !also.is_empty() {
            self.tx.show_script(&also.join("\n"));
            // A short list needs no more room than it fills.
            self.tx.log_scroll.set_vexpand(false);
        }
        self.tx.back.set_label("Back");
        self.tx.back.set_visible(true);
        self.tx.confirm.set_label(words.confirm);
        self.tx.confirm.add_css_class("destructive-action");
        self.tx.confirm.set_visible(true);
        self.arm_confirm();
    }
    /// Could not even begin: the first line says why, what the tool said stays
    /// behind Show details.
    fn show_failure(&self, error: &str) {
        let (why, output) = error.split_once('\n').unwrap_or((error, ""));
        self.tx.clear();
        self.tx.title.set_text(&self.words.borrow().failed);
        self.tx.outcome.set_text(why);
        self.tx.outcome.add_css_class("banner");
        self.tx.outcome.set_visible(true);
        self.tx.log.buffer().set_text(output.trim());
        self.tx.details.set_visible(!output.trim().is_empty());
        // A wrapped line measures short, which would clip it behind a scrollbar.
        self.tx.log_scroll.set_vexpand(true);
        self.tx.back.set_label("Back to Packages");
        self.tx.back.set_visible(true);
        self.tx.retry.set_visible(true);
        self.tx.retry.grab_focus();
    }
    /// A prompt left open gets the keyboard back when its page does.
    fn focus_prompt(&self) {
        if self.tx.password_box.is_visible() {
            self.tx.password.grab_focus();
        } else if self.tx.question_box.is_visible() && self.tx.answer.get_visible() {
            self.tx.answer.grab_focus();
        }
    }
    /// Says so when this page is not what the user is looking at: the window hidden
    /// or behind another, or on another page. Off this thread, as the shell itself
    /// serves notifications and would wait on its own call.
    // ponytail: the Notify call of dock.rs copied, as that file is not this surface's; share them when both can change.
    fn notify(&self, summary: String, body: &'static str) {
        // Not `is_active`: it stays false once the window was hidden and opened again.
        let focused = self
            .widget
            .native()
            .and_then(|native| native.surface())
            .and_downcast::<gtk::gdk::Toplevel>()
            .is_some_and(|window| window.state().contains(gtk::gdk::ToplevelState::FOCUSED));
        if self.widget.is_mapped() && focused {
            return;
        }
        std::thread::spawn(move || {
            let sent = zbus::blocking::Connection::session().and_then(|bus| {
                bus.call_method(
                    Some("org.freedesktop.Notifications"),
                    "/org/freedesktop/Notifications",
                    Some("org.freedesktop.Notifications"),
                    "Notify",
                    &(
                        "Packages",
                        0u32,
                        "system-software-install-symbolic",
                        summary,
                        body,
                        Vec::<&str>::new(),
                        std::collections::HashMap::<&str, zbus::zvariant::Value>::new(),
                        -1i32,
                    ),
                )
            });
            if let Err(e) = sent {
                eprintln!("Packages: could not notify: {e}");
            }
        });
    }
    fn start(&self) {
        self.preparing.set(self.preparing.get() + 1);
        let Some(change) = self.change.borrow().clone() else {
            return;
        };
        let command = match change.command(manager()) {
            Ok(command) => command,
            Err(e) => return self.show_failure(&e),
        };
        self.build_scripts.borrow_mut().clear();
        if let Err(e) = self.ensure_session() {
            return self.show_failure(&e);
        }
        self.tx.clear();
        self.tx.title.set_text(&self.words.borrow().doing);
        if change == Change::UpdateAll {
            self.tx
                .summary
                .set_text(&self.update_names.borrow().join(", "));
            self.tx
                .summary
                .set_visible(!self.update_names.borrow().is_empty());
        }
        self.tx.stage("Starting…");
        self.tx.bar.set_visible(true);
        self.tx.cancel.set_visible(true);
        self.tx.log.buffer().set_text("");
        self.output.borrow_mut().clear();
        self.running.set(true);
        self.committed.set(false);
        self.counted.set(false);
        let started = self.session.borrow_mut().as_mut().map_or(
            Err("The manager's session is gone.".to_string()),
            |session| session.run(&command),
        );
        if let Err(e) = started {
            self.running.set(false);
            self.show_failure(&e);
        }
    }
    /// The running session, or a new one, watched until it is no longer wanted.
    fn ensure_session(&self) -> Result<(), String> {
        let mut session = self.session.borrow_mut();
        if session.as_mut().is_some_and(Session::alive) {
            return Ok(());
        }
        let new = Session::start()?;
        let rx = new.receiver.clone();
        *session = Some(new);
        let weak = self.this.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(s) = weak.upgrade() else {
                    return;
                };
                let follow = s.tx.at_bottom();
                s.handle(event);
                // A busy build arrives in bursts: one redraw for each.
                for _ in 0..256 {
                    match rx.try_recv() {
                        Ok(event) => s.handle(event),
                        Err(_) => break,
                    }
                }
                if follow {
                    s.tx.log.scroll_mark_onscreen(&s.tx.log_end);
                }
            }
        });
        let weak = self.this.clone();
        glib::timeout_add_seconds_local(5, move || {
            let Some(s) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            s.check_lock();
            s.watch_session(false)
        });
        Ok(())
    }
    /// Off this thread, as going through `/proc` can wait on a stuck process. That
    /// then holds up this check alone, one thread at most, and `KEEP` still counts.
    fn check_lock(&self) {
        if self.checking_lock.replace(true) {
            return;
        }
        let (answer, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = answer.send_blocking(tx::screen_locked());
        });
        let weak = self.this.clone();
        glib::spawn_future_local(async move {
            let locked = rx.recv().await.unwrap_or(false);
            if let Some(s) = weak.upgrade() {
                s.checking_lock.set(false);
                if locked {
                    s.watch_session(true);
                }
            }
        });
    }
    /// On screen lock the password goes at once. An idle session goes too then,
    /// or after `KEEP`: sudo's timestamp cannot be used without it.
    fn watch_session(&self, locked: bool) -> glib::ControlFlow {
        let mut session = self.session.borrow_mut();
        let Some(current) = session.as_ref() else {
            return glib::ControlFlow::Break;
        };
        if locked {
            current.forget();
            self.tx.password.set_text("");
        }
        if !self.running.get() && (locked || self.last_used.get().elapsed() > tx::KEEP) {
            *session = None;
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    }
    fn handle(&self, event: Event) {
        match event {
            Event::Line(line) => {
                // Whatever sudo or the manager was waiting for, it no longer is.
                self.tx.hide_password();
                self.tx.question_box.set_visible(false);
                if line.starts_with('(') || line.starts_with("::") || line.starts_with("==>") {
                    self.tx.stage(&line);
                }
                if !self.counted.get() {
                    self.tx.bar.pulse();
                }
                self.tx.details.set_visible(true);
                let mut output = self.output.borrow_mut();
                output.push_back(line);
                let full = output.len() > MAX_LOG_LINES;
                if full {
                    output.pop_front();
                }
                self.tx
                    .append(output.back().map_or("", String::as_str), full);
            }
            Event::Transient(text) => {
                self.tx.question_box.set_visible(false);
                // A bar with its own `(3/12)` count says how far that one package is.
                let percent = text
                    .rsplit(char::is_whitespace)
                    .next()
                    .filter(|_| !text.starts_with('('))
                    .and_then(|word| word.strip_suffix('%'))
                    .and_then(|n| n.parse::<f64>().ok());
                if let Some(percent) = percent {
                    self.counted.set(true);
                    self.tx.bar.set_fraction((percent / 100.).clamp(0., 1.));
                }
                self.tx.stage(&text);
            }
            Event::Progress(done, total) if total > 0 => {
                self.counted.set(true);
                self.tx
                    .bar
                    .set_fraction((f64::from(done) / f64::from(total)).min(1.));
            }
            Event::Progress(..) => {}
            Event::Password { rejected } => {
                self.tx.stage("Waiting for your password");
                // pam_faillock says so in the output, and more attempts would be in vain.
                let output = self.output.borrow();
                let locked = output.back().filter(|line| line.contains("is locked"));
                self.tx
                    .password_error
                    .set_text(locked.map_or("Wrong password. Try again.", String::as_str));
                self.tx
                    .password_error
                    .set_visible(rejected || locked.is_some());
                self.tx.password_box.set_visible(true);
                self.tx.password.grab_focus();
                // A prompt is never in the middle of changing the system.
                self.tx.allow_cancel(true);
                if !self.counted.get() {
                    self.tx.bar.set_fraction(0.);
                }
                self.notify(
                    format!("{}: password needed", self.words.borrow().doing),
                    "Open Packages to continue.",
                );
            }
            Event::Question(text) => {
                let yes_or_no =
                    text.trim_end().ends_with("[Y/n]") || text.trim_end().ends_with("[y/N]");
                self.tx.question.set_text(text.trim());
                self.tx.answer.set_text("");
                self.tx.answer.set_visible(!yes_or_no);
                self.tx.send.set_visible(!yes_or_no);
                self.tx.yes.set_visible(yes_or_no);
                self.tx.no.set_visible(yes_or_no);
                self.tx.question_box.set_visible(true);
                self.tx.stage("Waiting for your answer");
                if !yes_or_no {
                    self.tx.answer.grab_focus();
                }
                self.notify(
                    format!("{}: answer needed", self.words.borrow().doing),
                    "Open Packages to continue.",
                );
            }
            Event::Committing(committing) => {
                self.committed.set(self.committed.get() || committing);
                // What was still on its way cannot bring Cancel back once it is over.
                if self.running.get() {
                    self.tx.allow_cancel(!committing);
                }
            }
            Event::Done(code) => self.finish(code),
        }
    }
    fn send_password(&self) {
        // Read in place: `text()` would leave a copy of it that nothing wipes.
        let secret = {
            let entry: &gtk::Editable = self.tx.password.upcast_ref();
            // SAFETY: a valid editable, and its text is copied before it can change.
            let text = unsafe {
                std::ffi::CStr::from_ptr(gtk::ffi::gtk_editable_get_text(entry.to_glib_none().0))
            };
            Secret::new(text.to_bytes())
        };
        let session = self.session.borrow();
        let Some(session) = session.as_ref().filter(|_| !secret.is_empty()) else {
            return;
        };
        self.tx.password.set_text("");
        match session.answer(secret) {
            Ok(()) => {
                self.tx.hide_password();
                self.tx.stage("Checking your password…");
            }
            Err(e) => {
                self.tx.password_error.set_text(&e);
                self.tx.password_error.set_visible(true);
            }
        }
    }
    fn reply(&self, text: &str) {
        self.tx.question_box.set_visible(false);
        self.tx.keep_focus();
        self.tx.stage("Continuing…");
        if let Some(session) = self.session.borrow().as_ref()
            && let Err(e) = session.reply(text)
        {
            self.tx.append(&e, false);
        }
    }
    fn finish(&self, code: Option<i32>) {
        self.running.set(false);
        self.last_used.set(Instant::now());
        {
            // It took the last signal to stop: nothing is left to keep.
            let mut session = self.session.borrow_mut();
            if !session.as_mut().is_some_and(Session::alive) {
                *session = None;
            }
        }
        let words = self.words.borrow();
        self.tx.clear();
        let has_output = !self.output.borrow().is_empty();
        self.tx.details.set_visible(has_output);
        // A failure, or a stop that left some packages changed.
        let bad =
            matches!(code, Some(status) if status != 0) || (code.is_none() && self.committed.get());
        self.tx.details.set_active(has_output && bad);
        let (title, outcome, class) = match code {
            Some(0) => (words.done.as_str(), String::new(), "package-done"),
            None => (
                words.stopped,
                if self.committed.get() {
                    "Stopped after some packages had already changed. The output below has the details."
                        .to_string()
                } else {
                    "No package was changed.".to_string()
                },
                "",
            ),
            Some(status) => (
                words.failed.as_str(),
                match tx::failure_text(self.output.borrow().iter()) {
                    text if text.is_empty() => format!("The manager stopped with status {status}."),
                    text => text,
                },
                "banner",
            ),
        };
        self.tx.title.set_text(title);
        if code == Some(0) {
            self.tx.outcome.set_text("Finished without errors.");
        } else {
            self.tx.outcome.set_text(&outcome);
        }
        if !class.is_empty() {
            self.tx.outcome.add_css_class(class);
        }
        self.tx.outcome.set_visible(true);
        self.tx.back.set_label("Back to Packages");
        self.tx.back.set_visible(true);
        self.tx.back.grab_focus();
        self.tx
            .retry
            .set_visible(matches!(code, Some(status) if status != 0));
        self.notify(
            title.to_string(),
            if class == "banner" {
                "Open Packages to see why."
            } else {
                ""
            },
        );
        drop(words);
        self.all_installed.borrow_mut().clear();
        self.refresh();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_output_is_read_with_its_descriptions() {
        let found = parse_search(
            "extra/ripgrep 14.1.1-1 (1.9 MiB 6.0 MiB) [installed]\n    A search tool\n\
             aur/ripgrep-all 0.10.9-1 (+5 0.42) (Orphaned)\n    rga: search in PDFs\n\
             noise without a slash\n",
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "ripgrep");
        assert_eq!(found[0].repo, "extra");
        assert_eq!(found[0].description, "A search tool");
        assert!(found[0].installed);
        assert_eq!(found[1].repo, "aur");
        assert!(!found[1].installed);
    }
    #[test]
    fn likeliest_package_comes_first() {
        let package = |repo: &str, name: &str, description: &str| Package {
            repo: repo.into(),
            name: name.into(),
            version: "1".into(),
            description: description.into(),
            installed: false,
        };
        let mut found = vec![
            package("extra", "texlive-latexextra", "mentions firefox"),
            package("aur", "firefox-bin", ""),
            package("extra", "firefox-developer-edition", ""),
            package("extra", "firefox", ""),
        ];
        rank(&mut found, "firefox");
        let names: Vec<_> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "firefox",
                "firefox-developer-edition",
                "firefox-bin",
                "texlive-latexextra"
            ]
        );
    }
    #[test]
    fn a_search_cannot_pass_options() {
        assert_eq!(search_args("fire fox"), ["-Ss", "fire", "fox"]);
        assert_eq!(search_args("-y --config=/x foo"), ["-Ss", "foo"]);
        assert_eq!(search_args("-Rns"), ["-Ss"]);
    }
    #[test]
    fn dependencies_are_read_from_yay_info() {
        let info = "Repository      : aur\nName            : foo\n\
                    Depends On      : glibc>=2  bar-git\n                  baz\n\
                    Optional Deps   : opt: why\nMake Deps       : None\nCheck Deps      : qux=1\n\
                    URL             : https://x\n";
        assert_eq!(dependencies(info), ["glibc", "bar-git", "baz", "qux"]);
    }
    /// What the commands answer: AUR packages `foo`, which needs `bar` and `glibc`,
    /// and `bar`. `glibc` is installed; `gone` is nowhere.
    fn lookup(program: &str, args: &[&str]) -> Result<String, String> {
        let dep = |depends: &str| format!("Name : x\nDepends On : {depends}\n");
        match (program, args) {
            ("yay", ["-Sia", "foo"]) => Ok(dep("bar glibc gone")),
            ("yay", ["-Sia", "bar"]) => Ok(dep("None")),
            ("yay", ["-Sia", name]) => Err(format!(" -> No AUR package found for {name}")),
            ("pacman", ["-T", "glibc"]) => Ok(String::new()),
            ("pacman", ["-T", dep]) => Ok(format!("{dep}\n")),
            ("pacman", ["-Ssq", _]) => Ok(String::new()),
            _ => panic!("{program} {args:?}"),
        }
    }
    #[test]
    fn dependencies_are_followed_and_a_failed_lookup_stops_the_review() {
        let roots = || vec!["foo".to_string()];
        assert_eq!(
            with_aur_dependencies(roots(), lookup).unwrap(),
            ["foo", "bar"]
        );
        let outage = " -> request failed: no network\n -> No AUR package found for bar";
        for broken in ["bar", "foo"] {
            let result = with_aur_dependencies(roots(), |program, args| match args {
                ["-Sia", name] if *name == broken => Err(outage.to_string()),
                _ => lookup(program, args),
            });
            assert_eq!(result.unwrap_err(), outage);
        }
        for command in ["-T", "-Ssq"] {
            let result = with_aur_dependencies(roots(), |program, args| {
                if args[0] == command {
                    Err("error: no database".to_string())
                } else {
                    lookup(program, args)
                }
            });
            assert_eq!(result.unwrap_err(), "error: no database");
        }
        let silent = with_aur_dependencies(roots(), |program, args| match args {
            ["-Sia", "bar"] => Ok(String::new()),
            _ => lookup(program, args),
        });
        assert!(silent.is_err());
        assert!(with_aur_dependencies(vec!["gone".into()], lookup).is_err());
    }
    #[test]
    fn too_many_dependencies_stop_the_review() {
        // Each package needs the next one, without end.
        let chain = |program: &str, args: &[&str]| match args {
            ["-Sia", name] => Ok(format!("Depends On : {name}x\n")),
            ["-T", dep] => Ok(format!("{dep}\n")),
            ["-Ssq", _] => Ok(String::new()),
            _ => panic!("{program} {args:?}"),
        };
        let error = with_aur_dependencies(vec!["a".into()], chain).unwrap_err();
        assert!(error.contains("more than 64"), "{error}");
    }
    #[test]
    fn update_counts_read_naturally() {
        assert_eq!(plural(1, "package"), "1 package");
        assert_eq!(plural(12, "package"), "12 packages");
    }
    #[test]
    fn lists_read_versions_and_upgrades() {
        let list = parse_list("foot 1.20-1\nmesa 25.0-1 -> 25.1-1\n");
        assert_eq!(list[0].version, "1.20-1");
        assert_eq!(list[1].version, "25.0-1 → 25.1-1");
    }
}
