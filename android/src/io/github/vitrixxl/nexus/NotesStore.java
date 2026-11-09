package io.github.vitrixxl.nexus;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.RandomAccessFile;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.StandardCopyOption;
import java.security.MessageDigest;
import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Base64;
import java.util.Date;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.UUID;

/**
 * The phone's copy of Nexus Notes, in the desktop's format: `A.md` is a page,
 * `A/` holds its subpages, `.order` orders a folder, deleted pages go to
 * `.trash`. The desktop's daemon drives the sync over the link (see
 * nexus's src/notesync.rs); this answers its requests and serves the app.
 * Plain Java, no Android: the same class runs on a desktop JVM in the tests.
 * Every method holds the store's lock, so a sync and an edit never cross.
 */
final class NotesStore {
    static final int CHUNK = 256 << 10;
    private static final int MAX_FILES = 5000;
    private static final int MAX_PATH = 512;
    final File root;
    private final File idFile, staging;
    /** Each file's hash by modification time and size: a listing reads only what changed. */
    private final Map<String, Object[]> cache = new HashMap<>();
    /** Told when a sync changed files, from the link's thread. */
    static volatile Runnable onSynced;

    NotesStore(File root) {
        this.root = root;
        idFile = new File(root.getParentFile(), "notes-id");
        staging = new File(root, ".nexus-sync");
    }

    // ---- Paths

    /** The daemon's rule: `a/b.md` or `a/.order`, nothing hidden, no `..`. */
    static boolean valid(String path) {
        if (path == null || path.isEmpty() || path.getBytes(StandardCharsets.UTF_8).length > MAX_PATH) return false;
        String[] parts = path.split("/", -1);
        for (int i = 0; i < parts.length; i++) {
            String p = parts[i];
            if (p.isEmpty() || p.indexOf('\\') >= 0) return false;
            for (int k = 0; k < p.length(); k++) if (Character.isISOControl(p.charAt(k))) return false;
            boolean last = i == parts.length - 1;
            if (!last && p.startsWith(".")) return false;
            if (last && !p.equals(".order") && (p.startsWith(".") || p.length() <= 3 || !p.endsWith(".md"))) return false;
        }
        return true;
    }

    /** Where `path` lives, refusing one that leaves the folder or goes through a link. */
    private File file(String path) throws IOException {
        if (!valid(path)) throw new IOException("Not a notes path: " + path);
        File f = root;
        for (String part : path.split("/")) {
            f = new File(f, part);
            if (Files.isSymbolicLink(f.toPath())) throw new IOException(path + " goes through a link");
        }
        return f;
    }

    static String hex(byte[] digest) {
        StringBuilder s = new StringBuilder();
        for (byte b : digest) s.append(String.format("%02x", b));
        return s.toString();
    }

    static String sha256(byte[] bytes) {
        try { return hex(MessageDigest.getInstance("SHA-256").digest(bytes)); }
        catch (Exception e) { throw new RuntimeException(e); }
    }

    static String sha256(String text) { return sha256(text.getBytes(StandardCharsets.UTF_8)); }

    private static String sha256(File f) throws IOException {
        try (InputStream in = new FileInputStream(f)) {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            byte[] buf = new byte[64 << 10];
            int n;
            while ((n = in.read(buf)) > 0) md.update(buf, 0, n);
            return hex(md.digest());
        } catch (java.security.NoSuchAlgorithmException e) { throw new IOException(e); }
    }

    /** The hash of what `f` holds, or null if nothing is there. */
    private static String current(File f) throws IOException {
        if (!f.exists()) return null;
        if (!f.isFile()) throw new IOException(f.getName() + " is not a file");
        return sha256(f);
    }

    /** Writes beside the file, flushes, then renames over it. */
    static void writeAtomic(File f, byte[] bytes) throws IOException {
        File dir = f.getParentFile();
        if (!dir.isDirectory() && !dir.mkdirs()) throw new IOException("Can't create " + dir);
        File tmp = new File(dir, "." + f.getName() + ".tmp");
        try (FileOutputStream out = new FileOutputStream(tmp)) {
            out.write(bytes);
            out.getFD().sync();
        }
        Files.move(tmp.toPath(), f.toPath(), StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
    }

    private static void moveFile(File from, File to) throws IOException {
        File dir = to.getParentFile();
        if (!dir.isDirectory() && !dir.mkdirs()) throw new IOException("Can't create " + dir);
        Files.move(from.toPath(), to.toPath(), StandardCopyOption.ATOMIC_MOVE);
    }

    /** Removes the folders `f` leaves empty, up to the root. */
    private void prune(File f) {
        for (File d = f.getParentFile(); d != null && !d.equals(root) && d.getPath().startsWith(root.getPath()); d = d.getParentFile()) {
            if (!d.delete()) break;
        }
    }

    // ---- What the daemon asks

    /** This store's id: new when the notes are, so a wiped phone is not taken for one that deleted everything. */
    synchronized String id() throws IOException {
        if (idFile.isFile()) {
            String id = new String(Files.readAllBytes(idFile.toPath()), StandardCharsets.UTF_8).trim();
            if (!id.isEmpty()) return id;
        }
        String id = UUID.randomUUID().toString();
        writeAtomic(idFile, id.getBytes(StandardCharsets.UTF_8));
        return id;
    }

    synchronized JSONObject list() throws Exception {
        JSONArray files = new JSONArray();
        Map<String, Object[]> seen = new HashMap<>();
        walk(root, "", files, seen);
        cache.keySet().retainAll(seen.keySet());
        return new JSONObject().put("store", id()).put("files", files);
    }

    private void walk(File dir, String rel, JSONArray out, Map<String, Object[]> seen) throws Exception {
        File[] list = dir.listFiles();
        if (list == null) return;
        for (File f : list) {
            String name = f.getName(), path = rel.isEmpty() ? name : rel + "/" + name;
            if (Files.isSymbolicLink(f.toPath())) continue;
            if (f.isDirectory()) {
                if (!name.startsWith(".")) walk(f, path, out, seen);
            } else if (f.isFile() && valid(path)) {
                Object[] c = cache.get(path);
                if (c == null || (long) c[0] != f.lastModified() || (long) c[1] != f.length()) {
                    c = new Object[]{f.lastModified(), f.length(), sha256(f)};
                    cache.put(path, c);
                }
                seen.put(path, c);
                out.put(new JSONArray().put(path).put(c[2]));
                if (out.length() > MAX_FILES) throw new IOException("Over " + MAX_FILES + " notes: too many to sync");
            }
        }
    }

    synchronized JSONObject read(String path, long offset, int len) throws Exception {
        try (RandomAccessFile f = new RandomAccessFile(file(path), "r")) {
            byte[] buf = new byte[Math.max(0, Math.min(len, CHUNK))];
            f.seek(offset);
            int n = 0, k;
            while (n < buf.length && (k = f.read(buf, n, buf.length - n)) > 0) n += k;
            return new JSONObject().put("b64", Base64.getEncoder().encodeToString(Arrays.copyOf(buf, n)));
        }
    }

    private File staged(String path) {
        return new File(staging, sha256(path).substring(0, 32));
    }

    synchronized void write(String path, long offset, String b64) throws Exception {
        file(path);
        File part = staged(path);
        if (!staging.isDirectory() && !staging.mkdirs()) throw new IOException("Can't stage " + path);
        try (RandomAccessFile f = new RandomAccessFile(part, "rw")) {
            if (offset == 0) f.setLength(0);
            if (f.length() != offset) throw new IOException("Chunk out of order for " + path);
            f.seek(offset);
            f.write(Base64.getDecoder().decode(b64));
        }
    }

    /** Puts the staged `path` in place if it hashes to `hash` and the target still holds `expect` ("": nothing). */
    synchronized void commit(String path, String hash, String expect) throws Exception {
        File target = file(path), part = staged(path);
        try {
            if (!sha256(part).equals(hash)) throw new IOException(path + " changed while it was copied");
            String now = current(target);
            if (!(expect.isEmpty() ? now == null : expect.equals(now))) throw new IOException(path + " changed meanwhile");
            File dir = target.getParentFile();
            if (!dir.isDirectory() && !dir.mkdirs()) throw new IOException("Can't create " + dir);
            Files.move(part.toPath(), target.toPath(), StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
        } finally {
            part.delete();
            staging.delete();
        }
        synced();
    }

    synchronized void trash(String path, String expect, String stamp) throws Exception {
        File f = file(path);
        if (!expect.equals(current(f))) throw new IOException(path + " changed meanwhile");
        if (!stamp.matches("[0-9]{1,20}")) throw new IOException("Bad trash stamp");
        moveFile(f, new File(new File(new File(root, ".trash"), stamp), path));
        prune(f);
        synced();
    }

    synchronized void move(String path, String to, String expect) throws Exception {
        File from = file(path), dest = file(to);
        if (!expect.equals(current(from))) throw new IOException(path + " changed meanwhile");
        if (dest.exists()) throw new IOException(to + " is taken");
        moveFile(from, dest);
        prune(from);
        synced();
    }

    private static void synced() {
        Runnable r = onSynced;
        if (r != null) r.run();
    }

    // ---- What the app shows

    static final class Page {
        final String rel, title;
        final int depth;
        final boolean kids;
        Page(String rel, int depth, boolean kids) {
            this.rel = rel;
            this.depth = depth;
            this.kids = kids;
            title = name(rel);
        }
    }

    static String name(String rel) { int i = rel.lastIndexOf('/'); return i < 0 ? rel : rel.substring(i + 1); }
    static String parent(String rel) { int i = rel.lastIndexOf('/'); return i < 0 ? "" : rel.substring(0, i); }
    static String join(String parent, String name) { return parent.isEmpty() ? name : parent + "/" + name; }

    private File page(String rel) throws IOException { return file(rel + ".md"); }
    private File dir(String rel) throws IOException { return rel.isEmpty() ? root : file(rel + "/x.md").getParentFile(); }
    private boolean exists(String rel) throws IOException { return page(rel).exists() || dir(rel).isDirectory(); }

    /** A title as a file name, as the desktop makes it: no separators, nothing hidden. */
    static String fileName(String title) {
        StringBuilder s = new StringBuilder();
        for (char c : title.toCharArray()) s.append(c == '/' || c == '\\' || Character.isISOControl(c) ? '-' : c);
        String n = s.toString().trim();
        while (n.startsWith(".")) n = n.substring(1).trim();
        return n.length() > 120 ? n.substring(0, 120) : n;
    }

    /** Every page, depth first, each folder in its `.order` then by name. `open` says which to go into. */
    synchronized List<Page> tree(java.util.Set<String> open) {
        List<Page> out = new ArrayList<>();
        kids("", 0, open, out);
        return out;
    }

    /** The pages of folder `rel`, in order. */
    synchronized List<String> children(String rel) {
        List<String> names = new ArrayList<>();
        File[] list;
        try { list = dir(rel).listFiles(); } catch (IOException e) { return names; }
        if (list == null) return names;
        for (File f : list) {
            String n = f.getName();
            if (n.startsWith(".") || Files.isSymbolicLink(f.toPath())) continue;
            String stem = f.isDirectory() ? n : n.endsWith(".md") ? n.substring(0, n.length() - 3) : null;
            if (stem != null && !stem.isEmpty() && !names.contains(stem)) names.add(stem);
        }
        names.sort(String.CASE_INSENSITIVE_ORDER);
        List<String> order = order(rel);
        names.sort((a, b) -> Integer.compare(rank(order, a), rank(order, b)));
        List<String> rels = new ArrayList<>();
        for (String n : names) rels.add(join(rel, n));
        return rels;
    }

    private static int rank(List<String> order, String n) { int i = order.indexOf(n); return i < 0 ? Integer.MAX_VALUE : i; }

    private List<String> order(String rel) {
        try {
            File f = new File(dir(rel), ".order");
            if (!f.isFile()) return new ArrayList<>();
            return new ArrayList<>(Arrays.asList(new String(Files.readAllBytes(f.toPath()), StandardCharsets.UTF_8).split("\n")));
        } catch (IOException e) { return new ArrayList<>(); }
    }

    private void kids(String rel, int depth, java.util.Set<String> open, List<Page> out) {
        for (String k : children(rel)) {
            boolean has;
            try { has = dir(k).isDirectory() && !children(k).isEmpty(); } catch (IOException e) { has = false; }
            out.add(new Page(k, depth, has));
            if (has && open.contains(k)) kids(k, depth + 1, open, out);
        }
    }

    /** A page's text; "" for a folder without its page; null if it is gone. */
    synchronized String load(String rel) {
        try {
            File f = page(rel);
            if (f.isFile()) return new String(Files.readAllBytes(f.toPath()), StandardCharsets.UTF_8);
            return dir(rel).isDirectory() ? "" : null;
        } catch (IOException e) { return null; }
    }

    static String stamp() { return new SimpleDateFormat("yyyy-MM-dd HH.mm.ss", Locale.ROOT).format(new Date()); }

    /**
     * Writes a page. If it changed on disk since `known` (its hash when
     * loaded, null for none), that version is kept beside it first, as the
     * desktop does; returns that copy's page, or null.
     */
    synchronized String save(String rel, String text, String known) throws IOException {
        File f = page(rel);
        byte[] bytes = text.getBytes(StandardCharsets.UTF_8);
        String disk = current(f);
        if (sha256(bytes).equals(disk)) return null;
        String copy = null;
        if (disk != null && !disk.equals(known)) {
            copy = free(parent(rel), name(rel) + " (changed elsewhere " + stamp() + ")");
            moveFile(f, page(copy));
        }
        writeAtomic(f, bytes);
        return copy;
    }

    /** `base`, else `base 2`, `base 3`… */
    synchronized String free(String parent, String base) throws IOException {
        for (int i = 1; ; i++) {
            String rel = join(parent, i == 1 ? base : base + " " + i);
            if (!exists(rel)) return rel;
        }
    }

    synchronized String create(String parent) throws IOException {
        String rel = free(parent, "Untitled");
        String date = new SimpleDateFormat("yyyy-MM-dd", Locale.ROOT).format(new Date());
        writeAtomic(page(rel), ("---\ncreated: " + date + "\n---\n\n\n").getBytes(StandardCharsets.UTF_8));
        return rel;
    }

    /** Renames a page and its subpages to `title`; a taken or empty name keeps the old one. */
    synchronized String rename(String rel, String title) throws IOException {
        String n = fileName(title);
        if (n.isEmpty() || n.equals(name(rel))) return rel;
        String to = join(parent(rel), n);
        if (exists(to)) return rel;
        File file = page(rel), dir = dir(rel);
        if (file.exists()) moveFile(file, page(to));
        if (dir.isDirectory()) moveFile(dir, dir(to));
        List<String> order = order(parent(rel));
        int i = order.indexOf(name(rel));
        if (i >= 0) {
            order.set(i, n);
            writeAtomic(new File(dir(parent(rel)), ".order"), (String.join("\n", order) + "\n").getBytes(StandardCharsets.UTF_8));
        }
        return to;
    }

    /** Moves a page and its subpages into `.trash`; returns where, for {@link #restore}. */
    synchronized File delete(String rel) throws IOException {
        File trash = new File(new File(root, ".trash"), String.valueOf(System.currentTimeMillis() * 1000));
        File file = page(rel), dir = dir(rel);
        if (file.exists()) moveFile(file, new File(trash, name(rel) + ".md"));
        if (dir.isDirectory()) moveFile(dir, new File(trash, name(rel)));
        return trash;
    }

    synchronized void restore(File trash, String rel) throws IOException {
        if (exists(rel)) throw new IOException("A page named “" + name(rel) + "” is already there");
        File file = new File(trash, name(rel) + ".md"), dir = new File(trash, name(rel));
        if (file.exists()) moveFile(file, page(rel));
        if (dir.isDirectory()) moveFile(dir, dir(rel));
        trash.delete();
    }

    /** Files changed since `since` (ms): what waits for the next sync. */
    synchronized int changedSince(long since) {
        int[] n = {0};
        count(root, since, n);
        return n[0];
    }

    private void count(File dir, long since, int[] n) {
        File[] list = dir.listFiles();
        if (list != null) for (File f : list) {
            if (f.getName().startsWith(".")) continue;
            if (f.isDirectory()) count(f, since, n);
            else if (f.getName().endsWith(".md") && f.lastModified() > since) n[0]++;
        }
    }

    static final class Hit {
        final String rel, snippet;
        Hit(String rel, String snippet) { this.rel = rel; this.snippet = snippet; }
    }

    /** Pages whose title or text hold every word of `query`, titles first. */
    List<Hit> search(String query, java.util.function.BooleanSupplier stale) {
        String[] words = query.toLowerCase(Locale.ROOT).trim().split("\\s+");
        List<Hit> titles = new ArrayList<>(), texts = new ArrayList<>();
        List<String> todo = new ArrayList<>(children(""));
        while (!todo.isEmpty() && titles.size() + texts.size() < 200) {
            if (stale.getAsBoolean()) return new ArrayList<>();
            String rel = todo.remove(0);
            todo.addAll(0, children(rel));
            String text = load(rel);
            String body = text == null ? "" : body(text);
            String title = name(rel).toLowerCase(Locale.ROOT), lower = body.toLowerCase(Locale.ROOT);
            boolean all = true, inTitle = true;
            for (String w : words) {
                if (!title.contains(w)) inTitle = false;
                if (!title.contains(w) && !lower.contains(w)) all = false;
            }
            if (!all) continue;
            String snippet = "";
            for (String line : body.split("\n")) {
                if (line.toLowerCase(Locale.ROOT).contains(words[0]) || (snippet.isEmpty() && !line.trim().isEmpty())) {
                    snippet = line.replaceAll("^\\s*(#{1,3} |[-*+] (\\[[ xX]\\] )?|\\d+[.)] |> (\\[!\\w+\\])?)", "").trim();
                    if (line.toLowerCase(Locale.ROOT).contains(words[0])) break;
                }
            }
            (inTitle ? titles : texts).add(new Hit(rel, snippet.length() > 160 ? snippet.substring(0, 160) : snippet));
        }
        titles.addAll(texts);
        return titles;
    }

    /** Front matter, fences and the blank line after them included. */
    static String frontMatter(String raw) {
        if (!raw.startsWith("---\n")) return "";
        int at = 4;
        for (String l : raw.substring(4).split("(?<=\n)")) {
            at += l.length();
            String t = l.trim();
            if (t.equals("---") || t.equals("...")) {
                return raw.startsWith("\n", at) ? raw.substring(0, at + 1) : raw.substring(0, at);
            }
        }
        return "";
    }

    static String body(String raw) { return raw.substring(frontMatter(raw).length()); }
}
