package io.github.vitrixxl.nexus;

import android.app.PendingIntent;
import android.content.Intent;
import android.os.Build;
import android.service.quicksettings.TileService;

/** A Quick Settings tile: this phone's clipboard, onto the desktop's. */
public class ClipTile extends TileService {
    @Override public void onTileAdded() { Main.tileAdded(this, true); }
    @Override public void onTileRemoved() { Main.tileAdded(this, false); }

    @Override public void onClick() {
        Intent i = new Intent(this, Send.class).setAction(Send.CLIPBOARD).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        if (Build.VERSION.SDK_INT >= 34) {
            startActivityAndCollapse(PendingIntent.getActivity(this, 0, i, PendingIntent.FLAG_IMMUTABLE));
        } else {
            startActivityAndCollapse(i);
        }
    }
}
