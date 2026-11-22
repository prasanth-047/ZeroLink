package org.zerolink.clipboard

import android.content.BroadcastReceiver
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.widget.Toast
import org.zerolink.MainActivity
import org.zerolink.ZeroLinkService

class ClipboardActionReceiver : BroadcastReceiver() {

    companion object {
        const val ACTION_SYNC_CLIPBOARD = "org.zerolink.action.SYNC_CLIPBOARD"
    }

    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == ACTION_SYNC_CLIPBOARD) {
            val textToCopy = intent.getStringExtra("text")
            val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            if (!textToCopy.isNullOrEmpty()) {
                val clip = ClipData.newPlainText("ZeroLink Direct", textToCopy)
                cm.setPrimaryClip(clip)
            }
            val service = ZeroLinkService.instance
            if (service != null) {
                if (!textToCopy.isNullOrEmpty()) {
                    service.clipboardManager.sendClipboardText(textToCopy)
                } else {
                    service.clipboardManager.syncClipboardNow()
                }
                Toast.makeText(context, "📋 Clipboard synced to PC!", Toast.LENGTH_SHORT).show()
                MainActivity.appendLogStatic("📋 Clipboard synced to PC")
            } else {
                Toast.makeText(context, "⚠️ ZeroLink service is not running", Toast.LENGTH_SHORT).show()
            }
        }
    }
}

