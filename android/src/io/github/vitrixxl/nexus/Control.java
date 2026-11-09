package io.github.vitrixxl.nexus;

import android.accessibilityservice.AccessibilityService;
import android.accessibilityservice.GestureDescription;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.graphics.Bitmap;
import android.graphics.Path;
import android.graphics.Point;
import android.graphics.Rect;
import android.hardware.HardwareBuffer;
import android.os.Build;
import android.os.Bundle;
import android.os.PowerManager;
import android.util.Base64;
import android.view.Display;
import android.view.WindowManager;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Semaphore;
import java.util.concurrent.TimeUnit;

/** What the desktop, and agents through it, see and do on this screen. */
public class Control extends AccessibilityService {
    static volatile Control self;
    private static final int MAX_NODES = 2000;
    /** One reply must fit the daemon's 1 MiB line, in base64. */
    private static final int MAX_JPEG = 700 * 1024;
    private final Semaphore shooting = new Semaphore(1);

    @Override protected void onServiceConnected() { self = this; }
    @Override public boolean onUnbind(Intent i) { self = null; return false; }
    @Override public void onDestroy() { self = null; super.onDestroy(); }
    @Override public void onAccessibilityEvent(AccessibilityEvent e) {}
    @Override public void onInterrupt() {}

    static JSONObject op(String op, JSONObject a) throws Exception {
        Control c = self;
        if (c == null) throw new Exception("Turn on Nexus in the phone's Accessibility settings");
        JSONObject ok = new JSONObject();
        switch (op) {
            case "screen_shot": return c.shot(a.optInt("max"), a.optInt("q", 80));
            case "screen_tree": return c.tree();
            case "screen_tap": c.tap(a, 50); return ok;
            case "screen_hold": c.tap(a, 650); return ok;
            case "screen_swipe":
                c.stroke((float) a.getDouble("x"), (float) a.getDouble("y"),
                        (float) a.getDouble("x2"), (float) a.getDouble("y2"), a.optLong("ms", 300));
                return ok;
            case "screen_text": c.insert(a.getString("text")); return ok;
            case "screen_key": c.key(a.getString("key")); return ok;
            case "screen_apps": return c.apps();
            case "screen_open": c.open(a.getString("package")); return ok;
            case "cast_start": Cast.begin(c); return ok;
            default: throw new Exception("Unknown request");
        }
    }

    private Point size() {
        Point p = new Point();
        getSystemService(WindowManager.class).getDefaultDisplay().getRealSize(p);
        return p;
    }

    /** ponytail: Android allows about 3 of these a second; the live stream is Cast. */
    private JSONObject shot(int max, int quality) throws Exception {
        if (Build.VERSION.SDK_INT < 30) throw new Exception("Screenshots need Android 11");
        shooting.acquire();
        try {
            Bitmap full = capture();
            Point size = size();
            int longest = Math.max(full.getWidth(), full.getHeight());
            Bitmap image = max > 0 && longest > max
                    ? Bitmap.createScaledBitmap(full, full.getWidth() * max / longest, full.getHeight() * max / longest, true)
                    : full;
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            for (int q = quality; ; q -= 15) {
                out.reset();
                image.compress(Bitmap.CompressFormat.JPEG, q, out);
                if (out.size() <= MAX_JPEG || q <= 20) break;
            }
            return new JSONObject().put("b64", Base64.encodeToString(out.toByteArray(), Base64.NO_WRAP))
                    .put("w", size.x).put("h", size.y);
        } finally { shooting.release(); }
    }

    private Bitmap capture() throws Exception {
        Bitmap[] done = new Bitmap[1];
        int[] error = {0};
        for (int attempt = 0; attempt < 8; attempt++) {
            CountDownLatch latch = new CountDownLatch(1);
            error[0] = 0;
            takeScreenshot(Display.DEFAULT_DISPLAY, getMainExecutor(), new TakeScreenshotCallback() {
                @Override public void onSuccess(ScreenshotResult r) {
                    try (HardwareBuffer b = r.getHardwareBuffer()) {
                        Bitmap wrapped = Bitmap.wrapHardwareBuffer(b, r.getColorSpace());
                        if (wrapped != null) done[0] = wrapped.copy(Bitmap.Config.ARGB_8888, false);
                    }
                    latch.countDown();
                }
                @Override public void onFailure(int code) { error[0] = code; latch.countDown(); }
            });
            if (!latch.await(5, TimeUnit.SECONDS)) throw new Exception("The screenshot timed out");
            if (done[0] != null) return done[0];
            if (error[0] != ERROR_TAKE_SCREENSHOT_INTERVAL_TIME_SHORT) break;
            Thread.sleep(120);
        }
        throw new Exception("Android refused the screenshot (" + error[0] + ")");
    }

    /** Every window, top one first, with what can be read or used in it. */
    private JSONObject tree() throws Exception {
        Point size = size();
        JSONArray windows = new JSONArray();
        int[] left = {MAX_NODES};
        for (AccessibilityWindowInfo w : getWindows()) {
            AccessibilityNodeInfo root = w.getRoot();
            if (root == null) continue;
            JSONArray nodes = new JSONArray();
            walk(root, nodes, left);
            CharSequence title = w.getTitle();
            windows.put(new JSONObject().put("app", String.valueOf(root.getPackageName()))
                    .put("title", title == null ? "" : title.toString()).put("active", w.isActive())
                    .put("nodes", nodes));
        }
        return new JSONObject().put("w", size.x).put("h", size.y).put("windows", windows);
    }

    private void walk(AccessibilityNodeInfo n, JSONArray out, int[] left) throws Exception {
        if (left[0] <= 0 || !n.isVisibleToUser()) return;
        CharSequence text = n.getText(), desc = n.getContentDescription();
        JSONArray can = new JSONArray();
        if (n.isClickable()) can.put("tap");
        if (n.isLongClickable()) can.put("hold");
        if (n.isScrollable()) can.put("scroll");
        if (n.isEditable()) can.put("type");
        if (text != null || desc != null || can.length() > 0) {
            Rect r = new Rect();
            n.getBoundsInScreen(r);
            JSONObject o = new JSONObject().put("box", new JSONArray().put(r.left).put(r.top).put(r.right).put(r.bottom));
            if (text != null && text.length() > 0) o.put("text", text.toString());
            if (desc != null && desc.length() > 0) o.put("desc", desc.toString());
            if (n.getViewIdResourceName() != null) o.put("id", n.getViewIdResourceName());
            CharSequence cls = n.getClassName();
            if (cls != null) o.put("class", cls.toString().substring(cls.toString().lastIndexOf('.') + 1));
            if (can.length() > 0) o.put("can", can);
            if (n.isCheckable()) o.put("checked", n.isChecked());
            if (n.isFocused()) o.put("focused", true);
            if (!n.isEnabled()) o.put("disabled", true);
            out.put(o);
            left[0]--;
        }
        for (int i = 0; i < n.getChildCount(); i++) {
            AccessibilityNodeInfo child = n.getChild(i);
            if (child != null) walk(child, out, left);
        }
    }

    private void tap(JSONObject a, long ms) throws Exception {
        float x = (float) a.getDouble("x"), y = (float) a.getDouble("y");
        stroke(x, y, x, y, ms);
    }

    private void stroke(float x, float y, float x2, float y2, long ms) throws Exception {
        wake();
        Path path = new Path();
        path.moveTo(x, y);
        path.lineTo(x2, y2);
        GestureDescription g = new GestureDescription.Builder()
                .addStroke(new GestureDescription.StrokeDescription(path, 0, Math.max(1, ms))).build();
        CountDownLatch latch = new CountDownLatch(1);
        boolean[] finished = {false};
        boolean started = dispatchGesture(g, new GestureResultCallback() {
            @Override public void onCompleted(GestureDescription d) { finished[0] = true; latch.countDown(); }
            @Override public void onCancelled(GestureDescription d) { latch.countDown(); }
        }, null);
        if (!started || !latch.await(ms + 5000, TimeUnit.MILLISECONDS) || !finished[0]) {
            throw new Exception("The gesture did not go through");
        }
    }

    private void wake() {
        PowerManager power = getSystemService(PowerManager.class);
        if (power.isInteractive()) return;
        power.newWakeLock(PowerManager.SCREEN_BRIGHT_WAKE_LOCK | PowerManager.ACQUIRE_CAUSES_WAKEUP, "nexus:control").acquire(3000);
    }

    private AccessibilityNodeInfo field() throws Exception {
        AccessibilityNodeInfo n = findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        if (n == null || !n.isEditable()) throw new Exception("No text field has the focus: tap one first");
        return n;
    }

    private static String content(AccessibilityNodeInfo n) {
        return n.getText() == null || n.isShowingHintText() ? "" : n.getText().toString();
    }

    /** At the cursor, over the selection. */
    private void insert(String text) throws Exception {
        AccessibilityNodeInfo n = field();
        String now = content(n);
        int a = Math.min(n.getTextSelectionStart(), n.getTextSelectionEnd());
        int b = Math.max(n.getTextSelectionStart(), n.getTextSelectionEnd());
        if (a < 0 || b > now.length()) a = b = now.length();
        replace(n, now.substring(0, a) + text + now.substring(b), a + text.length());
    }

    private void backspace() throws Exception {
        AccessibilityNodeInfo n = field();
        String now = content(n);
        int a = Math.min(n.getTextSelectionStart(), n.getTextSelectionEnd());
        int b = Math.max(n.getTextSelectionStart(), n.getTextSelectionEnd());
        if (a < 0 || b > now.length()) a = b = now.length();
        if (a == b) a = Math.max(0, now.offsetByCodePoints(b, b > 0 ? -1 : 0));
        replace(n, now.substring(0, a) + now.substring(b), a);
    }

    private void replace(AccessibilityNodeInfo n, String text, int caret) throws Exception {
        Bundle args = new Bundle();
        args.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text);
        if (!n.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) throw new Exception("The field refused the text");
        Bundle at = new Bundle();
        at.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, caret);
        at.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, caret);
        n.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, at);
    }

    private void key(String key) throws Exception {
        int action;
        switch (key) {
            case "back": action = GLOBAL_ACTION_BACK; break;
            case "home": action = GLOBAL_ACTION_HOME; break;
            case "recents": action = GLOBAL_ACTION_RECENTS; break;
            case "notifications": action = GLOBAL_ACTION_NOTIFICATIONS; break;
            case "quick": action = GLOBAL_ACTION_QUICK_SETTINGS; break;
            case "lock":
                if (Build.VERSION.SDK_INT < 28) throw new Exception("Locking needs Android 9");
                action = GLOBAL_ACTION_LOCK_SCREEN;
                break;
            case "wake": wake(); return;
            case "backspace": backspace(); return;
            case "enter":
                if (Build.VERSION.SDK_INT < 30) throw new Exception("Enter needs Android 11");
                if (!field().performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.getId())) {
                    throw new Exception("The field refused Enter");
                }
                return;
            default: throw new Exception("Unknown key: " + key);
        }
        if (!performGlobalAction(action)) throw new Exception("Android refused " + key);
    }

    private JSONObject apps() throws Exception {
        PackageManager pm = getPackageManager();
        Intent launcher = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER);
        JSONArray list = new JSONArray();
        for (ResolveInfo r : pm.queryIntentActivities(launcher, 0)) {
            list.put(new JSONObject().put("package", r.activityInfo.packageName).put("name", r.loadLabel(pm).toString()));
        }
        return new JSONObject().put("apps", list);
    }

    private void open(String pkg) throws Exception {
        Intent i = getPackageManager().getLaunchIntentForPackage(pkg);
        if (i == null) throw new Exception("No app: " + pkg);
        wake();
        startActivity(i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
    }
}
