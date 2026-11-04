package org.zerolink.clipboard

import android.os.Build
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.widget.Toast
import androidx.annotation.RequiresApi
import org.zerolink.MainActivity
import org.zerolink.ZeroLinkService

@RequiresApi(Build.VERSION_CODES.N)
class ZeroClipboardTileService : TileService() {

    override fun onStartListening() {
        super.onStartListening()
        val tile = qsTile ?: return
        val isServiceRunning = ZeroLinkService.instance != null
        tile.state = if (isServiceRunning) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE
        tile.updateTile()
    }

    override fun onClick() {
        super.onClick()
        val service = ZeroLinkService.instance
        if (service != null) {
            service.clipboardManager.syncClipboardNow()
            Toast.makeText(this, "📋 Clipboard synced to PC", Toast.LENGTH_SHORT).show()
            MainActivity.appendLogStatic("📋 Quick Settings Tile tapped: Clipboard synced to PC")
        } else {
            Toast.makeText(this, "⚠️ ZeroLink service is not active", Toast.LENGTH_SHORT).show()
        }
    }
}

