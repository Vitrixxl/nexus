package io.github.vitrixxl.nexus;

import android.app.ActivityManager;
import android.app.Notification;
import android.app.NotificationManager;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.drawable.Drawable;
import android.media.ThumbnailUtils;
import android.net.ConnectivityManager;
import android.net.NetworkCapabilities;
import android.net.Uri;
import android.os.BatteryManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.os.Handler;
import android.os.Looper;
import android.os.StatFs;
import android.os.SystemClock;
import android.provider.ContactsContract;
import android.provider.Telephony;
import android.service.notification.NotificationListenerService;
import android.service.notification.StatusBarNotification;
import android.telecom.TelecomManager;
import android.telephony.SmsManager;
import android.telephony.TelephonyManager;
import android.util.Base64;
import android.util.Size;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.RandomAccessFile;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * The link to the desktop. A notification listener, so the system keeps it
 * running for as long as notification access is granted; it holds one TCP
 * connection to nexusd over Tailscale, newline-delimited JSON both ways.
 */
public class Link extends NotificationListenerService {
    static volatile Link self;
    private final ExecutorService pool = Executors.newFixedThreadPool(3);
    private final Map<String, String> icons = new HashMap<>();
    /** The connection to the desktop in use, by host:port: one at a time. */
    private final Map<String, Conn> conns = new ConcurrentHashMap<>();

    static { VaultStore.changed = Link::vaultChanged; }

    /** The paired desktops: [{id, host, port, token, name, vault}]; vault is the sync consent. */
    static JSONArray pairs(Context c) {
        SharedPreferences p = c.getSharedPreferences("pair", MODE_PRIVATE);
        try {
            JSONArray list = new JSONArray(p.getString("list", "[]"));
            // The single pairing of the first version.
            if (list.length() == 0 && p.contains("token")) {
                String host = p.getString("host", "");
                int port = p.getInt("port", 47913);
                list.put(new JSONObject().put("id", host + ":" + port).put("host", host).put("port", port)
                        .put("token", p.getString("token", "")).put("name", host));
                p.edit().clear().putString("list", list.toString()).apply();
            }
            return list;
        } catch (Exception e) { return new JSONArray(); }
    }

    /** The desktop in use: the one tapped or paired last. */
    static String active(Context c) {
        String id = c.getSharedPreferences("pair", MODE_PRIVATE).getString("active", "");
        JSONArray list = pairs(c);
        for (int i = 0; i < list.length(); i++) {
            if (list.optJSONObject(i).optString("id").equals(id)) return id;
        }
        JSONObject last = list.optJSONObject(list.length() - 1);
        return last == null ? "" : last.optString("id");
    }

    /** Connects to the desktop `id` now, dropping the one in use. */
    static void use(Context c, String id) {
        c.getSharedPreferences("pair", MODE_PRIVATE).edit().putString("active", id).apply();
        Link l = self;
        if (l == null) return;
        Conn old = l.conns.remove(id);
        if (old != null) old.stop();
        l.sync();
    }

    /** Names the paired desktop `id`, without touching its link. */
    static void rename(Context c, String id, String name) {
        JSONArray list = pairs(c);
        for (int i = 0; i < list.length(); i++) {
            JSONObject p = list.optJSONObject(i);
            if (p != null && p.optString("id").equals(id)) try { p.put("name", name); } catch (Exception ignored) {}
        }
        c.getSharedPreferences("pair", MODE_PRIVATE).edit().putString("list", list.toString()).apply();
    }

    static void savePairs(Context c, JSONArray list) {
        c.getSharedPreferences("pair", MODE_PRIVATE).edit().putString("list", list.toString()).apply();
        Link l = self;
        if (l != null) l.sync();
    }

    /**
     * Puts `text` on the connected desktop's clipboard; says how it went.
     * Android lets only the app in front read the clipboard, so this comes
     * from {@link Send}: a share, or the Quick Settings tile.
     */
    static String sendClip(String text) {
        Link l = self;
        if (l != null) for (Conn c : l.conns.values()) {
            if (c.connected()) {
                l.pool.execute(() -> { try { c.send(new JSONObject().put("t", "clip").put("text", text)); } catch (Exception ignored) {} });
                return "Copied on " + c.name;
            }
        }
        return "No desktop is connected";
    }

    /** What the desktop copied, on this phone's clipboard. */
    private volatile String pendingClip;
    private final android.content.BroadcastReceiver unlocked = new android.content.BroadcastReceiver() {
        @Override public void onReceive(Context c, Intent i) {
            String text = pendingClip;
            pendingClip = null;
            if (text != null) try { setClip(text); } catch (Exception ignored) {}
        }
    };

    /**
     * What the desktop copied, on this phone's clipboard; while the phone is
     * locked, once it unlocks. Says which, or throws what Android refused.
     */
    private String setClip(String text) throws Exception {
        if (getSystemService(android.app.KeyguardManager.class).isKeyguardLocked()) {
            pendingClip = text;
            return "pending";
        }
        java.util.concurrent.FutureTask<Void> task = new java.util.concurrent.FutureTask<>(() -> {
            getSystemService(ClipboardManager.class).setPrimaryClip(ClipData.newPlainText("Nexus", text));
            return null;
        });
        new Handler(Looper.getMainLooper()).post(task);
        task.get(5, java.util.concurrent.TimeUnit.SECONDS);
        return "copied";
    }

    private static NotesStore notes;
    /** The notes, kept in step with the desktop by its daemon. */
    static synchronized NotesStore notes(Context c) {
        if (notes == null) notes = new NotesStore(new File(c.getApplicationContext().getFilesDir(), "notes"));
        return notes;
    }
    /** What the desktop last said of the notes sync: {on, busy, last, conflicts, error}, and which desktop. */
    static volatile JSONObject notesState;
    static volatile String notesDesktop = "";

    /** Asks the connected desktop to sync the notes soon. */
    static void notesChanged() {
        Link l = self;
        if (l != null) for (Conn c : l.conns.values()) {
            if (c.connected()) l.pool.execute(() -> { try { c.send(new JSONObject().put("t", "notes_changed")); } catch (Exception ignored) {} });
        }
    }

    /** Tells the connected desktop that the vault, or the consent, changed. */
    static void vaultChanged() {
        Link l = self;
        if (l != null) for (Conn c : l.conns.values()) {
            if (c.connected()) l.pool.execute(() -> { try { c.send(new JSONObject().put("t", "vault_changed")); } catch (Exception ignored) {} });
        }
    }

    static final int CONSENT_NOTIFICATION = 7301;

    /** "ask", "yes" or "no" for this desktop. Forgotten with its pairing: a new pairing asks again. */
    static String consent(Context c, String id) {
        JSONArray list = pairs(c);
        for (int i = 0; i < list.length(); i++) {
            JSONObject p = list.optJSONObject(i);
            if (p != null && p.optString("id").equals(id)) return p.optString("vault", "ask");
        }
        return "no";
    }

    static void setConsent(Context c, String id, String consent) {
        JSONArray list = pairs(c);
        for (int i = 0; i < list.length(); i++) {
            JSONObject p = list.optJSONObject(i);
            if (p != null && p.optString("id").equals(id)) try { p.put("vault", consent); } catch (Exception ignored) {}
        }
        c.getSharedPreferences("pair", MODE_PRIVATE).edit().putString("list", list.toString()).apply();
        vaultChanged();
    }

    /** Tells the connected desktop to read the calendars again. */
    static void calendarChanged() {
        Link l = self;
        if (l != null) for (Conn c : l.conns.values()) {
            if (c.connected()) l.pool.execute(() -> { try { c.send(new JSONObject().put("t", "calendar_changed")); } catch (Exception ignored) {} });
        }
    }
    private final Handler calendarHandler = new Handler(Looper.getMainLooper());
    /** One word for a burst of changes, as a calendar account's sync makes them. */
    private final Runnable calendarSettled = () -> {
        calendarChanged();
        pool.execute(() -> { try { CalWidget.refresh(this); } catch (Exception ignored) {} });
    };
    private final android.database.ContentObserver calendarObserver = new android.database.ContentObserver(calendarHandler) {
        @Override public void onChange(boolean self) {
            calendarHandler.removeCallbacks(calendarSettled);
            calendarHandler.postDelayed(calendarSettled, 1500);
        }
    };

    /** Starts following the calendars; Android refuses it until access is granted, so again after that. */
    static void watchCalendar() {
        Link l = self;
        if (l == null) return;
        l.getContentResolver().unregisterContentObserver(l.calendarObserver);
        try { l.getContentResolver().registerContentObserver(android.provider.CalendarContract.CONTENT_URI, true, l.calendarObserver); } catch (Exception ignored) {}
    }

    /** The connected desktop's name, or null. */
    static String connectedDesktop() {
        Link l = self;
        if (l != null) for (Conn c : l.conns.values()) if (c.connected()) return c.name;
        return null;
    }

    /** What each desktop's link is doing, by id, for the app's screen. */
    static Map<String, String> statuses() {
        Map<String, String> m = new HashMap<>();
        Link l = self;
        if (l != null) for (Conn c : l.conns.values()) m.put(c.id, c.status);
        return m;
    }

    @Override public void onListenerConnected() {
        self = this;
        try { registerReceiver(unlocked, new IntentFilter(Intent.ACTION_USER_PRESENT)); } catch (Exception ignored) {}
        watchCalendar();
        // Chats that arrived while the listener was away, still in the shade.
        pool.execute(() -> {
            StatusBarNotification[] shown = getActiveNotifications();
            if (shown != null) for (StatusBarNotification n : shown) Chats.record(this, n);
        });
        sync();
    }

    @Override public void onListenerDisconnected() {
        stopAll();
        try { unregisterReceiver(unlocked); } catch (Exception ignored) {}
        getContentResolver().unregisterContentObserver(calendarObserver);
    }
    @Override public void onDestroy() { stopAll(); super.onDestroy(); }

    private void stopAll() {
        for (Conn c : conns.values()) c.stop();
        conns.clear();
    }

    /** Starts the link to the desktop in use, and stops any other. */
    synchronized void sync() {
        JSONArray list = pairs(this);
        String active = active(this);
        Map<String, JSONObject> wanted = new HashMap<>();
        for (int i = 0; i < list.length(); i++) {
            JSONObject p = list.optJSONObject(i);
            if (p != null && p.optString("id").equals(active)) wanted.put(active, p);
        }
        for (Conn c : conns.values()) {
            JSONObject p = wanted.get(c.id);
            if (p == null || !p.optString("token").equals(c.token)) {
                c.stop();
                conns.remove(c.id);
            }
        }
        for (JSONObject p : wanted.values()) {
            if (!conns.containsKey(p.optString("id"))) {
                Conn c = new Conn(p);
                conns.put(c.id, c);
                new Thread(c::loop, "nexus-link-" + c.id).start();
            }
        }
    }

    /** The link to one desktop: a TCP connection, newline-delimited JSON both ways. */
    final class Conn {
        final String id, host, token, name;
        final int port;
        volatile String status = "Connecting…";
        private volatile boolean running = true;
        private long backoff = 2000;
        private volatile Socket socket;
        private volatile OutputStream out;
        private final Object wake = new Object();
        /** Notifications this desktop has, by key, with their post time. */
        final Map<String, Long> sent = new HashMap<>();
        /** The question was asked on this connection. */
        volatile boolean asked;

        Conn(JSONObject p) {
            id = p.optString("id");
            host = p.optString("host");
            port = p.optInt("port", 47913);
            token = p.optString("token");
            name = p.optString("name", host);
        }

        void stop() {
            running = false;
            Socket s = socket;
            if (s != null) try { s.close(); } catch (Exception ignored) {}
            synchronized (wake) { wake.notifyAll(); }
        }

        /**
         * The desktop is locked and shows nothing until it unlocks; or it let
         * go of this phone, or another one took its place: it waits for a tap.
         */
        private void bye(JSONObject m) {
            switch (m.optString("reason")) {
                case "released": status = "Disconnected · tap to reconnect"; running = false; break;
                case "replaced": status = "Another phone took over · tap to reconnect"; running = false; break;
                default: status = "Locked"; backoff = 20000;
            }
        }

        private boolean waiting() {
            return status.equals("Locked") || status.startsWith("Refused") || !running;
        }

        void loop() {
            while (running) {
                try (Socket s = new Socket()) {
                    s.connect(new InetSocketAddress(host, port), 10000);
                    s.setSoTimeout(90000);
                    s.setKeepAlive(true);
                    out = s.getOutputStream();
                    socket = s;
                    BufferedReader in = new BufferedReader(new InputStreamReader(s.getInputStream(), StandardCharsets.UTF_8));
                    send(new JSONObject().put("t", "hello").put("v", 1).put("token", token)
                            .put("name", Build.MANUFACTURER + " " + Build.MODEL));
                    JSONObject welcome = new JSONObject(String.valueOf(in.readLine()));
                    if (welcome.optString("t").equals("bye")) {
                        bye(welcome);
                        throw new Exception("bye");
                    }
                    if (!welcome.optString("t").equals("welcome")) {
                        status = "Refused: scan the code again";
                        backoff = 60000;
                        throw new Exception("refused");
                    }
                    status = "Connected";
                    backoff = 2000;
                    asked = false;
                    // Paired from a pasted link, it knew only the address.
                    String desktop = welcome.optString("name");
                    if (!desktop.isEmpty() && name.equals(host)) rename(Link.this, id, desktop);
                    synchronized (sent) { sent.clear(); }
                    send(info());
                    StatusBarNotification[] active = getActiveNotifications();
                    if (active != null) for (StatusBarNotification n : active) post(this, n);
                    String line;
                    while ((line = in.readLine()) != null) {
                        JSONObject m = new JSONObject(line);
                        String t = m.optString("t");
                        if (t.equals("ping")) {
                            pool.execute(() -> { try { send(info().put("t", "pong")); } catch (Exception ignored) {} });
                        } else if (t.equals("clip")) {
                            String text = m.optString("text");
                            pool.execute(() -> { try { setClip(text); } catch (Exception ignored) {} });
                        } else if (t.equals("notes_state")) {
                            notesState = m.optJSONObject("state");
                            notesDesktop = name;
                        } else if (t.equals("req")) {
                            pool.execute(() -> respond(m));
                        } else if (t.equals("bye")) {
                            bye(m);
                            break;
                        }
                    }
                } catch (Exception e) {
                    if (status.equals("Connected")) status = "Offline, retrying…";
                    else if (!waiting()) status = "Offline";
                } finally {
                    socket = null;
                }
                if (running) pause(backoff);
                if (!status.equals("Locked")) backoff = Math.min(backoff * 2, 60000);
            }
        }

        private void pause(long ms) {
            synchronized (wake) { try { wake.wait(ms); } catch (InterruptedException ignored) {} }
        }

        boolean connected() { return socket != null && status.equals("Connected"); }

        void send(JSONObject o) throws Exception {
            byte[] line = (o.toString() + "\n").getBytes(StandardCharsets.UTF_8);
            synchronized (this) {
                if (socket == null) throw new Exception("not connected");
                out.write(line);
                out.flush();
            }
        }

        private void respond(JSONObject m) {
            JSONObject r = new JSONObject();
            try {
                r.put("t", "res").put("id", m.getLong("id"));
                try {
                    r.put("ok", true).put("data", op(this, m.getString("op"), m));
                } catch (Throwable e) {
                    r.put("ok", false).put("error", e.getMessage() != null ? e.getMessage() : e.toString());
                    r.remove("data");
                }
                send(r);
            } catch (Exception ignored) {}
        }
    }

    private static final java.util.List<String> FILE_OPS = Arrays.asList("ls", "read", "write", "mkdir", "delete", "rename", "thumb");

    /** Inside this app's private storage: where the vault and the consent live. */
    private boolean secret(String path) throws Exception {
        if (path.isEmpty()) return false;
        String dir = getDataDir().getCanonicalPath(), p = new File(path).getCanonicalPath();
        return p.equals(dir) || p.startsWith(dir + "/");
    }

    /** Sync-consent notification, once per connection; never a yes on the notification itself. */
    private void askConsent(Conn c) {
        NotificationManager nm = getSystemService(NotificationManager.class);
        nm.createNotificationChannel(new android.app.NotificationChannel("vault", "Password sync", NotificationManager.IMPORTANCE_DEFAULT));
        Intent open = new Intent(this, Vault.class).putExtra("ask", c.id).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        nm.notify(c.id, CONSENT_NOTIFICATION, new Notification.Builder(this, "vault").setSmallIcon(R.drawable.ic_fg)
                .setContentTitle("Sync passwords with " + c.name + "?")
                .setContentText("Tap to choose. Nothing syncs until you say yes.")
                .setAutoCancel(true)
                .setContentIntent(android.app.PendingIntent.getActivity(this, c.id.hashCode(), open, android.app.PendingIntent.FLAG_IMMUTABLE | android.app.PendingIntent.FLAG_UPDATE_CURRENT))
                .build());
    }

    private JSONObject vaultOp(Conn c, String op, JSONObject a) throws Exception {
        String consent = consent(this, c.id);
        VaultStore store = VaultStore.get(this);
        if (op.equals("vault_hello")) {
            if (consent.equals("ask") && !c.asked) {
                c.asked = true;
                askConsent(c);
            }
            String id = store.id();
            return new JSONObject().put("consent", consent).put("id", id == null ? JSONObject.NULL : id);
        }
        if (!consent.equals("yes")) throw new Exception("Password sync isn't allowed for this desktop");
        if (op.equals("vault_pull")) {
            JSONObject file = store.file();
            return new JSONObject().put("vault", file == null ? JSONObject.NULL : file);
        }
        store.push(a.getJSONObject("vault"), a.optBoolean("adopt"));
        return new JSONObject();
    }

    private JSONObject op(Conn c, String op, JSONObject a) throws Exception {
        // The desktop browses shared storage, never this app's: consent couldn't hold otherwise.
        if (FILE_OPS.contains(op) && (secret(a.optString("path")) || secret(a.optString("to")))) throw new Exception("That folder is private to Nexus");
        switch (op) {
            case "vault_hello": case "vault_pull": case "vault_push": return vaultOp(c, op, a);
            case "ls": return ls(a.optString("path"));
            case "read": {
                try (RandomAccessFile f = new RandomAccessFile(a.getString("path"), "r")) {
                    int len = (int) Math.min(a.optLong("len", 1 << 20), 1 << 20);
                    byte[] buf = new byte[len];
                    f.seek(a.optLong("offset"));
                    int n = 0, k;
                    while (n < len && (k = f.read(buf, n, len - n)) > 0) n += k;
                    return new JSONObject().put("b64", Base64.encodeToString(buf, 0, n, Base64.NO_WRAP)).put("size", f.length());
                }
            }
            case "write": {
                File file = new File(a.getString("path"));
                long offset = a.optLong("offset");
                try (RandomAccessFile f = new RandomAccessFile(file, "rw")) {
                    if (offset == 0) f.setLength(0);
                    f.seek(offset);
                    f.write(Base64.decode(a.optString("b64"), Base64.NO_WRAP));
                }
                return new JSONObject();
            }
            case "mkdir":
                if (!new File(a.getString("path")).mkdirs()) throw new Exception("Could not create the folder");
                return new JSONObject();
            case "delete":
                if (!delete(new File(a.getString("path")))) throw new Exception("Could not delete");
                return new JSONObject();
            case "rename":
                if (!new File(a.getString("path")).renameTo(new File(a.getString("to")))) throw new Exception("Could not rename");
                return new JSONObject();
            case "thumb": return thumb(a.getString("path"), a.optInt("size", 256));
            case "notes_list": return notes(this).list();
            case "notes_read": return notes(this).read(a.getString("path"), a.optLong("offset"), a.optInt("len", NotesStore.CHUNK));
            case "notes_write": notes(this).write(a.getString("path"), a.optLong("offset"), a.optString("b64")); return new JSONObject();
            case "notes_commit": notes(this).commit(a.getString("path"), a.getString("hash"), a.optString("expect")); return new JSONObject();
            case "notes_trash": notes(this).trash(a.getString("path"), a.getString("expect"), a.getString("stamp")); return new JSONObject();
            case "notes_move": notes(this).move(a.getString("path"), a.getString("to"), a.getString("expect")); return new JSONObject();
            case "calendar_list": return Agenda.list(this, a.getLong("from"), a.getLong("to"));
            case "calendar_save": Agenda.save(this, Agenda.Item.from(a.getJSONObject("event"))); return new JSONObject();
            case "calendar_delete": Agenda.delete(this, Agenda.Item.from(a.getJSONObject("event"))); return new JSONObject();
            case "info": return info();
            case "sms_threads": return threads(a.optString("stamp"));
            case "sms_list": return messages(a.getLong("thread"), a.optString("name"), a.optLong("since"));
            case "call": return placeCall(a.getString("to"));
            case "call_end":
                if (!getSystemService(TelecomManager.class).endCall()) throw new Exception("No call to hang up");
                return new JSONObject();
            case "call_state": {
                int s = getSystemService(TelephonyManager.class).getCallState();
                return new JSONObject().put("state", s == TelephonyManager.CALL_STATE_IDLE ? "idle" : s == TelephonyManager.CALL_STATE_RINGING ? "ringing" : "active");
            }
            case "screen_shot": case "screen_tree": case "screen_tap": case "screen_hold": case "screen_swipe":
            case "screen_text": case "screen_key": case "screen_apps": case "screen_open": case "cast_start":
                return Control.op(op, a);
            case "sms_send": {
                SmsManager sms = Build.VERSION.SDK_INT >= 31 ? getSystemService(SmsManager.class) : SmsManager.getDefault();
                sms.sendMultipartTextMessage(a.getString("to"), null, sms.divideMessage(a.getString("body")), null, null);
                return new JSONObject();
            }
            case "dismiss":
                cancelNotification(a.getString("key"));
                return new JSONObject();
            case "mms_put": case "mms_send": case "mms_read": return Mms.op(this, op, a);
            default: throw new Exception("unknown op");
        }
    }

    private static boolean delete(File f) {
        File[] children = f.isDirectory() ? f.listFiles() : null;
        if (children != null) for (File c : children) delete(c);
        return f.delete();
    }

    private JSONObject ls(String path) throws Exception {
        File dir = (path.isEmpty() ? Environment.getExternalStorageDirectory() : new File(path)).getCanonicalFile();
        File[] files = dir.listFiles();
        if (files == null) throw new Exception("Cannot read " + dir + ": grant All files access in the Nexus app");
        Arrays.sort(files, (x, y) -> x.isDirectory() != y.isDirectory()
                ? (x.isDirectory() ? -1 : 1) : x.getName().compareToIgnoreCase(y.getName()));
        JSONArray entries = new JSONArray();
        for (File f : files) {
            entries.put(new JSONObject().put("name", f.getName()).put("dir", f.isDirectory())
                    .put("size", f.length()).put("mtime", f.lastModified() / 1000));
        }
        return new JSONObject().put("path", dir.getPath()).put("entries", entries);
    }

    private JSONObject thumb(String path, int size) throws Exception {
        if (Build.VERSION.SDK_INT < 29) throw new Exception("Thumbnails need Android 10");
        File f = new File(path);
        Size s = new Size(size, size);
        String name = f.getName().toLowerCase();
        Bitmap b = name.matches(".*\\.(mp4|mkv|webm|mov|3gp|avi)$")
                ? ThumbnailUtils.createVideoThumbnail(f, s, null)
                : ThumbnailUtils.createImageThumbnail(f, s, null);
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        b.compress(Bitmap.CompressFormat.JPEG, 80, o);
        return new JSONObject().put("b64", Base64.encodeToString(o.toByteArray(), Base64.NO_WRAP));
    }

    /** What the desktop's Phone page shows. */
    private JSONObject info() throws Exception {
        JSONObject j = new JSONObject();
        j.put("model", Build.MANUFACTURER + " " + Build.MODEL).put("android", Build.VERSION.RELEASE)
                .put("uptime", SystemClock.elapsedRealtime() / 1000);
        Intent bat = registerReceiver(null, new IntentFilter(Intent.ACTION_BATTERY_CHANGED));
        if (bat != null) {
            int level = bat.getIntExtra(BatteryManager.EXTRA_LEVEL, -1), scale = bat.getIntExtra(BatteryManager.EXTRA_SCALE, 100);
            int st = bat.getIntExtra(BatteryManager.EXTRA_STATUS, 0);
            j.put("battery", new JSONObject().put("level", level * 100 / Math.max(scale, 1))
                    .put("charging", st == BatteryManager.BATTERY_STATUS_CHARGING || st == BatteryManager.BATTERY_STATUS_FULL)
                    .put("temp", bat.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, 0) / 10.0));
        }
        StatFs fs = new StatFs(Environment.getExternalStorageDirectory().getPath());
        j.put("storage", new JSONObject().put("total", fs.getTotalBytes()).put("free", fs.getAvailableBytes()));
        ActivityManager.MemoryInfo mem = new ActivityManager.MemoryInfo();
        getSystemService(ActivityManager.class).getMemoryInfo(mem);
        j.put("ram", new JSONObject().put("total", mem.totalMem).put("free", mem.availMem));
        JSONObject cpu = new JSONObject().put("cores", Runtime.getRuntime().availableProcessors());
        if (Build.VERSION.SDK_INT >= 31) cpu.put("soc", Build.SOC_MANUFACTURER + " " + Build.SOC_MODEL);
        long cur = 0, max = 0;
        for (int i = 0; i < 16; i++) {
            cur = Math.max(cur, readLong("/sys/devices/system/cpu/cpu" + i + "/cpufreq/scaling_cur_freq"));
            max = Math.max(max, readLong("/sys/devices/system/cpu/cpu" + i + "/cpufreq/cpuinfo_max_freq"));
        }
        if (max > 0) cpu.put("mhz", cur / 1000).put("max_mhz", max / 1000);
        j.put("cpu", cpu);
        ConnectivityManager cm = getSystemService(ConnectivityManager.class);
        NetworkCapabilities nc = null;
        try { nc = cm.getNetworkCapabilities(cm.getActiveNetwork()); } catch (Exception ignored) {}
        j.put("network", nc == null ? "Offline"
                : nc.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) ? "Wi-Fi"
                : nc.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) ? "Mobile data" : "Online");
        j.put("permissions", new JSONObject()
                .put("read_sms", checkSelfPermission(android.Manifest.permission.READ_SMS) == PackageManager.PERMISSION_GRANTED)
                .put("send_sms", checkSelfPermission(android.Manifest.permission.SEND_SMS) == PackageManager.PERMISSION_GRANTED)
                .put("contacts", checkSelfPermission(android.Manifest.permission.READ_CONTACTS) == PackageManager.PERMISSION_GRANTED));
        return new JSONObject().put("t", "info").put("info", j);
    }

    private static long readLong(String path) {
        try (BufferedReader r = new BufferedReader(new java.io.FileReader(path))) {
            return Long.parseLong(r.readLine().trim());
        } catch (Exception e) { return 0; }
    }

    /** The newest first, from SMS, MMS and the chats kept from notifications. */
    /** Numbers only: `tel:` would also run carrier codes such as call forwarding. */
    private JSONObject placeCall(String to) throws Exception {
        if (!to.matches("\\+?[0-9 ().-]{2,32}")) throw new Exception("That isn't a phone number");
        if (checkSelfPermission(android.Manifest.permission.CALL_PHONE) != PackageManager.PERMISSION_GRANTED)
            throw new Exception("Open Nexus on the phone and allow phone calls");
        getSystemService(TelecomManager.class).placeCall(Uri.fromParts("tel", to.replaceAll("[ ().-]", ""), null), null);
        return new JSONObject();
    }

    private int rows(Uri uri, String where, String... args) {
        try (Cursor c = getContentResolver().query(uri, new String[]{"_id"}, where, args, null)) {
            return c == null ? 0 : c.getCount();
        } catch (Exception e) { return 0; }
    }

    private long newest(Uri uri) {
        try (Cursor c = getContentResolver().query(uri, new String[]{"date"}, null, null, "date DESC LIMIT 1")) {
            return c != null && c.moveToFirst() ? c.getLong(0) : 0;
        } catch (Exception e) { return 0; }
    }

    /** Changes whenever the conversation list could. */
    private String stamp() {
        JSONArray chats = Chats.snapshot(this);
        long chat = 0;
        for (int i = 0; i < chats.length(); i++) chat = Math.max(chat, chats.optJSONObject(i).optLong("time"));
        return newest(Telephony.Sms.CONTENT_URI) + ":" + newest(Telephony.Mms.CONTENT_URI) + ":" + rows(Telephony.Sms.CONTENT_URI, null)
                + ":" + rows(Telephony.Mms.CONTENT_URI, null) + ":" + chats.length() + ":" + chat;
    }

    /** The conversations, or just `same` when the desktop's `known` list is current. */
    private JSONObject threads(String known) throws Exception {
        String stamp = stamp();
        if (stamp.equals(known)) return new JSONObject().put("same", true);
        Map<Long, JSONObject> byThread = new HashMap<>();
        try (Cursor c = getContentResolver().query(Telephony.Sms.CONTENT_URI,
                new String[]{"thread_id", "address", "body", "date"}, null, null, "date DESC")) {
            while (c != null && c.moveToNext() && byThread.size() < 60) {
                offer(byThread, c.getLong(0), c.getString(1), c.getString(2), c.getLong(3));
            }
        }
        try (Cursor c = getContentResolver().query(Telephony.Mms.CONTENT_URI,
                new String[]{"_id", "thread_id", "date"}, null, null, "date DESC LIMIT 200")) {
            while (c != null && c.moveToNext()) {
                long id = c.getLong(0), thread = c.getLong(1), date = c.getLong(2) * 1000;
                JSONObject t = byThread.get(thread);
                if (t != null && t.optLong("date") >= date) continue;
                String text = mmsText(id);
                offer(byThread, thread, t != null ? t.optString("address") : mmsAddress(id), text.isEmpty() ? "Attachment" : text, date);
            }
        } catch (Exception ignored) {}
        Map<String, JSONObject> byName = new HashMap<>();
        for (JSONObject t : byThread.values()) {
            String name = contact(t.optString("address"));
            t.put("name", name);
            byName.put(name.isEmpty() ? t.optString("address") : name, t);
        }
        JSONArray chats = Chats.snapshot(this);
        for (int i = 0; i < chats.length(); i++) {
            JSONObject m = chats.getJSONObject(i);
            String conv = m.optString("conv");
            JSONObject t = byName.get(conv);
            if (t == null) {
                // A chat without SMS: no number to answer to.
                t = new JSONObject().put("thread", -1L - (conv.hashCode() & 0x7fffffffL)).put("address", "").put("name", conv).put("date", 0L);
                byName.put(conv, t);
                byThread.put(t.getLong("thread"), t);
            }
            if (m.optLong("time") >= t.optLong("date")) t.put("snippet", m.optString("text")).put("date", m.optLong("time"));
        }
        java.util.List<JSONObject> sorted = new java.util.ArrayList<>(byThread.values());
        sorted.sort((a, b) -> Long.compare(b.optLong("date"), a.optLong("date")));
        JSONArray list = new JSONArray();
        for (JSONObject t : sorted.subList(0, Math.min(60, sorted.size()))) list.put(t);
        return new JSONObject().put("threads", list).put("stamp", stamp);
    }

    private static void offer(Map<Long, JSONObject> byThread, long thread, String address, String snippet, long date) {
        JSONObject t = byThread.get(thread);
        if (t != null && t.optLong("date") >= date) return;
        try {
            byThread.put(thread, new JSONObject().put("thread", thread).put("address", address == null ? "" : address)
                    .put("snippet", snippet == null ? "" : snippet).put("date", date));
        } catch (Exception ignored) {}
    }

    /** An MMS's sender, or for one this phone sent, its first recipient. */
    private String mmsAddress(long id) {
        try (Cursor c = getContentResolver().query(Uri.parse("content://mms/" + id + "/addr"),
                new String[]{"address", "type"}, null, null, null)) {
            String to = "";
            while (c != null && c.moveToNext()) {
                String a = c.getString(0);
                if (a == null || a.equals("insert-address-token")) continue;
                if (c.getInt(1) == 137) return a;
                if (to.isEmpty()) to = a;
            }
            return to;
        } catch (Exception e) { return ""; }
    }

    private String mmsText(long id) {
        StringBuilder text = new StringBuilder();
        try (Cursor c = getContentResolver().query(Uri.parse("content://mms/part"),
                new String[]{"text"}, "mid = ? AND ct = 'text/plain'", new String[]{String.valueOf(id)}, null)) {
            while (c != null && c.moveToNext()) if (c.getString(0) != null) text.append(c.getString(0));
        } catch (Exception ignored) {}
        return text.toString();
    }

    private String contact(String address) {
        if (address.isEmpty() || checkSelfPermission(android.Manifest.permission.READ_CONTACTS) != PackageManager.PERMISSION_GRANTED) return "";
        Uri uri = Uri.withAppendedPath(ContactsContract.PhoneLookup.CONTENT_FILTER_URI, Uri.encode(address));
        try (Cursor c = getContentResolver().query(uri, new String[]{ContactsContract.PhoneLookup.DISPLAY_NAME}, null, null, null)) {
            return c != null && c.moveToFirst() ? c.getString(0) : "";
        } catch (Exception e) { return ""; }
    }

    /**
     * A conversation, oldest first: SMS, MMS, then chats kept under `name`,
     * from `since` on. Looks 90 s further back, to spot the duplicates below.
     * `count` lets the desktop notice messages deleted here.
     */
    private JSONObject messages(long thread, String name, long since) throws Exception {
        java.util.List<JSONObject> all = new java.util.ArrayList<>();
        long from = since - 90000;
        String id = String.valueOf(thread);
        if (thread > 0) {
            try (Cursor c = getContentResolver().query(Telephony.Sms.CONTENT_URI, new String[]{"body", "date", "type", "_id"},
                    "thread_id = ? AND date >= ?", new String[]{id, String.valueOf(from)}, "date DESC LIMIT 200")) {
                while (c != null && c.moveToNext()) {
                    all.add(new JSONObject().put("id", "s" + c.getLong(3)).put("body", c.getString(0)).put("date", c.getLong(1))
                            .put("out", c.getInt(2) != Telephony.Sms.MESSAGE_TYPE_INBOX).put("status", Mms.smsStatus(c.getInt(2))));
                }
            }
            try (Cursor c = getContentResolver().query(Telephony.Mms.CONTENT_URI, new String[]{"_id", "date", "msg_box"},
                    "thread_id = ? AND date >= ?", new String[]{id, String.valueOf(from / 1000)}, "date DESC LIMIT 100")) {
                while (c != null && c.moveToNext()) {
                    all.add(new JSONObject().put("id", "m" + c.getLong(0)).put("body", mmsText(c.getLong(0))).put("date", c.getLong(1) * 1000)
                            .put("out", c.getInt(2) != Telephony.Mms.MESSAGE_BOX_INBOX).put("status", Mms.mmsStatus(c.getInt(2)))
                            .put("parts", Mms.parts(this, c.getLong(0))));
                }
            } catch (Exception ignored) {}
        }
        if (!name.isEmpty()) {
            JSONArray chats = Chats.snapshot(this);
            for (int i = 0; i < chats.length(); i++) {
                JSONObject m = chats.getJSONObject(i);
                if (!m.optString("conv").equals(name) || m.optLong("time") < from) continue;
                all.add(new JSONObject().put("id", "c" + m.optLong("time")).put("body", m.optString("text")).put("date", m.optLong("time")).put("out", m.optBoolean("out")));
            }
        }
        all.sort((a, b) -> Long.compare(a.optLong("date"), b.optLong("date")));
        // A chat message may also be in the SMS store: the same text within a minute and a half.
        JSONArray list = new JSONArray();
        JSONObject last = null;
        for (JSONObject m : all) {
            if (last != null && !m.optString("body").isEmpty() && last.optString("body").equals(m.optString("body"))
                    && m.optLong("date") - last.optLong("date") < 90000) continue;
            if (m.optLong("date") >= since) list.put(m);
            last = m;
        }
        JSONArray newest = new JSONArray();
        for (int i = Math.max(0, list.length() - 300); i < list.length(); i++) newest.put(list.get(i));
        int count = thread <= 0 ? 0 : rows(Telephony.Sms.CONTENT_URI, "thread_id = ?", id) + rows(Telephony.Mms.CONTENT_URI, "thread_id = ?", id);
        return new JSONObject().put("messages", newest).put("count", count);
    }

    @Override public void onNotificationPosted(StatusBarNotification n) {
        pool.execute(() -> Chats.record(this, n));
        for (Conn c : conns.values()) if (c.connected()) pool.execute(() -> post(c, n));
    }

    @Override public void onNotificationRemoved(StatusBarNotification n) {
        for (Conn c : conns.values()) {
            synchronized (c.sent) { if (c.sent.remove(n.getKey()) == null) continue; }
            pool.execute(() -> { try { c.send(new JSONObject().put("t", "notif_gone").put("key", n.getKey())); } catch (Exception ignored) {} });
        }
    }

    private void post(Conn c, StatusBarNotification n) {
        try {
            Notification no = n.getNotification();
            int skip = Notification.FLAG_ONGOING_EVENT | Notification.FLAG_FOREGROUND_SERVICE | Notification.FLAG_GROUP_SUMMARY;
            if ((no.flags & skip) != 0 || n.getPackageName().equals(getPackageName())) return;
            // Only what the phone itself shows: not silent or minimised ones, nor
            // what Do not disturb holds back or a suspended app posts.
            Ranking r = new Ranking();
            RankingMap ranking = getCurrentRanking();
            if (ranking != null && ranking.getRanking(n.getKey(), r)
                    && (r.getImportance() < NotificationManager.IMPORTANCE_DEFAULT || !r.matchesInterruptionFilter()
                    || (Build.VERSION.SDK_INT >= 28 && r.isSuspended()))) return;
            Bundle e = no.extras;
            CharSequence title = e.getCharSequence(Notification.EXTRA_TITLE);
            CharSequence text = e.getCharSequence(Notification.EXTRA_BIG_TEXT);
            if (text == null) text = e.getCharSequence(Notification.EXTRA_TEXT);
            if (title == null && text == null) return;
            synchronized (c.sent) {
                Long was = c.sent.get(n.getKey());
                if (was != null && was == n.getPostTime()) return;
                c.sent.put(n.getKey(), n.getPostTime());
            }
            String pkg = n.getPackageName();
            PackageManager pm = getPackageManager();
            String app = pkg;
            try { app = pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)).toString(); } catch (Exception ignored) {}
            JSONObject o = new JSONObject().put("t", "notif").put("key", n.getKey()).put("app", app).put("pkg", pkg)
                    .put("title", title == null ? "" : title.toString()).put("text", text == null ? "" : text.toString())
                    .put("time", n.getPostTime());
            String icon = icon(pkg);
            if (icon != null) o.put("icon", icon);
            c.send(o);
        } catch (Exception ignored) {}
    }

    /** The app's icon as a 64 px PNG, once per package. */
    private String icon(String pkg) {
        synchronized (icons) {
            if (icons.containsKey(pkg)) return icons.get(pkg);
        }
        String b64 = null;
        try {
            Drawable d = getPackageManager().getApplicationIcon(pkg);
            Bitmap b = Bitmap.createBitmap(64, 64, Bitmap.Config.ARGB_8888);
            d.setBounds(0, 0, 64, 64);
            d.draw(new Canvas(b));
            ByteArrayOutputStream o = new ByteArrayOutputStream();
            b.compress(Bitmap.CompressFormat.PNG, 100, o);
            b64 = Base64.encodeToString(o.toByteArray(), Base64.NO_WRAP);
        } catch (Exception ignored) {}
        synchronized (icons) { icons.put(pkg, b64); }
        return b64;
    }
}
