//! Desktop notification server (freedesktop.org Desktop Notifications 1.2).
//!
//! It lives in the shell and runs on the GTK main context. Applications reach it
//! on the session bus GLib finds, which is the one they use themselves, even in
//! sessions that only autolaunch it.
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const NAME: &str = "org.freedesktop.Notifications";
pub const PATH: &str = "/org/freedesktop/Notifications";
/// Older notifications fall out of the history beyond this.
pub const HISTORY: usize = 100;
/// How long a popup stays when the application leaves it to the server.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(6);
const CAPABILITIES: [&str; 6] = [
    "actions",
    "body",
    "body-hyperlinks",
    "body-markup",
    "icon-static",
    "persistence",
];
const INTROSPECTION: &str = r#"<node>
  <interface name="org.freedesktop.Notifications">
    <method name="GetCapabilities">
      <arg name="capabilities" type="as" direction="out"/>
    </method>
    <method name="Notify">
      <arg name="app_name" type="s" direction="in"/>
      <arg name="replaces_id" type="u" direction="in"/>
      <arg name="app_icon" type="s" direction="in"/>
      <arg name="summary" type="s" direction="in"/>
      <arg name="body" type="s" direction="in"/>
      <arg name="actions" type="as" direction="in"/>
      <arg name="hints" type="a{sv}" direction="in"/>
      <arg name="expire_timeout" type="i" direction="in"/>
      <arg name="id" type="u" direction="out"/>
    </method>
    <method name="CloseNotification">
      <arg name="id" type="u" direction="in"/>
    </method>
    <method name="GetServerInformation">
      <arg name="name" type="s" direction="out"/>
      <arg name="vendor" type="s" direction="out"/>
      <arg name="version" type="s" direction="out"/>
      <arg name="spec_version" type="s" direction="out"/>
    </method>
    <signal name="NotificationClosed">
      <arg name="id" type="u"/>
      <arg name="reason" type="u"/>
    </signal>
    <signal name="ActionInvoked">
      <arg name="id" type="u"/>
      <arg name="action_key" type="s"/>
    </signal>
  </interface>
</node>"#;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}
/// Raw pixels from an `image-data` hint, validated against their dimensions.
#[derive(Clone, Debug, PartialEq)]
pub struct Pixels {
    pub width: i32,
    pub height: i32,
    pub rowstride: i32,
    pub has_alpha: bool,
    pub data: glib::Bytes,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Image {
    Pixels(Pixels),
    File(String),
    /// A themed icon name.
    Named(String),
}
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub app_icon: Option<Image>,
    /// A picture of the content itself, such as an avatar or album art.
    pub image: Option<Image>,
    pub summary: String,
    /// Markup that a GTK label renders as is.
    pub body: String,
    /// Key and label of each action; `default` is a click on the notification.
    pub actions: Vec<(String, String)>,
    pub urgency: Urgency,
    /// How long the popup stays; `None` keeps it until it is dismissed.
    pub timeout: Option<Duration>,
    /// Leaves no trace in the history once its popup is gone.
    pub transient: bool,
    /// Stays after one of its actions is invoked.
    pub resident: bool,
    /// Progress between 0 and 100, as volume and download notifications show.
    pub value: Option<u8>,
    pub desktop_entry: Option<String>,
    /// Volume and brightness scripts replace their previous notification by tag.
    pub stack_tag: Option<String>,
    /// Seconds since the Unix epoch.
    pub time: i64,
}
impl Notification {
    pub fn default_action(&self) -> bool {
        self.actions.iter().any(|(key, _)| key == "default")
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Expired = 1,
    Dismissed = 2,
    Closed = 3,
    Undefined = 4,
}
/// Live notifications, newest first.
#[derive(Default)]
pub struct Store {
    list: VecDeque<Notification>,
    last_id: u32,
}
impl Store {
    /// Adds `n`, or replaces the notification it updates in place: the one named
    /// by `replaces` or, failing that, one of the same application with the same
    /// stack tag. Returns its id and the notifications pushed out of the history.
    pub fn post(&mut self, replaces: u32, mut n: Notification) -> (u32, Vec<u32>) {
        let existing = self
            .list
            .iter()
            .position(|o| replaces != 0 && o.id == replaces)
            .or_else(|| {
                let tag = n.stack_tag.as_ref()?;
                self.list
                    .iter()
                    .position(|o| o.app_name == n.app_name && o.stack_tag.as_ref() == Some(tag))
            });
        n.id = match existing.and_then(|i| self.list.remove(i)) {
            Some(old) => old.id,
            None => {
                self.last_id = self.last_id.checked_add(1).unwrap_or(1);
                self.last_id
            }
        };
        let id = n.id;
        self.list.push_front(n);
        let evicted = self.list.drain(HISTORY.min(self.list.len())..);
        (id, evicted.map(|n| n.id).collect())
    }
    pub fn remove(&mut self, id: u32) -> Option<Notification> {
        let i = self.list.iter().position(|n| n.id == id)?;
        self.list.remove(i)
    }
    pub fn get(&self, id: u32) -> Option<&Notification> {
        self.list.iter().find(|n| n.id == id)
    }
    pub fn list(&self) -> impl Iterator<Item = &Notification> {
        self.list.iter()
    }
}

fn string_hint(hints: &HashMap<String, glib::Variant>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| hints.get(*k)?.str().map(str::to_owned))
        .filter(|s| !s.is_empty())
}
fn bool_hint(hints: &HashMap<String, glib::Variant>, key: &str) -> bool {
    hints
        .get(key)
        .and_then(|v| v.get::<bool>())
        .unwrap_or(false)
}
/// Integer hints arrive as whatever type the sending library picked.
fn int_hint(hints: &HashMap<String, glib::Variant>, key: &str) -> Option<i64> {
    let v = hints.get(key)?;
    v.get::<u8>()
        .map(i64::from)
        .or_else(|| v.get::<i32>().map(i64::from))
        .or_else(|| v.get::<u32>().map(i64::from))
        .or_else(|| v.get::<i64>())
}
/// An icon given as an icon name, an absolute path or a `file://` URI.
fn icon(name: &str) -> Option<Image> {
    if name.is_empty() {
        None
    } else if name.starts_with("file://") {
        gio::File::for_uri(name)
            .path()
            .map(|p| Image::File(p.to_string_lossy().into_owned()))
    } else if name.starts_with('/') {
        Some(Image::File(name.into()))
    } else {
        Some(Image::Named(name.into()))
    }
}
/// The `(iiibiiay)` image structure, if its data covers its dimensions.
fn pixels(v: &glib::Variant) -> Option<Image> {
    if v.type_().as_str() != "(iiibiiay)" {
        return None;
    }
    let width = v.child_value(0).get::<i32>()?;
    let height = v.child_value(1).get::<i32>()?;
    let rowstride = v.child_value(2).get::<i32>()?;
    let has_alpha = v.child_value(3).get::<bool>()?;
    let bits = v.child_value(4).get::<i32>()?;
    let channels = v.child_value(5).get::<i32>()?;
    let data = v.child_value(6).data_as_bytes();
    let row = width.checked_mul(channels)?;
    let needed = (height.checked_sub(1)? as usize)
        .checked_mul(usize::try_from(rowstride).ok()?)?
        .checked_add(row as usize)?;
    ((1..=4096).contains(&width)
        && (1..=4096).contains(&height)
        && bits == 8
        && channels == if has_alpha { 4 } else { 3 }
        && rowstride >= row
        && data.len() >= needed)
        .then_some(Image::Pixels(Pixels {
            width,
            height,
            rowstride,
            has_alpha,
            data,
        }))
}
/// Reads the arguments of a `Notify` call: `(susssasa{sv}i)`.
pub fn parse(params: &glib::Variant) -> Option<(u32, Notification)> {
    if params.type_().as_str() != "(susssasa{sv}i)" {
        return None;
    }
    let text = |i| params.child_value(i).str().unwrap_or_default().to_owned();
    let replaces = params.child_value(1).get::<u32>()?;
    let actions = params.child_value(5).get::<Vec<String>>()?;
    let hints = params
        .child_value(6)
        .get::<HashMap<String, glib::Variant>>()?;
    let timeout = params.child_value(7).get::<i32>()?;
    let urgency = match int_hint(&hints, "urgency") {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    };
    let image = ["image-data", "image_data"]
        .iter()
        .find_map(|k| pixels(hints.get(*k)?))
        .or_else(|| icon(&string_hint(&hints, &["image-path", "image_path"])?));
    let app_icon = icon(&text(2)).or_else(|| pixels(hints.get("icon_data")?));
    Some((
        replaces,
        Notification {
            id: 0,
            app_name: text(0),
            app_icon,
            image,
            summary: text(3),
            body: markup(&text(4)),
            actions: actions
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[key, label]| (key.clone(), label.clone()))
                .collect(),
            urgency,
            timeout: match timeout {
                0 => None,
                ms if ms > 0 => Some(Duration::from_millis(ms as u64)),
                // Critical notifications wait for the user unless the sender says otherwise.
                _ if urgency == Urgency::Critical => None,
                _ => Some(DEFAULT_TIMEOUT),
            },
            transient: bool_hint(&hints, "transient"),
            resident: bool_hint(&hints, "resident"),
            value: int_hint(&hints, "value").map(|v| v.clamp(0, 100) as u8),
            desktop_entry: string_hint(&hints, &["desktop-entry"]),
            stack_tag: string_hint(
                &hints,
                &["x-canonical-private-synchronous", "x-dunst-stack-tag"],
            ),
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
        },
    ))
}

fn escape(text: &str) -> String {
    glib::markup_escape_text(text).into()
}
/// Reduces body markup to what the specification allows (`b`, `i`, `u`, `a`)
/// as balanced markup a GTK label accepts. Other tags are dropped, `br` becomes
/// a line break and stray `<`, `>` and `&` are escaped.
pub fn markup(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut open: Vec<&str> = vec![];
    let mut rest = body;
    while let Some(c) = rest.chars().next() {
        match c {
            '<' => {
                let tag = rest[1..]
                    .find('>')
                    .map(|end| &rest[1..end + 1])
                    .filter(|t| t.starts_with(|c: char| c == '/' || c.is_ascii_alphabetic()));
                let Some(tag) = tag else {
                    out.push_str("&lt;");
                    rest = &rest[1..];
                    continue;
                };
                rest = &rest[tag.len() + 2..];
                let closing = tag.starts_with('/');
                let name = tag
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let name = match name.as_str() {
                    "b" => "b",
                    "i" => "i",
                    "u" => "u",
                    "a" => "a",
                    "br" => {
                        out.push('\n');
                        continue;
                    }
                    _ => continue,
                };
                if closing {
                    // Close whatever is still open inside it; ignore a stray closing tag.
                    if let Some(depth) = open.iter().rposition(|t| *t == name) {
                        for t in open.drain(depth..).rev() {
                            out.push_str(&format!("</{t}>"));
                        }
                    }
                } else if name == "a" {
                    // Links without a target only keep their text.
                    if let Some(href) = attribute(tag, "href") {
                        out.push_str(&format!("<a href=\"{}\">", escape(&unescape(href))));
                        open.push(name);
                    }
                } else {
                    out.push_str(&format!("<{name}>"));
                    open.push(name);
                }
            }
            '&' => {
                let entity = rest[1..]
                    .find(';')
                    .map(|end| &rest[1..end + 1])
                    .filter(|e| {
                        matches!(*e, "amp" | "lt" | "gt" | "quot" | "apos")
                            || e.strip_prefix("#x").is_some_and(|h| {
                                !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit())
                            })
                            || e.strip_prefix('#').is_some_and(|d| {
                                !d.is_empty() && d.chars().all(|c| c.is_ascii_digit())
                            })
                    });
                match entity {
                    Some(e) => {
                        out.push_str(&rest[..e.len() + 2]);
                        rest = &rest[e.len() + 2..];
                    }
                    None if rest.starts_with("&nbsp;") => {
                        out.push('\u{a0}');
                        rest = &rest[6..];
                    }
                    None => {
                        out.push_str("&amp;");
                        rest = &rest[1..];
                    }
                }
            }
            '>' => {
                out.push_str("&gt;");
                rest = &rest[1..];
            }
            c => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    for t in open.into_iter().rev() {
        out.push_str(&format!("</{t}>"));
    }
    out
}
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(at) = rest.find(name) {
        let after = rest[at + name.len()..].trim_start();
        let boundary = rest[..at].ends_with(char::is_whitespace);
        rest = &rest[at + name.len()..];
        if let (true, Some(value)) = (boundary, after.strip_prefix('=')) {
            let value = value.trim_start();
            let quote = value.chars().next().filter(|q| *q == '"' || *q == '\'')?;
            return value[1..].split(quote).next();
        }
    }
    None
}
fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

pub enum Change {
    /// A new notification, or a new version of one already shown.
    Posted(Box<Notification>),
    Closed(u32),
    /// The server gained or lost the bus name.
    Status,
}
/// The notification server. All of it runs on the main context it started on.
pub struct Server {
    store: RefCell<Store>,
    connection: RefCell<Option<gio::DBusConnection>>,
    status: RefCell<Option<String>>,
    owner: Cell<Option<gio::OwnerId>>,
    changed: Box<dyn Fn(Change)>,
}
impl Server {
    /// Claims the notification bus name; `changed` hears about every change.
    /// Another running notification daemon keeps the name until it exits.
    ///
    /// The name is requested before this returns, without waiting for the main
    /// loop: an application notifying in the meantime would otherwise have the
    /// bus activate another daemon, which may never give the name back. Calls
    /// queue until the main loop runs.
    pub fn start(changed: impl Fn(Change) + 'static) -> Rc<Self> {
        let server = Rc::new(Self {
            store: RefCell::default(),
            connection: RefCell::default(),
            status: RefCell::default(),
            owner: Cell::new(None),
            changed: Box::new(changed),
        });
        let connection = match gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) {
            Ok(connection) => connection,
            Err(e) => {
                *server.status.borrow_mut() = Some(format!("No session bus is available: {e}"));
                return server;
            }
        };
        server.register(&connection);
        let (acquired, lost) = (Rc::downgrade(&server), Rc::downgrade(&server));
        let owner = gio::bus_own_name_on_connection(
            &connection,
            NAME,
            gio::BusNameOwnerFlags::ALLOW_REPLACEMENT | gio::BusNameOwnerFlags::REPLACE,
            move |_, _| {
                if let Some(server) = acquired.upgrade() {
                    server.set_status(None);
                }
            },
            move |_, _| {
                if let Some(server) = lost.upgrade() {
                    server.set_status(Some(
                        "Another notification daemon is running. Nexus takes over when it exits.",
                    ));
                }
            },
        );
        server.owner.set(Some(owner));
        server
    }
    fn register(self: &Rc<Self>, connection: &gio::DBusConnection) {
        let info = gio::DBusNodeInfo::for_xml(INTROSPECTION)
            .ok()
            .and_then(|node| node.lookup_interface(NAME));
        let Some(info) = info else { return };
        let weak = Rc::downgrade(self);
        let registered = connection
            .register_object(PATH, &info)
            .method_call(move |_, _, _, _, method, params, invocation| {
                if let Some(server) = weak.upgrade() {
                    server.call(method, &params, invocation);
                }
            })
            .build();
        match registered {
            Ok(_) => *self.connection.borrow_mut() = Some(connection.clone()),
            Err(e) => eprintln!("Could not serve notifications: {e}"),
        }
    }
    fn set_status(&self, status: Option<&str>) {
        *self.status.borrow_mut() = status.map(str::to_owned);
        (self.changed)(Change::Status);
    }
    /// Why notifications cannot arrive, if they cannot.
    pub fn status(&self) -> Option<String> {
        self.status.borrow().clone()
    }
    fn call(&self, method: &str, params: &glib::Variant, invocation: gio::DBusMethodInvocation) {
        match method {
            "GetCapabilities" => {
                invocation.return_value(Some(&(CAPABILITIES.to_vec(),).to_variant()));
            }
            "GetServerInformation" => invocation.return_value(Some(
                &("Nexus", "Vitrixxl", env!("CARGO_PKG_VERSION"), "1.2").to_variant(),
            )),
            "Notify" => match parse(params) {
                Some((replaces, n)) => {
                    let id = self.post(replaces, n);
                    invocation.return_value(Some(&(id,).to_variant()));
                }
                None => invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.InvalidArgs",
                    "Invalid notification",
                ),
            },
            "CloseNotification" => {
                if let Some(id) = params.child_value(0).get::<u32>() {
                    self.close(id, Reason::Closed);
                }
                invocation.return_value(None);
            }
            _ => invocation
                .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", "Unknown method"),
        }
    }
    /// Adds a notification as if it came over the bus; returns its id.
    pub fn post(&self, replaces: u32, n: Notification) -> u32 {
        let (id, evicted) = self.store.borrow_mut().post(replaces, n);
        for old in evicted {
            self.signal("NotificationClosed", &(old, Reason::Undefined as u32));
            (self.changed)(Change::Closed(old));
        }
        let posted = self.store.borrow().get(id).cloned();
        if let Some(n) = posted {
            (self.changed)(Change::Posted(Box::new(n)));
        }
        id
    }
    fn close(&self, id: u32, reason: Reason) {
        let removed = self.store.borrow_mut().remove(id);
        if removed.is_some() {
            self.signal("NotificationClosed", &(id, reason as u32));
            (self.changed)(Change::Closed(id));
        }
    }
    fn signal(&self, name: &str, args: &impl ToVariant) {
        if let Some(connection) = self.connection.borrow().as_ref()
            && let Err(e) = connection.emit_signal(None, PATH, NAME, name, Some(&args.to_variant()))
        {
            eprintln!("Could not send {name}: {e}");
        }
    }
    /// The user closed it.
    pub fn dismiss(&self, id: u32) {
        self.close(id, Reason::Dismissed);
    }
    /// Its popup timed out. Only transient notifications go; the others stay in
    /// the history and remain actionable.
    pub fn expire(&self, id: u32) {
        if self.store.borrow().get(id).is_some_and(|n| n.transient) {
            self.close(id, Reason::Expired);
        }
    }
    pub fn invoke(&self, id: u32, key: &str) {
        let resident = match self.store.borrow().get(id) {
            Some(n) => n.resident,
            None => return,
        };
        self.signal("ActionInvoked", &(id, key));
        if !resident {
            self.dismiss(id);
        }
    }
    pub fn clear(&self) {
        let ids: Vec<_> = self.store.borrow().list().map(|n| n.id).collect();
        for id in ids {
            self.dismiss(id);
        }
    }
    pub fn count(&self) -> usize {
        self.store.borrow().list().count()
    }
    /// Live notifications, newest first.
    pub fn list(&self) -> Vec<Notification> {
        self.store.borrow().list().cloned().collect()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            gio::bus_unown_name(owner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn notify(app: &str, replaces: u32, hints: HashMap<String, glib::Variant>) -> glib::Variant {
        (
            app,
            replaces,
            "dialog-information",
            "Summary",
            "Body",
            vec!["default", "Open", "reply", "Reply"],
            hints,
            -1i32,
        )
            .to_variant()
    }
    #[test]
    fn parses_notify_arguments() {
        let hints = HashMap::from([
            ("urgency".to_string(), 2u8.to_variant()),
            ("value".to_string(), 140i32.to_variant()),
            ("desktop-entry".to_string(), "firefox".to_variant()),
        ]);
        let (replaces, n) = parse(&notify("Firefox", 7, hints)).unwrap();
        assert_eq!(replaces, 7);
        assert_eq!(n.app_name, "Firefox");
        assert_eq!(n.app_icon, Some(Image::Named("dialog-information".into())));
        assert_eq!(n.urgency, Urgency::Critical);
        assert_eq!(n.timeout, None, "critical notifications wait for the user");
        assert_eq!(n.value, Some(100));
        assert_eq!(n.desktop_entry.as_deref(), Some("firefox"));
        assert!(n.default_action());
        assert_eq!(n.actions[1], ("reply".into(), "Reply".into()));
        assert!(parse(&("only", 1u32).to_variant()).is_none());
    }
    #[test]
    fn validates_image_data() {
        let image = |len: usize| {
            HashMap::from([(
                "image-data".to_string(),
                (2i32, 2i32, 8i32, true, 8i32, 4i32, vec![0u8; len]).to_variant(),
            )])
        };
        let (_, n) = parse(&notify("A", 0, image(16))).unwrap();
        assert!(matches!(n.image, Some(Image::Pixels(ref p)) if p.width == 2 && p.has_alpha));
        // Data shorter than the last row would make GTK read past the buffer.
        let (_, n) = parse(&notify("A", 0, image(15))).unwrap();
        assert_eq!(n.image, None);
        let path = HashMap::from([(
            "image-path".to_string(),
            "file:///tmp/cover%20art.png".to_variant(),
        )]);
        let (_, n) = parse(&notify("A", 0, path)).unwrap();
        assert_eq!(n.image, Some(Image::File("/tmp/cover art.png".into())));
    }
    #[test]
    fn replaces_by_id_and_stack_tag() {
        let mut store = Store::default();
        let n = |app: &str, tag: Option<&str>| {
            let (_, mut n) = parse(&notify(app, 0, HashMap::new())).unwrap();
            n.stack_tag = tag.map(str::to_owned);
            n
        };
        let (first, _) = store.post(0, n("A", None));
        let (second, _) = store.post(0, n("B", Some("volume")));
        assert_eq!((first, second), (1, 2));
        assert_eq!(store.post(first, n("A", None)).0, first);
        assert_eq!(store.post(0, n("B", Some("volume"))).0, second);
        // Another application's tag is its own.
        assert_eq!(store.post(0, n("C", Some("volume"))).0, 3);
        // An unknown id gets a fresh one rather than reviving a closed notification.
        assert_eq!(store.post(99, n("A", None)).0, 4);
        assert_eq!(store.list().map(|n| n.id).collect::<Vec<_>>(), [4, 3, 2, 1]);
        assert!(store.remove(2).is_some());
        assert!(store.remove(2).is_none());
        for _ in 0..HISTORY {
            store.post(0, n("D", None));
        }
        assert_eq!(store.list().count(), HISTORY);
        assert!(
            store.get(1).is_none(),
            "the oldest falls out of the history"
        );
    }
    #[test]
    fn sanitizes_body_markup() {
        assert_eq!(markup("plain & simple"), "plain &amp; simple");
        assert_eq!(markup("a < b > c"), "a &lt; b &gt; c");
        assert_eq!(
            markup("<b>bold</b> &amp; <I>it</I>"),
            "<b>bold</b> &amp; <i>it</i>"
        );
        assert_eq!(markup("<b><i>open"), "<b><i>open</i></b>");
        assert_eq!(markup("<b>x<i>y</b>z</i>"), "<b>x<i>y</i></b>z");
        assert_eq!(markup("line<br/>next<br>"), "line\nnext\n");
        assert_eq!(
            markup(r#"<img src="x.png" alt="pic"/>text <font color="red">red</font>"#),
            "text red"
        );
        assert_eq!(
            markup(r#"<a href="https://x.org/?a=1&amp;b=&quot;2&quot;">link</a>"#),
            r#"<a href="https://x.org/?a=1&amp;b=&quot;2&quot;">link</a>"#
        );
        assert_eq!(markup("<a>no target</a>"), "no target");
        assert_eq!(
            markup("&nbsp;&#169;&#xA9;&bogus;"),
            "\u{a0}&#169;&#xA9;&amp;bogus;"
        );
        assert_eq!(markup("unterminated <b"), "unterminated &lt;b");
    }
}
