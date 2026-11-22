package org.zerolink.media

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.content.ContextCompat

object PhotoAccess {
    fun permissionsToRequest(): Array<String> = permissionsToRequest(Build.VERSION.SDK_INT)

    fun permissionsToRequest(sdkVersion: Int): Array<String> = when {
        sdkVersion >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE -> arrayOf(
            Manifest.permission.READ_MEDIA_IMAGES,
            Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED
        )
        sdkVersion >= Build.VERSION_CODES.TIRAMISU -> arrayOf(
            Manifest.permission.READ_MEDIA_IMAGES
        )
        else -> arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE)
    }

    fun hasImageAccess(context: Context): Boolean {
        val sdkVersion = Build.VERSION.SDK_INT
        return hasImageAccess(
            sdkVersion,
            hasPermission(context, fullReadPermission(sdkVersion)),
            sdkVersion >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE &&
                hasPermission(context, Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED)
        )
    }

    fun hasPartialImageAccess(context: Context): Boolean {
        val sdkVersion = Build.VERSION.SDK_INT
        return hasPartialImageAccess(
            sdkVersion,
            hasPermission(context, fullReadPermission(sdkVersion)),
            sdkVersion >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE &&
                hasPermission(context, Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED)
        )
    }

    fun hasImageAccess(sdkVersion: Int, hasFullAccess: Boolean, hasSelectedAccess: Boolean): Boolean =
        hasFullAccess || (sdkVersion >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE && hasSelectedAccess)

    fun hasPartialImageAccess(sdkVersion: Int, hasFullAccess: Boolean, hasSelectedAccess: Boolean): Boolean =
        sdkVersion >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE && hasSelectedAccess && !hasFullAccess

    private fun fullReadPermission(sdkVersion: Int): String =
        if (sdkVersion >= Build.VERSION_CODES.TIRAMISU) {
            Manifest.permission.READ_MEDIA_IMAGES
        } else {
            Manifest.permission.READ_EXTERNAL_STORAGE
        }

    private fun hasPermission(context: Context, permission: String): Boolean =
        ContextCompat.checkSelfPermission(context, permission) == PackageManager.PERMISSION_GRANTED
}
