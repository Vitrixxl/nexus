package io.github.vitrixxl.nexus;

import org.json.JSONObject;

import java.io.File;
import java.lang.reflect.Field;
import java.nio.file.Files;
import java.util.Arrays;
import java.util.Base64;
import java.util.HashSet;
import java.util.Set;

/** Runs on the device under app_process: see android/check-vault.sh. */
public class VaultCheck {
    static int checks;

    static void ok(boolean c, String what) {
        checks++;
        if (!c) throw new AssertionError(what);
        System.out.println("ok " + what);
    }

    static void fails(Class<? extends Throwable> kind, String what, Thrower t) throws Exception {
        try { t.run(); } catch (Throwable x) {
            ok(kind.isInstance(x), what + " (" + x.getClass().getSimpleName() + ")");
            return;
        }
        throw new AssertionError("no failure: " + what);
    }
    interface Thrower { void run() throws Exception; }

    static String hex(byte[] b) { StringBuilder s = new StringBuilder(); for (byte x : b) s.append(String.format("%02x", x)); return s.toString(); }
    static byte[] unhex(String s) { byte[] b = new byte[s.length() / 2]; for (int i = 0; i < b.length; i++) b[i] = (byte) Integer.parseInt(s.substring(2 * i, 2 * i + 2), 16); return b; }

    public static void main(String[] a) throws Exception {
        JSONObject doc = new JSONObject(new String(Files.readAllBytes(new File(a[0]).toPath()), "UTF-8"));
        File dir = new File(a[1]);
        JSONObject vector = doc.getJSONObject("vault");
        String pw = doc.getString("_password");
        long t0 = System.currentTimeMillis();

        // The vector.
        JSONObject kdf = vector.getJSONObject("kdf");
        byte[] kek = VaultStore.derive(pw.toCharArray(), Base64.getDecoder().decode(kdf.getString("salt")), kdf.getInt("iters"));
        ok(hex(kek).equals(doc.getString("_kek_hex")), "vector KEK");
        byte[] vk = VaultStore.unwrap(kek, vector.getString("id"), vector.getJSONObject("key"));
        ok(hex(vk).equals(doc.getString("_vault_key_hex")), "vector VK");
        JSONObject entry = vector.getJSONObject("entries").getJSONObject("ffeeddccbbaa99887766554433221100");
        ok(VaultStore.open(vk, vector.getString("id"), "ffeeddccbbaa99887766554433221100", entry).equals(doc.getString("_entry_plaintext")), "vector entry plaintext");
        JSONObject grave = vector.getJSONObject("entries").getJSONObject("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        ok(VaultStore.open(vk, vector.getString("id"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", grave).equals("{}"), "vector tombstone authenticates");
        fails(Exception.class, "tombstone with deleted=false fails", () -> {
            JSONObject bad = new JSONObject(grave.toString()).put("deleted", false);
            VaultStore.open(vk, vector.getString("id"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", bad);
        });
        fails(Exception.class, "entry with altered updated fails", () -> {
            JSONObject bad = new JSONObject(entry.toString()).put("updated", entry.getLong("updated") + 1);
            VaultStore.open(vk, vector.getString("id"), "ffeeddccbbaa99887766554433221100", bad);
        });

        // Unlocking the vector as a file, and the iteration floor.
        File vf = new File(dir, "vec.json");
        Files.write(vf.toPath(), vector.toString().getBytes("UTF-8"));
        VaultStore s = new VaultStore(vf, "check");
        fails(VaultStore.WrongPassword.class, "wrong password", () -> s.unlock("nope".toCharArray()));
        s.unlock(pw.toCharArray());
        ok(s.unlocked() && s.list().size() == 1 && s.list().get(0).password.equals("p@ss-é-1"), "unlock and list");
        s.lock();
        Field vkf = VaultStore.class.getDeclaredField("vk");
        vkf.setAccessible(true);
        ok(vkf.get(s) == null, "lock forgets the key");
        s.unlock(pw.toCharArray());
        byte[] held = (byte[]) vkf.get(s);
        s.lock();
        ok(Arrays.equals(held, new byte[32]), "lock wipes the key bytes");
        fails(Exception.class, "locked vault refuses list", s::list);
        JSONObject weak = new JSONObject(vector.toString());
        weak.getJSONObject("kdf").put("iters", 99999);
        File wf = new File(dir, "weak.json");
        Files.write(wf.toPath(), weak.toString().getBytes("UTF-8"));
        fails(Exception.class, "iters below 100000 refused", () -> new VaultStore(wf, "x").unlock(pw.toCharArray()));

        // Create, save, delete: fresh nonces, monotonic updated, tombstone.
        File nf = new File(dir, "new.json");
        nf.delete();
        VaultStore n = new VaultStore(nf, "phone");
        long t1 = System.currentTimeMillis();
        n.create("hunter2 é".toCharArray());
        System.out.println("create (600000 iters): " + (System.currentTimeMillis() - t1) + " ms");
        n.lock();
        t1 = System.currentTimeMillis();
        n.unlock("hunter2 é".toCharArray());
        System.out.println("unlock (600000 iters): " + (System.currentTimeMillis() - t1) + " ms");
        VaultStore.Entry e = new VaultStore.Entry();
        e.title = "Site"; e.username = "u"; e.password = "first"; e.urls.add("https://site.example/x");
        String id = n.save(e);
        JSONObject f1 = n.file();
        Set<String> nonces = new HashSet<>();
        nonces.add(f1.getJSONObject("entries").getJSONObject(id).getString("nonce"));
        long u1 = f1.getJSONObject("entries").getJSONObject(id).getLong("updated");
        e = n.get(id);
        e.id = id;
        e.password = "second";
        n.save(e);
        JSONObject f2 = n.file();
        ok(nonces.add(f2.getJSONObject("entries").getJSONObject(id).getString("nonce")), "fresh nonce per write");
        ok(f2.getJSONObject("entries").getJSONObject(id).getLong("updated") > u1, "updated increases");
        VaultStore.Entry back = n.get(id);
        ok(back.password.equals("second") && back.history.length() == 1 && back.history.getJSONObject(0).getString("password").equals("first"), "history keeps replaced password");
        for (int i = 3; i <= 8; i++) { back.password = "p" + i; n.save(back); }
        back = n.get(id);
        ok(back.history.length() == 5, "history capped at 5");
        n.delete(id);
        ok(n.list().isEmpty() && n.file().getJSONObject("entries").getJSONObject(id).getBoolean("deleted"), "delete leaves a tombstone");
        ok(VaultStore.open(vkf.get(n) == null ? null : (byte[]) vkf.get(n), n.id(), id, n.file().getJSONObject("entries").getJSONObject(id)).equals("{}"), "our tombstone is {}");

        // Merge, without any key.
        JSONObject base = new JSONObject(vector.toString());
        JSONObject older = new JSONObject(base.toString()), newer = new JSONObject(base.toString());
        String eid = "ffeeddccbbaa99887766554433221100";
        newer.getJSONObject("entries").getJSONObject(eid).put("updated", entry.getLong("updated") + 10).put("ct", "bmV3");
        ok(VaultStore.merge(older, newer) && older.getJSONObject("entries").getJSONObject(eid).getString("ct").equals("bmV3"), "newer entry wins");
        ok(!VaultStore.merge(older, base), "older entry loses, no change");
        JSONObject tie = new JSONObject(base.toString()), other = new JSONObject(base.toString());
        other.getJSONObject("entries").getJSONObject(eid).put("device", "zz").put("ct", "AAAA");
        ok(VaultStore.merge(tie, other) && tie.getJSONObject("entries").getJSONObject(eid).getString("device").equals("zz"), "same updated: device breaks the tie");
        JSONObject tieA = new JSONObject(base.toString()), tieB = new JSONObject(base.toString());
        tieB.getJSONObject("entries").getJSONObject(eid).put("device", "😀");  // above BMP, above U+FFFF in UTF-8 and below in UTF-16
        tieA.getJSONObject("entries").getJSONObject(eid).put("device", "�");
        ok(VaultStore.merge(tieA, tieB) && tieA.getJSONObject("entries").getJSONObject(eid).getString("device").equals("😀"), "device compared by UTF-8 bytes");
        JSONObject graveM = new JSONObject(base.toString()), graveT = new JSONObject(base.toString());
        graveT.getJSONObject("entries").getJSONObject(eid).put("updated", entry.getLong("updated") + 1).put("deleted", true);
        VaultStore.merge(graveM, graveT);
        ok(graveM.getJSONObject("entries").getJSONObject(eid).getBoolean("deleted"), "newer tombstone wins");
        JSONObject newKey = new JSONObject(base.toString()), keyOther = new JSONObject(base.toString());
        keyOther.getJSONObject("key").put("updated", base.getJSONObject("key").getLong("updated") + 1).put("ct", "new");
        keyOther.getJSONObject("kdf").put("salt", "AAAAAAAAAAAAAAAAAAAAAA==");
        VaultStore.merge(newKey, keyOther);
        ok(newKey.getJSONObject("key").getString("ct").equals("new") && newKey.getJSONObject("kdf").getString("salt").startsWith("AAAA"), "key and kdf move together");
        JSONObject keep = new JSONObject(base.toString()).put("extra", 1);
        VaultStore.merge(keep, newKey);
        ok(keep.getInt("extra") == 1, "unknown top-level fields survive a merge");

        // Push: id check, adopt, validation.
        File pf = new File(dir, "push.json");
        pf.delete();
        VaultStore p = new VaultStore(pf, "phone");
        fails(Exception.class, "garbage refused", () -> p.push(new JSONObject().put("v", 1), false));
        p.push(new JSONObject(base.toString()), false);
        ok(p.exists() && p.id().equals(base.getString("id")), "a phone without a vault stores the push");
        JSONObject stranger = new JSONObject(base.toString()).put("id", "0123456789abcdef0123456789abcdef");
        fails(Exception.class, "different vault refused", () -> p.push(stranger, false));
        p.unlock(pw.toCharArray());
        p.push(stranger, true);
        ok(p.id().equals(stranger.getString("id")) && !p.unlocked(), "adopt replaces and locks");
        File[] baks = dir.listFiles((d, name) -> name.startsWith("push.json.bak-"));
        ok(baks != null && baks.length == 1, "adopt keeps a backup");
        for (File b : baks) b.delete();

        // URL rules.
        ok(VaultStore.urlMatches("https://example.com/login", "https://www.example.com/x"), "same registrable domain");
        ok(!VaultStore.urlMatches("https://example.com", "http://example.com"), "http page, https-only entry: no match");
        ok(VaultStore.urlMatches("http://example.com", "https://example.com"), "https page, http entry: match");
        ok(!VaultStore.urlMatches("https://example.com", "https://example.org"), "other domain");
        ok(!VaultStore.urlMatches("https://example.com", "https://example.com.evil.net"), "suffix attack");
        ok(!VaultStore.urlMatches("https://example.com", "https://evil.net/?u=example.com"), "path doesn't match");
        ok(!VaultStore.urlMatches("https://example.com", "https://example.com@evil.net/"), "userinfo doesn't match");
        ok(!VaultStore.urlMatches("https://a.co.uk", "https://b.co.uk"), "co.uk siblings differ");
        ok(VaultStore.urlMatches("https://a.co.uk", "https://x.a.co.uk"), "co.uk same registrable");
        ok(!VaultStore.urlMatches("https://foo.github.io", "https://bar.github.io"), "github.io customers differ");
        ok(VaultStore.urlMatches("http://192.168.1.2:8080", "http://192.168.1.2"), "IP matches itself");
        ok(!VaultStore.urlMatches("http://192.168.1.2", "http://192.168.1.3"), "other IP");
        ok(VaultStore.urlMatches("http://localhost:3000", "http://localhost:9000"), "localhost");
        ok(!VaultStore.urlMatches("androidapp://com.x", "https://com.x"), "androidapp is not a web URL");
        ok(!VaultStore.urlMatches("ftp://example.com", "https://example.com"), "ftp isn't matched");
        ok(VaultStore.urlMatches("example.com", "https://example.com"), "bare domain means https");

        // The generator.
        Set<String> seen = new HashSet<>();
        for (int i = 0; i < 2000; i++) {
            String g = VaultStore.generate(20, true);
            ok(g.length() == 20 && g.matches(".*[a-z].*") && g.matches(".*[A-Z].*") && g.matches(".*[0-9].*") && g.matches(".*[^a-zA-Z0-9].*"), "generator classes");
            ok(seen.add(g), "generator no repeat");
        }
        ok(VaultStore.generate(20, false).matches("[a-zA-Z0-9]+"), "generator no symbols");
        ok(VaultStore.generate(1, true).length() == 8, "generator length floor");
        nf.delete(); vf.delete(); wf.delete(); pf.delete();
        System.out.println("PASS " + checks + " checks in " + (System.currentTimeMillis() - t0) + " ms");
    }
}
