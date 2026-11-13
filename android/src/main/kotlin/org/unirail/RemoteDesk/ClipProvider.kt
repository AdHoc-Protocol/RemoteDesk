// The pictures the computer copied, on the clipboard of the phone: a clipboard of Android holds a picture as a content URI, and this
// gives the apps that paste it its bytes - Android lets them read it as the clipboard hands it to them.

package org.unirail.RemoteDesk

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import java.io.File

class ClipProvider : ContentProvider() {
    companion object {
        const val AUTHORITY = "org.unirail.RemoteDesk.clip"

        /** The folder of the pictures: the last one only is kept. */
        fun folder(context: android.content.Context) = File(context.cacheDir, "clip").apply { mkdirs() }
    }

    override fun onCreate(): Boolean = true

    private fun file(uri: Uri): File? {
        val name = uri.lastPathSegment ?: return null
        val f = File(folder(context!!), name)
        return if (f.parentFile == folder(context!!) && f.isFile) f else null
    }

    override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor? =
        file(uri)?.let { ParcelFileDescriptor.open(it, ParcelFileDescriptor.MODE_READ_ONLY) }

    override fun getType(uri: Uri): String = "image/png"

    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, args: Array<out String>?, order: String?): Cursor? {
        val f = file(uri) ?: return null
        return MatrixCursor(arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)).apply { addRow(arrayOf<Any>(f.name, f.length())) }
    }

    override fun insert(uri: Uri, values: ContentValues?): Uri? = null
    override fun delete(uri: Uri, selection: String?, args: Array<out String>?): Int = 0
    override fun update(uri: Uri, values: ContentValues?, selection: String?, args: Array<out String>?): Int = 0
}
