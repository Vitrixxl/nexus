package io.github.vitrixxl.nexus;

import android.Manifest;
import android.content.ContentUris;
import android.content.ContentValues;
import android.content.Context;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.net.Uri;
import android.provider.CalendarContract;
import android.provider.CalendarContract.Calendars;
import android.provider.CalendarContract.Events;
import android.provider.CalendarContract.Instances;
import android.provider.CalendarContract.Reminders;

import org.json.JSONArray;
import org.json.JSONObject;

import java.util.ArrayList;
import java.util.Calendar;
import java.util.List;
import java.util.TimeZone;

/** This phone's calendars: what the app, its widget and the desktop read and change. */
final class Agenda {
    private static final int MAX = 5000;
    private static final String NEXUS = "Nexus";
    private static final long DAY = 86_400_000L;

    private Agenda() {}

    /** One occurrence. All-day times are midnights UTC, the end excluded. */
    static final class Item {
        String id = "", title = "", location = "";
        long begin, end, instance, calendar;
        boolean allDay, repeats, writable = true;
        int color;

        JSONObject json() throws Exception {
            return new JSONObject().put("id", id).put("title", title).put("location", location)
                    .put("begin", begin).put("end", end).put("instance", instance).put("calendar", calendar)
                    .put("all_day", allDay).put("repeats", repeats).put("writable", writable)
                    .put("color", color & 0xffffffffL);
        }
        static Item from(JSONObject o) {
            Item e = new Item();
            e.id = o.optString("id");
            e.title = o.optString("title");
            e.location = o.optString("location");
            e.begin = o.optLong("begin");
            e.end = o.optLong("end");
            e.instance = o.optLong("instance");
            e.calendar = o.optLong("calendar");
            e.allDay = o.optBoolean("all_day");
            e.repeats = o.optBoolean("repeats");
            return e;
        }
        /** Whether it takes place on the local date that starts at `day`. */
        boolean on(Calendar day) {
            Calendar next = (Calendar) day.clone();
            next.add(Calendar.DATE, 1);
            if (allDay) {
                long utc = utcMidnight(day);
                return begin <= utc && utc < Math.max(end, begin + 1);
            }
            return begin < next.getTimeInMillis() && (end > day.getTimeInMillis() || begin >= day.getTimeInMillis());
        }
    }

    static final class Cal {
        long id;
        String name;
        int color;
        boolean writable, primary;
    }

    static long utcMidnight(Calendar local) {
        Calendar u = Calendar.getInstance(TimeZone.getTimeZone("UTC"));
        u.clear();
        u.set(local.get(Calendar.YEAR), local.get(Calendar.MONTH), local.get(Calendar.DATE));
        return u.getTimeInMillis();
    }
    /** The local midnight of the UTC date at `utc`. */
    static Calendar localDate(long utc) {
        Calendar u = Calendar.getInstance(TimeZone.getTimeZone("UTC"));
        u.setTimeInMillis(utc);
        Calendar l = Calendar.getInstance();
        l.clear();
        l.set(u.get(Calendar.YEAR), u.get(Calendar.MONTH), u.get(Calendar.DATE));
        return l;
    }
    static Calendar midnight(Calendar c) {
        Calendar m = (Calendar) c.clone();
        m.set(Calendar.HOUR_OF_DAY, 0);
        m.set(Calendar.MINUTE, 0);
        m.set(Calendar.SECOND, 0);
        m.set(Calendar.MILLISECOND, 0);
        return m;
    }

    static boolean allowed(Context c) {
        return c.checkSelfPermission(Manifest.permission.READ_CALENDAR) == PackageManager.PERMISSION_GRANTED
                && c.checkSelfPermission(Manifest.permission.WRITE_CALENDAR) == PackageManager.PERMISSION_GRANTED;
    }

    static List<Item> events(Context c, long from, long to) {
        List<Item> out = new ArrayList<>();
        Uri.Builder uri = Instances.CONTENT_URI.buildUpon();
        ContentUris.appendId(uri, from);
        ContentUris.appendId(uri, to);
        String[] cols = {Instances.EVENT_ID, Instances.BEGIN, Instances.END, Instances.TITLE, Instances.ALL_DAY,
                Instances.EVENT_LOCATION, Instances.DISPLAY_COLOR, Instances.CALENDAR_ID, Instances.RRULE,
                Instances.CALENDAR_ACCESS_LEVEL};
        String where = Instances.VISIBLE + "=1 AND (" + Instances.STATUS + " IS NULL OR " + Instances.STATUS + "!=" + Instances.STATUS_CANCELED + ")";
        try (Cursor q = c.getContentResolver().query(uri.build(), cols, where, null, Instances.BEGIN + " ASC")) {
            while (q != null && q.moveToNext() && out.size() < MAX) {
                Item e = new Item();
                e.id = Long.toString(q.getLong(0));
                e.begin = e.instance = q.getLong(1);
                e.end = q.getLong(2);
                e.title = q.isNull(3) ? "" : q.getString(3);
                e.allDay = q.getInt(4) != 0;
                e.location = q.isNull(5) ? "" : q.getString(5);
                e.color = q.getInt(6);
                e.calendar = q.getLong(7);
                e.repeats = !q.isNull(8) && !q.getString(8).isEmpty();
                e.writable = q.getInt(9) >= Calendars.CAL_ACCESS_CONTRIBUTOR;
                out.add(e);
            }
        }
        return out;
    }

    static List<Cal> calendars(Context c) {
        List<Cal> out = new ArrayList<>();
        String[] cols = {Calendars._ID, Calendars.CALENDAR_DISPLAY_NAME, Calendars.CALENDAR_COLOR, Calendars.CALENDAR_ACCESS_LEVEL, Calendars.IS_PRIMARY};
        try (Cursor q = c.getContentResolver().query(Calendars.CONTENT_URI, cols, Calendars.VISIBLE + "=1", null, Calendars.CALENDAR_DISPLAY_NAME + " ASC")) {
            while (q != null && q.moveToNext()) {
                Cal k = new Cal();
                k.id = q.getLong(0);
                k.name = q.isNull(1) ? "Calendar" : q.getString(1);
                k.color = q.getInt(2);
                k.writable = q.getInt(3) >= Calendars.CAL_ACCESS_CONTRIBUTOR;
                k.primary = q.getInt(4) != 0;
                out.add(k);
            }
        }
        return out;
    }

    /** {events, calendars} for the desktop. */
    static JSONObject list(Context c, long from, long to) throws Exception {
        if (!allowed(c)) throw new Exception("Allow calendar access in Nexus on your phone");
        JSONArray events = new JSONArray();
        for (Item e : events(c, from, to)) events.put(e.json());
        JSONArray cals = new JSONArray();
        for (Cal k : calendars(c)) {
            cals.put(new JSONObject().put("id", k.id).put("name", k.name).put("color", k.color & 0xffffffffL)
                    .put("writable", k.writable).put("primary", k.primary));
        }
        return new JSONObject().put("events", events).put("calendars", cals);
    }

    /** The calendar new events go to: the primary one, else any, else a new one on this phone. */
    private static long target(Context c) throws Exception {
        Cal any = null;
        for (Cal k : calendars(c)) {
            if (!k.writable) continue;
            if (k.primary) return k.id;
            if (any == null) any = k;
        }
        if (any != null) return any.id;
        Uri uri = Calendars.CONTENT_URI.buildUpon()
                .appendQueryParameter(CalendarContract.CALLER_IS_SYNCADAPTER, "true")
                .appendQueryParameter(Calendars.ACCOUNT_NAME, NEXUS)
                .appendQueryParameter(Calendars.ACCOUNT_TYPE, CalendarContract.ACCOUNT_TYPE_LOCAL).build();
        ContentValues v = new ContentValues();
        v.put(Calendars.ACCOUNT_NAME, NEXUS);
        v.put(Calendars.ACCOUNT_TYPE, CalendarContract.ACCOUNT_TYPE_LOCAL);
        v.put(Calendars.NAME, NEXUS);
        v.put(Calendars.CALENDAR_DISPLAY_NAME, NEXUS);
        v.put(Calendars.CALENDAR_COLOR, 0xff0a84ff);
        v.put(Calendars.CALENDAR_ACCESS_LEVEL, Calendars.CAL_ACCESS_OWNER);
        v.put(Calendars.OWNER_ACCOUNT, NEXUS);
        v.put(Calendars.CALENDAR_TIME_ZONE, TimeZone.getDefault().getID());
        v.put(Calendars.SYNC_EVENTS, 1);
        v.put(Calendars.VISIBLE, 1);
        Uri made = c.getContentResolver().insert(uri, v);
        if (made == null) throw new Exception("Could not create a calendar");
        return ContentUris.parseId(made);
    }

    private static ContentValues fields(Item e) {
        ContentValues v = new ContentValues();
        v.put(Events.TITLE, e.title);
        v.put(Events.DTSTART, e.begin);
        v.put(Events.DTEND, e.end);
        v.put(Events.ALL_DAY, e.allDay ? 1 : 0);
        v.put(Events.EVENT_TIMEZONE, e.allDay ? "UTC" : TimeZone.getDefault().getID());
        return v;
    }

    /** A new event if it has no id yet; only this occurrence of a repeating one. */
    static void save(Context c, Item e) throws Exception {
        if (!allowed(c)) throw new Exception("Allow calendar access in Nexus on your phone");
        ContentValues v = fields(e);
        if (e.id.isEmpty()) {
            v.put(Events.CALENDAR_ID, e.calendar != 0 ? e.calendar : target(c));
            Uri made = c.getContentResolver().insert(Events.CONTENT_URI, v);
            if (made == null) throw new Exception("The calendar refused the event");
            if (!e.allDay) {
                ContentValues r = new ContentValues();
                r.put(Reminders.EVENT_ID, ContentUris.parseId(made));
                r.put(Reminders.MINUTES, 10);
                r.put(Reminders.METHOD, Reminders.METHOD_ALERT);
                c.getContentResolver().insert(Reminders.CONTENT_URI, r);
            }
        } else if (e.repeats) {
            v.put(Events.ORIGINAL_INSTANCE_TIME, e.instance);
            exception(c, e, v);
        } else if (c.getContentResolver().update(ContentUris.withAppendedId(Events.CONTENT_URI, Long.parseLong(e.id)), v, null, null) == 0) {
            throw new Exception("This event is gone from the phone");
        }
    }

    /** Deletes it; of a repeating event, this occurrence alone. */
    static void delete(Context c, Item e) throws Exception {
        if (!allowed(c)) throw new Exception("Allow calendar access in Nexus on your phone");
        if (e.repeats) {
            ContentValues v = new ContentValues();
            v.put(Events.ORIGINAL_INSTANCE_TIME, e.instance);
            v.put(Events.STATUS, Events.STATUS_CANCELED);
            exception(c, e, v);
        } else {
            c.getContentResolver().delete(ContentUris.withAppendedId(Events.CONTENT_URI, Long.parseLong(e.id)), null, null);
        }
    }

    private static void exception(Context c, Item e, ContentValues v) throws Exception {
        if (c.getContentResolver().insert(ContentUris.withAppendedId(Events.CONTENT_EXCEPTION_URI, Long.parseLong(e.id)), v) == null) {
            throw new Exception("The calendar refused the change");
        }
    }

    /** What is left of today and what follows, soonest first. */
    static List<Item> upcoming(Context c, int count) {
        long now = System.currentTimeMillis();
        Calendar today = midnight(Calendar.getInstance());
        List<Item> out = new ArrayList<>();
        for (Item e : events(c, today.getTimeInMillis() - DAY, now + 45 * DAY)) {
            boolean over = e.allDay ? !e.on(today) && e.end <= utcMidnight(today) : e.end <= now;
            if (over) continue;
            out.add(e);
        }
        out.sort((a, b) -> Long.compare(a.allDay ? localDate(a.begin).getTimeInMillis() : a.begin, b.allDay ? localDate(b.begin).getTimeInMillis() : b.begin));
        return out.size() > count ? new ArrayList<>(out.subList(0, count)) : out;
    }
}
