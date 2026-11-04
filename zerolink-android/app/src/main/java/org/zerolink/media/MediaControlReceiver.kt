package org.zerolink.media

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import org.zerolink.ZeroLinkService

/**
 * Receives playback actions from the foreground notification (prev / play-pause / next)
 * and forwards them to Windows through the active ZeroLink transport.
 */
class MediaControlReceiver : BroadcastReceiver() {

    companion object {
        const val ACTION_MEDIA_CONTROL = "org.zerolink.action.MEDIA_CONTROL"
        const val EXTRA_COMMAND = "command"
    }

    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_MEDIA_CONTROL) return
        val command = intent.getIntExtra(EXTRA_COMMAND, -1)
        if (command >= 0) {
            ZeroLinkService.instance?.sendMediaCommand(command)
        }
    }
}
