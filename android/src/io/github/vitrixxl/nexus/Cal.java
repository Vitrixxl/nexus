package io.github.vitrixxl.nexus;

import android.Manifest;
import android.app.Activity;
import android.app.DatePickerDialog;
import android.app.Dialog;
import android.app.TimePickerDialog;
import android.content.Context;
import android.content.Intent;
import android.content.res.ColorStateList;
import android.content.res.Configuration;
import android.database.ContentObserver;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Typeface;
import android.graphics.drawable.ColorDrawable;
import android.graphics.drawable.GradientDrawable;
import android.graphics.drawable.RippleDrawable;
import android.net.Uri;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.CalendarContract;
import android.text.Editable;
import android.text.InputType;
import android.text.TextUtils;
import android.text.TextWatcher;
import android.text.format.DateFormat;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowManager;
import android.widget.ArrayAdapter;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.Spinner;
import android.widget.Switch;
import android.widget.TextView;
import android.widget.Toast;

import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Calendar;
import java.util.List;
import java.util.Locale;

/** The month, the day's events, and adding or changing them. The widget opens here. */
public class Cal extends Activity {
    private static final int ACCENT = Color.rgb(10, 132, 255);
    private static final long DAY = 86_400_000L;
    private int bg, surface, fg, muted, tone, danger;
    private float dp;
    private final Handler handler = new Handler(Looper.getMainLooper());
    private final Calendar shown = Calendar.getInstance();
    private Calendar selected = Agenda.midnight(Calendar.getInstance());
    private List<Agenda.Item> items = new ArrayList<>();
    private List<Agenda.Cal> calendars = new ArrayList<>();
    private TextView title, dayTitle;
    private LinearLayout grid, list;
    private View add;
    private final ContentObserver changes = new ContentObserver(handler) {
        @Override public void onChange(boolean self) { load(); }
    };

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

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        dp = getResources().getDisplayMetrics().density;
        boolean dark = (getResources().getConfiguration().uiMode & Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES;
        bg = dark ? Color.rgb(20, 20, 22) : Color.rgb(242, 242, 245);
        surface = dark ? Color.rgb(31, 31, 34) : Color.WHITE;
        fg = dark ? Color.rgb(244, 244, 245) : Color.rgb(29, 29, 31);
        muted = dark ? Color.rgb(180, 180, 187) : Color.rgb(98, 98, 106);
        tone = dark ? Color.argb(38, 255, 255, 255) : Color.argb(26, 0, 0, 0);
        danger = dark ? Color.rgb(255, 69, 58) : Color.rgb(215, 0, 21);
        getWindow().setStatusBarColor(bg);
        getWindow().setNavigationBarColor(bg);
        if (!dark) getWindow().getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR | View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR);

        FrameLayout frame = new FrameLayout(this);
        frame.setFitsSystemWindows(true);
        frame.setBackgroundColor(bg);
        ScrollView scroll = new ScrollView(this);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(px(16), px(12), px(16), px(96));
        scroll.addView(root);
        frame.addView(scroll);

        LinearLayout head = new LinearLayout(this);
        head.setGravity(Gravity.CENTER_VERTICAL);
        head.addView(icon(R.drawable.ic_back, fg, 44, "Back", this::finish));
        title = text("", 22, fg, true);
        title.setPadding(px(6), 0, 0, 0);
        title.setSingleLine(true);
        title.setEllipsize(TextUtils.TruncateAt.END);
        head.addView(title, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
        ImageView before = icon(R.drawable.ic_chevron, fg, 44, "Previous month", () -> step(-1));
        before.setRotation(180);
        head.addView(before);
        head.addView(icon(R.drawable.ic_chevron, fg, 44, "Next month", () -> step(1)));
        root.addView(head);

        grid = new LinearLayout(this);
        grid.setOrientation(LinearLayout.VERTICAL);
        grid.setBackground(rounded(surface, 20));
        grid.setPadding(px(8), px(10), px(8), px(10));
        LinearLayout.LayoutParams gridAt = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        gridAt.topMargin = px(8);
        root.addView(grid, gridAt);

        dayTitle = text("", 15, fg, true);
        dayTitle.setPadding(px(4), px(22), 0, px(8));
        root.addView(dayTitle);
        list = new LinearLayout(this);
        list.setOrientation(LinearLayout.VERTICAL);
        root.addView(list);

        ImageView fab = new ImageView(this);
        fab.setImageResource(R.drawable.ic_add);
        fab.setColorFilter(Color.WHITE);
        fab.setScaleType(ImageView.ScaleType.CENTER);
        fab.setContentDescription("Add an event");
        fab.setBackground(new RippleDrawable(ColorStateList.valueOf(0x33ffffff), rounded(ACCENT, 28), null));
        fab.setElevation(px(6));
        fab.setOnClickListener(v -> edit(null));
        FrameLayout.LayoutParams at = new FrameLayout.LayoutParams(px(56), px(56), Gravity.BOTTOM | Gravity.END);
        at.setMargins(0, 0, px(20), px(20));
        frame.addView(fab, at);
        add = fab;
        setContentView(frame);
        open(getIntent());
    }

    @Override protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        open(intent);
    }

    /** nexus://calendar/2026-11-09[/add], from the widget. */
    private void open(Intent intent) {
        Uri data = intent == null ? null : intent.getData();
        boolean adding = false;
        if (data != null && data.getPathSegments().size() > 0) {
            try {
                Calendar c = Calendar.getInstance();
                c.setTime(new SimpleDateFormat("yyyy-MM-dd", Locale.ROOT).parse(data.getPathSegments().get(0)));
                selected = Agenda.midnight(c);
                adding = data.getPathSegments().size() > 1;
            } catch (Exception ignored) {}
        }
        shown.setTimeInMillis(selected.getTimeInMillis());
        shown.set(Calendar.DATE, 1);
        draw();
        if (!Agenda.allowed(this)) {
            requestPermissions(new String[]{Manifest.permission.READ_CALENDAR, Manifest.permission.WRITE_CALENDAR}, 1);
        } else if (adding) {
            edit(null);
        }
    }

    @Override public void onRequestPermissionsResult(int code, String[] permissions, int[] results) {
        load();
        CalWidget.refresh(this);
        Link.watchCalendar();
        Link.calendarChanged();
    }

    @Override protected void onStart() {
        super.onStart();
        getContentResolver().registerContentObserver(CalendarContract.CONTENT_URI, true, changes);
        load();
    }
    @Override protected void onStop() {
        getContentResolver().unregisterContentObserver(changes);
        super.onStop();
    }

    private void step(int by) {
        shown.add(Calendar.MONTH, by);
        load();
        draw();
    }

    private Calendar firstCell() {
        Calendar c = Agenda.midnight(shown);
        c.add(Calendar.DATE, -((c.get(Calendar.DAY_OF_WEEK) - c.getFirstDayOfWeek() + 7) % 7));
        return c;
    }

    private void load() {
        if (!Agenda.allowed(this)) { draw(); return; }
        Context app = getApplicationContext();
        long from = firstCell().getTimeInMillis() - DAY, to = from + 44 * DAY;
        new Thread(() -> {
            List<Agenda.Item> e = Agenda.events(app, from, to);
            List<Agenda.Cal> k = Agenda.calendars(app);
            runOnUiThread(() -> { items = e; calendars = k; draw(); });
        }).start();
    }

    /** One day of the grid, a view of its own so that TalkBack reads each. */
    private final class DayView extends View {
        final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
        final String number;
        final boolean today, chosen, outside;
        final int[] colors;

        DayView(Context c, Calendar day, boolean today, boolean chosen, boolean outside, int[] colors) {
            super(c);
            number = Integer.toString(day.get(Calendar.DATE));
            this.today = today;
            this.chosen = chosen;
            this.outside = outside;
            this.colors = colors;
            setContentDescription(new SimpleDateFormat("EEEE d MMMM", Locale.getDefault()).format(day.getTime())
                    + (colors.length > 0 ? ", has events" : ""));
            setFocusable(true);
            setClickable(true);
        }
        @Override protected void onDraw(Canvas c) {
            float cx = getWidth() / 2f, cy = px(21), r = px(18);
            if (today || chosen) {
                paint.setColor(today ? ACCENT : tone);
                c.drawCircle(cx, cy, r, paint);
            }
            paint.setColor(today ? Color.WHITE : outside ? (muted & 0x00ffffff) | 0x88000000 : fg);
            paint.setTextAlign(Paint.Align.CENTER);
            paint.setTextSize(px(15));
            paint.setTypeface(Typeface.create(today || chosen ? "sans-serif-medium" : "sans-serif", Typeface.NORMAL));
            c.drawText(number, cx, cy - (paint.descent() + paint.ascent()) / 2, paint);
            float d = px(2.5f), gap = px(3.5f), w = colors.length * 2 * d + (colors.length - 1) * gap;
            float x = cx - w / 2 + d;
            for (int color : colors) {
                paint.setColor(color | 0xff000000);
                c.drawCircle(x, px(46), d, paint);
                x += 2 * d + gap;
            }
        }
    }

    private void draw() {
        title.setText(new SimpleDateFormat("LLLL yyyy", Locale.getDefault()).format(shown.getTime()));
        grid.removeAllViews();
        Calendar names = firstCell();
        LinearLayout header = new LinearLayout(this);
        SimpleDateFormat shortName = new SimpleDateFormat("EEE", Locale.getDefault());
        for (int i = 0; i < 7; i++) {
            TextView n = text(shortName.format(names.getTime()), 12, muted, true);
            n.setGravity(Gravity.CENTER);
            header.addView(n, new LinearLayout.LayoutParams(0, px(24), 1));
            names.add(Calendar.DATE, 1);
        }
        grid.addView(header);
        Calendar cell = firstCell(), today = Agenda.midnight(Calendar.getInstance());
        Calendar end = Agenda.midnight(shown);
        end.add(Calendar.MONTH, 1);
        for (int w = 0; w < 6 && cell.before(end); w++) {
            LinearLayout week = new LinearLayout(this);
            for (int i = 0; i < 7; i++) {
                List<Integer> colors = new ArrayList<>();
                for (Agenda.Item e : items) if (e.on(cell) && !colors.contains(e.color) && colors.size() < 3) colors.add(e.color);
                int[] dots = new int[colors.size()];
                for (int k = 0; k < dots.length; k++) dots[k] = colors.get(k);
                Calendar day = (Calendar) cell.clone();
                DayView v = new DayView(this, day, cell.equals(today), cell.equals(selected), cell.get(Calendar.MONTH) != shown.get(Calendar.MONTH), dots);
                v.setOnClickListener(x -> pick(day));
                week.addView(v, new LinearLayout.LayoutParams(0, px(54), 1));
                cell.add(Calendar.DATE, 1);
            }
            grid.addView(week);
        }
        dayTitle.setText((selected.equals(today) ? "Today, " + new SimpleDateFormat("MMMM d", Locale.getDefault()).format(selected.getTime())
                : new SimpleDateFormat("EEEE, MMMM d", Locale.getDefault()).format(selected.getTime())));
        list.removeAllViews();
        if (!Agenda.allowed(this)) {
            TextView t = text("Allow calendar access to see your events.", 14, muted, false);
            t.setPadding(px(4), px(4), px(4), px(4));
            list.addView(t);
            add.setVisibility(View.GONE);
            return;
        }
        add.setVisibility(View.VISIBLE);
        boolean any = false;
        for (Agenda.Item e : items) {
            if (!e.on(selected)) continue;
            any = true;
            list.addView(row(e));
        }
        if (!any) {
            TextView t = text("Nothing on this day", 14, muted, false);
            t.setPadding(px(4), px(4), px(4), px(4));
            list.addView(t);
        }
    }

    private void pick(Calendar day) {
        boolean moved = day.get(Calendar.MONTH) != shown.get(Calendar.MONTH);
        selected = day;
        if (moved) {
            shown.setTimeInMillis(day.getTimeInMillis());
            shown.set(Calendar.DATE, 1);
            load();
        }
        draw();
    }

    private String clock(long at) { return DateFormat.getTimeFormat(this).format(at); }

    private View row(Agenda.Item e) {
        LinearLayout r = new LinearLayout(this);
        r.setGravity(Gravity.CENTER_VERTICAL);
        r.setMinimumHeight(px(60));
        r.setPadding(px(16), px(10), px(12), px(10));
        View dot = new View(this);
        GradientDrawable round = new GradientDrawable();
        round.setShape(GradientDrawable.OVAL);
        round.setColor(e.color | 0xff000000);
        dot.setBackground(round);
        LinearLayout.LayoutParams at = new LinearLayout.LayoutParams(px(10), px(10));
        at.setMarginEnd(px(14));
        r.addView(dot, at);
        LinearLayout texts = new LinearLayout(this);
        texts.setOrientation(LinearLayout.VERTICAL);
        TextView name = text(e.title.isEmpty() ? "No title" : e.title, 16, fg, true);
        name.setSingleLine(true);
        name.setEllipsize(TextUtils.TruncateAt.END);
        texts.addView(name);
        String line = e.allDay ? "All day" : clock(e.begin) + " – " + clock(e.end);
        if (!e.location.isEmpty()) line += " · " + e.location;
        TextView time = text(line, 13, muted, false);
        time.setSingleLine(true);
        time.setEllipsize(TextUtils.TruncateAt.END);
        texts.addView(time);
        r.addView(texts, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
        if (e.writable) {
            r.setBackground(pressable(surface, 16));
            r.setOnClickListener(v -> edit(e));
        } else {
            r.setBackground(rounded(surface, 16));
        }
        LinearLayout.LayoutParams gap = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        gap.bottomMargin = px(8);
        r.setLayoutParams(gap);
        return r;
    }

    private TextView chip(String s) {
        TextView t = text(s, 15, fg, false);
        t.setGravity(Gravity.CENTER);
        t.setMinHeight(px(44));
        t.setPadding(px(14), 0, px(14), 0);
        t.setBackground(pressable(tone, 12));
        return t;
    }
    private LinearLayout formRow(String name, View... fields) {
        LinearLayout r = new LinearLayout(this);
        r.setGravity(Gravity.CENTER_VERTICAL);
        r.setMinimumHeight(px(48));
        r.addView(text(name, 15, fg, false), new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
        for (View f : fields) {
            LinearLayout.LayoutParams at = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
            at.setMarginStart(px(8));
            r.addView(f, at);
        }
        return r;
    }

    /** The sheet for `event`, or for a new one on the selected day. */
    private void edit(Agenda.Item event) {
        Dialog d = new Dialog(this);
        d.requestWindowFeature(Window.FEATURE_NO_TITLE);
        LinearLayout body = new LinearLayout(this);
        body.setOrientation(LinearLayout.VERTICAL);
        body.setPadding(px(20), px(20), px(20), px(16));
        GradientDrawable sheet = new GradientDrawable();
        sheet.setColor(surface);
        float r = 24 * dp;
        sheet.setCornerRadii(new float[]{r, r, r, r, 0, 0, 0, 0});
        body.setBackground(sheet);

        Calendar start = Calendar.getInstance(), end = Calendar.getInstance();
        boolean[] allDay = {false};
        if (event != null) {
            allDay[0] = event.allDay;
            if (event.allDay) {
                start.setTimeInMillis(Agenda.localDate(event.begin).getTimeInMillis());
                end.setTimeInMillis(Agenda.localDate(event.end - 1).getTimeInMillis());
            } else {
                start.setTimeInMillis(event.begin);
                end.setTimeInMillis(event.end);
            }
        } else {
            int hour = selected.equals(Agenda.midnight(start)) ? Math.min(start.get(Calendar.HOUR_OF_DAY) + 1, 23) : 9;
            start.setTimeInMillis(selected.getTimeInMillis());
            start.set(Calendar.HOUR_OF_DAY, hour);
            end.setTimeInMillis(start.getTimeInMillis() + 3_600_000L);
        }

        EditText name = new EditText(this);
        name.setHint("Add a title");
        name.setHintTextColor(muted);
        name.setTextColor(fg);
        name.setTextSize(20);
        name.setSingleLine(true);
        name.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
        name.setBackground(null);
        name.setPadding(0, 0, 0, px(12));
        if (event != null) name.setText(event.title);
        body.addView(name);

        Switch whole = new Switch(this);
        whole.setChecked(allDay[0]);
        whole.setThumbTintList(null);
        whole.setContentDescription("All day");
        body.addView(formRow("All day", whole));

        TextView startDate = chip(""), startTime = chip(""), endDate = chip(""), endTime = chip("");
        LinearLayout starts = formRow("Starts", startDate, startTime), ends = formRow("Ends", endDate, endTime);
        body.addView(starts);
        body.addView(ends);
        Runnable show = () -> {
            SimpleDateFormat f = new SimpleDateFormat("EEE d MMM", Locale.getDefault());
            startDate.setText(f.format(start.getTime()));
            endDate.setText(f.format(end.getTime()));
            startTime.setText(clock(start.getTimeInMillis()));
            endTime.setText(clock(end.getTimeInMillis()));
            startTime.setVisibility(allDay[0] ? View.GONE : View.VISIBLE);
            endTime.setVisibility(allDay[0] ? View.GONE : View.VISIBLE);
            boolean bad = !allDay[0] && end.getTimeInMillis() <= start.getTimeInMillis();
            endTime.setTextColor(bad ? danger : fg);
        };
        show.run();
        whole.setOnCheckedChangeListener((b, on) -> { allDay[0] = on; show.run(); });
        // A start moved past the end takes the end with it.
        Runnable startMoved = () -> {
            if (end.before(start)) end.setTimeInMillis(start.getTimeInMillis() + (allDay[0] ? 0 : 3_600_000L));
            show.run();
        };
        pickDate(startDate, start, startMoved);
        pickTime(startTime, start, startMoved);
        pickDate(endDate, end, show);
        pickTime(endTime, end, show);

        List<Agenda.Cal> writable = new ArrayList<>();
        for (Agenda.Cal k : calendars) if (k.writable) writable.add(k);
        Spinner which = new Spinner(this);
        if (event == null && writable.size() > 1) {
            List<String> names = new ArrayList<>();
            int chosen = 0;
            for (int i = 0; i < writable.size(); i++) {
                names.add(writable.get(i).name);
                if (writable.get(i).primary) chosen = i;
            }
            ArrayAdapter<String> a = new ArrayAdapter<>(this, android.R.layout.simple_spinner_item, names);
            a.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item);
            which.setAdapter(a);
            which.setSelection(chosen);
            body.addView(formRow("Calendar", which));
        }
        if (event != null && event.repeats) {
            TextView note = text("Repeats. Changes apply to this occurrence only.", 13, muted, false);
            note.setPadding(0, px(4), 0, 0);
            body.addView(note);
        }

        LinearLayout actions = new LinearLayout(this);
        actions.setGravity(Gravity.CENTER_VERTICAL);
        actions.setPadding(0, px(14), 0, 0);
        if (event != null) {
            TextView del = text(event.repeats ? "Delete this one" : "Delete", 15, danger, true);
            del.setPadding(px(4), px(12), px(12), px(12));
            del.setOnClickListener(v -> { d.dismiss(); run(() -> Agenda.delete(getApplicationContext(), event)); });
            actions.addView(del);
        }
        actions.addView(new View(this), new LinearLayout.LayoutParams(0, 1, 1));
        TextView cancel = text("Cancel", 15, muted, true);
        cancel.setPadding(px(14), px(12), px(14), px(12));
        cancel.setOnClickListener(v -> d.dismiss());
        actions.addView(cancel);
        TextView save = text("Save", 15, Color.WHITE, true);
        save.setPadding(px(22), px(12), px(22), px(12));
        save.setBackground(new RippleDrawable(ColorStateList.valueOf(0x33ffffff), rounded(ACCENT, 12), null));
        save.setOnClickListener(v -> {
            if (name.getText().toString().trim().isEmpty()) { name.setHintTextColor(danger); name.requestFocus(); return; }
            if (!allDay[0] && end.getTimeInMillis() <= start.getTimeInMillis()) { toast("The end must be after the start"); return; }
            if (allDay[0] && Agenda.midnight(end).before(Agenda.midnight(start))) { toast("The end must be after the start"); return; }
            Agenda.Item e = event != null ? event : new Agenda.Item();
            e.title = name.getText().toString().trim();
            e.allDay = allDay[0];
            if (e.allDay) {
                Calendar after = Agenda.midnight(end);
                after.add(Calendar.DATE, 1);
                e.begin = Agenda.utcMidnight(start);
                e.end = Agenda.utcMidnight(after);
            } else {
                e.begin = start.getTimeInMillis();
                e.end = end.getTimeInMillis();
            }
            if (event == null && writable.size() > 1) e.calendar = writable.get(which.getSelectedItemPosition()).id;
            d.dismiss();
            run(() -> Agenda.save(getApplicationContext(), e));
        });
        actions.addView(save);
        body.addView(actions);

        d.setContentView(body);
        Window w = d.getWindow();
        w.setBackgroundDrawable(new ColorDrawable(Color.TRANSPARENT));
        w.setGravity(Gravity.BOTTOM);
        w.setLayout(WindowManager.LayoutParams.MATCH_PARENT, WindowManager.LayoutParams.WRAP_CONTENT);
        w.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE | (event == null ? WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE : 0));
        if (event == null) name.requestFocus();
        d.show();
    }

    private void pickDate(View chip, Calendar c, Runnable done) {
        chip.setOnClickListener(v -> new DatePickerDialog(this, (p, y, m, day) -> { c.set(y, m, day); done.run(); },
                c.get(Calendar.YEAR), c.get(Calendar.MONTH), c.get(Calendar.DATE)).show());
    }
    private void pickTime(View chip, Calendar c, Runnable done) {
        chip.setOnClickListener(v -> new TimePickerDialog(this, (p, h, m) -> {
            c.set(Calendar.HOUR_OF_DAY, h);
            c.set(Calendar.MINUTE, m);
            done.run();
        }, c.get(Calendar.HOUR_OF_DAY), c.get(Calendar.MINUTE), DateFormat.is24HourFormat(this)).show());
    }

    private interface Change { void apply() throws Exception; }

    /** The change off the main thread; the widget follows, and the desktop by the provider. */
    private void run(Change change) {
        Context app = getApplicationContext();
        new Thread(() -> {
            String error = null;
            try { change.apply(); } catch (Exception e) { error = e.getMessage(); }
            CalWidget.refresh(app);
            String failed = error;
            runOnUiThread(() -> { if (failed != null) toast(failed); load(); });
        }).start();
    }
    private void toast(String s) { Toast.makeText(this, s, Toast.LENGTH_LONG).show(); }
}
