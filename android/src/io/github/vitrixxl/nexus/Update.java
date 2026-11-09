package io.github.vitrixxl.nexus;

import android.app.Activity;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageInstaller;
import android.net.Uri;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.Settings;
import android.widget.Toast;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.security.MessageDigest;

/**
 * Updates this app from the APK its desktop holds. Android asks for a
 * confirmation and checks the signature. This activity only receives the
 * installer's answer, so it isn't exported: what it starts is that answer's.
 */
public final class Update extends Activity {
    private static final long MAX = 64L << 20;
    private static volatile boolean busy;

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        Intent answer = getIntent();
        int status = answer.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE);
        if (status == PackageInstaller.STATUS_PENDING_USER_ACTION) {
            Intent confirm = answer.getParcelableExtra(Intent.EXTRA_INTENT);
            if (confirm != null) startActivity(confirm);
        } else {
            busy = false;
            if (status != PackageInstaller.STATUS_SUCCESS && status != PackageInstaller.STATUS_FAILURE_ABORTED) {
                toast(this, "Update failed: " + answer.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE));
            }
        }
        finish();
    }

    /** The first desktop reached, if its APK isn't the one installed here; else null. */
    // ponytail: a different APK counts as newer; a version number if several desktops ever hold different builds.
    static JSONObject available(Context c) {
        try {
            String mine = sha256(new File(c.getPackageCodePath()));
            JSONArray desktops = Link.pairs(c);
            for (int i = 0; i < desktops.length(); i++) {
                JSONObject d = desktops.getJSONObject(i);
                try {
                    String theirs = sum(d);
                    return theirs.length() == 64 && !theirs.equals(mine) ? d : null;
                } catch (Exception offline) { /* the next one */ }
            }
        } catch (Exception ignored) {}
        return null;
    }

    static void start(Activity a, JSONObject desktop) {
        if (!a.getPackageManager().canRequestPackageInstalls()) {
            toast(a, "Allow Nexus to install updates, then come back and tap Update");
            a.startActivity(new Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:" + a.getPackageName())));
            return;
        }
        if (busy) return;
        busy = true;
        toast(a, "Downloading the update…");
        Context c = a.getApplicationContext();
        new Thread(() -> {
            try {
                install(c, desktop);
            } catch (Exception e) {
                busy = false;
                toast(c, "Update failed: " + e.getMessage());
            }
        }).start();
    }

    private static void install(Context c, JSONObject desktop) throws Exception {
        String want = sum(desktop);
        File apk = new File(c.getCacheDir(), "update.apk");
        try {
            HttpURLConnection h = open(desktop, "/nexus.apk");
            try (InputStream in = h.getInputStream(); OutputStream out = new FileOutputStream(apk)) {
                copy(in, out);
            } finally {
                h.disconnect();
            }
            if (!sha256(apk).equals(want)) throw new Exception("The download was damaged");
            PackageInstaller installer = c.getPackageManager().getPackageInstaller();
            PackageInstaller.SessionParams params = new PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL);
            params.setAppPackageName(c.getPackageName());
            int id = installer.createSession(params);
            try (PackageInstaller.Session s = installer.openSession(id)) {
                try (OutputStream out = s.openWrite("base.apk", 0, apk.length()); InputStream in = new FileInputStream(apk)) {
                    copy(in, out);
                    s.fsync(out);
                }
                Intent answer = new Intent(c, Update.class);
                s.commit(PendingIntent.getActivity(c, 1, answer, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_MUTABLE).getIntentSender());
            } catch (Exception e) {
                installer.abandonSession(id);
                throw e;
            }
        } finally {
            apk.delete();
        }
    }

    private static void copy(InputStream in, OutputStream out) throws Exception {
        byte[] buffer = new byte[64 * 1024];
        long total = 0;
        for (int n; (n = in.read(buffer)) > 0; ) {
            if ((total += n) > MAX) throw new Exception("The update is too large");
            out.write(buffer, 0, n);
        }
    }

    private static HttpURLConnection open(JSONObject desktop, String path) throws Exception {
        HttpURLConnection h = (HttpURLConnection) new URL("http", desktop.getString("host"), desktop.optInt("port", 47913),
                path + "?t=" + desktop.getString("token")).openConnection();
        h.setConnectTimeout(4000);
        h.setReadTimeout(20000);
        if (h.getResponseCode() != 200) throw new Exception("The desktop answered " + h.getResponseCode());
        return h;
    }

    private static String sum(JSONObject desktop) throws Exception {
        HttpURLConnection h = open(desktop, "/nexus.apk.sha256");
        try (InputStream in = h.getInputStream()) {
            java.io.ByteArrayOutputStream text = new java.io.ByteArrayOutputStream();
            copy(in, text);
            return text.toString("US-ASCII").trim();
        } finally {
            h.disconnect();
        }
    }

    private static String sha256(File f) throws Exception {
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        try (InputStream in = new FileInputStream(f)) {
            byte[] buffer = new byte[64 * 1024];
            for (int n; (n = in.read(buffer)) > 0; ) digest.update(buffer, 0, n);
        }
        return NotesStore.hex(digest.digest());
    }

    private static void toast(Context c, String text) {
        new Handler(Looper.getMainLooper()).post(() -> Toast.makeText(c, text, Toast.LENGTH_LONG).show());
    }
}
