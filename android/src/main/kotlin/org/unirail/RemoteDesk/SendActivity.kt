// Text and pictures from the phone to the computer. Android lets an app read the clipboard only while it is in front, so the user of
// the phone sends them: selects text in any app and picks "To the computer" in the menu of the selection, shares text or a picture to
// RemoteDesk, or presses "Send the clipboard" in the notification - this window, unseen, comes in front for a moment and reads it
// then. A picture goes as PNG: one of another kind is made PNG here.

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
        if (intent?.action == Intent.ACTION_SEND && intent.type?.startsWith("image/") == true) {
            @Suppress("DEPRECATION")
            val uri: android.net.Uri? = if (android.os.Build.VERSION.SDK_INT >= 33) intent.getParcelableExtra(Intent.EXTRA_STREAM, android.net.Uri::class.java)
            else intent.getParcelableExtra(Intent.EXTRA_STREAM)
            return picture(uri)
        }
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
            val item = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)
            val uri = item?.uri
            if (uri != null && (contentResolver.getType(uri)?.startsWith("image/") == true)) return picture(uri)
            send(item?.coerceToText(this))
        }
    }

    /** The picture at `uri` to the computer, as PNG. */
    private fun picture(uri: android.net.Uri?) {
        val png = uri?.let {
            runCatching {
                if (contentResolver.getType(it) == "image/png") contentResolver.openInputStream(it)?.use { s -> s.readBytes() }
                else contentResolver.openInputStream(it)?.use { s -> android.graphics.BitmapFactory.decodeStream(s) }?.let { bitmap ->
                    java.io.ByteArrayOutputStream().also { out -> bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG, 100, out) }.toByteArray()
                }
            }.getOrNull()
        }
        when {
            !Host.running -> Toast.makeText(this, "Start RemoteDesk first: the computer takes the picture from it", Toast.LENGTH_LONG).show()
            png == null || png.isEmpty() -> Toast.makeText(this, "No picture to send", Toast.LENGTH_SHORT).show()
            png.size > 16 * 1024 * 1024 -> Toast.makeText(this, "The picture is larger than 16 MB: send it as a file", Toast.LENGTH_LONG).show()
            else -> {
                Host.clipImage(png)
                Toast.makeText(this, "The picture went to the computer: paste it there", Toast.LENGTH_SHORT).show()
            }
        }
        finish()
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
