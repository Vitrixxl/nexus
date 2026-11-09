package io.github.vitrixxl.nexus;

import android.app.Notification;
import android.app.Person;
import android.content.Context;
import android.os.Build;
import android.os.Bundle;
import android.os.Parcelable;
import android.provider.Telephony;
import android.service.notification.StatusBarNotification;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.HashSet;
import java.util.Set;

/**
 * Chat (RCS) messages, which no content provider shares: the messaging app's
 * notifications carry them, so they are kept from there, [{conv, text, time, out}].
 */
final class Chats {
    private static final int KEEP = 2000;
    private static JSONArray cache;

    private Chats() {}

    private static File file(Context c) { return new File(c.getFilesDir(), "chats.json"); }

    /** A copy, safe to read while notifications keep arriving. */
    static synchronized JSONArray snapshot(Context c) {
        try { return new JSONArray(load(c).toString()); } catch (Exception e) { return new JSONArray(); }
    }

    private static JSONArray load(Context c) {
        if (cache == null) {
            try { cache = new JSONArray(new String(Files.readAllBytes(file(c).toPath()), StandardCharsets.UTF_8)); }
            catch (Exception e) { cache = new JSONArray(); }
        }
        return cache;
    }

    private static boolean messaging(Context c, String pkg) {
        return pkg.equals(Telephony.Sms.getDefaultSmsPackage(c))
                || pkg.equals("com.samsung.android.messaging") || pkg.equals("com.google.android.apps.messaging");
    }

    /** Keeps the messages of a messaging app's notification. */
    static synchronized void record(Context c, StatusBarNotification n) {
        if (!messaging(c, n.getPackageName())) return;
        Bundle e = n.getNotification().extras;
        Parcelable[] messages = e.getParcelableArray(Notification.EXTRA_MESSAGES);
        if (messages == null) return;
        CharSequence title = e.getCharSequence(Notification.EXTRA_CONVERSATION_TITLE);
        if (title == null) title = e.getCharSequence(Notification.EXTRA_TITLE);
        if (title == null) return;
        boolean group = e.getBoolean(Notification.EXTRA_IS_GROUP_CONVERSATION);
        JSONArray list = load(c);
        Set<String> seen = new HashSet<>();
        for (int i = Math.max(0, list.length() - 200); i < list.length(); i++) {
            JSONObject m = list.optJSONObject(i);
            seen.add(m.optString("conv") + "\n" + m.optLong("time") + "\n" + m.optString("text"));
        }
        boolean changed = false;
        for (Parcelable p : messages) {
            if (!(p instanceof Bundle)) continue;
            Bundle b = (Bundle) p;
            CharSequence text = b.getCharSequence("text");
            if (text == null) continue;
            CharSequence sender = b.getCharSequence("sender");
            if (sender == null && Build.VERSION.SDK_INT >= 28) {
                Person person = b.getParcelable("sender_person");
                if (person != null) sender = person.getName();
            }
            // The phone's own messages have no sender.
            boolean out = sender == null;
            String body = group && !out ? sender + ": " + text : text.toString();
            long time = b.getLong("time");
            if (!seen.add(title + "\n" + time + "\n" + body)) continue;
            try {
                list.put(new JSONObject().put("conv", title.toString()).put("text", body).put("time", time).put("out", out));
                changed = true;
            } catch (Exception ignored) {}
        }
        if (!changed) return;
        if (list.length() > KEEP) {
            JSONArray kept = new JSONArray();
            for (int i = list.length() - KEEP; i < list.length(); i++) kept.put(list.opt(i));
            cache = list = kept;
        }
        try { Files.write(file(c).toPath(), list.toString().getBytes(StandardCharsets.UTF_8)); } catch (Exception ignored) {}
    }
}
