package io.github.vitrixxl.nexus;

import android.Manifest;
import android.app.Activity;
import android.app.AlertDialog;
import android.content.ComponentName;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.res.ColorStateList;
import android.content.res.Configuration;
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
import android.text.InputType;
import android.view.Gravity;
import android.view.View;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONArray;
import org.json.JSONObject;

/**
 * Pairing, the link's state and the permissions it needs; everything else runs
 * in {@link Link}. Drawn in the desktop's colours, light or dark as the system is.
 */
public class Main extends Activity {
    /** Nexus's palette: background, surface, text, secondary text, accent, error. */
    private int bg, surface, fg, muted, line;
    private static final int ACCENT = Color.rgb(10, 132, 255);
    private int error, warn;
    private LinearLayout status, notes, desktops, setup;
    /** The desktop holding a different APK from this one, if any. */
    private JSONObject update;
    /** The notes row's line, redrawn when it changes. */
    private String notesDrawn = "";
    private float dp;
    /** What the desktops' part shows, so that it is redrawn only when it changes. */
    private String drawn = "";
    private final Handler handler = new Handler(Looper.getMainLooper());
    private final Runnable tick = new Runnable() {
        @Override public void run() {
            showDesktops();
            showNotes();
            handler.postDelayed(this, 1000);
        }
    };

    private int px(float v) { return (int) (v * dp); }

    private GradientDrawable rounded(int color, float radius) {
        GradientDrawable d = new GradientDrawable();
        d.setColor(color);
        d.setCornerRadius(radius * dp);
        return d;
    }

    /** A surface that answers touch. */
    private RippleDrawable pressable(int color, float radius) {
        return new RippleDrawable(ColorStateList.valueOf((fg & 0x00ffffff) | 0x22000000), rounded(color, radius), rounded(Color.WHITE, radius));
    }

    private TextView text(String s, float size, int color, boolean bold) {
        TextView t = new TextView(this);
        t.setText(s);
        t.setTextSize(size);
        t.setTextColor(color);
        if (bold) t.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        return t;
    }

    private LinearLayout column() {
        LinearLayout l = new LinearLayout(this);
        l.setOrientation(LinearLayout.VERTICAL);
        return l;
    }

    private TextView heading(String s) {
        TextView t = text(s, 13, muted, true);
        t.setPadding(px(4), px(24), 0, px(8));
        return t;
    }

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        dp = getResources().getDisplayMetrics().density;
        boolean dark = (getResources().getConfiguration().uiMode & Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES;
        bg = dark ? Color.rgb(20, 20, 22) : Color.rgb(242, 242, 245);
        surface = dark ? Color.rgb(31, 31, 34) : Color.WHITE;
        fg = dark ? Color.rgb(244, 244, 245) : Color.rgb(29, 29, 31);
        muted = dark ? Color.rgb(180, 180, 187) : Color.rgb(98, 98, 106);
        line = dark ? Color.argb(20, 255, 255, 255) : Color.argb(20, 0, 0, 0);
        error = dark ? Color.rgb(255, 69, 58) : Color.rgb(215, 0, 21);
        warn = dark ? Color.rgb(255, 214, 10) : Color.rgb(178, 128, 0);
        getWindow().setStatusBarColor(bg);
        getWindow().setNavigationBarColor(bg);
        if (!dark) {
            getWindow().getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR | View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR);
        }
        // The system bars' insets pad the frame; the margins pad the content.
        ScrollView frame = new ScrollView(this);
        frame.setFitsSystemWindows(true);
        frame.setBackgroundColor(bg);
        LinearLayout root = column();
        int pad = px(20);
        root.setPadding(pad, px(28), pad, pad);
        frame.addView(root);
        root.addView(text("Nexus", 30, fg, true));
        TextView subtitle = text("Your phone, on your desktop", 15, muted, false);
        subtitle.setPadding(0, px(2), 0, px(20));
        root.addView(subtitle);
        status = column();
        root.addView(status);
        root.addView(heading("Notes"));
        notes = column();
        notes.setBackground(rounded(surface, 16));
        root.addView(notes);
        root.addView(heading("Desktops"));
        desktops = column();
        desktops.setBackground(rounded(surface, 16));
        root.addView(desktops);
        root.addView(heading("Setup"));
        setup = column();
        root.addView(setup);
        setContentView(frame);
        pair(getIntent() == null ? null : getIntent().getData());
    }

    @Override protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        pair(intent.getData());
    }

    /**
     * nexus://pair?host=…&port=…&token=…&name=…, from the page a desktop's QR
     * code opens; or http://host:port/pair?t=…, the link the desktop copies.
     */
    private boolean pair(Uri u) {
        if (u == null) return false;
        try {
            String host, token, name;
            int port;
            if ("nexus".equals(u.getScheme()) && "pair".equals(u.getHost())) {
                host = u.getQueryParameter("host");
                port = Integer.parseInt(u.getQueryParameter("port"));
                token = u.getQueryParameter("token");
                name = u.getQueryParameter("name");
            } else if ("http".equals(u.getScheme()) && "/pair".equals(u.getPath())) {
                host = u.getHost();
                port = u.getPort() > 0 ? u.getPort() : 47913;
                token = u.getQueryParameter("t");
                // The desktop says its name on connecting.
                name = null;
            } else {
                return false;
            }
            if (host == null || token == null || token.length() < 32) return false;
            JSONObject entry = new JSONObject().put("id", host + ":" + port).put("host", host).put("port", port)
                    .put("token", token).put("name", name == null ? host : name);
            JSONArray list = Link.pairs(this), next = new JSONArray();
            for (int i = 0; i < list.length(); i++) {
                if (!list.getJSONObject(i).optString("id").equals(entry.getString("id"))) next.put(list.get(i));
            }
            next.put(entry);
            Link.savePairs(this, next);
            Link.use(this, entry.getString("id"));
        } catch (Exception e) {
            return false;
        }
        NotificationListenerService.requestRebind(new ComponentName(this, Link.class));
        drawn = "";
        return true;
    }

    /** Pasting the link the desktop's Phone page copies. */
    private void askLink() {
        EditText field = new EditText(this);
        field.setHint("http://100.x.y.z:47913/pair?t=…");
        field.setInputType(InputType.TYPE_TEXT_VARIATION_URI);
        field.setSingleLine(true);
        LinearLayout box = column();
        box.setPadding(px(20), px(8), px(20), 0);
        box.addView(field);
        new AlertDialog.Builder(this)
                .setTitle("Pair with a link")
                .setMessage("On the desktop: Settings › Phone › Copy link. Paste it here.")
                .setView(box)
                .setNegativeButton("Cancel", null)
                .setPositiveButton("Pair", (d, w) -> {
                    if (!pair(Uri.parse(field.getText().toString().trim()))) {
                        Toast.makeText(this, "That is not a Nexus pairing link. Copy it again from the desktop.", Toast.LENGTH_LONG).show();
                    }
                    showDesktops();
                })
                .show();
    }

    /**
     * What a link state means and what to do: a title, a line and a colour.
     * The states are {@link Link}'s.
     */
    private String[] describe(String state, String desktop) {
        if (state.equals("Connected")) return new String[]{"Connected to " + desktop, "Notifications, files, messages and the clipboard are shared.", "on"};
        if (state.startsWith("Connecting")) return new String[]{"Connecting to " + desktop + "…", "Over Tailscale.", "wait"};
        if (state.equals("Locked")) return new String[]{desktop + " is locked", "It reconnects once the desktop unlocks.", "wait"};
        if (state.startsWith("Offline")) return new String[]{"Can't reach " + desktop, "Check that Tailscale is on, on both devices. This retries by itself.", "bad"};
        if (state.startsWith("Refused")) return new String[]{desktop + " refused this phone", "It no longer knows it: scan its code again in Settings › Phone.", "bad"};
        if (state.startsWith("Another phone")) return new String[]{"Another phone took over", "Tap " + desktop + " below to take it back.", "off"};
        if (state.startsWith("Disconnected")) return new String[]{"Disconnected", "Tap " + desktop + " below to reconnect.", "off"};
        return new String[]{"Waiting for notification access", "Allow it under Setup: the link runs inside it.", "wait"};
    }

    private int color(String tone) {
        switch (tone) {
            case "on": return ACCENT;
            case "bad": return error;
            case "wait": return warn;
            default: return muted;
        }
    }

    private View dot(int color, int size) {
        View dot = new View(this);
        GradientDrawable d = new GradientDrawable();
        d.setShape(GradientDrawable.OVAL);
        d.setColor(color);
        dot.setBackground(d);
        dot.setLayoutParams(new LinearLayout.LayoutParams(px(size), px(size)));
        return dot;
    }

    /** The state of the link on top, then a row per paired desktop. Tapping one connects to it. */
    private void showDesktops() {
        JSONArray list = Link.pairs(this);
        java.util.Map<String, String> statuses = Link.statuses();
        String active = Link.active(this);
        String now = list + "|" + statuses + "|" + active;
        if (now.equals(drawn)) return;
        drawn = now;
        status.removeAllViews();
        desktops.removeAllViews();
        LinearLayout card = column();
        card.setBackground(rounded(surface, 16));
        card.setPadding(px(18), px(18), px(18), px(18));
        if (list.length() == 0) {
            card.addView(text("Not paired", 18, fg, true));
            TextView how = text("On your computer, open Settings › Phone and scan the code with this phone's camera.", 14, muted, false);
            how.setPadding(0, px(4), 0, 0);
            card.addView(how);
            status.addView(card);
            desktops.addView(row("Pair with a link", "Paste the link the desktop copies", null, this::askLink, null));
            return;
        }
        String activeName = "";
        for (int i = 0; i < list.length(); i++) {
            JSONObject p = list.optJSONObject(i);
            if (p.optString("id").equals(active)) activeName = p.optString("name");
        }
        String[] d = describe(statuses.getOrDefault(active, ""), activeName);
        LinearLayout top = new LinearLayout(this);
        top.setGravity(Gravity.CENTER_VERTICAL);
        View big = dot(color(d[2]), 10);
        ((LinearLayout.LayoutParams) big.getLayoutParams()).setMarginEnd(px(10));
        top.addView(big);
        top.addView(text(d[0], 18, fg, true), new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1));
        card.addView(top);
        TextView hint = text(d[1], 14, muted, false);
        hint.setPadding(px(20), px(4), 0, 0);
        card.addView(hint);
        status.addView(card);

        for (int i = 0; i < list.length(); i++) {
            JSONObject p = list.optJSONObject(i);
            String id = p.optString("id"), name = p.optString("name");
            boolean current = id.equals(active);
            String state = current ? describe(statuses.getOrDefault(id, ""), name)[0] : "Tap to connect";
            if (current && state.startsWith("Connected")) state = "Connected";
            View r = row(name, state, current ? color(describe(statuses.getOrDefault(id, ""), name)[2]) : null, () -> {
                Link.use(this, id);
                NotificationListenerService.requestRebind(new ComponentName(this, Link.class));
                drawn = "";
                showDesktops();
            }, () -> new AlertDialog.Builder(this)
                    .setTitle("Unpair " + name + "?")
                    .setMessage("This phone stops sharing with it until you scan its code again.")
                    .setNegativeButton("Cancel", null)
                    .setPositiveButton("Unpair", (dialog, w) -> {
                        JSONArray next = new JSONArray();
                        JSONArray all = Link.pairs(this);
                        for (int j = 0; j < all.length(); j++) {
                            if (!all.optJSONObject(j).optString("id").equals(id)) next.put(all.opt(j));
                        }
                        Link.savePairs(this, next);
                        drawn = "";
                        showDesktops();
                    }).show());
            desktops.addView(r);
        }
        desktops.addView(row("Pair with a link", "Paste the link the desktop copies", null, this::askLink, null));
    }

    /** Nexus Notes: how many pages, and where the sync stands. Opens them. */
    private void showNotes() {
        NotesStore store = Link.notes(this);
        // ponytail: walks the notes folder each second while this screen shows; cache the count if notes grow to thousands.
        String[] line = Notes.syncLine(this, store);
        int pages = store.changedSince(0);
        String detail = (pages == 0 ? "No pages yet" : pages + (pages == 1 ? " page" : " pages")) + " · " + line[0];
        if (detail.equals(notesDrawn)) return;
        notesDrawn = detail;
        notes.removeAllViews();
        notes.addView(row("Notes", detail, color(line[1]), () -> startActivity(new Intent(this, Notes.class)), null));
    }

    /** A row in a group: a title, a line under it, a state dot, and an Unpair action. */
    private View row(String title, String detail, Integer tone, Runnable tap, Runnable unpair) {
        LinearLayout r = new LinearLayout(this);
        r.setGravity(Gravity.CENTER_VERTICAL);
        r.setMinimumHeight(px(56));
        r.setPadding(px(16), px(12), px(8), px(12));
        r.setBackground(pressable(surface, 16));
        r.setOnClickListener(v -> tap.run());
        if (tone != null) {
            View dot = dot(tone, 8);
            ((LinearLayout.LayoutParams) dot.getLayoutParams()).setMarginEnd(px(14));
            r.addView(dot);
        }
        LinearLayout texts = column();
        texts.addView(text(title, 16, fg, true));
        texts.addView(text(detail, 13, muted, false));
        r.addView(texts, new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1));
        if (unpair != null) {
            TextView u = text("Unpair", 14, muted, true);
            u.setGravity(Gravity.CENTER);
            u.setMinWidth(px(48));
            u.setMinHeight(px(48));
            u.setPadding(px(12), 0, px(12), 0);
            u.setBackground(pressable(surface, 12));
            u.setOnClickListener(v -> unpair.run());
            r.addView(u);
        }
        return r;
    }

    @Override protected void onResume() {
        super.onResume();
        refresh();
        new Thread(() -> {
            JSONObject found = Update.available(this);
            runOnUiThread(() -> {
                if ((found == null) == (update == null)) return;
                update = found;
                refresh();
            });
        }).start();
        drawn = "";
        notesDrawn = "";
        handler.post(tick);
    }

    @Override protected void onPause() {
        super.onPause();
        handler.removeCallbacks(tick);
    }

    private void refresh() {
        setup.removeAllViews();
        if (update != null) {
            JSONObject from = update;
            button("Update Nexus", "A new version is on " + from.optString("name", "your desktop")).setOnClickListener(v -> Update.start(this, from));
        }
        String listeners = Settings.Secure.getString(getContentResolver(), "enabled_notification_listeners");
        if (listeners == null || !listeners.contains(getPackageName())) {
            add("Allow notification access", "The link runs inside it, and forwards notifications", new Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS));
        }
        if (Control.self == null) {
            add("Allow screen control", "Under Downloaded apps, turn on Nexus. Your desktop and its agents can then see and use this screen", new Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS));
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
        if (!granted(Manifest.permission.CALL_PHONE) || !granted(Manifest.permission.READ_PHONE_STATE) || !granted(Manifest.permission.ANSWER_PHONE_CALLS)) {
            ask("Allow phone calls", "Place and hang up calls from the desktop", Manifest.permission.CALL_PHONE, Manifest.permission.READ_PHONE_STATE, Manifest.permission.ANSWER_PHONE_CALLS);
        }
        if (!granted(Manifest.permission.READ_CONTACTS)) {
            ask("Show contact names", "Names instead of numbers in conversations", Manifest.permission.READ_CONTACTS);
        }
        if (!getSystemService(PowerManager.class).isIgnoringBatteryOptimizations(getPackageName())) {
            add("Stay connected", "Keep the link alive while the screen is off", new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:" + getPackageName())));
        }
        if (Build.VERSION.SDK_INT >= 33 && !getSharedPreferences("pair", MODE_PRIVATE).getBoolean("tile", false)) {
            button("Add the clipboard tile", "Send what you copy here to the desktop").setOnClickListener(v ->
                    getSystemService(android.app.StatusBarManager.class).requestAddTileService(
                            new ComponentName(this, ClipTile.class), "Send clipboard",
                            android.graphics.drawable.Icon.createWithResource(this, R.drawable.ic_fg), getMainExecutor(), result -> {
                                // 1: already there, 2: added.
                                if (result == 1 || result == 2) tileAdded(this, true);
                                refresh();
                            }));
        }
        if (setup.getChildCount() == 0) {
            TextView done = text("All set. Nexus runs in the background.", 15, muted, false);
            done.setPadding(px(4), 0, 0, 0);
            setup.addView(done);
        }
    }

    static void tileAdded(android.content.Context c, boolean added) {
        c.getSharedPreferences("pair", MODE_PRIVATE).edit().putBoolean("tile", added).apply();
    }

    private boolean granted(String permission) {
        return checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED;
    }

    /** A rounded tile: the first one missing stands out in the accent colour. */
    private View button(String title, String detail) {
        boolean primary = setup.getChildCount() == 0;
        LinearLayout b = column();
        b.setPadding(px(18), px(14), px(18), px(14));
        b.setBackground(pressable(primary ? ACCENT : surface, 16));
        b.addView(text(title, 16, primary ? Color.WHITE : fg, true));
        TextView d = text(detail, 13, primary ? Color.argb(215, 255, 255, 255) : muted, false);
        d.setPadding(0, px(2), 0, 0);
        b.addView(d);
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT);
        lp.bottomMargin = px(8);
        setup.addView(b, lp);
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
