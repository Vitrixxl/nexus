package io.github.vitrixxl.nexus;

import android.app.Activity;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.ContentProvider;
import android.content.ContentValues;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.database.Cursor;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.Matrix;
import android.media.ExifInterface;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.provider.Telephony;
import android.telephony.SmsManager;
import android.util.Base64;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.io.RandomAccessFile;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * MMS for the desktop: files come in chunks (`mms_put`), go out as one
 * message (`mms_send`), and the parts of received ones go back in chunks
 * (`mms_read`). Android sends an MMS for any app holding SEND_SMS, from a
 * PDU the app encodes itself and serves through {@link Store}; the system
 * then keeps it in the phone's store like any other.
 */
final class Mms {
    private Mms() {}

    private static final long MAX_FILE = 20L << 20;
    /** What carriers commonly take when the SIM does not say. */
    private static final int DEFAULT_MAX = 300 * 1024;

    static JSONObject op(Context c, String op, JSONObject a) throws Exception {
        switch (op) {
            case "mms_put": return put(c, a);
            case "mms_send": return send(c, a);
            case "mms_read": return read(c, a);
            default: throw new Exception("unknown op");
        }
    }

    static File dir(Context c) {
        File d = new File(c.getCacheDir(), "mms");
        d.mkdirs();
        return d;
    }

    /** Letters, digits, `.`, `-` and `_` alone, so that a name is never a path. */
    static String safe(String name) {
        String s = name.replaceAll("[^A-Za-z0-9._-]", "_").replaceAll("^\\.+", "");
        return s.isEmpty() ? "attachment" : s;
    }

    private static JSONObject put(Context c, JSONObject a) throws Exception {
        long offset = a.optLong("offset");
        byte[] data = Base64.decode(a.optString("b64"), Base64.NO_WRAP);
        if (offset < 0 || offset + data.length > MAX_FILE) throw new Exception("An attachment is over 20 MB");
        if (offset == 0) {
            // What an earlier send left behind, after a failure or a cut link.
            File[] old = dir(c).listFiles();
            if (old != null) for (File f : old) if (f.lastModified() < System.currentTimeMillis() - 3600_000) f.delete();
        }
        try (RandomAccessFile f = new RandomAccessFile(new File(dir(c), safe(a.getString("name"))), "rw")) {
            if (offset == 0) f.setLength(0);
            f.seek(offset);
            f.write(data);
        }
        return new JSONObject();
    }

    /** A received part, from `offset`, up to 1 MiB at a time. */
    private static JSONObject read(Context c, JSONObject a) throws Exception {
        long offset = a.optLong("offset");
        int len = (int) Math.max(1, Math.min(a.optLong("len", 1 << 20), 1 << 20));
        Uri uri = Uri.parse("content://mms/part/" + a.getLong("part"));
        try (InputStream in = c.getContentResolver().openInputStream(uri)) {
            if (in == null) throw new Exception("No such attachment");
            for (long skipped = 0; skipped < offset; ) {
                long k = in.skip(offset - skipped);
                if (k <= 0) {
                    if (in.read() < 0) break;
                    k = 1;
                }
                skipped += k;
            }
            byte[] buf = new byte[len];
            int n = 0, k;
            while (n < len && (k = in.read(buf, n, len - n)) > 0) n += k;
            boolean eof = n < len || in.read() < 0;
            return new JSONObject().put("b64", Base64.encodeToString(buf, 0, n, Base64.NO_WRAP)).put("eof", eof);
        }
    }

    /** A received MMS's attachments: [{id, ct, name}], text and layout left out. */
    static JSONArray parts(Context c, long mid) {
        JSONArray list = new JSONArray();
        try (Cursor q = c.getContentResolver().query(Uri.parse("content://mms/part"),
                new String[]{"_id", "ct", "name", "cl", "fn"}, "mid = ?", new String[]{String.valueOf(mid)}, "seq")) {
            while (q != null && q.moveToNext()) {
                String ct = String.valueOf(q.getString(1)).toLowerCase();
                if (ct.equals("text/plain") || ct.equals("application/smil")) continue;
                String name = null;
                for (int i = 2; i <= 4 && (name == null || name.isEmpty()); i++) name = q.getString(i);
                list.put(new JSONObject().put("id", q.getLong(0)).put("ct", ct).put("name", name == null ? "" : name));
            }
        } catch (Exception ignored) {}
        return list;
    }

    /** How sending a message from the store goes, if it is not done: "sending" or "failed". */
    static String smsStatus(int type) {
        return type == Telephony.Sms.MESSAGE_TYPE_OUTBOX || type == Telephony.Sms.MESSAGE_TYPE_QUEUED ? "sending"
                : type == Telephony.Sms.MESSAGE_TYPE_FAILED ? "failed" : null;
    }
    static String mmsStatus(int box) {
        return box == Telephony.Mms.MESSAGE_BOX_OUTBOX ? "sending" : box == Telephony.Mms.MESSAGE_BOX_FAILED ? "failed" : null;
    }

    static final class Part {
        final String mime, name;
        final byte[] data;
        Part(String mime, String name, byte[] data) { this.mime = mime; this.name = name; this.data = data; }
    }

    private static JSONObject send(Context c, JSONObject a) throws Exception {
        String to = a.getString("to").replaceAll("[^0-9+*#]", "");
        if (to.isEmpty()) throw new Exception("No phone number to send to");
        SmsManager sms = Build.VERSION.SDK_INT >= 31 ? c.getSystemService(SmsManager.class) : SmsManager.getDefault();
        int max = DEFAULT_MAX;
        try {
            Bundle config = sms.getCarrierConfigValues();
            if (config != null) max = config.getInt(SmsManager.MMS_CONFIG_MAX_MESSAGE_SIZE, max);
        } catch (Exception ignored) {}
        File dir = dir(c);
        List<File> uploaded = new ArrayList<>();
        List<Part> parts = new ArrayList<>();
        String body = a.optString("body");
        if (!body.isEmpty()) parts.add(new Part("text/plain", "text.txt", body.getBytes(StandardCharsets.UTF_8)));
        JSONArray files = a.optJSONArray("parts");
        int count = files == null ? 0 : files.length();
        try {
            int images = 0, fixed = 1024;
            for (Part p : parts) fixed += p.data.length;
            List<Object[]> given = new ArrayList<>();
            for (int i = 0; i < count; i++) {
                JSONObject f = files.getJSONObject(i);
                File file = new File(dir, safe(f.getString("name")));
                uploaded.add(file);
                if (!file.isFile()) throw new Exception("An attachment did not arrive: send again");
                String mime = f.optString("mime", "application/octet-stream");
                byte[] data = Files.readAllBytes(file.toPath());
                boolean shrinkable = mime.startsWith("image/") && !mime.equals("image/gif");
                if (shrinkable) images++; else fixed += data.length;
                given.add(new Object[]{mime, file.getName(), data, shrinkable});
            }
            // Pictures share what the rest leaves; anything else goes as it is.
            int share = images == 0 ? 0 : (max - fixed) / images;
            for (Object[] g : given) {
                String mime = (String) g[0], name = (String) g[1];
                byte[] data = (byte[]) g[2];
                if ((Boolean) g[3] && data.length > share) {
                    if (share < 16 * 1024) throw new Exception("Too much for one MMS: send the pictures apart");
                    data = shrink(data, share);
                    mime = "image/jpeg";
                    name = name.replaceAll("\\.[^.]*$", "") + ".jpg";
                }
                parts.add(new Part(mime, name, data));
            }
            long total = 0;
            for (Part p : parts) total += p.data.length;
            if (total > max) {
                throw new Exception("Too large for MMS: " + total / 1024 + " KB, the carrier takes " + max / 1024 + " KB");
            }
            return submit(c, sms, pdu(to, parts, "T" + Long.toHexString(System.currentTimeMillis())));
        } finally {
            for (File f : uploaded) f.delete();
        }
    }

    /** Hands the PDU to the system and waits for the outcome, a while. */
    private static JSONObject submit(Context c, SmsManager sms, byte[] pdu) throws Exception {
        String name = "send-" + System.nanoTime() + ".pdu";
        File file = new File(dir(c), name);
        Files.write(file.toPath(), pdu);
        String action = c.getPackageName() + ".MMS_SENT." + name;
        CountDownLatch done = new CountDownLatch(1);
        int[] code = {Activity.RESULT_OK};
        BroadcastReceiver receiver = new BroadcastReceiver() {
            @Override public void onReceive(Context ctx, Intent i) {
                code[0] = getResultCode();
                done.countDown();
            }
        };
        if (Build.VERSION.SDK_INT >= 33) c.registerReceiver(receiver, new IntentFilter(action), Context.RECEIVER_NOT_EXPORTED);
        else c.registerReceiver(receiver, new IntentFilter(action));
        try {
            PendingIntent sent = PendingIntent.getBroadcast(c, 0, new Intent(action).setPackage(c.getPackageName()),
                    PendingIntent.FLAG_ONE_SHOT | PendingIntent.FLAG_IMMUTABLE);
            sms.sendMultimediaMessage(c, Uri.parse("content://" + c.getPackageName() + ".mms/" + name), null, null, sent);
            // The desktop waits 30 s for an answer; the phone's store tells the rest.
            if (!done.await(25, TimeUnit.SECONDS)) return new JSONObject().put("pending", true);
        } finally {
            c.unregisterReceiver(receiver);
        }
        file.delete();
        if (code[0] == Activity.RESULT_OK) return new JSONObject();
        throw new Exception(error(code[0]));
    }

    private static String error(int code) {
        switch (code) {
            case SmsManager.MMS_ERROR_NO_DATA_NETWORK: return "MMS needs mobile data: turn it on on the phone";
            case SmsManager.MMS_ERROR_INVALID_APN:
            case SmsManager.MMS_ERROR_UNABLE_CONNECT_MMS: return "The phone can't reach the carrier's MMS service";
            case SmsManager.MMS_ERROR_HTTP_FAILURE: return "The carrier refused the MMS";
            case SmsManager.MMS_ERROR_CONFIGURATION_ERROR: return "MMS is not set up for this SIM";
            default: return "MMS failed (code " + code + ")";
        }
    }

    /** A JPEG of the picture, turned upright, small enough for `budget` bytes. */
    private static byte[] shrink(byte[] data, int budget) throws Exception {
        BitmapFactory.Options bounds = new BitmapFactory.Options();
        bounds.inJustDecodeBounds = true;
        BitmapFactory.decodeByteArray(data, 0, data.length, bounds);
        int side = Math.max(bounds.outWidth, bounds.outHeight);
        if (side <= 0) throw new Exception("Can't read this picture");
        int degrees = 0;
        try {
            switch (new ExifInterface(new ByteArrayInputStream(data)).getAttributeInt(ExifInterface.TAG_ORIENTATION, 1)) {
                case ExifInterface.ORIENTATION_ROTATE_90: degrees = 90; break;
                case ExifInterface.ORIENTATION_ROTATE_180: degrees = 180; break;
                case ExifInterface.ORIENTATION_ROTATE_270: degrees = 270; break;
                default: break;
            }
        } catch (IOException ignored) {}
        for (int target = 1600; target >= 320; target = target * 3 / 4) {
            BitmapFactory.Options o = new BitmapFactory.Options();
            o.inSampleSize = 1;
            while (side / (o.inSampleSize * 2) >= target) o.inSampleSize *= 2;
            Bitmap b = BitmapFactory.decodeByteArray(data, 0, data.length, o);
            if (b == null) throw new Exception("Can't read this picture");
            float scale = Math.min(1f, target / (float) Math.max(b.getWidth(), b.getHeight()));
            Matrix m = new Matrix();
            m.postScale(scale, scale);
            m.postRotate(degrees);
            b = Bitmap.createBitmap(b, 0, 0, b.getWidth(), b.getHeight(), m, true);
            for (int q = 85; q >= 55; q -= 15) {
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                b.compress(Bitmap.CompressFormat.JPEG, q, out);
                if (out.size() <= budget) return out.toByteArray();
            }
        }
        throw new Exception("This picture can't be made small enough for MMS");
    }

    /**
     * An m-send-req (OMA MMS encapsulation 1.2): who to, then a
     * multipart/mixed body, each part with its type, name and location.
     */
    // ponytail: multipart/mixed with no SMIL layout; multipart/related with a
    // SMIL part if a carrier or a recipient's app refuses it.
    static byte[] pdu(String to, List<Part> parts, String transaction) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        o.write(0x8C); o.write(0x80);                  // X-Mms-Message-Type: m-send-req
        o.write(0x98); text(o, transaction);           // X-Mms-Transaction-ID
        o.write(0x8D); o.write(0x92);                  // X-Mms-MMS-Version: 1.2
        o.write(0x89); o.write(0x01); o.write(0x81);   // From: the phone's own, filled in by the network
        o.write(0x97); text(o, to + "/TYPE=PLMN");     // To
        o.write(0x84); o.write(0xA3);                  // Content-Type: application/vnd.wap.multipart.mixed
        uintvar(o, parts.size());
        for (Part p : parts) {
            ByteArrayOutputStream type = new ByteArrayOutputStream();
            text(type, p.mime);
            if (p.mime.startsWith("text/")) { type.write(0x81); type.write(0xEA); } // Charset: UTF-8
            type.write(0x85); text(type, p.name);      // Name
            ByteArrayOutputStream h = new ByteArrayOutputStream();
            valueLength(h, type.size());
            h.write(type.toByteArray(), 0, type.size());
            h.write(0x8E); text(h, p.name);            // Content-Location
            h.write(0xC0); text(h, "\"<" + p.name + ">"); // Content-ID, a quoted string
            uintvar(o, h.size());
            uintvar(o, p.data.length);
            o.write(h.toByteArray(), 0, h.size());
            o.write(p.data, 0, p.data.length);
        }
        return o.toByteArray();
    }

    /** A Text-string: quoted when it starts above ASCII, then NUL-ended. */
    private static void text(ByteArrayOutputStream o, String s) {
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        if (b.length > 0 && (b[0] & 0xFF) >= 0x80) o.write(0x7F);
        o.write(b, 0, b.length);
        o.write(0);
    }

    static void uintvar(ByteArrayOutputStream o, long v) {
        int n = 1;
        while (n < 10 && (v >>> (7 * n)) != 0) n++;
        for (int i = n - 1; i >= 0; i--) {
            int b = (int) ((v >>> (7 * i)) & 0x7F);
            o.write(i > 0 ? b | 0x80 : b);
        }
    }

    private static void valueLength(ByteArrayOutputStream o, int len) {
        if (len <= 30) o.write(len);
        else { o.write(31); uintvar(o, len); }
    }

    /** Serves the PDUs being sent to the system's MMS service, read-only. */
    public static final class Store extends ContentProvider {
        @Override public boolean onCreate() { return true; }
        @Override public ParcelFileDescriptor openFile(Uri uri, String mode) throws java.io.FileNotFoundException {
            File f = new File(dir(getContext()), safe(String.valueOf(uri.getLastPathSegment())));
            if (!f.getName().endsWith(".pdu")) throw new java.io.FileNotFoundException();
            return ParcelFileDescriptor.open(f, ParcelFileDescriptor.MODE_READ_ONLY);
        }
        @Override public String getType(Uri uri) { return "application/vnd.wap.mms-message"; }
        @Override public Cursor query(Uri u, String[] p, String s, String[] a, String o) { return null; }
        @Override public Uri insert(Uri u, ContentValues v) { return null; }
        @Override public int delete(Uri u, String s, String[] a) { return 0; }
        @Override public int update(Uri u, ContentValues v, String s, String[] a) { return 0; }
    }
}
