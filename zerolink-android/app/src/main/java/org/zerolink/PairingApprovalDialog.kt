package org.zerolink

import android.app.Activity
import androidx.appcompat.app.AlertDialog

object PairingApprovalDialog {
    fun show(activity: Activity, peerName: String, code: String, onDecision: (Boolean) -> Unit): AlertDialog =
        AlertDialog.Builder(activity)
            .setTitle("Pair ZeroLink device?")
            .setMessage(
                "Windows device: $peerName\n\nCompare this code with the Windows screen before approving:\n\n$code\n\nOnly approve if both codes match."
            )
            .setPositiveButton("Approve") { _, _ -> onDecision(true) }
            .setNegativeButton("Reject") { _, _ -> onDecision(false) }
            .setOnCancelListener { onDecision(false) }
            .show()
}
