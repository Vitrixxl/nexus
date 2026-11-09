package io.github.vitrixxl.nexus;

import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.RectF;
import android.graphics.Typeface;
import android.text.Editable;
import android.text.Layout;
import android.text.Spanned;
import android.text.style.ForegroundColorSpan;
import android.text.style.LeadingMarginSpan;
import android.text.style.LineBackgroundSpan;
import android.text.style.RelativeSizeSpan;
import android.text.style.ReplacementSpan;
import android.text.style.StrikethroughSpan;
import android.text.style.StyleSpan;
import android.text.style.TypefaceSpan;

import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * The page editor's Markdown: the text stays the file, exactly as the desktop
 * writes it, and spans draw it as blocks. Headings, bulleted, numbered and
 * to-do lists, quote, callout (`> [!NOTE]`), code and divider, as in Nexus
 * Notes; bold, italic, code and links inline. Markers stay in the text, muted,
 * so that editing never loses or rewrites anything.
 */
final class Markdown {
    /** Every span this draws, so that a restyle removes exactly those. */
    interface Md {}
    static final class Bold extends StyleSpan implements Md { Bold() { super(Typeface.BOLD); } }
    static final class Italic extends StyleSpan implements Md { Italic() { super(Typeface.ITALIC); } }
    static final class Size extends RelativeSizeSpan implements Md { Size(float f) { super(f); } }
    static final class Color extends ForegroundColorSpan implements Md { Color(int c) { super(c); } }
    static final class Mono extends TypefaceSpan implements Md { Mono() { super("monospace"); } }
    static final class Strike extends StrikethroughSpan implements Md {}

    final int fg, muted, accent, code, callout;
    final float dp;

    Markdown(int fg, int muted, int accent, int code, int callout, float dp) {
        this.fg = fg; this.muted = muted; this.accent = accent; this.code = code; this.callout = callout; this.dp = dp;
    }

    /** A list marker drawn as a dot, in the width of the marker. */
    final class Bullet extends ReplacementSpan implements Md {
        @Override public int getSize(Paint p, CharSequence t, int s, int e, Paint.FontMetricsInt fm) { return (int) p.measureText("- "); }
        @Override public void draw(Canvas c, CharSequence t, int s, int e, float x, int top, int y, int bottom, Paint p) {
            int was = p.getColor();
            p.setColor(muted);
            float r = 2.6f * dp;
            c.drawCircle(x + p.measureText("-") / 2 + r / 2, (y + p.ascent() / 2.6f), r, p);
            p.setColor(was);
        }
    }

    /** `- [ ]` drawn as a box, ticked for `- [x]`; tapping it toggles. */
    final class Check extends ReplacementSpan implements Md {
        final boolean done;
        Check(boolean done) { this.done = done; }
        @Override public int getSize(Paint p, CharSequence t, int s, int e, Paint.FontMetricsInt fm) { return (int) (p.getTextSize() * 1.15f + 8 * dp); }
        @Override public void draw(Canvas c, CharSequence t, int s, int e, float x, int top, int y, int bottom, Paint p) {
            int was = p.getColor();
            Paint.Style style = p.getStyle();
            float size = p.getTextSize() * 1.05f, cy = y + p.ascent() / 2.8f;
            RectF box = new RectF(x + dp, cy - size / 2, x + dp + size, cy + size / 2);
            float r = 5 * dp;
            if (done) {
                p.setStyle(Paint.Style.FILL);
                p.setColor(accent);
                c.drawRoundRect(box, r, r, p);
                p.setColor(0xffffffff);
                p.setStyle(Paint.Style.STROKE);
                p.setStrokeWidth(2 * dp);
                p.setStrokeCap(Paint.Cap.ROUND);
                float l = box.left, w = size;
                c.drawLine(l + w * 0.26f, cy + w * 0.02f, l + w * 0.44f, cy + w * 0.2f, p);
                c.drawLine(l + w * 0.44f, cy + w * 0.2f, l + w * 0.76f, cy - w * 0.18f, p);
            } else {
                p.setStyle(Paint.Style.STROKE);
                p.setStrokeWidth(1.6f * dp);
                p.setColor(muted);
                box.inset(0.8f * dp, 0.8f * dp);
                c.drawRoundRect(box, r, r, p);
            }
            p.setStyle(style);
            p.setColor(was);
        }
    }

    /** The bar of a quote or callout, in the margin. */
    final class Bar implements LeadingMarginSpan, Md {
        final int color;
        Bar(int color) { this.color = color; }
        @Override public int getLeadingMargin(boolean first) { return (int) (14 * dp); }
        @Override public void drawLeadingMargin(Canvas c, Paint p, int x, int dir, int top, int baseline, int bottom,
                                                CharSequence t, int s, int e, boolean first, Layout l) {
            int was = p.getColor();
            Paint.Style style = p.getStyle();
            p.setStyle(Paint.Style.FILL);
            p.setColor(color);
            c.drawRect(x, top, x + 3 * dp, bottom, p);
            p.setStyle(style);
            p.setColor(was);
        }
    }

    /** A tint behind whole lines: code and callouts. */
    static final class Fill implements LineBackgroundSpan, Md {
        final int color;
        Fill(int color) { this.color = color; }
        @Override public void drawBackground(Canvas c, Paint p, int left, int right, int top, int baseline, int bottom,
                                             CharSequence t, int s, int e, int line) {
            int was = p.getColor();
            p.setColor(color);
            c.drawRect(left, top, right, bottom, p);
            p.setColor(was);
        }
    }

    /** `---` drawn as a rule across the page. */
    final class Rule implements LineBackgroundSpan, Md {
        @Override public void drawBackground(Canvas c, Paint p, int left, int right, int top, int baseline, int bottom,
                                             CharSequence t, int s, int e, int line) {
            int was = p.getColor();
            p.setColor(muted & 0x66ffffff);
            float y = (top + bottom) / 2f;
            c.drawRect(left, y - dp / 2, right, y + dp / 2, p);
            p.setColor(was);
        }
    }

    static final Pattern HEADING = Pattern.compile("^(#{1,3}) ");
    static final Pattern TODO = Pattern.compile("^(\\s*)[-*+] \\[([ xX])\\] ");
    static final Pattern BULLET = Pattern.compile("^(\\s*)[-*+] ");
    static final Pattern NUMBER = Pattern.compile("^(\\s*)(\\d{1,9})([.)]) ");
    static final Pattern CALLOUT = Pattern.compile("^> ?\\[!\\w+\\]");
    static final Pattern DIVIDER = Pattern.compile("^ {0,3}([-*_])( *\\1){2,} *$");
    static final Pattern INLINE = Pattern.compile("(\\*\\*|__)(?=\\S)(.+?)(?<=\\S)\\1|(?<![*\\w])([*_])(?=\\S)(.+?)(?<=\\S)\\3(?![*\\w])|`([^`\\n]+)`|\\[([^\\]\\n]+)\\]\\(([^)\\n]+)\\)");
    /** Any block marker at the start of a line, for changing a line's kind. */
    static final Pattern BLOCK = Pattern.compile("^(#{1,3} |\\s*[-*+] \\[[ xX]\\] |\\s*[-*+] |\\s*\\d{1,9}[.)] |> ?\\[!\\w+\\] ?|> ?)");

    /** Draws all of `e` afresh. */
    void style(Editable e) {
        for (Object o : e.getSpans(0, e.length(), Md.class)) e.removeSpan(o);
        String text = e.toString();
        boolean fence = false, callout = false;
        int at = 0;
        while (at <= text.length()) {
            int end = text.indexOf('\n', at);
            if (end < 0) end = text.length();
            String line = text.substring(at, end);
            int lineEnd = Math.min(end + 1, text.length());
            if (line.trim().startsWith("```")) {
                set(e, new Mono(), at, end);
                set(e, new Color(muted), at, end);
                set(e, new Fill(code), at, lineEnd);
                fence = !fence;
            } else if (fence) {
                set(e, new Mono(), at, end);
                set(e, new Fill(code), at, lineEnd);
            } else {
                callout = line.startsWith(">") && (callout || CALLOUT.matcher(line).find());
                block(e, line, at, end, lineEnd, callout);
            }
            if (end >= text.length()) break;
            at = end + 1;
        }
    }

    private void block(Editable e, String line, int at, int end, int lineEnd, boolean callout) {
        Matcher m;
        int body = at;
        if ((m = HEADING.matcher(line)).find()) {
            int level = m.group(1).length();
            set(e, new Size(level == 1 ? 1.55f : level == 2 ? 1.3f : 1.12f), at, end);
            set(e, new Bold(), at, end);
            set(e, new Color(muted), at, at + m.end());
            body = at + m.end();
        } else if ((m = TODO.matcher(line)).find()) {
            boolean done = !m.group(2).equals(" ");
            set(e, new Check(done), at + m.end(1), at + m.end() - 1);
            if (done) {
                set(e, new Strike(), at + m.end(), end);
                set(e, new Color(muted), at + m.end(), end);
            }
            body = at + m.end();
        } else if ((m = BULLET.matcher(line)).find()) {
            set(e, new Bullet(), at + m.end(1), at + m.end());
            body = at + m.end();
        } else if ((m = NUMBER.matcher(line)).find()) {
            set(e, new Color(muted), at + m.end(1), at + m.end());
            body = at + m.end();
        } else if (line.startsWith(">")) {
            set(e, new Bar(callout ? accent : muted), at, lineEnd);
            if (callout) set(e, new Fill(this.callout), at, lineEnd);
            Matcher c = CALLOUT.matcher(line);
            if (c.find()) {
                set(e, new Color(muted), at, at + 2);
                set(e, new Color(accent), at + 2, at + c.end());
                set(e, new Bold(), at + 2, at + c.end());
                set(e, new Size(0.8f), at, at + c.end());
                return;
            }
            int marker = line.startsWith("> ") ? 2 : 1;
            set(e, new Color(muted), at, at + marker);
            if (!callout) set(e, new Color(mix()), at + marker, end);
            body = at + marker;
        } else if (DIVIDER.matcher(line).find()) {
            set(e, new Rule(), at, lineEnd);
            set(e, new Color(0), at, end);
            return;
        }
        inline(e, line.substring(body - at), body);
    }

    /** Quote text: between the text and muted colours. */
    private int mix() {
        int r = (((fg >> 16) & 0xff) * 3 + ((muted >> 16) & 0xff)) / 4;
        int g = (((fg >> 8) & 0xff) * 3 + ((muted >> 8) & 0xff)) / 4;
        int b = ((fg & 0xff) * 3 + (muted & 0xff)) / 4;
        return 0xff000000 | (r << 16) | (g << 8) | b;
    }

    private void inline(Editable e, String s, int at) {
        Matcher m = INLINE.matcher(s);
        while (m.find()) {
            int a = at + m.start(), b = at + m.end();
            if (m.group(2) != null) {
                set(e, new Bold(), a, b);
                set(e, new Color(muted), a, a + 2);
                set(e, new Color(muted), b - 2, b);
            } else if (m.group(4) != null) {
                set(e, new Italic(), a, b);
                set(e, new Color(muted), a, a + 1);
                set(e, new Color(muted), b - 1, b);
            } else if (m.group(5) != null) {
                set(e, new Mono(), a, b);
                set(e, new Color(muted), a, a + 1);
                set(e, new Color(muted), b - 1, b);
            } else {
                int close = at + m.start(7) - 2;
                set(e, new Color(accent), a + 1, close);
                set(e, new Color(muted), a, a + 1);
                set(e, new Color(muted), close, b);
            }
        }
    }

    private static void set(Editable e, Object span, int a, int b) {
        if (b > a) e.setSpan(span, a, b, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
    }

    // ---- Editing

    static int lineStart(CharSequence s, int at) {
        while (at > 0 && s.charAt(at - 1) != '\n') at--;
        return at;
    }

    static int lineEnd(CharSequence s, int at) {
        while (at < s.length() && s.charAt(at) != '\n') at++;
        return at;
    }

    /**
     * After a line break typed at `at`: the list or quote goes on; an empty
     * item ends it instead. Returns whether it changed the text.
     */
    static boolean continueBlock(Editable e, int at) {
        if (inFence(e, at)) return false;
        int start = lineStart(e, at);
        String prev = e.subSequence(start, at).toString();
        String next = null;
        int marker = 0;
        Matcher m;
        if ((m = TODO.matcher(prev)).find()) next = m.group(1) + "- [ ] ";
        else if ((m = BULLET.matcher(prev)).find()) next = m.group(0);
        else if ((m = NUMBER.matcher(prev)).find()) next = m.group(1) + (Long.parseLong(m.group(2)) + 1) + m.group(3) + " ";
        else if (prev.startsWith(">")) next = "> ";
        if (next == null) return false;
        marker = prev.startsWith(">") ? (prev.startsWith("> ") ? 2 : 1) : m.end();
        if (prev.substring(marker).trim().isEmpty() && !CALLOUT.matcher(prev).find()) {
            // An empty item: the list ends here, the break stays the only one.
            e.replace(start, at + 1, "");
            return true;
        }
        e.insert(at + 1, next);
        return true;
    }

    static boolean inFence(CharSequence s, int at) {
        boolean fence = false;
        int i = 0;
        while (i < at) {
            int end = lineEnd(s, i);
            if (end >= at) break;
            if (s.subSequence(i, end).toString().trim().startsWith("```")) fence = !fence;
            i = end + 1;
        }
        return fence;
    }

    /**
     * Makes the lines from `from` to `to` the block `prefix` starts ("# ",
     * "- ", "- [ ] ", "1. ", "> ", "> [!NOTE]"); lines that already are turn
     * back into text.
     */
    static void setBlock(Editable e, int from, int to, String prefix) {
        int start = lineStart(e, from), end = lineEnd(e, Math.max(from, to));
        String[] lines = e.subSequence(start, end).toString().split("\n", -1);
        boolean all = true;
        for (String l : lines) if (!marker(l).trim().equals(prefix.trim()) && !(prefix.startsWith("1") && NUMBER.matcher(l).find())) all = false;
        StringBuilder out = new StringBuilder();
        int n = 1;
        for (int i = 0; i < lines.length; i++) {
            String rest = lines[i].substring(marker(lines[i]).length());
            if (i > 0) out.append('\n');
            if (all) out.append(rest);
            else if (prefix.equals("> [!NOTE]")) out.append(i == 0 ? "> [!NOTE]\n> " : "> ").append(rest);
            else out.append(prefix.startsWith("1") ? (n++) + ". " : prefix).append(rest);
        }
        e.replace(start, end, out.toString());
    }

    static String marker(String line) {
        Matcher m = BLOCK.matcher(line);
        return m.find() ? m.group() : "";
    }

    /** Fences the lines from `from` to `to` as code, or unfences them. */
    static void toggleCode(Editable e, int from, int to) {
        int start = lineStart(e, from), end = lineEnd(e, Math.max(from, to));
        String body = e.subSequence(start, end).toString();
        if (inFence(e, start)) return;
        e.replace(start, end, "```\n" + body + "\n```");
    }

    /** A divider on its own line after the line at `at`. */
    static int divider(Editable e, int at) {
        int end = lineEnd(e, at);
        String add = (lineStart(e, at) == end ? "" : "\n") + "---" + (end == e.length() ? "\n" : "");
        e.insert(end, add);
        return Math.min(e.length(), end + add.length() + (add.endsWith("\n") ? 0 : 1));
    }

    /** `- [ ]` ↔ `- [x]` for the box at the start of the to-do the span covers. */
    static void toggleTodo(Editable e, Check box) {
        int s = e.getSpanStart(box);
        if (s < 0 || s + 3 >= e.length()) return;
        e.replace(s + 3, s + 4, box.done ? " " : "x");
    }
}
