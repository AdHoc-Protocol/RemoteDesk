// The service that shows the screen of the phone: in the foreground while it does, as Android wants of MediaProjection, with its
// notification - send the clipboard to the computer, stop. One virtual display mirrors the screen for the whole time ( Android 14
// lets a MediaProjection make one ); the encoder of each video of the host is set as its surface, and the display takes its size.

package org.unirail.RemoteDesk

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ComponentCallbacks
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.content.res.Configuration
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.Process
import android.provider.Settings
import android.view.Gravity
import android.view.Surface
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.ImageView
import java.io.File
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicInteger

class HostService : Service() {
    companion object {
        const val STOP = "org.unirail.RemoteDesk.STOP"
        const val CONSENT_CODE = "code"
        const val CONSENT_DATA = "data"
        const val WAY = "way"
        const val ARGUMENTS = "arguments"
        private const val CHANNEL = "host"
        private const val NOTIFICATION = 1
    }

    private val main = Handler(Looper.getMainLooper())
    private var projection: MediaProjection? = null
    private var display: VirtualDisplay? = null
    private var width = 0
    private var height = 0
    private var dpi = 0

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == STOP) {
            stop()
            return START_NOT_STICKY
        }
        if (projection != null || intent == null) return START_NOT_STICKY
        // the foreground first: Android gives the projection to a service of its kind only
        val type = if (Build.VERSION.SDK_INT >= 29) ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION else 0
        startForeground(NOTIFICATION, notification(), type)
        val manager = getSystemService(MediaProjectionManager::class.java)
        @Suppress("DEPRECATION")
        val data: Intent? = if (Build.VERSION.SDK_INT >= 33) intent.getParcelableExtra(CONSENT_DATA, Intent::class.java) else intent.getParcelableExtra(CONSENT_DATA)
        val p = data?.let { manager.getMediaProjection(intent.getIntExtra(CONSENT_CODE, 0), it) }
        if (p == null) {
            Host.starting = false
            stop()
            return START_NOT_STICKY
        }
        projection = p
        p.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                Host.ended()
                // Android stops it when the phone locks, or when its user stops it: the notification says it ended
                main.post { stop(ended = true) }
            }
        }, main)
        measure()
        display = p.createVirtualDisplay("RemoteDesk", width, height, dpi, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, null, null, null)
        Host.service = this
        Host.screen(width, height, dpi)
        registerComponentCallbacks(turns)
        awake()
        val arguments = (intent.getStringArrayExtra(ARGUMENTS) ?: emptyArray()) + arrayOf("--key", File(filesDir, "host.key").path)
        val downloads = getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS) ?: File(filesDir, "Download")
        Host.running = true
        Host.start(
            intent.getIntExtra(WAY, 0),
            arguments.map { it.toByteArray() }.toTypedArray(),
            Build.MODEL.toByteArray(),
            File(applicationInfo.nativeLibraryDir, "libadhoc-crypto-0.1.so").path.toByteArray(),
            downloads.path.toByteArray(),
        )
        return START_NOT_STICKY
    }

    /** The size of the screen in pixels, as it stands now. */
    private fun measure() {
        val wm = getSystemService(WindowManager::class.java)
        if (Build.VERSION.SDK_INT >= 30) {
            val b = wm.maximumWindowMetrics.bounds
            width = b.width()
            height = b.height()
        } else {
            val m = android.util.DisplayMetrics()
            @Suppress("DEPRECATION")
            wm.defaultDisplay.getRealMetrics(m)
            width = m.widthPixels
            height = m.heightPixels
        }
        dpi = resources.displayMetrics.densityDpi
    }

    /** The screen turned: the host makes its video again for the new size. */
    private val turns = object : ComponentCallbacks {
        override fun onConfigurationChanged(newConfig: Configuration) {
            val (w, h) = width to height
            measure()
            if (w != width || h != height) Host.screen(width, height, dpi)
        }

        @Deprecated("Deprecated in Java")
        override fun onLowMemory() {}
    }

    private fun stop(ended: Boolean = false) {
        Host.service = null
        runCatching { asleep() }
        runCatching { overlayGone() }
        runCatching { soundStop() }
        runCatching { videoStop() }
        runCatching { display?.release() }
        runCatching { projection?.stop() }
        stopForeground(STOP_FOREGROUND_REMOVE)
        if (ended && Host.running) runCatching { told() }
        stopSelf()
        // the host has no end of its own: its process ends with the app
        if (Host.running) Process.killProcess(Process.myPid())
    }

    // ── the screen stays on while it is shown: Android stops showing it when the phone locks ──

    /** A window of nothing that keeps the screen on: over the other apps, or of the service of the control. */
    private var keeper: android.view.View? = null
    private var keeperBy: WindowManager? = null
    private var wake: android.os.PowerManager.WakeLock? = null

    private fun awake() {
        val type = when {
            Settings.canDrawOverlays(this) -> WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY
            ControlService.running != null -> WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY
            else -> 0
        }
        if (type != 0) {
            val context: android.content.Context = if (type == WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY) ControlService.running!! else this
            val wm = context.getSystemService(WindowManager::class.java)
            val v = android.view.View(context)
            val p = WindowManager.LayoutParams(1, 1, type,
                WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON,
                PixelFormat.TRANSLUCENT)
            p.gravity = Gravity.TOP or Gravity.START
            if (runCatching { wm.addView(v, p) }.isSuccess) {
                keeper = v
                keeperBy = wm
                return
            }
        }
        // neither: the screen dims, but stays on
        @Suppress("DEPRECATION")
        wake = getSystemService(android.os.PowerManager::class.java).newWakeLock(android.os.PowerManager.SCREEN_DIM_WAKE_LOCK, "RemoteDesk:shown").apply { acquire() }
    }

    private fun asleep() {
        keeper?.let { v -> runCatching { keeperBy?.removeView(v) } }
        keeper = null
        keeperBy = null
        wake?.let { if (it.isHeld) it.release() }
        wake = null
    }

    /** The notification that the screen is no longer shown. */
    private fun told() {
        val open = PendingIntent.getActivity(this, 3, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
        getSystemService(NotificationManager::class.java).notify(2, Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.icon)
            .setContentTitle("RemoteDesk no longer shows this screen")
            .setContentText("The phone locked, or showing was stopped: open RemoteDesk to show it again")
            .setContentIntent(open)
            .setAutoCancel(true)
            .build())
    }

    // ── the notification ──

    private fun notification(): Notification {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "Showing the screen", NotificationManager.IMPORTANCE_LOW))
        val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java), flags)
        val send = PendingIntent.getActivity(this, 1, Intent(this, SendActivity::class.java).setAction(SendActivity.CLIPBOARD).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK), flags)
        val stop = PendingIntent.getService(this, 2, Intent(this, HostService::class.java).setAction(STOP), flags)
        return Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.icon)
            .setContentTitle("RemoteDesk shows this screen")
            .setContentText("To whoever has the line")
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, "Send the clipboard", send).build())
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
            .build()
    }

    /** `text` on the clipboard of the phone. */
    fun copy(text: String) {
        getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("RemoteDesk", text))
    }

    // ── the video: the encoder of the phone behind the virtual display ──

    private var codec: MediaCodec? = null
    private var surface: Surface? = null
    private var output: Thread? = null
    @Volatile private var encoding = false

    /** A new encoder for frames of `fps` a second at `bitrate`, its surface on the display: width << 32 | height of its frames. */
    @Synchronized
    fun videoStart(fps: Int, bitrate: Int): Long {
        videoStop()
        val c = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        val caps = c.codecInfo.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
        val video = caps.videoCapabilities
        // the largest size the encoder takes, of the shape of the screen
        var scale = 1.0
        var w: Int
        var h: Int
        while (true) {
            w = (width * scale).toInt() / video.widthAlignment * video.widthAlignment
            h = (height * scale).toInt() / video.heightAlignment * video.heightAlignment
            if (video.isSizeSupported(w, h) || scale < 0.2) break
            scale *= 0.9
        }
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, h).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 10)
            // a screen that stands still gives a frame now and then: a key frame asked for comes
            setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 250_000)
            setFloat(MediaFormat.KEY_MAX_FPS_TO_ENCODER, fps.toFloat())
            setInteger(MediaFormat.KEY_PREPEND_HEADER_TO_SYNC_FRAMES, 1)
            setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LATENCY, 1)
            if (caps.encoderCapabilities.isBitrateModeSupported(MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)) {
                setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)
            }
        }
        c.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val s = c.createInputSurface()
        c.start()
        codec = c
        surface = s
        display?.resize(w, h, dpi)
        display?.surface = s
        encoding = true
        output = Thread({
            val info = MediaCodec.BufferInfo()
            while (encoding) {
                val i = try {
                    c.dequeueOutputBuffer(info, 100_000)
                } catch (e: IllegalStateException) {
                    break
                }
                if (i < 0) continue
                val buffer = c.getOutputBuffer(i)
                if (buffer != null && info.size > 0) Host.unit(buffer, info.offset, info.size, info.flags)
                c.releaseOutputBuffer(i, false)
            }
        }, "encoder").apply { start() }
        return (w.toLong() shl 32) or h.toLong()
    }

    @Synchronized
    fun videoStop() {
        val c = codec ?: return
        encoding = false
        display?.surface = null
        output?.join()
        output = null
        runCatching { c.stop() }
        c.release()
        surface?.release()
        codec = null
        surface = null
    }

    fun keyFrame() {
        codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
    }

    fun bitrate(bitrate: Int) {
        codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, bitrate) })
    }

    // ── the sound: what the apps of the phone play, as they let it be captured ──

    private var record: AudioRecord? = null
    private var recording: Thread? = null
    @Volatile private var hearing = false

    @Synchronized
    fun soundStart(): Boolean {
        soundStop()
        val p = projection ?: return false
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) return false
        val config = AudioPlaybackCaptureConfiguration.Builder(p)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
            .build()
        val format = AudioFormat.Builder().setEncoding(AudioFormat.ENCODING_PCM_16BIT).setSampleRate(48_000).setChannelMask(AudioFormat.CHANNEL_IN_STEREO).build()
        val piece = 48_000 * 2 * 2 / 50 // 20 ms
        val r = AudioRecord.Builder()
            .setAudioFormat(format)
            .setBufferSizeInBytes(maxOf(piece * 4, AudioRecord.getMinBufferSize(48_000, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT)))
            .setAudioPlaybackCaptureConfig(config)
            .build()
        r.startRecording()
        record = r
        hearing = true
        recording = Thread({
            val buffer = ByteBuffer.allocateDirect(piece)
            while (hearing) {
                val n = r.read(buffer, piece, AudioRecord.READ_BLOCKING)
                if (n < 0) break
                if (n > 0) Host.sound(buffer, n)
                buffer.clear()
            }
        }, "sound").apply { start() }
        return true
    }

    // ── the marks of the viewers: their images in a window over the other apps, the touches going through it ──

    private var overlay: FrameLayout? = null
    /** On the main thread: the image of each stroke, by number. */
    private val marks = HashMap<Int, ImageView>()
    /** On the thread of the marks of the host: two bitmaps of each stroke in turn - one is shown while the other is filled. */
    private val filling = HashMap<Int, Array<Bitmap?>>()
    private val turnOf = HashMap<Int, Int>()
    private val nextMark = AtomicInteger(1)

    fun markNew(): Int = if (Settings.canDrawOverlays(this)) nextMark.getAndIncrement() else -1

    fun markShow(id: Int, x: Int, y: Int, width: Int, height: Int, pixels: ByteBuffer, alpha: Float) {
        if (width <= 0 || height <= 0) return
        val pair = filling.getOrPut(id) { arrayOfNulls(2) }
        val turn = turnOf[id] ?: 0
        turnOf[id] = turn xor 1
        var bitmap = pair[turn]
        if (bitmap == null || bitmap.width != width || bitmap.height != height) {
            bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
            pair[turn] = bitmap
        }
        pixels.rewind()
        bitmap.copyPixelsFromBuffer(pixels) // the memory of the host is its own again after this call
        val shown: Bitmap = bitmap
        main.post {
            val o = overlay() ?: return@post
            val v = marks.getOrPut(id) { ImageView(this).also { o.addView(it, FrameLayout.LayoutParams(width, height)) } }
            if (v.layoutParams.width != width || v.layoutParams.height != height) v.layoutParams = FrameLayout.LayoutParams(width, height)
            v.setImageBitmap(shown)
            v.translationX = x.toFloat()
            v.translationY = y.toFloat()
            v.alpha = alpha
        }
    }

    fun markFade(id: Int, alpha: Float) {
        main.post { marks[id]?.alpha = alpha }
    }

    fun markGone(id: Int) {
        filling.remove(id)
        turnOf.remove(id)
        main.post {
            marks.remove(id)?.let { overlay?.removeView(it) }
            if (marks.isEmpty()) overlayGone()
        }
    }

    /** The window over the other apps, made the first time: all the screen, the touches going through. */
    private fun overlay(): FrameLayout? {
        overlay?.let { return it }
        if (!Settings.canDrawOverlays(this)) return null
        val o = FrameLayout(this)
        val p = WindowManager.LayoutParams(
            WindowManager.LayoutParams.MATCH_PARENT,
            WindowManager.LayoutParams.MATCH_PARENT,
            WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or
                WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN or WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
            PixelFormat.TRANSLUCENT,
        )
        p.gravity = Gravity.TOP or Gravity.START
        // Android 12 lets the touches through a window of another app only when it is this clear at least
        p.alpha = 0.8f
        p.layoutInDisplayCutoutMode =
            if (Build.VERSION.SDK_INT >= 30) WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS else WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
        if (Build.VERSION.SDK_INT >= 30) p.fitInsetsTypes = 0
        return try {
            getSystemService(WindowManager::class.java).addView(o, p)
            overlay = o
            o
        } catch (e: RuntimeException) {
            null
        }
    }

    private fun overlayGone() {
        val o = overlay ?: return
        overlay = null
        marks.clear()
        runCatching { getSystemService(WindowManager::class.java).removeView(o) }
    }

    @Synchronized
    fun soundStop() {
        val r = record ?: return
        hearing = false
        runCatching { r.stop() }
        recording?.join()
        recording = null
        r.release()
        record = null
    }
}
