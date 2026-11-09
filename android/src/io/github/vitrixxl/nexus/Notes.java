package io.github.vitrixxl.nexus;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.Context;
import android.content.res.ColorStateList;
import android.content.res.Configuration;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.graphics.drawable.RippleDrawable;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.Editable;
import android.text.InputType;
import android.text.TextUtils;
import android.text.TextWatcher;
import android.view.Gravity;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowInsets;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputMethodManager;
import android.widget.BaseAdapter;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.HorizontalScrollView;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.ListView;
import android.widget.PopupMenu;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.File;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Nexus Notes on the phone: the page tree, search, and a Markdown editor for
 * the desktop's blocks. Everything stays in the app's own storage and syncs
 * with the desktop over the link alone (see {@link NotesStore}).
 */
public class Notes extends Activity {
    private static final int ACCENT = Color.rgb(10, 132, 255);
    private int bg, surface, fg, muted, error, warn;
    private float dp;
    private NotesStore store;
    private final Handler ui = new Handler(Looper.getMainLooper());
    private final ExecutorService io = Executors.newSingleThreadExecutor();
    private final AtomicLong searching = new AtomicLong();

    private LinearLayout listScreen, pageScreen, toolbar, undoBar;
    private TextView status, notice, crumbs, undoText;
    private View statusDot;
    private EditText search, title, body;
    private ScrollView pageScroll;
    private final Rows rows = new Rows();
    private Markdown md;

    /** The open page, its front matter, and the hash of the file as last read or written. */
    private String open, front = "", known;
    private boolean dirty, restyling;
    private int lineBreak = -1;
    private final Set<String> expanded = new HashSet<>();
    private File undoTrash;
    private String undoRel, drawnStatus = "";

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

    private ImageView icon(int res, int color, String label, Runnable tap) {
        ImageView i = new ImageView(this);
        i.setImageResource(res);
        i.setImageTintList(ColorStateList.valueOf(color));
        i.setScaleType(ImageView.ScaleType.CENTER);
        i.setContentDescription(label);
        i.setMinimumWidth(px(48));
        i.setMinimumHeight(px(48));
        if (tap != null) {
            i.setBackground(pressable(bg, 24));
            i.setOnClickListener(v -> tap.run());
        }
        return i;
    }

    private static LinearLayout.LayoutParams fill() { return new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1); }

    private EditText field(String hint, float size) {
        EditText e = new EditText(this);
        e.setHint(hint);
        e.setTextSize(size);
        e.setTextColor(fg);
        e.setHintTextColor(muted);
        e.setBackground(null);
        e.setPadding(0, 0, 0, 0);
        return e;
    }

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        dp = getResources().getDisplayMetrics().density;
        boolean dark = (getResources().getConfiguration().uiMode & Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES;
        bg = dark ? Color.rgb(20, 20, 22) : Color.rgb(242, 242, 245);
        surface = dark ? Color.rgb(31, 31, 34) : Color.WHITE;
        fg = dark ? Color.rgb(244, 244, 245) : Color.rgb(29, 29, 31);
        muted = dark ? Color.rgb(180, 180, 187) : Color.rgb(98, 98, 106);
        error = dark ? Color.rgb(255, 69, 58) : Color.rgb(215, 0, 21);
        warn = dark ? Color.rgb(255, 214, 10) : Color.rgb(178, 128, 0);
        int code = dark ? Color.rgb(36, 36, 40) : Color.rgb(234, 234, 238);
        int callout = dark ? Color.argb(28, 10, 132, 255) : Color.argb(20, 10, 132, 255);
        md = new Markdown(fg, muted, ACCENT, code, callout, dp);
        getWindow().setStatusBarColor(bg);
        getWindow().setNavigationBarColor(bg);
        if (!dark) {
            getWindow().getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR | View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR);
        }
        store = Link.notes(this);
        try {
            JSONArray open = new JSONArray(getSharedPreferences("notes", MODE_PRIVATE).getString("expanded", "[]"));
            for (int i = 0; i < open.length(); i++) expanded.add(open.optString(i));
        } catch (Exception ignored) {}

        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(bg);
        // The system bars and the keyboard pad the content: the toolbar rides on the keyboard.
        if (Build.VERSION.SDK_INT >= 30) {
            root.setOnApplyWindowInsetsListener((v, insets) -> {
                android.graphics.Insets i = insets.getInsets(WindowInsets.Type.systemBars() | WindowInsets.Type.ime());
                v.setPadding(i.left, i.top, i.right, i.bottom);
                return WindowInsets.CONSUMED;
            });
        } else {
            root.setFitsSystemWindows(true);
        }
        buildList(root);
        buildPage(root);
        buildUndo(root);
        setContentView(root);
        String rel = state != null ? state.getString("open") : getIntent().getStringExtra("open");
        if (rel != null && store.load(rel) != null) openPage(rel, false);
        else showList();
    }

    // ---- The list: sync state, search, the tree

    private void buildList(FrameLayout root) {
        listScreen = new LinearLayout(this);
        listScreen.setOrientation(LinearLayout.VERTICAL);
        listScreen.setPadding(px(20), px(20), px(20), px(16));
        root.addView(listScreen, new FrameLayout.LayoutParams(-1, -1));
        listScreen.addView(text("Notes", 30, fg, true));
        LinearLayout line = new LinearLayout(this);
        line.setGravity(Gravity.CENTER_VERTICAL);
        line.setPadding(0, px(4), 0, px(16));
        statusDot = new View(this);
        LinearLayout.LayoutParams dlp = new LinearLayout.LayoutParams(px(8), px(8));
        dlp.setMarginEnd(px(8));
        line.addView(statusDot, dlp);
        status = text("", 14, muted, false);
        line.addView(status, fill());
        line.setOnClickListener(v -> openConflict());
        listScreen.addView(line);

        LinearLayout box = new LinearLayout(this);
        box.setGravity(Gravity.CENTER_VERTICAL);
        box.setBackground(rounded(surface, 14));
        box.setPadding(px(4), 0, px(14), 0);
        box.addView(icon(R.drawable.ic_search, muted, null, null), new LinearLayout.LayoutParams(px(40), px(48)));
        search = field("Search notes", 16);
        search.setSingleLine(true);
        search.setImeOptions(EditorInfo.IME_ACTION_SEARCH);
        search.addTextChangedListener(new Watch(this::runSearch));
        box.addView(search, fill());
        listScreen.addView(box);

        ListView list = new ListView(this);
        list.setDivider(null);
        list.setSelector(new android.graphics.drawable.ColorDrawable(Color.TRANSPARENT));
        list.setAdapter(rows);
        list.setPadding(0, px(10), 0, px(10));
        list.setClipToPadding(false);
        list.setOnItemClickListener((p, v, i, id) -> openPage(rows.get(i).rel, false));
        list.setOnItemLongClickListener((p, v, i, id) -> { menu(v, rows.get(i).rel, false); return true; });
        listScreen.addView(list, new LinearLayout.LayoutParams(-1, 0, 1));

        TextView add = text("New page", 16, Color.WHITE, true);
        add.setGravity(Gravity.CENTER);
        add.setCompoundDrawablesRelativeWithIntrinsicBounds(tinted(R.drawable.ic_add, Color.WHITE), null, null, null);
        add.setCompoundDrawablePadding(px(8));
        add.setPadding(px(20), 0, px(24), 0);
        add.setBackground(pressable(ACCENT, 26));
        add.setOnClickListener(v -> create(""));
        LinearLayout.LayoutParams alp = new LinearLayout.LayoutParams(-2, px(52));
        alp.gravity = Gravity.END;
        listScreen.addView(add, alp);
    }

    private android.graphics.drawable.Drawable tinted(int res, int color) {
        android.graphics.drawable.Drawable d = getDrawable(res).mutate();
        d.setTint(color);
        return d;
    }

    /** A row of the tree, or a search hit. */
    private static final class Row {
        final String rel, snippet;
        final int depth;
        final boolean kids;
        Row(String rel, int depth, boolean kids, String snippet) { this.rel = rel; this.depth = depth; this.kids = kids; this.snippet = snippet; }
    }

    private final class Rows extends BaseAdapter {
        List<Row> items = new ArrayList<>();
        Set<String> conflicts = new HashSet<>();
        @Override public int getCount() { return items.size(); }
        @Override public Row getItem(int i) { return items.get(i); }
        Row get(int i) { return items.get(i); }
        @Override public long getItemId(int i) { return i; }
        @Override public View getView(int i, View old, ViewGroup parent) {
            Row r = items.get(i);
            LinearLayout v = new LinearLayout(Notes.this);
            v.setGravity(Gravity.CENTER_VERTICAL);
            v.setMinimumHeight(px(48));
            v.setPadding(px(r.depth * 18), 0, px(12), 0);
            v.setBackground(pressable(bg, 12));
            ImageView chevron = icon(R.drawable.ic_chevron, muted, expanded.contains(r.rel) ? "Collapse" : "Expand",
                    r.kids ? () -> toggle(r.rel) : null);
            chevron.setRotation(expanded.contains(r.rel) ? 90 : 0);
            chevron.setVisibility(r.kids ? View.VISIBLE : View.INVISIBLE);
            chevron.setBackground(r.kids ? pressable(bg, 18) : null);
            v.addView(chevron, new LinearLayout.LayoutParams(px(36), px(44)));
            LinearLayout texts = new LinearLayout(Notes.this);
            texts.setOrientation(LinearLayout.VERTICAL);
            texts.setPadding(0, px(8), 0, px(8));
            TextView t = text(NotesStore.name(r.rel), 16, fg, false);
            t.setSingleLine(true);
            t.setEllipsize(TextUtils.TruncateAt.END);
            texts.addView(t);
            if (r.snippet != null) {
                String where = NotesStore.parent(r.rel).replace("/", " › ");
                String detail = where.isEmpty() ? r.snippet : r.snippet.isEmpty() ? where : where + " · " + r.snippet;
                if (!detail.isEmpty()) {
                    TextView d = text(detail, 13, muted, false);
                    d.setMaxLines(2);
                    d.setEllipsize(TextUtils.TruncateAt.END);
                    texts.addView(d);
                }
            }
            v.addView(texts, fill());
            if (conflicts.contains(r.rel + ".md")) {
                TextView c = text("Both kept", 12, warn, true);
                c.setPadding(px(8), 0, 0, 0);
                v.addView(c);
            }
            return v;
        }
    }

    private void toggle(String rel) {
        if (!expanded.remove(rel)) expanded.add(rel);
        getSharedPreferences("notes", MODE_PRIVATE).edit().putString("expanded", new JSONArray(expanded).toString()).apply();
        refreshList();
    }

    /** The tree, or the search's hits when there is a query. */
    private void refreshList() {
        if (search.getText().toString().trim().length() > 0) { runSearch(); return; }
        searching.incrementAndGet();
        List<Row> items = new ArrayList<>();
        for (NotesStore.Page p : store.tree(expanded)) items.add(new Row(p.rel, p.depth, p.kids, null));
        rows.items = items;
        rows.notifyDataSetChanged();
    }

    private void runSearch() {
        String q = search.getText().toString().trim();
        if (q.isEmpty()) { refreshList(); return; }
        long mine = searching.incrementAndGet();
        io.execute(() -> {
            List<NotesStore.Hit> hits = store.search(q, () -> searching.get() != mine);
            List<Row> items = new ArrayList<>();
            for (NotesStore.Hit h : hits) items.add(new Row(h.rel, 0, false, h.snippet));
            ui.post(() -> {
                if (searching.get() != mine) return;
                rows.items = items;
                rows.notifyDataSetChanged();
            });
        });
    }

    private void menu(View anchor, String rel, boolean fromPage) {
        PopupMenu m = new PopupMenu(this, anchor);
        m.getMenu().add(0, 1, 0, "New subpage");
        if (!fromPage) m.getMenu().add(0, 2, 1, "Rename");
        m.getMenu().add(0, 3, 2, "Delete");
        m.setOnMenuItemClickListener(item -> {
            switch (item.getItemId()) {
                case 1: create(rel); break;
                case 2: askRename(rel); break;
                default: delete(rel);
            }
            return true;
        });
        m.show();
    }

    private void askRename(String rel) {
        EditText f = field("Title", 17);
        f.setText(NotesStore.name(rel));
        f.setSingleLine(true);
        f.setSelectAllOnFocus(true);
        f.setBackground(rounded(surface, 12));
        f.setPadding(px(14), px(12), px(14), px(12));
        FrameLayout box = new FrameLayout(this);
        box.setPadding(px(20), px(8), px(20), 0);
        box.addView(f);
        new AlertDialog.Builder(this)
                .setTitle("Rename page")
                .setView(box)
                .setNegativeButton("Cancel", null)
                .setPositiveButton("Rename", (d, w) -> {
                    String want = NotesStore.fileName(f.getText().toString());
                    try {
                        String to = store.rename(rel, want);
                        if (to.equals(rel) && !want.isEmpty() && !want.equals(NotesStore.name(rel))) {
                            Toast.makeText(this, "A page named “" + want + "” is already there", Toast.LENGTH_LONG).show();
                        }
                        if (expanded.remove(rel)) expanded.add(to);
                        Link.notesChanged();
                    } catch (Exception e) {
                        Toast.makeText(this, "Could not rename: " + e.getMessage(), Toast.LENGTH_LONG).show();
                    }
                    refreshList();
                })
                .show();
        f.requestFocus();
    }

    private void create(String parent) {
        saveNow();
        try {
            String rel = store.create(parent);
            if (!parent.isEmpty()) expanded.add(parent);
            Link.notesChanged();
            openPage(rel, true);
        } catch (Exception e) {
            Toast.makeText(this, "Could not create a page: " + e.getMessage(), Toast.LENGTH_LONG).show();
        }
    }

    private void delete(String rel) {
        if (rel.equals(open)) { dirty = false; showList(); }
        try {
            undoTrash = store.delete(rel);
            undoRel = rel;
            Link.notesChanged();
            undoText.setText("Deleted “" + NotesStore.name(rel) + "”");
            undoBar.setVisibility(View.VISIBLE);
            ui.removeCallbacks(hideUndo);
            ui.postDelayed(hideUndo, 6000);
        } catch (Exception e) {
            Toast.makeText(this, "Could not delete: " + e.getMessage(), Toast.LENGTH_LONG).show();
        }
        refreshList();
    }

    private final Runnable hideUndo = () -> undoBar.setVisibility(View.GONE);

    private void buildUndo(FrameLayout root) {
        undoBar = new LinearLayout(this);
        undoBar.setGravity(Gravity.CENTER_VERTICAL);
        undoBar.setBackground(rounded(fg, 14));
        undoBar.setPadding(px(18), 0, px(6), 0);
        undoBar.setElevation(px(6));
        undoBar.setVisibility(View.GONE);
        undoText = text("", 15, bg, false);
        undoBar.addView(undoText, fill());
        TextView undo = text("Undo", 15, ACCENT, true);
        undo.setGravity(Gravity.CENTER);
        undo.setPadding(px(16), 0, px(16), 0);
        undo.setMinHeight(px(48));
        undo.setBackground(pressable(fg, 12));
        undo.setOnClickListener(v -> {
            try {
                store.restore(undoTrash, undoRel);
                Link.notesChanged();
            } catch (Exception e) {
                Toast.makeText(this, e.getMessage(), Toast.LENGTH_LONG).show();
            }
            undoBar.setVisibility(View.GONE);
            refreshList();
        });
        undoBar.addView(undo);
        FrameLayout.LayoutParams lp = new FrameLayout.LayoutParams(-1, px(56), Gravity.BOTTOM);
        lp.setMargins(px(16), 0, px(16), px(84));
        root.addView(undoBar, lp);
    }

    /** "Synced with Studio · 2 min ago", and its tone: on, wait, off or bad. */
    static String[] syncLine(Context c, NotesStore store) {
        JSONObject st = Link.notesState;
        String desk = Link.connectedDesktop();
        long last = st == null ? 0 : st.optLong("last");
        int conflicts = st == null || st.optJSONArray("conflicts") == null ? 0 : st.optJSONArray("conflicts").length();
        if (desk == null) {
            JSONArray pairs = Link.pairs(c);
            if (pairs.length() == 0) return new String[]{"On this phone only · pair a desktop to sync", "off"};
            String name = Link.notesDesktop.isEmpty() ? pairs.optJSONObject(pairs.length() - 1).optString("name") : Link.notesDesktop;
            int n = last > 0 ? store.changedSince(last) : 0;
            if (n > 0) return new String[]{n + (n == 1 ? " page" : " pages") + " changed here · syncs when " + name + " is back", "wait"};
            return new String[]{"Offline · syncs when " + name + " is connected", "off"};
        }
        if (st == null) return new String[]{"Connecting to " + desk + "…", "wait"};
        if (!st.optBoolean("on")) return new String[]{"Sync is off · turn it on in " + desk + "’s Settings › Phone", "off"};
        if (st.optBoolean("busy")) return new String[]{"Syncing with " + desk + "…", "wait"};
        if (!st.isNull("error")) return new String[]{st.optString("error"), "bad"};
        String line = last > 0 ? "Synced with " + desk + " · " + ago(last) : "Waiting for " + desk;
        if (conflicts > 0) return new String[]{line + " · " + conflicts + (conflicts == 1 ? " page" : " pages") + " kept in both versions", "wait"};
        return new String[]{line, "on"};
    }

    static String ago(long ms) {
        long m = Math.max(0, System.currentTimeMillis() - ms) / 60000;
        if (m < 1) return "just now";
        if (m < 60) return m + " min ago";
        if (m < 1440) return (m / 60) + " h ago";
        return (m / 1440) + " d ago";
    }

    private int tone(String t) {
        switch (t) {
            case "on": return ACCENT;
            case "bad": return error;
            case "wait": return warn;
            default: return muted;
        }
    }

    private final Runnable tick = new Runnable() {
        @Override public void run() {
            showStatus();
            ui.postDelayed(this, 1000);
        }
    };

    private void showStatus() {
        String[] s = syncLine(this, store);
        JSONObject st = Link.notesState;
        JSONArray c = st == null ? null : st.optJSONArray("conflicts");
        String now = s[0] + s[1] + c;
        if (now.equals(drawnStatus)) return;
        drawnStatus = now;
        status.setText(s[0]);
        status.setTextColor(s[1].equals("bad") ? error : muted);
        GradientDrawable d = new GradientDrawable();
        d.setShape(GradientDrawable.OVAL);
        d.setColor(tone(s[1]));
        statusDot.setBackground(d);
        Set<String> conflicts = new HashSet<>();
        if (c != null) for (int i = 0; i < c.length(); i++) conflicts.add(c.optString(i));
        if (!conflicts.equals(rows.conflicts)) {
            rows.conflicts = conflicts;
            rows.notifyDataSetChanged();
        }
    }

    /** The status line's tap: the first page kept in both versions. */
    private void openConflict() {
        JSONObject st = Link.notesState;
        JSONArray c = st == null ? null : st.optJSONArray("conflicts");
        if (c == null) return;
        for (int i = 0; i < c.length(); i++) {
            String rel = c.optString(i).replaceAll("\\.md$", "");
            if (store.load(rel) != null) { openPage(rel, false); return; }
        }
    }

    // ---- The page

    private void buildPage(FrameLayout root) {
        pageScreen = new LinearLayout(this);
        pageScreen.setOrientation(LinearLayout.VERTICAL);
        root.addView(pageScreen, new FrameLayout.LayoutParams(-1, -1));
        LinearLayout bar = new LinearLayout(this);
        bar.setGravity(Gravity.CENTER_VERTICAL);
        bar.setPadding(px(4), px(4), px(4), 0);
        bar.addView(icon(R.drawable.ic_back, fg, "Back to notes", this::onBackPressed));
        crumbs = text("", 13, muted, false);
        crumbs.setSingleLine(true);
        crumbs.setEllipsize(TextUtils.TruncateAt.START);
        bar.addView(crumbs, fill());
        ImageView more = icon(R.drawable.ic_more, fg, "More", null);
        more.setBackground(pressable(bg, 24));
        more.setOnClickListener(v -> { if (open != null) { saveNow(); menu(v, open, true); } });
        bar.addView(more);
        pageScreen.addView(bar);

        pageScroll = new ScrollView(this);
        pageScroll.setFillViewport(true);
        LinearLayout content = new LinearLayout(this);
        content.setOrientation(LinearLayout.VERTICAL);
        content.setPadding(px(22), px(8), px(22), px(40));
        title = field("Untitled", 28);
        title.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        title.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
        title.setImeOptions(EditorInfo.IME_ACTION_NEXT);
        title.setOnEditorActionListener((v, a, e) -> { body.requestFocus(); body.setSelection(0); return true; });
        title.addTextChangedListener(new Watch(this::changed));
        content.addView(title);
        notice = text("", 14, fg, false);
        notice.setBackground(rounded(surface, 12));
        notice.setPadding(px(14), px(10), px(14), px(10));
        notice.setVisibility(View.GONE);
        notice.setOnClickListener(v -> v.setVisibility(View.GONE));
        LinearLayout.LayoutParams nlp = new LinearLayout.LayoutParams(-1, -2);
        nlp.topMargin = px(10);
        content.addView(notice, nlp);
        body = field("Write, or tap a block below", 16);
        body.setGravity(Gravity.TOP);
        body.setLineSpacing(px(3), 1);
        body.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
        body.setHighlightColor((ACCENT & 0x00ffffff) | 0x55000000);
        if (Build.VERSION.SDK_INT >= 29) body.setTextCursorDrawable(caret());
        body.addTextChangedListener(new TextWatcher() {
            @Override public void beforeTextChanged(CharSequence s, int a, int b, int c) {}
            @Override public void onTextChanged(CharSequence s, int start, int before, int count) {
                if (!restyling && before == 0 && count == 1 && s.charAt(start) == '\n') lineBreak = start;
            }
            @Override public void afterTextChanged(Editable e) {
                if (restyling) return;
                restyling = true;
                if (lineBreak >= 0) Markdown.continueBlock(e, lineBreak);
                lineBreak = -1;
                md.style(e);
                restyling = false;
                changed();
            }
        });
        body.setOnTouchListener(this::tapBox);
        body.setOnFocusChangeListener((v, has) -> toolbar.setVisibility(has ? View.VISIBLE : View.GONE));
        LinearLayout.LayoutParams blp = new LinearLayout.LayoutParams(-1, -2);
        blp.topMargin = px(14);
        content.addView(body, blp);
        // Tapping below the text goes to its end.
        View rest = new View(this);
        rest.setOnClickListener(v -> { body.requestFocus(); body.setSelection(body.length()); keyboard(body); });
        content.addView(rest, new LinearLayout.LayoutParams(-1, 0, 1));
        pageScroll.addView(content);
        pageScreen.addView(pageScroll, new LinearLayout.LayoutParams(-1, 0, 1));

        HorizontalScrollView tools = new HorizontalScrollView(this);
        tools.setHorizontalScrollBarEnabled(false);
        tools.setBackgroundColor(surface);
        toolbar = new LinearLayout(this);
        toolbar.setPadding(px(6), 0, px(6), 0);
        toolbar.setVisibility(View.GONE);
        for (String[] h : new String[][]{{"H1", "# "}, {"H2", "## "}, {"H3", "### "}}) {
            TextView t = text(h[0], 15, fg, true);
            t.setGravity(Gravity.CENTER);
            t.setMinWidth(px(48));
            t.setMinHeight(px(48));
            t.setContentDescription("Heading " + h[0].substring(1));
            t.setBackground(pressable(surface, 12));
            t.setOnClickListener(v -> block(h[1]));
            toolbar.addView(t);
        }
        Object[][] tools2 = {
                {R.drawable.ic_bullets, "Bulleted list", "- "}, {R.drawable.ic_numbers, "Numbered list", "1. "},
                {R.drawable.ic_todo, "To-do", "- [ ] "}, {R.drawable.ic_quote, "Quote", "> "},
                {R.drawable.ic_callout, "Callout", "> [!NOTE]"}, {R.drawable.ic_code, "Code", "```"},
                {R.drawable.ic_divider, "Divider", "---"}};
        for (Object[] t : tools2) {
            ImageView i = icon((int) t[0], fg, (String) t[1], () -> block((String) t[2]));
            i.setBackground(pressable(surface, 12));
            toolbar.addView(i);
        }
        tools.addView(toolbar);
        pageScreen.addView(tools);
    }

    private GradientDrawable caret() {
        GradientDrawable d = new GradientDrawable();
        d.setColor(ACCENT);
        d.setSize(px(2), 0);
        return d;
    }

    /** Turns the selected lines into a block, from the toolbar. */
    private void block(String kind) {
        Editable e = body.getText();
        int a = Math.max(0, body.getSelectionStart()), b = Math.max(a, body.getSelectionEnd());
        restyling = true;
        int caret;
        switch (kind) {
            case "```": Markdown.toggleCode(e, a, b); caret = Math.min(e.length(), Markdown.lineEnd(e, a + 4)); break;
            case "---": caret = Markdown.divider(e, a); break;
            default: {
                int from = Markdown.lineStart(e, a);
                Markdown.setBlock(e, a, b, kind);
                caret = Markdown.lineEnd(e, from);
                if (kind.equals("> [!NOTE]") && caret < e.length()) caret = Markdown.lineEnd(e, caret + 1);
            }
        }
        md.style(e);
        restyling = false;
        body.setSelection(Math.max(0, Math.min(caret, e.length())));
        changed();
    }

    /** A tap on a to-do's box ticks it, without moving the caret there. */
    private boolean tapBox(View v, MotionEvent ev) {
        if (ev.getAction() != MotionEvent.ACTION_UP) return false;
        int off = body.getOffsetForPosition(ev.getX(), ev.getY());
        Editable e = body.getText();
        for (Markdown.Check c : e.getSpans(Math.max(0, off - 1), Math.min(e.length(), off + 1), Markdown.Check.class)) {
            int s = e.getSpanStart(c);
            float x = body.getLayout().getPrimaryHorizontal(s) + body.getTotalPaddingLeft() - body.getScrollX();
            if (ev.getX() < x - px(8) || ev.getX() > x + px(40)) continue;
            restyling = true;
            Markdown.toggleTodo(e, c);
            md.style(e);
            restyling = false;
            changed();
            return true;
        }
        return false;
    }

    private void keyboard(View v) {
        getSystemService(InputMethodManager.class).showSoftInput(v, InputMethodManager.SHOW_IMPLICIT);
    }

    private void openPage(String rel, boolean fresh) {
        saveNow();
        String raw = store.load(rel);
        if (raw == null) { refreshList(); return; }
        open = rel;
        front = NotesStore.frontMatter(raw);
        known = new File(store.root, rel + ".md").isFile() ? NotesStore.sha256(raw) : null;
        restyling = true;
        title.setText(NotesStore.name(rel));
        body.setText(raw.substring(front.length()));
        md.style(body.getText());
        restyling = false;
        dirty = false;
        notice.setVisibility(View.GONE);
        crumbs.setText(NotesStore.parent(rel).replace("/", " › "));
        listScreen.setVisibility(View.GONE);
        pageScreen.setVisibility(View.VISIBLE);
        pageScroll.scrollTo(0, 0);
        if (fresh) {
            title.requestFocus();
            title.selectAll();
            keyboard(title);
        } else {
            body.clearFocus();
            pageScroll.requestFocus();
        }
    }

    private void showList() {
        saveNow();
        open = null;
        pageScreen.setVisibility(View.GONE);
        listScreen.setVisibility(View.VISIBLE);
        getSystemService(InputMethodManager.class).hideSoftInputFromWindow(listScreen.getWindowToken(), 0);
        refreshList();
        showStatus();
    }

    private final Runnable autosave = this::saveNow;

    private void changed() {
        if (restyling || open == null) return;
        dirty = true;
        ui.removeCallbacks(autosave);
        ui.postDelayed(autosave, 600);
    }

    /** Writes the open page: its title renames it, as on the desktop. */
    private void saveNow() {
        ui.removeCallbacks(autosave);
        if (open == null || !dirty) return;
        dirty = false;
        try {
            String to = store.rename(open, title.getText().toString());
            if (!to.equals(open)) {
                if (expanded.remove(open)) expanded.add(to);
                open = to;
                crumbs.setText(NotesStore.parent(to).replace("/", " › "));
            }
            String raw = front + body.getText();
            String copy = store.save(open, raw, known);
            known = NotesStore.sha256(raw);
            if (copy != null) {
                notice.setText("This page also changed on the desktop. That version is kept as “" + NotesStore.name(copy) + "”.");
                notice.setVisibility(View.VISIBLE);
            }
            Link.notesChanged();
        } catch (Exception e) {
            dirty = true;
            Toast.makeText(this, "Could not save: " + e.getMessage(), Toast.LENGTH_LONG).show();
        }
    }

    /** A sync wrote files: the tree follows, and the open page if untouched here. */
    private void synced() {
        if (open == null) { refreshList(); return; }
        String raw = store.load(open);
        if (raw == null) {
            if (!dirty) {
                Toast.makeText(this, "“" + NotesStore.name(open) + "” was moved or deleted on the desktop", Toast.LENGTH_LONG).show();
                showList();
            }
            return;
        }
        if (dirty || NotesStore.sha256(raw).equals(known)) return;
        int at = body.getSelectionStart();
        restyling = true;
        front = NotesStore.frontMatter(raw);
        body.setText(raw.substring(front.length()));
        md.style(body.getText());
        restyling = false;
        known = NotesStore.sha256(raw);
        body.setSelection(Math.max(0, Math.min(at, body.length())));
    }

    @Override public void onBackPressed() {
        if (open != null) showList();
        else super.onBackPressed();
    }

    @Override protected void onResume() {
        super.onResume();
        NotesStore.onSynced = () -> ui.post(this::synced);
        drawnStatus = "";
        ui.post(tick);
        if (open == null) refreshList();
        else synced();
    }

    @Override protected void onPause() {
        super.onPause();
        saveNow();
        NotesStore.onSynced = null;
        ui.removeCallbacks(tick);
    }

    @Override protected void onSaveInstanceState(Bundle out) {
        super.onSaveInstanceState(out);
        saveNow();
        out.putString("open", open);
    }

    @Override protected void onDestroy() {
        io.shutdownNow();
        super.onDestroy();
    }

    /** A TextWatcher that only cares that something changed. */
    private static final class Watch implements TextWatcher {
        final Runnable r;
        Watch(Runnable r) { this.r = r; }
        @Override public void beforeTextChanged(CharSequence s, int a, int b, int c) {}
        @Override public void onTextChanged(CharSequence s, int a, int b, int c) {}
        @Override public void afterTextChanged(Editable e) { r.run(); }
    }
}
