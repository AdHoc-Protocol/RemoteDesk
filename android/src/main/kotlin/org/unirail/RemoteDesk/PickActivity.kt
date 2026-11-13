// A file of the phone to the viewer: the viewer asked for one ( Get a file ), and the user of the phone picks it in the picker of
// Android. The host gets a path that opens it under its name - a link to the descriptor the app holds: nothing is copied.

package org.unirail.RemoteDesk

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import java.io.File
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

class PickActivity : Activity() {
    companion object {
        private val answers = LinkedBlockingQueue<String>()
        @Volatile private var shown = false
        /** The descriptor of the file picked last: closed when another is picked. */
        private var held: ParcelFileDescriptor? = null

        /** Shows the picker and waits for its user: the path of the file; "" - cancelled; null - Android did not show it. */
        fun pick(context: Context): String? {
            answers.clear()
            shown = false
            context.startActivity(Intent(context, PickActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            // Android may refuse to open a window from the background: then nothing comes
            val first = answers.poll(5, TimeUnit.SECONDS)
            if (first != null) return first
            if (!shown) return null
            return answers.poll(10, TimeUnit.MINUTES) ?: ""
        }
    }

    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)
        shown = true
        if (saved == null) {
            @Suppress("DEPRECATION")
            startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("*/*"), 1)
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        val uri = data?.data
        answers.offer(if (resultCode == RESULT_OK && uri != null) runCatching { link(uri) }.getOrDefault("") else "")
        finish()
    }

    /** A path that opens `uri` under its own name: a link to the descriptor of the file, kept open by the app. */
    private fun link(uri: Uri): String {
        val name = contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { if (it.moveToFirst()) it.getString(0) else null }
            ?: "file"
        val file = contentResolver.openFileDescriptor(uri, "r") ?: return ""
        runCatching { held?.close() }
        held = file
        val dir = File(cacheDir, "picked").apply {
            mkdirs()
            listFiles()?.forEach { it.delete() }
        }
        val link = File(dir, name.replace('/', '_'))
        android.system.Os.symlink("/proc/self/fd/${file.fd}", link.path)
        return link.path
    }
}
