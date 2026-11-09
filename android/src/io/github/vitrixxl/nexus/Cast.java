package io.github.vitrixxl.nexus;

import android.app.Activity;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.graphics.Bitmap;
import android.graphics.PixelFormat;
import android.graphics.Point;
import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.Image;
import android.media.ImageReader;
import android.media.projection.MediaProjection;
import android.media.projection.MediaProjectionConfig;
import android.media.projection.MediaProjectionManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.util.DisplayMetrics;
import android.view.WindowManager;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.BufferedOutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;

/** Asks Android for the screen; Run then streams it to the desktop. */
public class Cast extends Activity {
    /** Starts streaming, or points a stream already running at the new viewer. */
    static void begin(Context c) {
        Run run = Run.self;
        if (run != null) run.reconnect();
        else c.startActivity(new Intent(c, Cast.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
    }

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        MediaProjectionManager m = getSystemService(MediaProjectionManager.class);
        // Otherwise Android 14 and later may offer to share a single app.
        startActivityForResult(Build.VERSION.SDK_INT >= 34
                ? m.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
                : m.createScreenCaptureIntent(), 1);
    }

    @Override protected void onActivityResult(int request, int result, Intent data) {
        if (result == RESULT_OK && data != null) {
            startForegroundService(new Intent(this, Run.class).putExtra("code", result).putExtra("data", data));
        }
        finish();
    }

    /** One JPEG per screen change: [length, screen width, screen height, bytes]. */
    public static class Run extends Service {
        static volatile Run self;
        private static final int MAX_SIDE = 1280, QUALITY = 60, FRAME_MS = 33, KEEP_ALIVE_MS = 3000;
        private MediaProjection projection;
        private volatile boolean running = true, again;
        private volatile Socket socket;

        @Override public IBinder onBind(Intent i) { return null; }

        void reconnect() {
            again = true;
            Socket s = socket;
            if (s != null) try { s.close(); } catch (Exception ignored) {}
        }

        @Override public int onStartCommand(Intent i, int flags, int id) {
            if (i == null || "stop".equals(i.getAction())) {
                stopSelf();
                return START_NOT_STICKY;
            }
            if (projection != null) return START_NOT_STICKY;
            NotificationManager nm = getSystemService(NotificationManager.class);
            nm.createNotificationChannel(new NotificationChannel("cast", "Screen sharing", NotificationManager.IMPORTANCE_LOW));
            Intent stop = new Intent(this, Run.class).setAction("stop");
            Notification n = new Notification.Builder(this, "cast").setSmallIcon(R.drawable.ic_fg)
                    .setContentTitle("Your screen is shared with your desktop").setOngoing(true)
                    .addAction(new Notification.Action.Builder(null, "Stop",
                            PendingIntent.getService(this, 0, stop, PendingIntent.FLAG_IMMUTABLE)).build())
                    .build();
            if (Build.VERSION.SDK_INT >= 29) startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION);
            else startForeground(1, n);
            self = this;
            projection = getSystemService(MediaProjectionManager.class)
                    .getMediaProjection(i.getIntExtra("code", 0), i.getParcelableExtra("data"));
            projection.registerCallback(new MediaProjection.Callback() {
                @Override public void onStop() { stopSelf(); }
            }, new Handler(Looper.getMainLooper()));
            new Thread(this::stream, "nexus-cast").start();
            return START_NOT_STICKY;
        }

        @Override public void onDestroy() {
            self = null;
            running = false;
            reconnect();
            if (projection != null) projection.stop();
            super.onDestroy();
        }

        private Point screen() {
            Point p = new Point();
            getSystemService(WindowManager.class).getDefaultDisplay().getRealSize(p);
            return p;
        }

        private static ImageReader reader(Point s) {
            int longest = Math.max(s.x, s.y), scale = Math.max(longest, MAX_SIDE);
            return ImageReader.newInstance((s.x * MAX_SIDE / scale) & ~1, (s.y * MAX_SIDE / scale) & ~1, PixelFormat.RGBA_8888, 2);
        }

        private void stream() {
            Point size = screen();
            ImageReader images = reader(size);
            DisplayMetrics metrics = getResources().getDisplayMetrics();
            VirtualDisplay display = projection.createVirtualDisplay("nexus", images.getWidth(), images.getHeight(), metrics.densityDpi,
                    DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, images.getSurface(), null, new Handler(Looper.getMainLooper()));
            byte[] last = null;
            try {
                do {
                    again = false;
                    try (Socket s = connect(); DataOutputStream out = new DataOutputStream(new BufferedOutputStream(s.getOutputStream(), 1 << 16))) {
                        socket = s;
                        if (again) continue;
                        long sent = 0;
                        while (running) {
                            Point now = screen();
                            if (!now.equals(size)) { // Rotated.
                                size = now;
                                ImageReader old = images;
                                images = reader(size);
                                display.resize(images.getWidth(), images.getHeight(), metrics.densityDpi);
                                display.setSurface(images.getSurface());
                                old.close();
                            }
                            byte[] frame = null;
                            try (Image image = images.acquireLatestImage()) {
                                if (image != null) frame = jpeg(image);
                            }
                            long time = System.currentTimeMillis();
                            if (frame != null) last = frame;
                            // The desktop learns a viewer left, or the phone did, by this.
                            if (last != null && (frame != null || time - sent > KEEP_ALIVE_MS)) {
                                out.writeInt(last.length);
                                out.writeInt(size.x);
                                out.writeInt(size.y);
                                out.write(last);
                                out.flush();
                                sent = time;
                            }
                            Thread.sleep(FRAME_MS);
                        }
                    } catch (Exception ignored) {
                        // The viewer left, unless it was replaced.
                    } finally {
                        socket = null;
                    }
                } while (running && again);
            } finally {
                display.release();
                images.close();
                new Handler(Looper.getMainLooper()).post(this::stopSelf);
            }
        }

        private Socket connect() throws Exception {
            JSONArray list = Link.pairs(this);
            String active = Link.active(this);
            for (int i = 0; i < list.length(); i++) {
                JSONObject p = list.getJSONObject(i);
                if (!p.optString("id").equals(active)) continue;
                Socket s = new Socket();
                s.connect(new InetSocketAddress(p.optString("host"), p.optInt("port", 47913)), 10000);
                s.setTcpNoDelay(true);
                String hello = new JSONObject().put("t", "cast").put("token", p.optString("token")) + "\n";
                s.getOutputStream().write(hello.getBytes(StandardCharsets.UTF_8));
                return s;
            }
            throw new Exception("not paired");
        }

        private static byte[] jpeg(Image image) {
            Image.Plane plane = image.getPlanes()[0];
            ByteBuffer pixels = plane.getBuffer();
            // Rows may be padded; compressing the padding would show as a stripe.
            Bitmap padded = Bitmap.createBitmap(plane.getRowStride() / plane.getPixelStride(), image.getHeight(), Bitmap.Config.ARGB_8888);
            padded.copyPixelsFromBuffer(pixels);
            Bitmap exact = padded.getWidth() == image.getWidth() ? padded : Bitmap.createBitmap(padded, 0, 0, image.getWidth(), image.getHeight());
            ByteArrayOutputStream out = new ByteArrayOutputStream(1 << 17);
            exact.compress(Bitmap.CompressFormat.JPEG, QUALITY, out);
            return out.toByteArray();
        }
    }
}
