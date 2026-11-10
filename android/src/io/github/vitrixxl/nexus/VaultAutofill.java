package io.github.vitrixxl.nexus;

import android.app.PendingIntent;
import android.app.assist.AssistStructure;
import android.app.assist.AssistStructure.ViewNode;
import android.content.Context;
import android.content.Intent;
import android.content.IntentSender;
import android.os.Build;
import android.os.CancellationSignal;
import android.service.autofill.AutofillService;
import android.service.autofill.Dataset;
import android.service.autofill.FillCallback;
import android.service.autofill.FillContext;
import android.service.autofill.FillRequest;
import android.service.autofill.FillResponse;
import android.service.autofill.SaveCallback;
import android.service.autofill.SaveInfo;
import android.service.autofill.SaveRequest;
import android.text.InputType;
import android.view.View;
import android.view.ViewStructure;
import android.view.autofill.AutofillId;
import android.view.autofill.AutofillManager;
import android.view.autofill.AutofillValue;
import android.widget.RemoteViews;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Set;

/**
 * Fills apps and browsers from the vault, and offers to save what is typed.
 *
 * Who is the site? An app's AssistStructure says what web domain it shows, and
 * any app could claim anything: only {@link #BROWSERS} are believed, and a
 * browser's page is matched by its web domain alone. Every other app is matched
 * by its package name against `androidapp://` entries, whatever domain it reports.
 * ponytail: a package-name allowlist, as the owner's brief allows; a look-alike
 * package could only be sideloaded where the real browser isn't installed. Pin
 * signing certificates if that threat matters.
 */
public class VaultAutofill extends AutofillService {
    private static final Set<String> BROWSERS = new HashSet<>(Arrays.asList(
            "com.android.chrome", "com.chrome.beta", "com.chrome.dev", "com.chrome.canary", "org.chromium.chrome",
            "com.brave.browser", "com.brave.browser_beta", "com.brave.browser_nightly",
            "org.mozilla.firefox", "org.mozilla.firefox_beta", "org.mozilla.fenix", "org.mozilla.focus",
            "com.microsoft.emmx", "com.sec.android.app.sbrowser", "com.opera.browser", "com.opera.mini.native",
            "com.vivaldi.browser", "com.duckduckgo.mobile.android", "com.kiwibrowser.browser", "org.bromite.bromite",
            "org.cromite.cromite"));

    private static final class Form {
        final List<AutofillId> users = new ArrayList<>(), passwords = new ArrayList<>(), fresh = new ArrayList<>();
        String pkg, domain, scheme, username = "", password = "", focusedDomain;
        boolean signup;

        /** The URL when a browser shows this form; null otherwise. */
        String page() {
            if (!BROWSERS.contains(pkg) || domain == null) return null;
            return (scheme == null ? "https" : scheme) + "://" + domain;
        }
    }

    /** Fields found before classifying, in screen order. */
    private static final class Field {
        ViewNode node;
        String domain, scheme;
        boolean password, user, fresh;
    }

    private static Form parse(AssistStructure s) {
        List<Field> fields = new ArrayList<>();
        Form f = new Form();
        f.pkg = s.getActivityComponent().getPackageName();
        for (int i = 0; i < s.getWindowNodeCount(); i++) walk(s.getWindowNodeAt(i).getRootViewNode(), null, null, f, fields);
        boolean anyPassword = false;
        for (Field x : fields) anyPassword |= x.password;
        if (!anyPassword) fields.removeIf(x -> !x.user);
        // Cross-origin frames report their own domain: only the focused field's domain counts.
        for (Field x : fields) {
            if (f.focusedDomain != null && x.domain != null && !same(x.domain, f.focusedDomain)) continue;
            if (x.domain != null && f.domain == null) { f.domain = x.domain; f.scheme = x.scheme; }
            AutofillId id = x.node.getAutofillId();
            if (id == null) continue;
            if (x.password) {
                f.passwords.add(id);
                if (x.fresh) f.fresh.add(id);
                if (x.node.getAutofillValue() != null && x.node.getAutofillValue().isText()) f.password = x.node.getAutofillValue().getTextValue().toString();
            } else if (x.user) {
                f.users.add(id);
                if (x.node.getAutofillValue() != null && x.node.getAutofillValue().isText()) f.username = x.node.getAutofillValue().getTextValue().toString();
            }
        }
        f.signup = !f.fresh.isEmpty() || f.passwords.size() >= 2;
        // A login form with no field named "user": the text field just before the password.
        if (f.users.isEmpty() && !f.passwords.isEmpty()) {
            Field before = null;
            for (Field x : fields) { if (x.password) break; if (!x.user) before = x; }
            if (before != null && before.node.getAutofillId() != null) {
                f.users.add(before.node.getAutofillId());
                if (before.node.getAutofillValue() != null && before.node.getAutofillValue().isText()) f.username = before.node.getAutofillValue().getTextValue().toString();
            }
        }
        return f;
    }

    private static boolean same(String a, String b) {
        return VaultStore.registrable(a).equals(VaultStore.registrable(b));
    }

    private static void walk(ViewNode n, String domain, String scheme, Form f, List<Field> out) {
        if (n.getWebDomain() != null) {
            domain = n.getWebDomain();
            if (Build.VERSION.SDK_INT >= 28 && n.getWebScheme() != null) scheme = n.getWebScheme();
        }
        if (n.isFocused() && domain != null) f.focusedDomain = domain;
        if (n.getAutofillId() != null && n.getAutofillType() == View.AUTOFILL_TYPE_TEXT && n.getImportantForAutofill() != View.IMPORTANT_FOR_AUTOFILL_NO
                && n.getImportantForAutofill() != View.IMPORTANT_FOR_AUTOFILL_NO_EXCLUDE_DESCENDANTS) {
            Field x = classify(n);
            if (x != null) { x.domain = domain; x.scheme = scheme; out.add(x); }
        }
        for (int i = 0; i < n.getChildCount(); i++) walk(n.getChildAt(i), domain, scheme, f, out);
    }

    private static Field classify(ViewNode n) {
        String tag = n.getHtmlInfo() == null ? "" : n.getHtmlInfo().getTag().toLowerCase(Locale.ROOT);
        String type = attr(n, "type"), auto = attr(n, "autocomplete");
        String id = (n.getIdEntry() + " " + attr(n, "id") + " " + attr(n, "name") + " " + n.getHint()).toLowerCase(Locale.ROOT);
        if (n.getHtmlInfo() != null && !tag.equals("input")) return null;
        if (type.equals("hidden") || type.equals("checkbox") || type.equals("radio") || type.equals("submit") || type.equals("button")
                || auto.equals("one-time-code") || id.contains("otp") || id.contains("search")) return null;
        String[] hints = n.getAutofillHints();
        boolean userHint = false, passHint = false, freshHint = false;
        if (hints != null) for (String h : hints) {
            String l = h.toLowerCase(Locale.ROOT);
            if (l.contains("otp") || l.contains("cc-") || l.contains("creditcard")) return null;
            userHint |= l.contains("username") || l.contains("email");
            passHint |= l.contains("password");
            freshHint |= l.contains("new");
        }
        int variation = n.getInputType() & InputType.TYPE_MASK_VARIATION;
        int cls = n.getInputType() & InputType.TYPE_MASK_CLASS;
        boolean textPassword = cls == InputType.TYPE_CLASS_TEXT && (variation == InputType.TYPE_TEXT_VARIATION_PASSWORD
                || variation == InputType.TYPE_TEXT_VARIATION_WEB_PASSWORD || variation == InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD);
        boolean numberPassword = cls == InputType.TYPE_CLASS_NUMBER && variation == InputType.TYPE_NUMBER_VARIATION_PASSWORD;
        boolean email = cls == InputType.TYPE_CLASS_TEXT && (variation == InputType.TYPE_TEXT_VARIATION_EMAIL_ADDRESS || variation == InputType.TYPE_TEXT_VARIATION_WEB_EMAIL_ADDRESS);
        Field x = new Field();
        x.node = n;
        x.password = passHint || textPassword || numberPassword || type.equals("password") || auto.contains("password");
        x.fresh = freshHint || auto.contains("new-password") || (x.password && (id.contains("new") || id.contains("confirm") || id.contains("repeat") || id.contains("register") || id.contains("signup")));
        x.user = !x.password && (userHint || email || type.equals("email") || auto.equals("username") || auto.equals("email")
                || id.contains("user") || id.contains("login") || id.contains("email") || id.contains("mail") || id.contains("identifier"));
        if (x.password) return x;
        // A plain text field, kept only to find the one before a password.
        return x.user || cls == InputType.TYPE_CLASS_TEXT || tag.equals("input") ? x : null;
    }

    private static String attr(ViewNode n, String name) {
        ViewStructure.HtmlInfo h = n.getHtmlInfo();
        if (h == null || h.getAttributes() == null) return "";
        for (android.util.Pair<String, String> p : h.getAttributes()) {
            if (name.equals(p.first)) return p.second == null ? "" : p.second.toLowerCase(Locale.ROOT);
        }
        return "";
    }

    private static RemoteViews row(Context c, String title, String detail) {
        RemoteViews v = new RemoteViews(c.getPackageName(), R.layout.autofill_row);
        v.setTextViewText(R.id.title, title);
        v.setTextViewText(R.id.detail, detail);
        return v;
    }

    /** What to offer for this form while unlocked; null when there is no login form. */
    static FillResponse response(Context c, AssistStructure s) throws Exception {
        Form f = parse(s);
        if (f.passwords.isEmpty() && f.users.isEmpty()) return null;
        VaultStore store = VaultStore.get(c);
        FillResponse.Builder b = new FillResponse.Builder();
        String page = f.page();
        boolean browser = BROWSERS.contains(f.pkg);
        int added = 0;
        for (VaultStore.Entry e : browser ? (page == null ? new ArrayList<VaultStore.Entry>() : store.match(null, page)) : store.match(f.pkg, null)) {
            Dataset.Builder d = new Dataset.Builder(row(c, e.title, e.username));
            boolean any = false;
            if (!e.username.isEmpty()) for (AutofillId id : f.users) { d.setValue(id, AutofillValue.forText(e.username)); any = true; }
            for (AutofillId id : f.passwords) {
                if (!f.signup || !f.fresh.contains(id)) { d.setValue(id, AutofillValue.forText(e.password)); any = true; }
            }
            if (any) { b.addDataset(d.build()); added++; }
        }
        if (f.signup && !f.passwords.isEmpty()) {
            Dataset.Builder d = new Dataset.Builder(row(c, "Generate a strong password", "Saved here once you submit"));
            String generated = VaultStore.generate(20, true);
            for (AutofillId id : f.fresh.isEmpty() ? f.passwords : f.fresh) d.setValue(id, AutofillValue.forText(generated));
            b.addDataset(d.build());
            added++;
        }
        if (!f.passwords.isEmpty()) b.setSaveInfo(saveInfo(f));
        else if (added == 0) return null;
        return b.build();
    }

    private static SaveInfo saveInfo(Form f) {
        return new SaveInfo.Builder(SaveInfo.SAVE_DATA_TYPE_PASSWORD | (f.users.isEmpty() ? 0 : SaveInfo.SAVE_DATA_TYPE_USERNAME),
                f.passwords.toArray(new AutofillId[0]))
                .setOptionalIds(f.users.isEmpty() ? null : f.users.toArray(new AutofillId[0]))
                .setFlags(SaveInfo.FLAG_SAVE_ON_ALL_VIEWS_INVISIBLE).build();
    }

    @Override public void onFillRequest(FillRequest request, CancellationSignal cancel, FillCallback callback) {
        try {
            List<FillContext> contexts = request.getFillContexts();
            AssistStructure s = contexts.get(contexts.size() - 1).getStructure();
            Form f = parse(s);
            VaultStore store = VaultStore.get(this);
            if (f.pkg.equals(getPackageName()) || (f.passwords.isEmpty() && f.users.isEmpty()) || !store.exists()) {
                callback.onSuccess(null);
                return;
            }
            if (store.unlocked()) {
                callback.onSuccess(response(this, s));
                return;
            }
            List<AutofillId> ids = new ArrayList<>(f.users);
            ids.addAll(f.passwords);
            Intent i = new Intent(this, Vault.class).putExtra("autofill", true);
            // The system adds the structure to this intent when it is opened.
            IntentSender sender = PendingIntent.getActivity(this, 1, i, PendingIntent.FLAG_MUTABLE | PendingIntent.FLAG_CANCEL_CURRENT).getIntentSender();
            FillResponse.Builder b = new FillResponse.Builder()
                    .setAuthentication(ids.toArray(new AutofillId[0]), sender, row(this, "Nexus Passwords", "Tap to unlock"));
            // What is typed while locked can still be saved: onSaveRequest unlocks first.
            if (!f.passwords.isEmpty()) b.setSaveInfo(saveInfo(f));
            callback.onSuccess(b.build());
        } catch (Exception e) {
            callback.onSuccess(null);
        }
    }

    /** Held in memory while the vault is unlocked for a save; never in an Intent. */
    private static volatile Form pendingSave;

    @Override public void onSaveRequest(SaveRequest request, SaveCallback callback) {
        Form merged = null;
        for (FillContext c : request.getFillContexts()) {
            Form f = parse(c.getStructure());
            if (merged == null) merged = f;
            else {
                if (!f.password.isEmpty()) merged.password = f.password;
                if (!f.username.isEmpty()) merged.username = f.username;
                if (f.domain != null) { merged.domain = f.domain; merged.scheme = f.scheme; }
            }
        }
        if (merged == null || merged.password.isEmpty() || merged.pkg.equals(getPackageName())) {
            callback.onSuccess();
            return;
        }
        VaultStore store = VaultStore.get(this);
        if (!store.exists()) {
            callback.onFailure("Create a vault in Nexus first");
            return;
        }
        try {
            if (store.unlocked()) {
                apply(this, merged);
                callback.onSuccess();
                return;
            }
            pendingSave = merged;
            Intent i = new Intent(this, Vault.class).putExtra("save", true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            if (Build.VERSION.SDK_INT >= 28) {
                callback.onSuccess(PendingIntent.getActivity(this, 2, i, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_CANCEL_CURRENT).getIntentSender());
            } else {
                startActivity(i);
                callback.onSuccess();
            }
        } catch (Exception e) {
            callback.onFailure("Could not save the password");
        }
    }

    /** The save a locked vault held back, once it is unlocked. */
    static void finishSave(Context c) throws Exception {
        Form f = pendingSave;
        pendingSave = null;
        if (f != null) apply(c, f);
    }

    private static void apply(Context c, Form f) throws Exception {
        VaultStore store = VaultStore.get(c);
        String page = f.page();
        boolean browser = BROWSERS.contains(f.pkg);
        if (browser && page == null) return; // A browser that hides its domain gives no site to save under.
        VaultStore.Entry found = null;
        for (VaultStore.Entry e : browser ? store.match(null, page) : store.match(f.pkg, null)) {
            if (e.username.equals(f.username)) { found = e; break; }
        }
        if (found == null) {
            found = new VaultStore.Entry();
            found.username = f.username;
            if (browser) {
                found.title = VaultStore.registrable(f.domain);
                found.urls.add(page);
            } else {
                android.content.pm.PackageManager pm = c.getPackageManager();
                try { found.title = pm.getApplicationLabel(pm.getApplicationInfo(f.pkg, 0)).toString(); } catch (Exception x) { found.title = f.pkg; }
                found.urls.add("androidapp://" + f.pkg);
            }
        }
        found.password = f.password;
        store.save(found);
    }

    /** The extra the system puts on our authentication intent. */
    static AssistStructure structure(Intent i) { return i.getParcelableExtra(AutofillManager.EXTRA_ASSIST_STRUCTURE); }
}
