package io.github.vitrixxl.nexus;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.appwidget.AppWidgetManager;
import android.appwidget.AppWidgetProvider;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;
import android.text.format.DateFormat;
import android.view.View;
import android.widget.RemoteViews;

import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Calendar;
import java.util.List;
import java.util.Locale;

/** The home-screen calendar: today and what follows; the month too, once tall enough. */
public class CalWidget extends AppWidgetProvider {
    private static final int TALL = 260;
    private static final int RING = 44, HEAD = 92, WEEK = 32;

    @Override public void onUpdate(Context c, AppWidgetManager m, int[] ids) {
        for (int id : ids) draw(c, m, id);
    }
    @Override public void onAppWidgetOptionsChanged(Context c, AppWidgetManager m, int id, Bundle o) {
        draw(c, m, id);
    }

    /** Draws every widget again; the calendars changed. */
    static void refresh(Context c) {
        AppWidgetManager m = AppWidgetManager.getInstance(c);
        for (int id : m.getAppWidgetIds(new ComponentName(c, CalWidget.class))) draw(c, m, id);
    }

    private static PendingIntent open(Context c, Calendar day, boolean add) {
        String date = new SimpleDateFormat("yyyy-MM-dd", Locale.ROOT).format(day.getTime());
        Intent i = new Intent(c, Cal.class).setData(Uri.parse("nexus://calendar/" + date + (add ? "/add" : "")))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        return PendingIntent.getActivity(c, 0, i, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    private static String when(Context c, Agenda.Item e, Calendar today) {
        Calendar day = e.allDay ? Agenda.localDate(e.begin) : Agenda.midnight(at(e.begin));
        // Already going on: it counts as today.
        if (day.before(today)) day = today;
        long days = Math.round((day.getTimeInMillis() - today.getTimeInMillis()) / 86_400_000.0);
        String name = days == 0 ? "Today" : days == 1 ? "Tomorrow"
                : new SimpleDateFormat(days < 7 ? "EEE" : "EEE d MMM", Locale.getDefault()).format(day.getTime());
        return e.allDay ? name : name + " " + DateFormat.getTimeFormat(c).format(e.begin);
    }
    private static Calendar at(long ms) {
        Calendar c = Calendar.getInstance();
        c.setTimeInMillis(ms);
        return c;
    }

    private static void draw(Context c, AppWidgetManager m, int id) {
        Bundle o = m.getAppWidgetOptions(id);
        // In portrait a widget is its minimum wide and its maximum tall.
        int height = o.getInt(AppWidgetManager.OPTION_APPWIDGET_MAX_HEIGHT, 110);
        boolean tall = height >= TALL;
        RemoteViews v = new RemoteViews(c.getPackageName(), tall ? R.layout.widget_tall : R.layout.widget_small);
        Calendar now = Calendar.getInstance(), today = Agenda.midnight(now);
        v.setTextViewText(R.id.w_month, new SimpleDateFormat(tall ? "LLLL yyyy" : "LLLL", Locale.getDefault()).format(now.getTime()));
        v.setOnClickPendingIntent(R.id.w_add, open(c, today, true));
        v.setOnClickPendingIntent(R.id.w_root, open(c, today, false));
        boolean allowed = Agenda.allowed(c);
        int weeks = 0;
        if (tall) weeks = month(c, v, today, allowed);
        else {
            v.setTextViewText(R.id.w_weekday, new SimpleDateFormat("EEEE", Locale.getDefault()).format(now.getTime()));
            v.setTextViewText(R.id.w_day, Integer.toString(now.get(Calendar.DATE)));
        }
        int rows = tall ? Math.max(0, (height - HEAD - weeks * WEEK) / RING) : Math.max(1, (height - 32) / RING);
        v.removeAllViews(R.id.w_list);
        List<Agenda.Item> items = allowed && rows > 0 ? Agenda.upcoming(c, rows) : new ArrayList<>();
        for (Agenda.Item e : items) {
            RemoteViews r = new RemoteViews(c.getPackageName(), R.layout.widget_event);
            r.setTextViewText(R.id.w_title, e.title.isEmpty() ? "No title" : e.title);
            r.setTextViewText(R.id.w_time, when(c, e, today));
            r.setInt(R.id.w_dot, "setColorFilter", e.color | 0xff000000);
            r.setOnClickPendingIntent(R.id.w_event, open(c, e.allDay ? Agenda.localDate(e.begin) : Agenda.midnight(at(e.begin)), false));
            v.addView(R.id.w_list, r);
        }
        boolean nothing = items.isEmpty() && (!tall || rows > 0);
        v.setViewVisibility(R.id.w_empty, nothing ? View.VISIBLE : View.GONE);
        v.setTextViewText(R.id.w_empty, allowed ? "Nothing coming up" : "Tap to allow calendar access");
        m.updateAppWidget(id, v);
        wake(c, today, items);
    }

    /** The month's weeks; how many it took. */
    private static int month(Context c, RemoteViews v, Calendar today, boolean allowed) {
        v.removeAllViews(R.id.w_grid);
        int firstWeekday = today.getFirstDayOfWeek();
        RemoteViews names = new RemoteViews(c.getPackageName(), R.layout.widget_week);
        Calendar d = (Calendar) today.clone();
        d.set(Calendar.DAY_OF_WEEK, firstWeekday);
        SimpleDateFormat letter = new SimpleDateFormat("EEEEE", Locale.getDefault());
        for (int i = 0; i < 7; i++) {
            RemoteViews n = new RemoteViews(c.getPackageName(), R.layout.widget_weekday);
            n.setTextViewText(R.id.w_name, letter.format(d.getTime()));
            names.addView(R.id.w_week, n);
            d.add(Calendar.DATE, 1);
        }
        v.addView(R.id.w_grid, names);
        Calendar first = (Calendar) today.clone();
        first.set(Calendar.DATE, 1);
        Calendar cell = (Calendar) first.clone();
        cell.add(Calendar.DATE, -((first.get(Calendar.DAY_OF_WEEK) - firstWeekday + 7) % 7));
        Calendar end = (Calendar) first.clone();
        end.add(Calendar.MONTH, 1);
        Calendar stop = (Calendar) cell.clone();
        stop.add(Calendar.DATE, 42);
        List<Agenda.Item> events = allowed ? Agenda.events(c, cell.getTimeInMillis() - 86_400_000L, stop.getTimeInMillis() + 86_400_000L) : new ArrayList<>();
        int muted = c.getColor(R.color.widget_outside);
        int weeks = 0;
        while (weeks < 6 && cell.before(end)) {
            RemoteViews week = new RemoteViews(c.getPackageName(), R.layout.widget_week);
            for (int i = 0; i < 7; i++) {
                boolean isToday = cell.equals(today);
                RemoteViews r = new RemoteViews(c.getPackageName(), isToday ? R.layout.widget_cell_today : R.layout.widget_cell);
                r.setTextViewText(R.id.w_num, Integer.toString(cell.get(Calendar.DATE)));
                if (cell.get(Calendar.MONTH) != today.get(Calendar.MONTH)) r.setTextColor(R.id.w_num, muted);
                for (Agenda.Item e : events) {
                    if (e.on(cell)) {
                        r.setViewVisibility(R.id.w_mark, View.VISIBLE);
                        r.setInt(R.id.w_mark, "setColorFilter", e.color | 0xff000000);
                        break;
                    }
                }
                r.setOnClickPendingIntent(R.id.w_cell, open(c, cell, false));
                week.addView(R.id.w_week, r);
                cell.add(Calendar.DATE, 1);
            }
            v.addView(R.id.w_grid, week);
            weeks++;
        }
        return weeks;
    }

    /** Draws again at midnight, or when the first event shown is over. */
    private static void wake(Context c, Calendar today, List<Agenda.Item> items) {
        Calendar next = (Calendar) today.clone();
        next.add(Calendar.DATE, 1);
        long at = next.getTimeInMillis();
        for (Agenda.Item e : items) if (!e.allDay) at = Math.min(at, e.end + 1000);
        Intent i = new Intent(c, CalWidget.class).setAction(AppWidgetManager.ACTION_APPWIDGET_UPDATE)
                .putExtra(AppWidgetManager.EXTRA_APPWIDGET_IDS, AppWidgetManager.getInstance(c).getAppWidgetIds(new ComponentName(c, CalWidget.class)));
        c.getSystemService(AlarmManager.class).setAndAllowWhileIdle(AlarmManager.RTC, at,
                PendingIntent.getBroadcast(c, 0, i, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT));
    }
}
