//! `nexus call`: a small window that places calls on the paired phone, and
//! what `tel:` links open. A link only fills in the number: calling takes a
//! click. The sound stays on the phone, or on the headset connected to it.

use super::super::{caption, install_theme, vbox};
use super::messages::minutes;
use super::phone::call;
use gtk::{gio, glib, prelude::*};
use nexus_control::theme;
use serde_json::{Value, json};
use std::{cell::Cell, cell::RefCell, rc::Rc, time::Instant};

const APP_ID: &str = "io.github.vitrixxl.NexusCall";
const IDLE: &str = "Calls go through your phone";

/// `nexus call [dial] [number or tel: link]`; `dial` calls straight away.
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let open: RefCell<Option<Rc<Call>>> = RefCell::default();
    app.connect_command_line(move |app, line| {
        let args: Vec<String> = line
            .arguments()
            .iter()
            .skip(2)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let call = open
            .borrow_mut()
            .get_or_insert_with(|| {
                if let Some(display) = gtk::gdk::Display::default() {
                    install_theme(&display, &theme::load());
                }
                Call::new(app)
            })
            .clone();
        let dial = args.first().is_some_and(|a| a == "dial");
        call.show(args.get(usize::from(dial)).map(|a| number_of(a)), dial);
        glib::ExitCode::SUCCESS
    });
    let mut argv = vec!["nexus".to_owned(), "call".to_owned()];
    argv.extend_from_slice(args);
    app.run_with_args(&argv);
    Ok(())
}

/// The number in `tel:+33%201-86;ext=2`, or the argument itself.
fn number_of(arg: &str) -> String {
    let rest = match arg.get(..4) {
        Some(scheme) if scheme.eq_ignore_ascii_case("tel:") => &arg[4..],
        _ => arg,
    };
    let rest = rest.trim_start_matches('/');
    let rest = rest.split([';', '?']).next().unwrap_or_default();
    glib::uri_unescape_string(rest, None::<&str>)
        .map_or_else(|| rest.to_owned(), |n| n.to_string())
        .trim()
        .to_owned()
}
/// The phone refuses anything else: `tel:` would also run carrier codes.
fn is_number(s: &str) -> bool {
    let digits = s.strip_prefix('+').unwrap_or(s);
    (2..=32).contains(&digits.chars().count())
        && digits.chars().any(|c| c.is_ascii_digit())
        && digits
            .chars()
            .all(|c| c.is_ascii_digit() || " ().-".contains(c))
}

struct Call {
    window: gtk::ApplicationWindow,
    number: gtk::Entry,
    status: gtk::Label,
    button: gtk::Button,
    /// When the call under way started.
    since: Cell<Option<Instant>>,
}

impl Call {
    fn new(app: &gtk::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Call")
            .default_width(340)
            .default_height(260)
            .resizable(false)
            .build();
        window.add_css_class("nexus");
        window.add_css_class("messages");
        window.add_css_class("call");
        let page = vbox(10);
        page.set_valign(gtk::Align::Center);
        page.set_halign(gtk::Align::Center);
        let number = gtk::Entry::builder()
            .placeholder_text("Phone number")
            .input_purpose(gtk::InputPurpose::Phone)
            .xalign(0.5)
            .width_chars(16)
            .build();
        number.add_css_class("call-number");
        page.append(&number);
        let status = caption(IDLE);
        status.set_xalign(0.5);
        status.set_justify(gtk::Justification::Center);
        status.set_wrap(true);
        status.set_max_width_chars(34);
        page.append(&status);
        let button = gtk::Button::from_icon_name("call-start-symbolic");
        button.add_css_class("compose-round");
        button.set_tooltip_text(Some("Call"));
        button.set_halign(gtk::Align::Center);
        button.set_margin_top(10);
        button.set_sensitive(false);
        page.append(&button);
        window.set_child(Some(&page));
        let c = Rc::new(Self {
            window,
            number,
            status,
            button,
            since: Cell::new(None),
        });
        let p = c.clone();
        c.number.connect_changed(move |n| {
            if p.since.get().is_none() {
                p.button.set_sensitive(is_number(n.text().trim()));
                p.status.set_text(IDLE);
                p.status.remove_css_class("error");
            }
        });
        let p = c.clone();
        c.number.connect_activate(move |_| p.press());
        let p = c.clone();
        c.button.connect_clicked(move |_| p.press());
        c
    }
    /// A second link while the window is open lands in it, unless a call is on.
    fn show(self: &Rc<Self>, number: Option<String>, dial: bool) {
        self.window.present();
        if self.since.get().is_some() {
            return;
        }
        if let Some(number) = number {
            self.number.set_text(&number);
        }
        self.number.grab_focus_without_selecting();
        self.number.set_position(-1);
        if dial {
            self.press();
        }
    }
    fn press(self: &Rc<Self>) {
        let p = self.clone();
        if self.since.get().is_some() {
            call("call_end", Value::Null, move |r| {
                if let Err(e) = r {
                    p.fail(&e);
                }
            });
            return;
        }
        let to = self.number.text().trim().to_owned();
        if !is_number(&to) || !self.button.is_sensitive() {
            return;
        }
        self.button.set_sensitive(false);
        self.status.set_text("Calling…");
        self.status.remove_css_class("error");
        call("call", json!({ "to": to }), move |r| {
            p.button.set_sensitive(true);
            match r {
                Ok(_) => p.follow(),
                Err(e) => p.fail(&e),
            }
        });
    }
    fn fail(&self, error: &str) {
        self.status.set_text(error);
        self.status.add_css_class("error");
    }
    fn set_call(&self, since: Option<Instant>) {
        self.since.set(since);
        self.number.set_editable(since.is_none());
        let (icon, tip) = match since {
            Some(_) => ("call-stop-symbolic", "Hang up"),
            None => ("call-start-symbolic", "Call"),
        };
        self.button.set_icon_name(icon);
        self.button.set_tooltip_text(Some(tip));
        if since.is_some() {
            self.button.add_css_class("hang-up");
            self.status.set_text("0:00");
        } else {
            self.button.remove_css_class("hang-up");
            self.status.set_text(IDLE);
        }
    }
    /// The timer, until the phone says the line is free again.
    fn follow(self: &Rc<Self>) {
        let start = Instant::now();
        self.set_call(Some(start));
        let w = Rc::downgrade(self);
        glib::timeout_add_seconds_local(1, move || {
            let Some(p) = w.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if p.since.get() != Some(start) {
                return glib::ControlFlow::Break;
            }
            let secs = start.elapsed().as_secs();
            p.status.set_text(&minutes(secs));
            // The line takes a moment to go busy after the call is placed.
            if secs >= 3 && secs % 2 == 1 {
                let q = p.clone();
                call("call_state", Value::Null, move |r| {
                    if r.map_or(true, |d| d["state"] == "idle") && q.since.get() == Some(start) {
                        q.set_call(None);
                    }
                });
            }
            glib::ControlFlow::Continue
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers_come_out_of_links() {
        assert_eq!(number_of("tel:0186040310"), "0186040310");
        assert_eq!(number_of("TEL:+33%201%2086"), "+33 1 86");
        assert_eq!(number_of("tel://0186;ext=2"), "0186");
        assert_eq!(number_of("0612"), "0612");
    }
    #[test]
    fn carrier_codes_are_not_numbers() {
        assert!(is_number("+33 (0)1 86-04.03"));
        assert!(!is_number("*21*0612#"));
        assert!(!is_number("+"));
        assert!(!is_number("()"));
        assert!(!is_number("abc"));
    }
}
