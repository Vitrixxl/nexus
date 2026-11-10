package io.github.vitrixxl.nexus;

import android.content.Context;
import android.os.Build;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.system.Os;
import android.util.Base64;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.net.URI;
import java.nio.CharBuffer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.security.GeneralSecurityException;
import java.security.SecureRandom;
import java.text.Normalizer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Iterator;
import java.util.List;
import java.util.Set;

import javax.crypto.BadPaddingException;
import javax.crypto.Cipher;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.PBEKeySpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * vault.json and its key, as VAULT-SPEC.md binds them. The merge works on the
 * file alone, so a locked phone still syncs. Decrypted entries are never cached here.
 */
final class VaultStore {
    static final int ITERS = 600000, MIN_ITERS = 100000;
    private static final int MAX_ITERS = 10000000;
    private static final long IDLE_MS = 5 * 60_000;
    private static final SecureRandom RANDOM = new SecureRandom();
    private static final String HEX32 = "[0-9a-f]{32}";
    /** Run after a change made here, not one a desktop pushed: {@link Link} tells the desktop. */
    static volatile Runnable changed;

    static final class WrongPassword extends Exception {}

    static final class Entry {
        String id, title = "", username = "", password = "", notes = "";
        List<String> urls = new ArrayList<>();
        JSONArray history = new JSONArray();
        long updated;
        /** Fields a newer version wrote, kept on save. */
        JSONObject raw = new JSONObject();
    }

    private static VaultStore shared;

    static synchronized VaultStore get(Context c) {
        if (shared == null) shared = new VaultStore(new File(c.getApplicationContext().getFilesDir(), "vault.json"), Build.MANUFACTURER + " " + Build.MODEL);
        return shared;
    }

    private final File file;
    private final String device;
    private byte[] vk;
    private long lastUsed;
    /** Bumps with every write, so screens know to read again. */
    volatile long generation;
    private final Handler timer = Looper.getMainLooper() == null ? null : new Handler(Looper.getMainLooper());
    /** ponytail: the timer sleeps with the phone; unlocked() checks the real clock, so a sleeping phone locks on its next use. An AlarmManager would wipe it during sleep too. */
    private final Runnable expire = () -> { if (unlocked()) schedule(); };

    VaultStore(File file, String device) { this.file = file; this.device = device; }

    boolean exists() { return file.exists(); }

    synchronized String id() {
        try { return load().getString("id"); } catch (Exception e) { return null; }
    }

    /** Also locks if the idle time has run out. */
    synchronized boolean unlocked() {
        if (vk != null && SystemClock.elapsedRealtime() - lastUsed >= IDLE_MS) lock();
        return vk != null;
    }

    synchronized void lock() {
        if (vk != null) Arrays.fill(vk, (byte) 0);
        vk = null;
        if (timer != null) timer.removeCallbacks(expire);
    }

    synchronized void touch() {
        if (vk == null) return;
        lastUsed = SystemClock.elapsedRealtime();
        schedule();
    }

    private void schedule() {
        if (timer == null) return;
        timer.removeCallbacks(expire);
        timer.postDelayed(expire, IDLE_MS);
    }

    private byte[] key() throws Exception {
        if (!unlocked()) throw new Exception("The vault is locked");
        touch();
        return vk;
    }

    /** A copy for the fingerprint wrap; the caller wipes it. */
    synchronized byte[] keyCopy() throws Exception { return key().clone(); }

    /** The key, unwrapped by the fingerprint. */
    synchronized void unlockWith(byte[] key) throws Exception {
        if (key.length != 32) throw new Exception("Fingerprint unlock didn't return a valid key");
        lock();
        vk = key.clone();
        touch();
    }

    // Crypto ----------------------------------------------------------------

    private static byte[] b64(String s) { return Base64.decode(s, Base64.NO_WRAP); }
    private static String b64(byte[] b) { return Base64.encodeToString(b, Base64.NO_WRAP); }

    private static byte[] random(int n) {
        byte[] b = new byte[n];
        RANDOM.nextBytes(b);
        return b;
    }

    private static String randomHex() {
        StringBuilder s = new StringBuilder(32);
        for (byte x : random(16)) s.append(String.format("%02x", x));
        return s.toString();
    }

    private static Cipher gcm(int mode, byte[] key, byte[] nonce, String aad) throws GeneralSecurityException {
        Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
        c.init(mode, new SecretKeySpec(key, "AES"), new GCMParameterSpec(128, nonce));
        c.updateAAD(aad.getBytes(StandardCharsets.UTF_8));
        return c;
    }

    /** Password as NFC, then UTF-8 inside PBKDF2. Strings can't be wiped: as good as Java gets. */
    static byte[] derive(char[] password, byte[] salt, int iters) throws GeneralSecurityException {
        char[] nfc = Normalizer.normalize(CharBuffer.wrap(password), Normalizer.Form.NFC).toCharArray();
        PBEKeySpec spec = new PBEKeySpec(nfc, salt, iters, 256);
        try {
            return SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256").generateSecret(spec).getEncoded();
        } finally {
            spec.clearPassword();
            Arrays.fill(nfc, '\0');
        }
    }

    private static String keyAad(String id) { return "nexus-vault-key-v1|" + id; }

    private static String entryAad(String id, String entry, long updated, boolean deleted) {
        return "nexus-vault-entry-v1|" + id + "|" + entry + "|" + updated + "|" + (deleted ? 1 : 0);
    }

    static byte[] unwrap(byte[] kek, String id, JSONObject key) throws Exception {
        try {
            return gcm(Cipher.DECRYPT_MODE, kek, b64(key.getString("nonce")), keyAad(id)).doFinal(b64(key.getString("ct")));
        } catch (BadPaddingException e) { throw new WrongPassword(); }
    }

    private static JSONObject wrap(byte[] kek, String id, byte[] key, long updated) throws Exception {
        byte[] nonce = random(12);
        return new JSONObject().put("updated", updated).put("nonce", b64(nonce))
                .put("ct", b64(gcm(Cipher.ENCRYPT_MODE, kek, nonce, keyAad(id)).doFinal(key)));
    }

    /** The UTF-8 JSON of one stored entry, or a failure if anything was changed. */
    static String open(byte[] key, String vault, String id, JSONObject e) throws Exception {
        return new String(gcm(Cipher.DECRYPT_MODE, key, b64(e.getString("nonce")),
                entryAad(vault, id, e.getLong("updated"), e.getBoolean("deleted"))).doFinal(b64(e.getString("ct"))), StandardCharsets.UTF_8);
    }

    private JSONObject seal(byte[] key, String vault, String id, JSONObject plain, long updated, boolean deleted) throws Exception {
        byte[] nonce = random(12);
        return new JSONObject().put("updated", updated).put("deleted", deleted).put("device", device).put("nonce", b64(nonce))
                .put("ct", b64(gcm(Cipher.ENCRYPT_MODE, key, nonce, entryAad(vault, id, updated, deleted))
                        .doFinal(plain.toString().getBytes(StandardCharsets.UTF_8))));
    }

    // The file ----------------------------------------------------------------

    private JSONObject load() throws Exception {
        try {
            return new JSONObject(new String(Files.readAllBytes(file.toPath()), StandardCharsets.UTF_8));
        } catch (JSONException e) { throw new Exception("The vault file is damaged"); }
    }

    private void write(JSONObject v) throws Exception {
        File tmp = new File(file.getPath() + ".tmp");
        try (FileOutputStream o = new FileOutputStream(tmp)) {
            o.write(v.toString().getBytes(StandardCharsets.UTF_8));
            o.getFD().sync();
        }
        Os.chmod(tmp.getPath(), 0600);
        if (!tmp.renameTo(file)) throw new IOException("Could not save the vault");
        generation++;
    }

    private void edited() {
        Runnable r = changed;
        if (r != null) r.run();
    }

    private static long next(long previous) { return Math.max(System.currentTimeMillis(), previous + 1); }

    void create(char[] password) throws Exception {
        if (exists()) throw new Exception("A vault already exists");
        byte[] salt = random(16), kek = derive(password, salt, ITERS), key = random(32);
        try {
            String id = randomHex();
            JSONObject v = new JSONObject().put("v", 1).put("id", id)
                    .put("kdf", new JSONObject().put("alg", "pbkdf2-sha256").put("iters", ITERS).put("salt", b64(salt)))
                    .put("key", wrap(kek, id, key, System.currentTimeMillis()))
                    .put("entries", new JSONObject());
            synchronized (this) {
                if (exists()) throw new Exception("A vault already exists");
                write(v);
                unlockWith(key);
            }
            edited();
        } finally {
            Arrays.fill(kek, (byte) 0);
            Arrays.fill(key, (byte) 0);
        }
    }

    /** Wrong password: {@link WrongPassword}. Slow: never on the main thread. */
    void unlock(char[] password) throws Exception {
        JSONObject v;
        synchronized (this) { v = load(); }
        byte[] kek = kek(password, v);
        try {
            byte[] key = unwrap(kek, v.getString("id"), v.getJSONObject("key"));
            unlockWith(key);
            Arrays.fill(key, (byte) 0);
        } finally { Arrays.fill(kek, (byte) 0); }
    }

    private static byte[] kek(char[] password, JSONObject v) throws Exception {
        JSONObject kdf = v.getJSONObject("kdf");
        int iters = kdf.getInt("iters");
        if (!"pbkdf2-sha256".equals(kdf.optString("alg"))) throw new Exception("This vault uses an unknown key format");
        if (iters < MIN_ITERS) throw new Exception("This vault's key settings are too weak to open safely");
        return derive(password, b64(kdf.getString("salt")), iters);
    }

    /** Same key, new wrap: other devices learn it from `key.updated`. */
    void changePassword(char[] old, char[] now) throws Exception {
        JSONObject v;
        synchronized (this) { v = load(); }
        byte[] kekOld = kek(old, v), key = unwrap(kekOld, v.getString("id"), v.getJSONObject("key"));
        Arrays.fill(kekOld, (byte) 0);
        byte[] salt = random(16), kek = derive(now, salt, ITERS);
        try {
            synchronized (this) {
                JSONObject cur = load();
                JSONObject kdf = new JSONObject().put("alg", "pbkdf2-sha256").put("iters", ITERS).put("salt", b64(salt));
                cur.put("key", wrap(kek, cur.getString("id"), key, next(cur.getJSONObject("key").getLong("updated")))).put("kdf", kdf);
                write(cur);
            }
            edited();
        } finally {
            Arrays.fill(kek, (byte) 0);
            Arrays.fill(key, (byte) 0);
        }
    }

    private static Entry parse(String id, JSONObject p, long updated) {
        Entry e = new Entry();
        e.id = id;
        e.raw = p;
        e.title = p.optString("title");
        e.username = p.optString("username");
        e.password = p.optString("password");
        e.notes = p.optString("notes");
        JSONArray urls = p.optJSONArray("urls");
        if (urls != null) for (int i = 0; i < urls.length(); i++) e.urls.add(urls.optString(i));
        if (p.optJSONArray("history") != null) e.history = p.optJSONArray("history");
        e.updated = updated;
        return e;
    }

    /** How many entries {@link #list} could not open, the last time it ran. */
    volatile int damaged;

    /** Live entries, by title. One bad entry is skipped and counted, not fatal. */
    synchronized List<Entry> list() throws Exception {
        byte[] key = key();
        JSONObject v = load(), entries = v.getJSONObject("entries");
        List<Entry> out = new ArrayList<>();
        int bad = 0;
        for (Iterator<String> i = entries.keys(); i.hasNext(); ) {
            String id = i.next();
            JSONObject e = entries.optJSONObject(id);
            if (e == null || e.optBoolean("deleted")) continue;
            try { out.add(parse(id, new JSONObject(open(key, v.getString("id"), id, e)), e.optLong("updated"))); }
            catch (Exception x) { bad++; }
        }
        damaged = bad;
        Collections.sort(out, (a, b) -> a.title.compareToIgnoreCase(b.title));
        return out;
    }

    synchronized Entry get(String id) throws Exception {
        byte[] key = key();
        JSONObject v = load(), e = v.getJSONObject("entries").getJSONObject(id);
        return parse(id, new JSONObject(open(key, v.getString("id"), id, e)), e.optLong("updated"));
    }

    /** Saves a new or changed entry; a changed password goes to the history. */
    synchronized String save(Entry e) throws Exception {
        byte[] key = key();
        JSONObject v = load(), entries = v.getJSONObject("entries");
        String id = e.id == null ? randomHex() : e.id;
        JSONObject prev = entries.optJSONObject(id), plain = e.raw;
        long now = next(prev == null ? 0 : prev.optLong("updated"));
        JSONArray history = new JSONArray();
        JSONObject newest = null;
        if (prev != null && !prev.optBoolean("deleted")) {
            JSONObject old = new JSONObject(open(key, v.getString("id"), id, prev));
            if (old.optJSONArray("history") != null) history = old.optJSONArray("history");
            String was = old.optString("password");
            if (!was.isEmpty() && !was.equals(e.password)) newest = new JSONObject().put("password", was).put("changed", now);
        }
        // Newest first, as the desktop keeps them: the two trim the same end.
        JSONArray kept = new JSONArray();
        if (newest != null) kept.put(newest);
        for (int i = 0; i < history.length() && kept.length() < 5; i++) kept.put(history.get(i));
        plain.put("title", e.title).put("username", e.username).put("password", e.password).put("notes", e.notes)
                .put("urls", new JSONArray(e.urls)).put("history", kept);
        entries.put(id, seal(key, v.getString("id"), id, plain, now, false));
        write(v);
        edited();
        return id;
    }

    /** A tombstone that syncs; the entry's content is gone. */
    synchronized void delete(String id) throws Exception {
        byte[] key = key();
        JSONObject v = load(), entries = v.getJSONObject("entries"), prev = entries.optJSONObject(id);
        if (prev == null) return;
        entries.put(id, seal(key, v.getString("id"), id, new JSONObject(), next(prev.optLong("updated")), true));
        write(v);
        edited();
    }

    /** The file, ciphertext only, or null when there is none. */
    synchronized JSONObject file() throws Exception { return exists() ? load() : null; }

    /** What a desktop sent: merged, or replacing this vault when `adopt`. Keyless. */
    synchronized void push(JSONObject other, boolean adopt) throws Exception {
        validate(other);
        if (!exists()) { write(other); return; }
        JSONObject mine = load();
        if (adopt) {
            Files.copy(file.toPath(), new File(file.getPath() + ".bak-" + System.currentTimeMillis()).toPath());
            lock();
            write(other);
            return;
        }
        if (!mine.optString("id").equals(other.optString("id"))) throw new Exception("different vault");
        if (merge(mine, other)) write(mine);
    }

    /** Refuses what can't be a vault before it reaches the disk. */
    static void validate(JSONObject v) throws Exception {
        try {
            JSONObject kdf = v.getJSONObject("kdf"), key = v.getJSONObject("key"), entries = v.getJSONObject("entries");
            int iters = kdf.getInt("iters");
            boolean ok = v.getInt("v") == 1 && v.getString("id").matches(HEX32) && "pbkdf2-sha256".equals(kdf.getString("alg"))
                    && iters >= MIN_ITERS && iters <= MAX_ITERS && b64(kdf.getString("salt")).length == 16
                    && key.getLong("updated") > 0 && b64(key.getString("nonce")).length == 12 && !key.getString("ct").isEmpty();
            for (Iterator<String> i = entries.keys(); ok && i.hasNext(); ) {
                String id = i.next();
                JSONObject e = entries.getJSONObject(id);
                ok = id.matches(HEX32) && e.getLong("updated") > 0 && e.get("deleted") instanceof Boolean
                        && e.get("device") instanceof String && b64(e.getString("nonce")).length == 12 && !e.getString("ct").isEmpty();
            }
            if (!ok) throw new Exception();
        } catch (Exception e) { throw new Exception("Not a valid vault"); }
    }

    /** `into` takes what `other` has newer. Says whether `into` changed. */
    static boolean merge(JSONObject into, JSONObject other) throws JSONException {
        boolean moved = false;
        JSONObject mine = into.getJSONObject("entries"), theirs = other.getJSONObject("entries");
        for (Iterator<String> i = theirs.keys(); i.hasNext(); ) {
            String id = i.next();
            JSONObject e = theirs.getJSONObject(id), m = mine.optJSONObject(id);
            if (m == null || newer(e, m)) { mine.put(id, e); moved = true; }
        }
        if (other.getJSONObject("key").getLong("updated") > into.getJSONObject("key").getLong("updated")) {
            into.put("key", other.get("key")).put("kdf", other.get("kdf"));
            moved = true;
        }
        return moved;
    }

    /** The (updated, device, ct) tuple; UTF-8 byte order, as every other platform sorts. */
    private static boolean newer(JSONObject a, JSONObject b) {
        int c = Long.compare(a.optLong("updated"), b.optLong("updated"));
        if (c == 0) c = bytewise(a.optString("device"), b.optString("device"));
        if (c == 0) c = bytewise(a.optString("ct"), b.optString("ct"));
        return c > 0;
    }

    private static int bytewise(String a, String b) {
        byte[] x = a.getBytes(StandardCharsets.UTF_8), y = b.getBytes(StandardCharsets.UTF_8);
        for (int i = 0; i < Math.min(x.length, y.length); i++) {
            if (x[i] != y[i]) return (x[i] & 0xff) - (y[i] & 0xff);
        }
        return x.length - y.length;
    }

    // Matching ----------------------------------------------------------------

    /** Suffixes under which each name belongs to someone else. A small public-suffix approach. */
    private static final Set<String> SUFFIXES = new HashSet<>(Arrays.asList(
            "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "net.uk", "com.au", "net.au", "org.au", "edu.au", "gov.au",
            "co.nz", "net.nz", "org.nz", "co.jp", "ne.jp", "or.jp", "ac.jp", "go.jp", "co.kr", "or.kr", "com.br", "net.br",
            "org.br", "com.cn", "net.cn", "org.cn", "co.in", "net.in", "org.in", "com.mx", "co.za", "com.tr", "com.tw",
            "com.hk", "com.sg", "com.ua", "co.il", "com.ar",
            // Hosts where each subdomain is a different customer.
            "github.io", "gitlab.io", "vercel.app", "netlify.app", "pages.dev", "web.app", "firebaseapp.com",
            "herokuapp.com", "blogspot.com", "azurewebsites.net", "cloudfront.net", "workers.dev",
            "ac.in", "ac.nz", "co.id", "co.th", "com.pl", "com.ru", "edu.cn", "gov.br", "gov.cn", "gov.in", "ltd.uk", "org.za", "plc.uk", "sch.uk"));

    static String registrable(String host) {
        String h = host.toLowerCase(java.util.Locale.ROOT);
        if (h.endsWith(".")) h = h.substring(0, h.length() - 1);
        if (h.equals("localhost") || h.startsWith("[") || h.matches("[0-9.]+") || !h.contains(".")) return h;
        String[] p = h.split("\\.");
        String two = p[p.length - 2] + "." + p[p.length - 1];
        return p.length >= 3 && SUFFIXES.contains(two) ? p[p.length - 3] + "." + two : two;
    }

    private static URI web(String url) {
        try {
            URI u = new URI(url.contains("://") ? url.trim() : "https://" + url.trim());
            return u.getHost() != null && ("https".equals(u.getScheme()) || "http".equals(u.getScheme())) ? u : null;
        } catch (Exception e) { return null; }
    }

    /** Same registrable domain, and never an http page against an https-only entry. */
    static boolean urlMatches(String entry, String page) {
        URI e = web(entry), p = web(page);
        if (e == null || p == null) return false;
        if ("http".equals(p.getScheme()) && "https".equals(e.getScheme())) return false;
        return registrable(e.getHost()).equals(registrable(p.getHost()));
    }

    /** Exactly one of `app` (a package) or `page` (a URL). */
    synchronized List<Entry> match(String app, String page) throws Exception {
        List<Entry> found = new ArrayList<>();
        for (Entry e : list()) {
            for (String u : e.urls) {
                if (app != null ? u.equals("androidapp://" + app) : urlMatches(u, page)) { found.add(e); break; }
            }
        }
        return found;
    }

    // The generator -----------------------------------------------------------

    private static final String LOWER = "abcdefghijklmnopqrstuvwxyz", UPPER = "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            DIGIT = "0123456789", SYMBOL = "!@#$%^&*-_=+?";

    /** Uniform over the alphabet (nextInt rejects), then whole strings rejected until each class is present. */
    static String generate(int length, boolean symbols) {
        int n = Math.max(8, Math.min(128, length));
        String alphabet = LOWER + UPPER + DIGIT + (symbols ? SYMBOL : "");
        while (true) {
            char[] c = new char[n];
            for (int i = 0; i < n; i++) c[i] = alphabet.charAt(RANDOM.nextInt(alphabet.length()));
            String s = new String(c);
            if (any(s, LOWER) && any(s, UPPER) && any(s, DIGIT) && (!symbols || any(s, SYMBOL))) return s;
        }
    }

    private static boolean any(String s, String set) {
        for (int i = 0; i < s.length(); i++) if (set.indexOf(s.charAt(i)) >= 0) return true;
        return false;
    }
}
