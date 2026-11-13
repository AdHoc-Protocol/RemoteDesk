// Text from the phone to the computer. Android lets an app read the clipboard only while it is in front, so the user of the phone
// sends the text: selects it in any app and picks "To the computer" in the menu of the selection, shares it to RemoteDesk, or
// presses "Send the clipboard" in the notification - this window, unseen, comes in front for a moment and reads it then.

package org.unirail.RemoteDesk

import android.app.Activity
import android.content.ClipboardManager
import android.content.Intent
import android.os.Bundle
import android.widget.Toast

class SendActivity : Activity() {
    companion object {
        const val CLIPBOARD = "org.unirail.RemoteDesk.CLIPBOARD"
    }

    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)
        val text = when (intent?.action) {
            Intent.ACTION_PROCESS_TEXT -> intent.getCharSequenceExtra(Intent.EXTRA_PROCESS_TEXT)
            Intent.ACTION_SEND -> intent.getCharSequenceExtra(Intent.EXTRA_TEXT)
            CLIPBOARD -> return // read once the window has the focus
            else -> null
        }
        send(text)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && intent?.action == CLIPBOARD) {
            val clip = getSystemService(ClipboardManager::class.java).primaryClip
            send(clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(this))
        }
    }

    private fun send(text: CharSequence?) {
        when {
            !Host.running -> Toast.makeText(this, "Start RemoteDesk first: the computer takes the text from it", Toast.LENGTH_LONG).show()
            text.isNullOrEmpty() -> Toast.makeText(this, "No text to send", Toast.LENGTH_SHORT).show()
            else -> {
                Host.clip(text.toString().toByteArray())
                Toast.makeText(this, "Sent to the computer: paste it there", Toast.LENGTH_SHORT).show()
            }
        }
        finish()
    }
}
