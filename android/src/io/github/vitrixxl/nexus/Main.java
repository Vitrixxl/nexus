package io.github.vitrixxl.nexus;

import android.Manifest;
import android.app.Activity;
import android.content.ComponentName;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.res.ColorStateList;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.graphics.drawable.RippleDrawable;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.os.Handler;
import android.os.Looper;
import android.os.PowerManager;
import android.provider.Settings;
import android.service.notification.NotificationListenerService;
import android.view.Gravity;
import android.view.View;
import android.widget.LinearLayout;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONArray;
import org.json.JSONObject;

/** Pairing, and the permissions the link needs; everything else runs in {@link Link}. */
public class Main extends Activity {
    private static final int BG = Color.rgb(13, 16, 20), SURFACE = Color.rgb(24, 29, 36),
            FG = Color.rgb(230, 232, 235), MUTED = Color.rgb(140, 148, 158), ACCENT = Color.rgb(122, 162, 247);
    private LinearLayout desktops, buttons;
    private float dp;
    private final Handler handler = new Handler(Looper.getMainLooper());
    private final Runnable tick = new Runnable() {
        @Override public void run() {
            showDesktops();
            handler.postDelayed(this, 1000);
        }
    };

    private GradientDrawable rounded(int color, float radius) {
        GradientDrawable d = new GradientDrawable();
        d.setColor(color);
        d.setCornerRadius(radius * dp);
        return d;
    }

    private TextView text(String s, float size, int color, boolean bold) {
        TextView t = new TextView(this);
        t.setText(s);
        t.setTextSize(size);
        t.setTextColor(color);
        if (bold) t.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        return t;
    }

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        dp = getResources().getDisplayMetrics().density;
        getWindow().setStatusBarColor(BG);
        getWindow().setNavigationBarColor(BG);
        // The system bars' insets pad the frame; the margins pad the content.
        android.widget.FrameLayout frame = new android.widget.FrameLayout(this);
        frame.setFitsSystemWindows(true);
        frame.setBackgroundColor(BG);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        int pad = (int) (24 * dp);
        root.setPadding(pad, pad, pad, pad);
        frame.addView(root);
        TextView title = text("Nexus", 34, FG, true);
        root.addView(title);
        TextView subtitle = text("Your phone, on your desktop", 15, MUTED, false);
        subtitle.setPadding(0, (int) (4 * dp), 0, pad);
        root.addView(subtitle);
        desktops = new LinearLayout(this);
        desktops.setOrientation(LinearLayout.VERTICAL);
        desktops.setBackground(rounded(SURFACE, 18));
        desktops.setPadding((int) (8 * dp), (int) (8 * dp), (int) (8 * dp), (int) (8 * dp));
        root.addView(desktops);
        buttons = new LinearLayout(this);
        buttons.setOrientation(LinearLayout.VERTICAL);
        buttons.setPadding(0, pad, 0, 0);
        root.addView(buttons);
        setContentView(frame);
        pair(getIntent());
    }

    @Override protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        pair(intent);
    }

    /** nexus://pair?host=…&port=…&token=…&name=…, from the page a desktop's QR code opens. */
    private void pair(Intent intent) {
        Uri u = intent == null ? null : intent.getData();
        if (u == null || !"pair".equals(u.getHost()) || u.getQueryParameter("token") == null) return;
        try {
            String host = u.getQueryParameter("host");
            int port = Integer.parseInt(u.getQueryParameter("port"));
            String name = u.getQueryParameter("name");
            JSONObject entry = new JSONObject().put("id", host + ":" + port).put("host", host).put("port", port)
                    .put("token", u.getQueryParameter("token")).put("name", name == null ? host : name);
            JSONArray list = Link.pairs(this), next = new JSONArray();
            for (int i = 0; i < list.length(); i++) {
                if (!list.getJSONObject(i).optString("id").equals(entry.getString("id"))) next.put(list.get(i));
            }
            next.put(entry);
            Link.savePairs(this, next);
            Link.use(this, entry.getString("id"));
        } catch (Exception ignored) {}
        NotificationListenerService.requestRebind(new ComponentName(this, Link.class));
    }

    /** A row per paired desktop: its state and an Unpair link. Tapping one connects to it. */
    private void showDesktops() {
        desktops.removeAllViews();
        JSONArray list = Link.pairs(this);
        java.util.Map<String, String> statuses = Link.statuses();
        String active = Link.active(this);
        if (list.length() == 0) {
            TextView none = text("Not paired. Scan the code in a desktop's Phone page.", 15, MUTED, false);
            none.setPadding((int) (10 * dp), (int) (10 * dp), (int) (10 * dp), (int) (10 * dp));
            desktops.addView(none);
            return;
        }
        for (int i = 0; i < list.length(); i++) {
            JSONObject p = list.optJSONObject(i);
            String id = p.optString("id"), state = !id.equals(active) ? "Tap to connect"
                    : statuses.getOrDefault(id, "Waiting for notification access");
            LinearLayout row = new LinearLayout(this);
            row.setGravity(Gravity.CENTER_VERTICAL);
            int edge = (int) (10 * dp);
            row.setPadding(edge, edge, edge, edge);
            row.setBackground(new RippleDrawable(ColorStateList.valueOf(Color.argb(40, 255, 255, 255)), null, rounded(Color.WHITE, 12)));
            row.setOnClickListener(v -> {
                Link.use(this, id);
                NotificationListenerService.requestRebind(new ComponentName(this, Link.class));
                showDesktops();
            });
            View dot = new View(this);
            GradientDrawable d = new GradientDrawable();
            d.setShape(GradientDrawable.OVAL);
            d.setColor(state.equals("Connected") ? Color.rgb(80, 200, 120)
                    : state.equals("Locked") || state.startsWith("Offline") ? Color.rgb(230, 180, 80) : MUTED);
            dot.setBackground(d);
            LinearLayout.LayoutParams size = new LinearLayout.LayoutParams((int) (10 * dp), (int) (10 * dp));
            size.setMarginEnd((int) (14 * dp));
            row.addView(dot, size);
            LinearLayout texts = new LinearLayout(this);
            texts.setOrientation(LinearLayout.VERTICAL);
            texts.addView(text(p.optString("name"), 16, FG, true));
            texts.addView(text(state, 13, MUTED, false));
            row.addView(texts, new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1));
            TextView unpair = text("Unpair", 14, Color.rgb(240, 113, 120), true);
            unpair.setPadding((int) (12 * dp), (int) (8 * dp), 0, (int) (8 * dp));
            unpair.setOnClickListener(v -> {
                JSONArray next = new JSONArray();
                JSONArray all = Link.pairs(this);
                for (int j = 0; j < all.length(); j++) {
                    if (!all.optJSONObject(j).optString("id").equals(id)) next.put(all.opt(j));
                }
                Link.savePairs(this, next);
                showDesktops();
            });
            row.addView(unpair);
            desktops.addView(row);
        }
    }

    @Override protected void onResume() {
        super.onResume();
        refresh();
        handler.post(tick);
    }

    @Override protected void onPause() {
        super.onPause();
        handler.removeCallbacks(tick);
    }

    private void refresh() {
        buttons.removeAllViews();
        String listeners = Settings.Secure.getString(getContentResolver(), "enabled_notification_listeners");
        if (listeners == null || !listeners.contains(getPackageName())) {
            add("Allow notifications", "Forward them to the desktop", new Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS));
        }
        boolean files = Build.VERSION.SDK_INT >= 30 ? Environment.isExternalStorageManager()
                : granted(Manifest.permission.WRITE_EXTERNAL_STORAGE);
        if (!files) {
            if (Build.VERSION.SDK_INT >= 30) {
                add("Allow all files", "Browse them from the desktop", new Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
                        Uri.parse("package:" + getPackageName())));
            } else {
                ask("Allow files", "Browse them from the desktop", Manifest.permission.READ_EXTERNAL_STORAGE, Manifest.permission.WRITE_EXTERNAL_STORAGE);
            }
        }
        if (!granted(Manifest.permission.READ_SMS)) {
            ask("Allow reading SMS", "See your conversations on the desktop", Manifest.permission.READ_SMS);
        }
        if (!granted(Manifest.permission.SEND_SMS)) {
            ask("Allow sending SMS", "Reply from the desktop", Manifest.permission.SEND_SMS);
        }
        if (!granted(Manifest.permission.READ_CONTACTS)) {
            ask("Show contact names", "Names instead of numbers in conversations", Manifest.permission.READ_CONTACTS);
        }
        if (!getSystemService(PowerManager.class).isIgnoringBatteryOptimizations(getPackageName())) {
            add("Stay connected", "Keep the link alive while the screen is off", new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:" + getPackageName())));
        }
        if (buttons.getChildCount() == 0) {
            TextView done = text("All set. Nexus runs in the background.", 15, MUTED, false);
            done.setPadding((int) (4 * dp), 0, 0, 0);
            buttons.addView(done);
        }
    }

    private boolean granted(String permission) {
        return checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED;
    }

    /** A rounded tile: the first one missing stands out in the accent colour. */
    private View button(String title, String detail) {
        boolean primary = buttons.getChildCount() == 0;
        LinearLayout b = new LinearLayout(this);
        b.setOrientation(LinearLayout.VERTICAL);
        int pad = (int) (18 * dp);
        b.setPadding(pad, (int) (16 * dp), pad, (int) (16 * dp));
        b.setBackground(new RippleDrawable(ColorStateList.valueOf(Color.argb(60, 255, 255, 255)),
                rounded(primary ? ACCENT : SURFACE, 16), null));
        b.addView(text(title, 16, primary ? BG : FG, true));
        TextView d = text(detail, 13, primary ? Color.argb(190, 13, 16, 20) : MUTED, false);
        d.setPadding(0, (int) (2 * dp), 0, 0);
        b.addView(d);
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT);
        lp.bottomMargin = (int) (10 * dp);
        buttons.addView(b, lp);
        return b;
    }

    private void add(String title, String detail, Intent intent) {
        button(title, detail).setOnClickListener(v -> startActivity(intent));
    }

    private void ask(String title, String detail, String... permissions) {
        button(title, detail).setOnClickListener(v -> requestPermissions(permissions, 1));
    }

    /**
     * A refusal without a dialog: Android restricts SMS permissions for apps
     * installed outside a store, so they are granted from the app's settings.
     */
    @Override public void onRequestPermissionsResult(int code, String[] permissions, int[] results) {
        for (int r : results) {
            if (r != PackageManager.PERMISSION_GRANTED) {
                Toast.makeText(this, "Turn it on in Permissions. Greyed out? Tap ⋮ › Allow restricted settings first.",
                        Toast.LENGTH_LONG).show();
                startActivity(new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:" + getPackageName())));
                break;
            }
        }
        refresh();
    }
}
