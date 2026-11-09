package io.github.vitrixxl.nexus;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Intent;
import android.os.Bundle;
import android.widget.Toast;

/**
 * Sends text to the desktop's clipboard, then goes away: shared text, or this
 * phone's clipboard when the Quick Settings tile asks. Android lets only the
 * app with focus read the clipboard, hence a window, invisible.
 */
public class Send extends Activity {
    static final String CLIPBOARD = "io.github.vitrixxl.nexus.CLIPBOARD";

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        Intent i = getIntent();
        if (Intent.ACTION_SEND.equals(i.getAction())) done(i.getStringExtra(Intent.EXTRA_TEXT));
    }

    @Override public void onWindowFocusChanged(boolean focus) {
        super.onWindowFocusChanged(focus);
        if (!focus || !CLIPBOARD.equals(getIntent().getAction())) return;
        ClipData clip = getSystemService(ClipboardManager.class).getPrimaryClip();
        CharSequence text = clip != null && clip.getItemCount() > 0 ? clip.getItemAt(0).coerceToText(this) : null;
        done(text == null ? null : text.toString());
    }

    private void done(String text) {
        Toast.makeText(this, text == null || text.isEmpty() ? "Nothing to send" : Link.sendClip(text), Toast.LENGTH_SHORT).show();
        finish();
    }
}
