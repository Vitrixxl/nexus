package io.github.vitrixxl.nexus;

import android.app.Activity;
import android.app.AlertDialog;
import android.app.NotificationManager;
import android.app.assist.AssistStructure;
import android.content.ClipData;
import android.content.ClipDescription;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.res.ColorStateList;
import android.content.res.Configuration;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.graphics.drawable.RippleDrawable;
import android.hardware.biometrics.BiometricManager;
import android.hardware.biometrics.BiometricPrompt;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.CancellationSignal;
import android.os.Handler;
import android.os.Looper;
import android.os.PersistableBundle;
import android.provider.Settings;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyPermanentlyInvalidatedException;
import android.security.keystore.KeyProperties;
import android.service.autofill.FillResponse;
import android.text.Editable;
import android.text.InputType;
import android.text.TextUtils;
import android.text.TextWatcher;
import android.text.format.DateFormat;
import android.util.Base64;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowManager;
import android.view.autofill.AutofillManager;
import android.view.inputmethod.EditorInfo;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.RadioButton;
import android.widget.RadioGroup;
import android.widget.ScrollView;
import android.widget.Switch;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONArray;
import org.json.JSONObject;

import java.security.KeyStore;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;

/**
 * The passwords: create, unlock, browse, edit and sync settings, drawn into one
 * activity. Also the unlock the autofill service opens, and the answer to a
 * desktop's sync question.
 */
public class Vault extends Activity {
    private static final int ACCENT = Color.rgb(10, 132, 255);
    private static final String BIO_ALIAS = "nexus_vault_bio";
    private static final long CLIP_MS = 30_000;

    private static volatile int visible;
    /** Control refuses the screen while this is true: FLAG_SECURE hides pixels, not the accessibility tree. */
    static boolean onScreen() { return visible > 0; }

    private static final Handler MAIN = new Handler(Looper.getMainLooper());
    private static String clipped;
    private static Runnable clipClear;

    /** Marks it sensitive, and clears it after 30 s if it is still ours. */
    static void copy(Context c, String value, String what) {
        ClipboardManager cm = c.getSystemService(ClipboardManager.class);
        ClipData clip = ClipData.newPlainText(what, value);
        if (Build.VERSION.SDK_INT >= 33) {
            PersistableBundle extras = new PersistableBundle();
            extras.putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true);
            clip.getDescription().setExtras(extras);
        }
        cm.setPrimaryClip(clip);
        clipped = value;
        if (clipClear != null) MAIN.removeCallbacks(clipClear);
        Context app = c.getApplicationContext();
        // ponytail: lives in the process, which notification access keeps running; AlarmManager if the process ever dies first.
        clipClear = () -> {
            ClipboardManager board = app.getSystemService(ClipboardManager.class);
            ClipData now = board.getPrimaryClip();
            // Android hides the clipboard from background apps: then clear it regardless.
            if (now != null && now.getItemCount() > 0 && !String.valueOf(now.getItemAt(0).getText()).equals(clipped)) { clipped = null; return; }
            if (Build.VERSION.SDK_INT >= 28) board.clearPrimaryClip();
            else board.setPrimaryClip(ClipData.newPlainText("", ""));
            clipped = null;
        };
        MAIN.postDelayed(clipClear, CLIP_MS);
    }

    private int bg, surface, fg, muted, danger;
    private float dp;
    private FrameLayout frame;
    private VaultStore store;
    private final Handler handler = new Handler(Looper.getMainLooper());
    /** The list while unlocked; cleared on lock. */
    private List<VaultStore.Entry> entries;
    private long seen = -1;
    private boolean inside;
    private Runnable onBack, redraw;
    private LinearLayout head;
    private String search = "";
    private final Runnable tick = new Runnable() {
        @Override public void run() {
            if (inside && !store.unlocked()) { entries = null; route(); }
            else if (inside && redraw != null && store.generation != seen) { redraw.run(); }
            handler.postDelayed(this, 1000);
        }
    };

    private boolean autofillMode() { return getIntent().getBooleanExtra("autofill", false); }
    private boolean saveMode() { return getIntent().getBooleanExtra("save", false); }

    private int px(float v) { return (int) (v * dp); }

    private GradientDrawable rounded(int color, float radius) {
        GradientDrawable d = new GradientDrawable();
        d.setColor(color);
        d.setCornerRadius(radius * dp);
        return d;
    }

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

    private ImageView icon(int res, int color, float size, String name, Runnable tap) {
        ImageView i = new ImageView(this);
        i.setImageResource(res);
        i.setColorFilter(color);
        i.setScaleType(ImageView.ScaleType.CENTER);
        i.setContentDescription(name);
        i.setBackground(pressable(Color.TRANSPARENT, size / 2));
        i.setLayoutParams(new LinearLayout.LayoutParams(px(size), px(size)));
        i.setOnClickListener(v -> tap.run());
        return i;
    }

    private TextView heading(String s) {
        TextView t = text(s, 13, muted, true);
        t.setPadding(px(4), px(20), 0, px(8));
        t.setAccessibilityHeading(true);
        return t;
    }

    private View gap(int h) {
        View v = new View(this);
        v.setLayoutParams(new LinearLayout.LayoutParams(1, px(h)));
        return v;
    }

    /** A full-width action; the accent one is the screen's main action. */
    private TextView button(String label, boolean primary, Runnable tap) {
        TextView b = text(label, 16, primary ? Color.WHITE : fg, true);
        b.setGravity(Gravity.CENTER);
        b.setMinHeight(px(52));
        b.setPadding(px(16), 0, px(16), 0);
        b.setBackground(pressable(primary ? ACCENT : surface, 14));
        b.setOnClickListener(v -> tap.run());
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        lp.topMargin = px(12);
        b.setLayoutParams(lp);
        return b;
    }

    /** A field that won't teach the keyboard what it types, nor reach another autofill. */
    private EditText field(String hint, int type) {
        EditText e = new EditText(this);
        e.setHint(hint);
        e.setHintTextColor(muted);
        e.setTextColor(fg);
        e.setTextSize(16);
        e.setInputType(type);
        // The password variations force monospace, hint included.
        e.setTypeface(Typeface.DEFAULT);
        e.setSingleLine((type & InputType.TYPE_TEXT_FLAG_MULTI_LINE) == 0);
        e.setImeOptions(EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING);
        e.setBackground(null);
        e.setPadding(px(14), px(14), px(14), px(14));
        e.setMinHeight(px(52));
        e.setImportantForAutofill(View.IMPORTANT_FOR_AUTOFILL_NO);
        return e;
    }

    /** A label over a surface holding `content`, read together by TalkBack. */
    private LinearLayout labelled(String label, View content, View focus) {
        LinearLayout box = column();
        TextView t = text(label, 13, muted, true);
        t.setPadding(px(4), px(14), 0, px(6));
        if (focus.getId() == View.NO_ID) focus.setId(View.generateViewId());
        t.setLabelFor(focus.getId());
        box.addView(t);
        content.setBackground(rounded(surface, 14));
        box.addView(content);
        return box;
    }

    /** A password field with a show/hide eye. */
    private LinearLayout passwordRow(EditText e) {
        LinearLayout r = new LinearLayout(this);
        r.setGravity(Gravity.CENTER_VERTICAL);
        r.addView(e, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
        ImageView eye = icon(R.drawable.ic_eye, muted, 48, "Show password", () -> {});
        eye.setOnClickListener(v -> {
            boolean show = (e.getInputType() & InputType.TYPE_MASK_VARIATION) == InputType.TYPE_TEXT_VARIATION_PASSWORD;
            int caret = e.getSelectionEnd();
            e.setInputType(InputType.TYPE_CLASS_TEXT | (show ? InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD : InputType.TYPE_TEXT_VARIATION_PASSWORD));
            e.setTypeface(show ? Typeface.MONOSPACE : Typeface.DEFAULT);
            e.setSelection(Math.max(0, caret));
            eye.setContentDescription(show ? "Hide password" : "Show password");
            eye.setColorFilter(show ? ACCENT : muted);
        });
        r.addView(eye);
        return r;
    }

    private static final int PASSWORD = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD;

    private TextView error() {
        TextView t = text("", 14, danger, false);
        t.setPadding(px(4), px(8), px(4), 0);
        t.setVisibility(View.GONE);
        t.setAccessibilityLiveRegion(View.ACCESSIBILITY_LIVE_REGION_POLITE);
        return t;
    }

    private void say(TextView t, String message) { t.setText(message); t.setVisibility(View.VISIBLE); }

    private static char[] chars(EditText e) {
        Editable t = e.getText();
        char[] c = new char[t.length()];
        t.getChars(0, c.length, c, 0);
        return c;
    }

    /** Starts a screen: the bar on top, a fixed part under it, then a scrolling body. */
    private LinearLayout page(String title, Runnable back, View... actions) {
        inside = false;
        redraw = null;
        onBack = back;
        frame.removeAllViews();
        LinearLayout screen = column();
        head = column();
        head.setPadding(px(8), px(4), px(8), 0);
        LinearLayout bar = new LinearLayout(this);
        bar.setGravity(Gravity.CENTER_VERTICAL);
        bar.setMinimumHeight(px(52));
        if (back != null) bar.addView(icon(R.drawable.ic_back, fg, 48, "Back", this::onBackPressed));
        TextView t = text(title, 20, fg, true);
        t.setAccessibilityHeading(true);
        t.setSingleLine(true);
        t.setEllipsize(TextUtils.TruncateAt.END);
        t.setPadding(px(back == null ? 12 : 4), 0, 0, 0);
        bar.addView(t, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
        for (View a : actions) bar.addView(a);
        head.addView(bar);
        screen.addView(head);
        ScrollView scroll = new ScrollView(this);
        scroll.setFillViewport(true);
        LinearLayout body = column();
        body.setPadding(px(16), px(4), px(16), px(24));
        scroll.addView(body);
        screen.addView(scroll, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1));
        frame.addView(screen);
        return body;
    }

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_SECURE);
        dp = getResources().getDisplayMetrics().density;
        boolean dark = (getResources().getConfiguration().uiMode & Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES;
        bg = dark ? Color.rgb(20, 20, 22) : Color.rgb(242, 242, 245);
        surface = dark ? Color.rgb(31, 31, 34) : Color.WHITE;
        fg = dark ? Color.rgb(244, 244, 245) : Color.rgb(29, 29, 31);
        muted = dark ? Color.rgb(180, 180, 187) : Color.rgb(98, 98, 106);
        danger = dark ? Color.rgb(255, 69, 58) : Color.rgb(215, 0, 21);
        getWindow().setStatusBarColor(bg);
        getWindow().setNavigationBarColor(bg);
        if (!dark) getWindow().getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR | View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR);
        frame = new FrameLayout(this);
        frame.setFitsSystemWindows(true);
        frame.setBackgroundColor(bg);
        frame.setImportantForAutofill(View.IMPORTANT_FOR_AUTOFILL_NO_EXCLUDE_DESCENDANTS);
        setContentView(frame);
        store = VaultStore.get(this);
        String ask = getIntent().getStringExtra("ask");
        if (ask != null) showAsk(ask);
        else route();
    }

    @Override protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        String ask = intent.getStringExtra("ask");
        if (ask != null) showAsk(ask);
    }

    @Override protected void onStart() { super.onStart(); visible++; }
    @Override protected void onStop() { super.onStop(); visible--; }

    @Override protected void onResume() {
        super.onResume();
        if (inside && !store.unlocked()) { entries = null; route(); }
        handler.post(tick);
    }

    @Override protected void onPause() {
        super.onPause();
        handler.removeCallbacks(tick);
    }

    @Override public void onUserInteraction() { store.touch(); }

    @Override public void onBackPressed() {
        if (onBack != null) onBack.run();
        else if (autofillMode()) { setResult(RESULT_CANCELED); finish(); }
        else finish();
    }

    /** Back to the top: what the vault's state calls for. */
    private void route() {
        if (!store.exists()) showCreate();
        else if (!store.unlocked()) showLock();
        else if (autofillMode() || saveMode()) done();
        else showList();
    }

    /** The unlock an autofill request or a save was waiting for. */
    private void done() {
        try {
            if (saveMode()) {
                VaultAutofill.finishSave(this);
                setResult(RESULT_OK);
                finish();
                return;
            }
            AssistStructure structure = VaultAutofill.structure(getIntent());
            FillResponse r = structure == null ? null : VaultAutofill.response(this, structure);
            if (r != null) setResult(RESULT_OK, new Intent().putExtra(AutofillManager.EXTRA_AUTHENTICATION_RESULT, r));
            else setResult(RESULT_CANCELED);
        } catch (Exception e) {
            setResult(RESULT_CANCELED);
        }
        finish();
    }

    // Create ------------------------------------------------------------------

    private void showCreate() {
        LinearLayout body = page("Passwords", null);
        TextView why = text("Create a master password. It opens your passwords on this phone, and on any desktop you sync with. Nexus can't recover it if it's lost.", 15, muted, false);
        why.setPadding(px(4), px(4), px(4), 0);
        body.addView(why);
        EditText first = field("At least 8 characters", PASSWORD), again = field("Type it again", PASSWORD);
        body.addView(labelled("Master password", passwordRow(first), first));
        body.addView(labelled("Repeat", passwordRow(again), again));
        TextView err = error();
        body.addView(err);
        TextView create = button("Create vault", true, () -> {});
        create.setOnClickListener(v -> {
            char[] a = chars(first), b = chars(again);
            if (a.length < 8) { say(err, "Use at least 8 characters."); return; }
            if (!Arrays.equals(a, b)) { say(err, "The two passwords don't match."); return; }
            Arrays.fill(b, '\0');
            busy(create, "Creating…", () -> store.create(a), a, null, msg -> say(err, msg));
        });
        body.addView(create);
        if (Link.pairs(this).length() > 0) {
            body.addView(heading("Already have a vault on a desktop?"));
            body.addView(button("Sync settings", false, this::showSettings));
        }
    }

    /** Runs the slow work off the main thread, then the fresh route; `done` on success if given. */
    private void busy(TextView action, String label, Work work, char[] wipe, char[] wipe2, java.util.function.Consumer<String> failed) {
        CharSequence was = action.getText();
        action.setEnabled(false);
        action.setText(label);
        new Thread(() -> {
            String message = null;
            try { work.run(); }
            catch (VaultStore.WrongPassword e) { message = "That isn't the master password."; }
            catch (Exception e) { message = e.getMessage() == null ? "That didn't work." : e.getMessage(); }
            finally {
                if (wipe != null) Arrays.fill(wipe, '\0');
                if (wipe2 != null) Arrays.fill(wipe2, '\0');
            }
            String result = message;
            runOnUiThread(() -> {
                if (result == null) { route(); return; }
                action.setEnabled(true);
                action.setText(was);
                failed.accept(result);
            });
        }).start();
    }
    private interface Work { void run() throws Exception; }

    // Unlock ------------------------------------------------------------------

    private void showLock() {
        entries = null;
        LinearLayout body = page(autofillMode() ? "Unlock to fill" : saveMode() ? "Unlock to save" : "Passwords", null);
        TextView why = text(autofillMode() ? "Enter your master password to fill this form." : saveMode() ? "Enter your master password to save this login." : "Locked. Enter your master password.", 15, muted, false);
        why.setPadding(px(4), px(4), px(4), 0);
        body.addView(why);
        EditText field = field("Master password", PASSWORD);
        field.setImeOptions(EditorInfo.IME_ACTION_GO | EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING);
        body.addView(labelled("Master password", passwordRow(field), field));
        TextView err = error();
        body.addView(err);
        TextView unlock = button("Unlock", true, () -> {});
        View.OnClickListener act = v -> {
            char[] p = chars(field);
            if (p.length == 0) { say(err, "Enter your master password."); return; }
            busy(unlock, "Unlocking…", () -> store.unlock(p), p, null, msg -> say(err, msg));
        };
        unlock.setOnClickListener(act);
        field.setOnEditorActionListener((v, a, k) -> { if (unlock.isEnabled()) act.onClick(v); return true; });
        body.addView(unlock);
        boolean bio = bioReady();
        if (bio) body.addView(button("Use fingerprint", false, () -> bioUnlock(err)));
        if (bioStale) {
            bioStale = false;
            say(err, "Your fingerprints changed on this phone, so fingerprint unlock was turned off. Use your master password, then turn it on again in Settings.");
        }
        if (bio && !autoPrompted) {
            autoPrompted = true;
            handler.post(() -> bioUnlock(err));
        }
        field.requestFocus();
    }

    // Fingerprint ----------------------------------------------------------------

    private boolean autoPrompted, bioStale;

    private SharedPreferences bio() { return getSharedPreferences("vault_bio", MODE_PRIVATE); }

    /** The wrap belongs to this vault: a vault adopted from a desktop has another key. */
    private boolean bioReady() {
        return Build.VERSION.SDK_INT >= 28 && bio().contains("ct") && bio().getString("id", "").equals(store.id());
    }

    private void bioClear() {
        try {
            KeyStore ks = KeyStore.getInstance("AndroidKeyStore");
            ks.load(null);
            if (ks.containsAlias(BIO_ALIAS)) ks.deleteEntry(BIO_ALIAS);
        } catch (Exception ignored) {}
        bio().edit().clear().apply();
    }

    private void prompt(Cipher c, java.util.function.Consumer<Cipher> done, java.util.function.Consumer<String> failed, String title) {
        BiometricPrompt.Builder b = new BiometricPrompt.Builder(this).setTitle(title);
        if (Build.VERSION.SDK_INT >= 30) b.setAllowedAuthenticators(BiometricManager.Authenticators.BIOMETRIC_STRONG);
        b.setNegativeButton("Use master password", getMainExecutor(), (d, w) -> {});
        b.build().authenticate(new BiometricPrompt.CryptoObject(c), new CancellationSignal(), getMainExecutor(), new BiometricPrompt.AuthenticationCallback() {
            @Override public void onAuthenticationSucceeded(BiometricPrompt.AuthenticationResult r) { done.accept(c); }
            @Override public void onAuthenticationError(int code, CharSequence why) {
                if (code != BiometricPrompt.BIOMETRIC_ERROR_USER_CANCELED && code != BiometricPrompt.BIOMETRIC_ERROR_CANCELED) failed.accept(String.valueOf(why));
            }
        });
    }

    private void bioUnlock(TextView err) {
        byte[] iv = Base64.decode(bio().getString("iv", ""), Base64.NO_WRAP);
        byte[] ct = Base64.decode(bio().getString("ct", ""), Base64.NO_WRAP);
        Cipher c;
        try {
            KeyStore ks = KeyStore.getInstance("AndroidKeyStore");
            ks.load(null);
            SecretKey key = (SecretKey) ks.getKey(BIO_ALIAS, null);
            if (key == null) throw new KeyPermanentlyInvalidatedException();
            c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, iv));
        } catch (KeyPermanentlyInvalidatedException e) {
            bioClear();
            bioStale = true;
            route();
            return;
        } catch (Exception e) {
            say(err, "Fingerprint unlock isn't available right now. Use your master password.");
            return;
        }
        prompt(c, done -> {
            byte[] vk = null;
            try {
                vk = done.doFinal(ct);
                store.unlockWith(vk);
                route();
            } catch (Exception e) {
                bioClear();
                bioStale = true;
                route();
            } finally {
                if (vk != null) Arrays.fill(vk, (byte) 0);
            }
        }, msg -> say(err, "Fingerprint: " + msg), "Unlock passwords");
    }

    private void bioEnable(Switch toggle, TextView err) {
        byte[] vk = null;
        try {
            KeyGenerator g = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore");
            KeyGenParameterSpec.Builder b = new KeyGenParameterSpec.Builder(BIO_ALIAS, KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT)
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).setKeySize(256)
                    .setUserAuthenticationRequired(true).setInvalidatedByBiometricEnrollment(true);
            if (Build.VERSION.SDK_INT >= 30) b.setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG);
            g.init(b.build());
            SecretKey key = g.generateKey();
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, key);
            vk = store.keyCopy();
            byte[] plain = vk;
            prompt(c, done -> {
                try {
                    byte[] sealed = done.doFinal(plain);
                    bio().edit().putString("iv", Base64.encodeToString(done.getIV(), Base64.NO_WRAP))
                            .putString("ct", Base64.encodeToString(sealed, Base64.NO_WRAP)).putString("id", store.id()).apply();
                    toggle.setChecked(true);
                    say(err, "Fingerprint unlock is on.");
                } catch (Exception e) {
                    bioClear();
                    toggle.setChecked(false);
                    say(err, "Fingerprint unlock could not be set up.");
                } finally {
                    Arrays.fill(plain, (byte) 0);
                }
            }, msg -> {
                Arrays.fill(plain, (byte) 0);
                bioClear();
                toggle.setChecked(false);
                say(err, "Fingerprint: " + msg);
            }, "Turn on fingerprint unlock");
        } catch (Exception e) {
            if (vk != null) Arrays.fill(vk, (byte) 0);
            bioClear();
            toggle.setChecked(false);
            say(err, "Add a fingerprint in the phone's Settings first, then try again.");
        }
    }

    // The list ----------------------------------------------------------------

    private void load() throws Exception {
        entries = store.list();
        seen = store.generation;
    }

    private void showList() {
        LinearLayout body = page("Passwords", null,
                icon(R.drawable.ic_add, fg, 48, "Add password", () -> showEdit(new VaultStore.Entry(), this::showList)),
                icon(R.drawable.ic_settings, fg, 48, "Settings", this::showSettings));
        inside = true;
        EditText q = field("Search", InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
        q.setCompoundDrawablePadding(px(10));
        q.setBackground(rounded(surface, 14));
        q.setCompoundDrawablesRelativeWithIntrinsicBounds(R.drawable.ic_search, 0, 0, 0);
        q.setCompoundDrawableTintList(ColorStateList.valueOf(muted));
        q.setText(search);
        q.setContentDescription("Search passwords");
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        lp.setMargins(px(8), 0, px(8), px(4));
        head.addView(q, lp);
        LinearLayout rows = column();
        body.addView(rows);
        Runnable fill = () -> fillList(rows, body);
        q.addTextChangedListener(new Watch(() -> { search = q.getText().toString(); fill.run(); }));
        redraw = fill;
        fill.run();
        onBack = null;
    }

    private static final class Watch implements TextWatcher {
        final Runnable on;
        Watch(Runnable on) { this.on = on; }
        @Override public void beforeTextChanged(CharSequence s, int a, int b, int c) {}
        @Override public void onTextChanged(CharSequence s, int a, int b, int c) {}
        @Override public void afterTextChanged(Editable s) { on.run(); }
    }

    private void fillList(LinearLayout rows, LinearLayout body) {
        rows.removeAllViews();
        try { load(); } catch (Exception e) {
            rows.addView(state("Can't read the vault", "It's locked or damaged. Lock it and try again.", null, null));
            return;
        }
        String needle = search.trim().toLowerCase();
        List<VaultStore.Entry> shown = new ArrayList<>();
        for (VaultStore.Entry e : entries) {
            if (needle.isEmpty() || e.title.toLowerCase().contains(needle) || e.username.toLowerCase().contains(needle) || e.urls.toString().toLowerCase().contains(needle)) shown.add(e);
        }
        if (store.damaged > 0) {
            TextView warn = text(store.damaged + (store.damaged == 1 ? " entry couldn't be read." : " entries couldn't be read."), 13, danger, false);
            warn.setPadding(px(4), px(8), 0, px(8));
            rows.addView(warn);
        }
        if (entries.isEmpty()) {
            rows.addView(state("No passwords yet", "Add one here, or sync with a desktop in Settings.", "Add password", () -> showEdit(new VaultStore.Entry(), this::showList)));
            return;
        }
        if (shown.isEmpty()) {
            rows.addView(state("No results", "Nothing matches \"" + search.trim() + "\".", null, null));
            return;
        }
        LinearLayout group = column();
        group.setBackground(rounded(surface, 16));
        for (VaultStore.Entry e : shown) {
            group.addView(row(e.title.isEmpty() ? "(no title)" : e.title, e.username, () -> showDetail(e.id)));
        }
        rows.addView(group);
    }

    private LinearLayout state(String title, String detail, String action, Runnable tap) {
        LinearLayout s = column();
        s.setGravity(Gravity.CENTER_HORIZONTAL);
        s.setPadding(px(16), px(48), px(16), px(16));
        TextView t = text(title, 18, fg, true);
        t.setGravity(Gravity.CENTER);
        s.addView(t);
        TextView d = text(detail, 14, muted, false);
        d.setGravity(Gravity.CENTER);
        d.setPadding(0, px(6), 0, 0);
        s.addView(d);
        if (action != null) {
            TextView b = button(action, true, tap);
            s.addView(b);
        }
        return s;
    }

    private View row(String title, String detail, Runnable tap) {
        LinearLayout r = column();
        r.setMinimumHeight(px(56));
        r.setGravity(Gravity.CENTER_VERTICAL);
        r.setPadding(px(16), px(10), px(16), px(10));
        r.setBackground(pressable(surface, 16));
        r.setOnClickListener(v -> tap.run());
        TextView t = text(title, 16, fg, true);
        t.setSingleLine(true);
        t.setEllipsize(TextUtils.TruncateAt.END);
        r.addView(t);
        if (!detail.isEmpty()) {
            TextView d = text(detail, 13, muted, false);
            d.setSingleLine(true);
            d.setEllipsize(TextUtils.TruncateAt.END);
            r.addView(d);
        }
        return r;
    }

    // Detail ------------------------------------------------------------------

    private void showDetail(String id) {
        VaultStore.Entry e;
        try { e = store.get(id); } catch (Exception x) { showList(); return; }
        LinearLayout body = page(e.title.isEmpty() ? "(no title)" : e.title, this::showList);
        inside = true;
        body.addView(detailRow("Username", e.username.isEmpty() ? "None" : e.username, false, e.username.isEmpty() ? null : e.username));
        body.addView(detailRow("Password", e.password, true, e.password));
        if (!e.urls.isEmpty()) body.addView(detailRow("Websites and apps", TextUtils.join("\n", e.urls), false, null));
        if (!e.notes.isEmpty()) body.addView(detailRow("Notes", e.notes, false, null));
        if (e.history.length() > 0) {
            body.addView(heading("Previous passwords"));
            LinearLayout group = column();
            group.setBackground(rounded(surface, 16));
            for (int i = 0; i < e.history.length(); i++) {
                JSONObject h = e.history.optJSONObject(i);
                if (h == null) continue;
                String was = h.optString("password");
                String date = DateFormat.getMediumDateFormat(this).format(new java.util.Date(h.optLong("changed")));
                group.addView(row("Changed " + date, "Tap to copy", () -> {
                    copy(this, was, "Password");
                    Toast.makeText(this, "Copied. Clears in 30 seconds.", Toast.LENGTH_SHORT).show();
                }));
            }
            body.addView(group);
        }
        body.addView(gap(8));
        body.addView(button("Edit", false, () -> showEdit(e, () -> showDetail(id))));
        body.addView(button("Delete", false, () -> confirmDelete(e)));
    }

    private View detailRow(String label, String value, boolean secret, String copyValue) {
        LinearLayout r = new LinearLayout(this);
        r.setGravity(Gravity.CENTER_VERTICAL);
        LinearLayout t = column();
        t.setPadding(px(14), px(12), px(14), px(12));
        TextView v = text(secret ? mask(value) : value, 16, fg, false);
        v.setTextIsSelectable(!secret);
        t.addView(v);
        r.addView(t, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
        if (secret) {
            ImageView eye = icon(R.drawable.ic_eye, muted, 48, "Show password", () -> {});
            eye.setOnClickListener(x -> {
                boolean show = v.getText().toString().equals(mask(value));
                v.setText(show ? value : mask(value));
                v.setTypeface(show ? Typeface.MONOSPACE : Typeface.DEFAULT);
                eye.setContentDescription(show ? "Hide password" : "Show password");
                eye.setColorFilter(show ? ACCENT : muted);
            });
            r.addView(eye);
        }
        if (copyValue != null) {
            r.addView(icon(R.drawable.ic_copy, muted, 48, "Copy " + label.toLowerCase(), () -> {
                copy(this, copyValue, label);
                Toast.makeText(this, secret ? "Copied. Clears in 30 seconds." : "Copied.", Toast.LENGTH_SHORT).show();
            }));
        }
        return labelled(label, r, v);
    }

    private static String mask(String value) { return value.isEmpty() ? "None" : "••••••••••••"; }

    private void confirmDelete(VaultStore.Entry e) {
        new AlertDialog.Builder(this)
                .setTitle("Delete " + (e.title.isEmpty() ? "this password" : e.title) + "?")
                .setMessage("It disappears on your synced devices too.")
                .setNegativeButton("Cancel", null)
                .setPositiveButton("Delete", (d, w) -> {
                    try { store.delete(e.id); showList(); } catch (Exception x) { Toast.makeText(this, "Could not delete it.", Toast.LENGTH_LONG).show(); }
                }).show();
    }

    // Edit ---------------------------------------------------------------------

    private int genLength = 20;
    private boolean genSymbols = true;

    private void showEdit(VaultStore.Entry e, Runnable back) {
        boolean creating = e.id == null;
        EditText title = field("Title", InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_CAP_WORDS);
        EditText user = field("Username or email", InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
        EditText pass = field("Password", PASSWORD);
        EditText urls = field("https://example.com, one per line", InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI | InputType.TYPE_TEXT_FLAG_MULTI_LINE);
        urls.setSingleLine(false);
        urls.setMinLines(2);
        EditText notes = field("Optional", InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
        notes.setSingleLine(false);
        notes.setMinLines(2);
        title.setText(e.title);
        user.setText(e.username);
        pass.setText(e.password);
        urls.setText(TextUtils.join("\n", e.urls));
        notes.setText(e.notes);
        String[] before = {snapshot(title, user, pass, urls, notes)};
        Runnable cancel = () -> {
            if (snapshot(title, user, pass, urls, notes).equals(before[0])) { back.run(); return; }
            new AlertDialog.Builder(this).setTitle("Discard changes?").setNegativeButton("Keep editing", null)
                    .setPositiveButton("Discard", (d, w) -> back.run()).show();
        };
        LinearLayout body = page(creating ? "New password" : "Edit password", cancel);
        inside = true;
        body.addView(labelled("Title", title, title));
        body.addView(labelled("Username", user, user));
        body.addView(labelled("Password", passwordRow(pass), pass));
        // The generator sits with the field it fills.
        TextView length = text("", 14, fg, false);
        length.setMinWidth(px(92));
        length.setGravity(Gravity.CENTER);
        length.setAccessibilityLiveRegion(View.ACCESSIBILITY_LIVE_REGION_POLITE);
        Runnable shown = () -> length.setText(genLength + " characters");
        shown.run();
        LinearLayout gen = new LinearLayout(this);
        gen.setGravity(Gravity.CENTER_VERTICAL);
        gen.setPadding(0, px(4), 0, 0);
        gen.addView(step("−", "Shorter", () -> { genLength = Math.max(8, genLength - 4); shown.run(); }));
        gen.addView(length);
        gen.addView(step("+", "Longer", () -> { genLength = Math.min(64, genLength + 4); shown.run(); }));
        View space = new View(this);
        gen.addView(space, new LinearLayout.LayoutParams(0, 1, 1));
        Switch symbols = new Switch(this);
        symbols.setText("Symbols");
        symbols.setTextColor(muted);
        symbols.setSwitchPadding(px(8));
        symbols.setChecked(genSymbols);
        symbols.setMinHeight(px(48));
        symbols.setOnCheckedChangeListener((b, on) -> genSymbols = on);
        gen.addView(symbols);
        body.addView(gen);
        TextView make = button("Generate password", false, () -> {
            pass.setText(VaultStore.generate(genLength, genSymbols));
            pass.setSelection(pass.getText().length());
        });
        make.setCompoundDrawablesRelativeWithIntrinsicBounds(R.drawable.ic_dice, 0, 0, 0);
        make.setCompoundDrawableTintList(ColorStateList.valueOf(ACCENT));
        make.setCompoundDrawablePadding(px(10));
        body.addView(make);
        body.addView(labelled("Websites and apps", urls, urls));
        TextView hint = text("Websites match their domain. For an app, add androidapp://com.example.app.", 13, muted, false);
        hint.setPadding(px(4), px(6), px(4), 0);
        body.addView(hint);
        body.addView(labelled("Notes", notes, notes));
        TextView err = error();
        body.addView(err);
        TextView save = button("Save", true, () -> {});
        save.setOnClickListener(v -> {
            String t = title.getText().toString().trim();
            String typed = pass.getText().toString();
            if (t.isEmpty() && urls.getText().toString().trim().isEmpty()) { say(err, "Give it a title or a website."); return; }
            if (typed.isEmpty()) { say(err, "Enter a password, or generate one."); return; }
            List<String> list = new ArrayList<>();
            for (String u : urls.getText().toString().split("\n")) {
                String trimmed = u.trim();
                if (!trimmed.isEmpty()) list.add(trimmed.contains("://") ? trimmed : "https://" + trimmed);
            }
            String host = list.isEmpty() ? null : Uri.parse(list.get(0)).getHost();
            e.title = !t.isEmpty() ? t : host != null ? host : list.get(0);
            e.username = user.getText().toString().trim();
            e.password = typed;
            e.notes = notes.getText().toString();
            e.urls = list;
            try {
                String id = store.save(e);
                if (creating) e.id = id;
                showDetail(id);
            } catch (Exception x) { say(err, "Could not save it."); }
        });
        body.addView(save);
    }

    private static String snapshot(EditText... fields) {
        StringBuilder b = new StringBuilder();
        for (EditText f : fields) b.append(f.getText()).append('\0');
        return b.toString();
    }

    private TextView step(String glyph, String name, Runnable tap) {
        TextView b = text(glyph, 20, fg, false);
        b.setContentDescription(name);
        b.setGravity(Gravity.CENTER);
        b.setBackground(pressable(surface, 12));
        b.setOnClickListener(v -> tap.run());
        b.setLayoutParams(new LinearLayout.LayoutParams(px(48), px(48)));
        return b;
    }

    // Settings ----------------------------------------------------------------

    private void showSettings() {
        boolean haveVault = store.exists() && store.unlocked();
        LinearLayout body = page("Settings", haveVault ? this::showList : this::route);
        inside = haveVault;
        TextView note = error();
        note.setTextColor(muted);
        if (haveVault) {
            LinearLayout group = column();
            group.setBackground(rounded(surface, 16));
            if (Build.VERSION.SDK_INT >= 28) {
                LinearLayout r = new LinearLayout(this);
                r.setGravity(Gravity.CENTER_VERTICAL);
                r.setPadding(px(16), px(10), px(12), px(10));
                r.setMinimumHeight(px(56));
                LinearLayout t = column();
                t.addView(text("Fingerprint unlock", 16, fg, true));
                t.addView(text("Your fingerprint opens the vault on this phone", 13, muted, false));
                r.addView(t, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
                Switch toggle = new Switch(this);
                toggle.setContentDescription("Fingerprint unlock");
                toggle.setChecked(bioReady());
                toggle.setOnClickListener(v -> {
                    if (toggle.isChecked()) { toggle.setChecked(false); bioEnable(toggle, note); }
                    else { bioClear(); say(note, "Fingerprint unlock is off."); }
                });
                r.addView(toggle);
                group.addView(r);
            }
            AutofillManager am = getSystemService(AutofillManager.class);
            if (am != null && am.isAutofillSupported()) {
                boolean on = am.hasEnabledAutofillServices();
                group.addView(row("Autofill", on ? "On: Nexus fills apps and browsers" : "Off: tap to use Nexus for autofill", () -> {
                    if (am.hasEnabledAutofillServices()) { say(note, "Nexus is your autofill service. Change it in the phone's Settings."); return; }
                    startActivity(new Intent(Settings.ACTION_REQUEST_SET_AUTOFILL_SERVICE).setData(Uri.parse("package:" + getPackageName())));
                }));
            }
            group.addView(row("Change master password", "Other devices pick it up when they sync", this::showChange));
            group.addView(row("Lock now", "Forget the key until you unlock again", () -> { store.lock(); entries = null; route(); }));
            body.addView(group);
        }
        body.addView(note);
        body.addView(heading("Sync with desktops"));
        JSONArray pairs = Link.pairs(this);
        if (pairs.length() == 0) {
            body.addView(state("No desktops paired", "Pair one from the Nexus screen first.", null, null));
            return;
        }
        TextView how = text("Nothing leaves this phone for a desktop until both sides say Yes. Only encrypted data moves.", 13, muted, false);
        how.setPadding(px(4), 0, px(4), px(8));
        body.addView(how);
        LinearLayout group = column();
        group.setBackground(rounded(surface, 16));
        for (int i = 0; i < pairs.length(); i++) {
            JSONObject p = pairs.optJSONObject(i);
            group.addView(consentRow(p.optString("id"), p.optString("name")));
        }
        body.addView(group);
    }

    private View consentRow(String id, String name) {
        LinearLayout r = column();
        r.setPadding(px(16), px(12), px(16), px(8));
        r.addView(text(name, 16, fg, true));
        RadioGroup g = new RadioGroup(this);
        g.setOrientation(LinearLayout.HORIZONTAL);
        String now = Link.consent(this, id);
        String[][] options = {{"ask", "Ask"}, {"yes", "Yes"}, {"no", "No"}};
        for (String[] o : options) {
            RadioButton b = new RadioButton(this);
            b.setText(o[1]);
            b.setTextColor(fg);
            b.setContentDescription(o[1] + ", password sync with " + name);
            b.setMinHeight(px(48));
            b.setPadding(px(6), 0, px(20), 0);
            b.setId(View.generateViewId());
            g.addView(b);
            if (o[0].equals(now)) b.setChecked(true);
            b.setOnClickListener(v -> Link.setConsent(this, id, o[0]));
        }
        r.addView(g);
        return r;
    }

    private void showChange() {
        LinearLayout body = page("Change master password", this::showSettings);
        inside = true;
        EditText old = field("Current password", PASSWORD), first = field("At least 8 characters", PASSWORD), again = field("Type it again", PASSWORD);
        body.addView(labelled("Current master password", passwordRow(old), old));
        body.addView(labelled("New master password", passwordRow(first), first));
        body.addView(labelled("Repeat new password", passwordRow(again), again));
        TextView err = error();
        body.addView(err);
        TextView go = button("Change password", true, () -> {});
        go.setOnClickListener(v -> {
            char[] o = chars(old), a = chars(first), b = chars(again);
            if (a.length < 8) { say(err, "Use at least 8 characters."); return; }
            if (!Arrays.equals(a, b)) { say(err, "The two new passwords don't match."); return; }
            Arrays.fill(b, '\0');
            busy(go, "Changing…", () -> store.changePassword(o, a), o, a, msg -> say(err, msg));
        });
        body.addView(go);
    }

    // The desktop's question ----------------------------------------------------

    /** Opened by the notification: yes or no, for one desktop. */
    private void showAsk(String id) {
        String name = id;
        JSONArray pairs = Link.pairs(this);
        for (int i = 0; i < pairs.length(); i++) {
            if (pairs.optJSONObject(i).optString("id").equals(id)) name = pairs.optJSONObject(i).optString("name");
        }
        String desktop = name;
        LinearLayout body = page("Password sync", null);
        TextView q = text("Sync passwords with " + desktop + "?", 20, fg, true);
        q.setPadding(px(4), px(12), px(4), 0);
        body.addView(q);
        TextView why = text("Only say Yes if this computer is yours. Your passwords go to it encrypted; it still needs your master password to open them. You can change this later in Settings.", 15, muted, false);
        why.setPadding(px(4), px(8), px(4), 0);
        body.addView(why);
        body.addView(button("Don't sync", false, () -> answer(id, "no")));
        body.addView(button("Sync with " + desktop, true, () -> answer(id, "yes")));
    }

    private void answer(String id, String consent) {
        Link.setConsent(this, id, consent);
        getSystemService(NotificationManager.class).cancel(id, Link.CONSENT_NOTIFICATION);
        Toast.makeText(this, consent.equals("yes") ? "Passwords will sync with this desktop." : "Passwords won't sync with this desktop.", Toast.LENGTH_SHORT).show();
        getIntent().removeExtra("ask");
        route();
    }
}
