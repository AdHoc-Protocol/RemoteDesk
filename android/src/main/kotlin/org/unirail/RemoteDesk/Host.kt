// The host of RemoteDesk in this app: the library of ../host ( librdp_host.so ), the same host a computer runs. What it asks of the
// phone it asks here - the static methods below, called by its threads - and what the phone gives it goes by the native methods.
// Text crosses as UTF-8 bytes both ways: the strings of JNI are "modified UTF-8", and an emoji would not cross as one.

package org.unirail.RemoteDesk

import android.os.Handler
import android.os.Looper
import java.nio.ByteBuffer

object Host {
    init {
        System.loadLibrary("rdp_host")
    }

    // ── to the host ──

    /** Starts the host: `way` 0 - this network, 1 - the internet, 2 - a meeting point, 3 - directly; `arguments` as a computer takes them. */
    @JvmStatic external fun start(way: Int, arguments: Array<ByteArray>, name: ByteArray, crypto: ByteArray, downloads: ByteArray)

    /** The size of the screen in pixels and its density, dots an inch: at the start, and when it turns. */
    @JvmStatic external fun screen(width: Int, height: Int, dpi: Int)

    /** What the encoder gave: a direct buffer of MediaCodec, its flags. */
    @JvmStatic external fun unit(buffer: ByteBuffer, offset: Int, size: Int, flags: Int)

    /** The service of the control is on, or off: the phone takes the input of the page, or not. */
    @JvmStatic external fun control(on: Boolean)

    /** The phone stopped showing the screen. */
    @JvmStatic external fun ended()

    /** What the phone played: a direct buffer, 48 kHz stereo, 16-bit. */
    @JvmStatic external fun sound(buffer: ByteBuffer, size: Int)

    /** Text the user of the phone sends to the computer. */
    @JvmStatic external fun clip(text: ByteArray)

    /** A picture the user of the phone sends to the computer: PNG. */
    @JvmStatic external fun clipImage(png: ByteArray)

    /** What the user typed or pressed where a computer would read its console. */
    @JvmStatic external fun answer(text: ByteArray)

    // ── what the app keeps of it ──

    /** One piece of news: the sign, the kind ( 0 good, 1 news, 2 waited for, 3 worth a look, 4 failed, 5 by the way ), the words. */
    class News(val sign: String, val tone: Int, val text: String)

    /** What the screen of the app shows: told on the main thread. */
    interface Watcher {
        fun news(news: News)
        fun line(line: String)
    }

    /** The host runs in this process. */
    @Volatile var running = false

    /** The service is asked to start the host: the screen of the app shows it running. */
    @Volatile var starting = false

    /** The news so far, the newest last: the screen of the app opened again shows them. */
    val news = ArrayList<News>()
    var line: String? = null
    var watcher: Watcher? = null

    /** The service that shows the screen: the video and the sound are its. */
    @Volatile var service: HostService? = null

    private val main = Handler(Looper.getMainLooper())

    // ── from the host: its threads call these ──

    @JvmStatic fun tell(sign: ByteArray, tone: Int, text: ByteArray) {
        val n = News(String(sign), tone, String(text))
        main.post {
            news.add(n)
            if (news.size > 300) news.subList(0, news.size - 300).clear()
            watcher?.news(n)
        }
    }

    @JvmStatic fun line(text: ByteArray) {
        val l = String(text)
        main.post {
            line = l
            service?.copy(l)
            watcher?.line(l)
        }
    }

    /** The encoder for frames of `fps` a second at `bitrate`: the size of its frames, width << 32 | height; 0 - it did not start. */
    @JvmStatic fun videoStart(fps: Int, bitrate: Int): Long = try {
        service?.videoStart(fps, bitrate) ?: 0L
    } catch (e: Throwable) {
        android.util.Log.e("RemoteDesk", "the encoder", e)
        0L
    }

    @JvmStatic fun videoStop() { runCatching { service?.videoStop() } }

    @JvmStatic fun keyFrame() { runCatching { service?.keyFrame() } }

    @JvmStatic fun bitrate(bitrate: Int) { runCatching { service?.bitrate(bitrate) } }

    @JvmStatic fun setClip(text: ByteArray): Boolean {
        val s = service ?: return false
        val t = String(text)
        main.post { runCatching { s.copy(t) } }
        return true
    }

    /** A picture from the viewer, PNG, on the clipboard of the phone. */
    @JvmStatic fun setClipImage(png: ByteArray): Boolean {
        val s = service ?: return false
        main.post { runCatching { s.copyImage(png) } }
        return true
    }

    @JvmStatic fun soundStart(): Boolean = try {
        service?.soundStart() ?: false
    } catch (e: Throwable) {
        android.util.Log.e("RemoteDesk", "the sound", e)
        false
    }

    @JvmStatic fun soundStop() { runCatching { service?.soundStop() } }

    // ── the microphone of the page on the speaker, the files of the phone ──

    @JvmStatic fun micStart(rate: Int): Boolean = try {
        service?.micStart(rate) ?: false
    } catch (e: Throwable) {
        android.util.Log.e("RemoteDesk", "the player", e)
        false
    }

    /** Samples of the microphone of the page, 16-bit stereo: waits while the buffer of the player is full. */
    @JvmStatic fun micWrite(buffer: ByteBuffer, size: Int) { runCatching { service?.micWrite(buffer, size) } }

    @JvmStatic fun micStop() { runCatching { service?.micStop() } }

    /** The picker of files on the screen of the phone: waits for its user; the path as UTF-8, empty - cancelled, null - not shown. */
    @JvmStatic fun pickFile(): ByteArray? = service?.let { s -> runCatching { PickActivity.pick(s) }.getOrNull()?.toByteArray() }

    // ── the marks of the viewers, over the other apps ──

    /** A new image of a stroke: its number; -1 - the app may not display over the other apps. */
    @JvmStatic fun markNew(): Int = try {
        service?.markNew() ?: -1
    } catch (e: Throwable) {
        -1
    }

    /** The image of stroke `id`: `pixels` RGBA premultiplied, valid during the call only - copied at once. */
    @JvmStatic fun markShow(id: Int, x: Int, y: Int, width: Int, height: Int, pixels: ByteBuffer, alpha: Float) {
        runCatching { service?.markShow(id, x, y, width, height, pixels, alpha) }
    }

    @JvmStatic fun markFade(id: Int, alpha: Float) { runCatching { service?.markFade(id, alpha) } }

    @JvmStatic fun markGone(id: Int) { runCatching { service?.markGone(id) } }

    // ── the control from the page, by the service of accessibility: false - its user has not turned it on ──

    /** 1 Back, 2 Home, 3 Recents. */
    @JvmStatic fun action(which: Int): Boolean = ControlService.running?.global(which) ?: false

    /** A key of Android with its meta state, to the field in focus. */
    @JvmStatic fun press(code: Int, meta: Int): Boolean = ControlService.running?.press(code, meta) ?: false

    @JvmStatic fun typeText(text: ByteArray): Boolean = ControlService.running?.typeText(String(text)) ?: false

    /** A finger: 0 down, 1 moved, 2 up, 3 canceled, pixels of the screen; `finger` -1 - all of them up. */
    @JvmStatic fun touch(finger: Int, phase: Int, x: Float, y: Float): Boolean = ControlService.running?.touch(finger, phase, x, y) ?: false

    /** Notches of the wheel at a point of the screen. */
    @JvmStatic fun scroll(x: Float, y: Float, dx: Float, dy: Float): Boolean = ControlService.running?.scroll(x, y, dx, dy) ?: false
}
