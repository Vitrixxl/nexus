package io.github.vitrixxl.nexus;

import android.app.ActivityManager;
import android.app.Notification;
import android.app.NotificationManager;
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
import android.os.StatFs;
import android.os.SystemClock;
import android.provider.ContactsContract;
import android.provider.Telephony;
import android.service.notification.NotificationListenerService;
import android.service.notification.StatusBarNotification;
import android.telephony.SmsManager;
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

    /** The paired desktops: [{id, host, port, token, name}]. */
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

    static void savePairs(Context c, JSONArray list) {
        c.getSharedPreferences("pair", MODE_PRIVATE).edit().putString("list", list.toString()).apply();
        Link l = self;
        if (l != null) l.sync();
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
        sync();
    }

    @Override public void onListenerDisconnected() { stopAll(); }
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
                    r.put("ok", true).put("data", op(m.getString("op"), m));
                } catch (Throwable e) {
                    r.put("ok", false).put("error", e.getMessage() != null ? e.getMessage() : e.toString());
                    r.remove("data");
                }
                send(r);
            } catch (Exception ignored) {}
        }
    }

    private JSONObject op(String op, JSONObject a) throws Exception {
        switch (op) {
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
            case "info": return info();
            case "sms_threads": return threads();
            case "sms_list": return messages(a.getLong("thread"));
            case "sms_send": {
                SmsManager sms = Build.VERSION.SDK_INT >= 31 ? getSystemService(SmsManager.class) : SmsManager.getDefault();
                sms.sendMultipartTextMessage(a.getString("to"), null, sms.divideMessage(a.getString("body")), null, null);
                return new JSONObject();
            }
            case "dismiss":
                cancelNotification(a.getString("key"));
                return new JSONObject();
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

    private JSONObject threads() throws Exception {
        Map<Long, JSONObject> byThread = new LinkedHashMap<>();
        try (Cursor c = getContentResolver().query(Telephony.Sms.CONTENT_URI,
                new String[]{"thread_id", "address", "body", "date"}, null, null, "date DESC")) {
            while (c != null && c.moveToNext() && byThread.size() < 50) {
                long thread = c.getLong(0);
                if (byThread.containsKey(thread)) continue;
                String address = c.getString(1) == null ? "" : c.getString(1);
                byThread.put(thread, new JSONObject().put("thread", thread).put("address", address)
                        .put("name", contact(address)).put("snippet", c.getString(2)).put("date", c.getLong(3)));
            }
        }
        JSONArray list = new JSONArray();
        for (JSONObject t : byThread.values()) list.put(t);
        return new JSONObject().put("threads", list);
    }

    private String contact(String address) {
        if (address.isEmpty() || checkSelfPermission(android.Manifest.permission.READ_CONTACTS) != PackageManager.PERMISSION_GRANTED) return "";
        Uri uri = Uri.withAppendedPath(ContactsContract.PhoneLookup.CONTENT_FILTER_URI, Uri.encode(address));
        try (Cursor c = getContentResolver().query(uri, new String[]{ContactsContract.PhoneLookup.DISPLAY_NAME}, null, null, null)) {
            return c != null && c.moveToFirst() ? c.getString(0) : "";
        } catch (Exception e) { return ""; }
    }

    private JSONObject messages(long thread) throws Exception {
        JSONArray list = new JSONArray();
        try (Cursor c = getContentResolver().query(Telephony.Sms.CONTENT_URI, new String[]{"body", "date", "type"},
                "thread_id = ?", new String[]{String.valueOf(thread)}, "date DESC LIMIT 100")) {
            while (c != null && c.moveToNext()) {
                list.put(new JSONObject().put("body", c.getString(0)).put("date", c.getLong(1))
                        .put("out", c.getInt(2) != Telephony.Sms.MESSAGE_TYPE_INBOX));
            }
        }
        JSONArray oldestFirst = new JSONArray();
        for (int i = list.length() - 1; i >= 0; i--) oldestFirst.put(list.get(i));
        return new JSONObject().put("messages", oldestFirst);
    }

    @Override public void onNotificationPosted(StatusBarNotification n) {
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
