// The control of the phone from the page: the service of accessibility of the app. Android lets no app inject input into the others;
// this service it lets make gestures of touch, press Back, Home and Recents, and type into the field in focus. Its user turns it on
// once in the settings of accessibility.
//
// A finger of the page is a stroke followed as it moves: each piece of it continues the one before ( StrokeDescription
// .continueStroke ), the finger stays down between them - a drag, a long press, a scroll go as they are made, not after. A piece
// goes when the one before is done; what moved meanwhile goes in the next piece, along the way it came.

package org.unirail.RemoteDesk

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Intent
import android.graphics.Path
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.KeyEvent
import android.view.WindowManager
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo

class ControlService : AccessibilityService() {
    companion object {
        /** The service, while its user has it on. */
        @Volatile var running: ControlService? = null

        /** By `Host.action`: 1 Back, 2 Home, 3 Recents. */
        private val ACTIONS = intArrayOf(0, GLOBAL_ACTION_BACK, GLOBAL_ACTION_HOME, GLOBAL_ACTION_RECENTS)
    }

    private val main = Handler(Looper.getMainLooper())

    override fun onServiceConnected() {
        running = this
    }

    override fun onUnbind(intent: Intent?): Boolean {
        running = null
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        running = null
        super.onDestroy()
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

    override fun onInterrupt() {}

    // ── fingers: on the main thread ──

    /** A finger of the page: where it is, where its last piece ended, the way it came since. */
    private class Finger(var x: Float, var y: Float) {
        var stroke: GestureDescription.StrokeDescription? = null
        var sentX = x
        var sentY = y
        var sentAt = SystemClock.uptimeMillis()
        val way = Path().apply { moveTo(x, y) }
        var up = false
    }

    private val fingers = HashMap<Int, Finger>()
    /** A gesture goes: the next waits for it. */
    private var busy = false
    /** Something changed since the last piece. */
    private var changed = false

    /** A finger: `phase` 0 down, 1 moved, 2 up, 3 canceled; `finger` -1 - all of them up. */
    fun touch(finger: Int, phase: Int, x: Float, y: Float): Boolean {
        main.post {
            if (finger < 0) {
                for (f in fingers.values) f.up = true
            } else {
                val f = fingers[finger]
                when {
                    phase == 0 && f == null && fingers.size < GestureDescription.getMaxStrokeCount() -> fingers[finger] = Finger(x, y)
                    f == null || f.up -> return@post
                    else -> {
                        if (x != f.x || y != f.y) f.way.lineTo(x, y)
                        f.x = x
                        f.y = y
                        if (phase >= 2) f.up = true
                    }
                }
            }
            changed = true
            next()
        }
        return true
    }

    /** The next piece of every finger, when the one before is done. */
    private fun next() {
        if (busy || !changed || fingers.isEmpty()) return
        changed = false
        val now = SystemClock.uptimeMillis()
        val gesture = GestureDescription.Builder()
        for (f in fingers.values) {
            val time = (now - f.sentAt).coerceIn(1, 250)
            val before = f.stroke
            f.stroke = try {
                if (before == null) GestureDescription.StrokeDescription(f.way, 0, if (f.up) maxOf(time, 40) else time, !f.up)
                else before.continueStroke(f.way, 0, time, !f.up)
            } catch (e: RuntimeException) {
                null // a piece Android does not take: the finger is let go
            }
            f.stroke?.let { gesture.addStroke(it) }
            f.sentX = f.x
            f.sentY = f.y
            f.sentAt = now
            f.way.reset()
            f.way.moveTo(f.x, f.y)
        }
        fingers.values.removeAll { it.up || it.stroke == null }
        busy = try {
            dispatchGesture(gesture.build(), done, main)
        } catch (e: RuntimeException) {
            false
        }
        if (!busy) fingers.clear()
    }

    private val done = object : GestureResultCallback() {
        override fun onCompleted(gestureDescription: GestureDescription?) {
            busy = false
            next()
        }

        override fun onCancelled(gestureDescription: GestureDescription?) {
            // another gesture, of the system or of the user, took the screen: the fingers of the page are up
            busy = false
            fingers.clear()
        }
    }

    /** Notches of the wheel at a point: a short swipe there, up - the content goes down, as a finger moves it. */
    fun scroll(x: Float, y: Float, dx: Float, dy: Float): Boolean {
        main.post {
            if (busy || fingers.isNotEmpty()) return@post
            val (w, h) = screen()
            // away from the edges: a swipe from one pulls the bars of the system
            val sx = x.coerceIn(w * 0.1f, w * 0.9f)
            val sy = y.coerceIn(h * 0.2f, h * 0.8f)
            val step = h / 10f
            val path = Path().apply {
                moveTo(sx, sy)
                lineTo((sx - dx * step).coerceIn(1f, w - 1f), (sy + dy * step).coerceIn(1f, h - 1f))
            }
            busy = try {
                dispatchGesture(GestureDescription.Builder().addStroke(GestureDescription.StrokeDescription(path, 0, 250)).build(), done, main)
            } catch (e: RuntimeException) {
                false
            }
        }
        return true
    }

    private fun screen(): Pair<Float, Float> {
        val wm = getSystemService(WindowManager::class.java)
        if (Build.VERSION.SDK_INT >= 30) {
            val b = wm.maximumWindowMetrics.bounds
            return b.width().toFloat() to b.height().toFloat()
        }
        val m = android.util.DisplayMetrics()
        @Suppress("DEPRECATION")
        wm.defaultDisplay.getRealMetrics(m)
        return m.widthPixels.toFloat() to m.heightPixels.toFloat()
    }

    // ── the buttons of the phone ──

    fun global(which: Int): Boolean {
        val action = ACTIONS.getOrNull(which) ?: return false
        main.post { performGlobalAction(action) }
        return true
    }

    // ── keys and text: into the field in focus ──

    /** A key of Android: Ctrl with A, C, X, V - the actions of the field; the others to it as keys ( Android 13 ), or what they do to its text. */
    fun press(code: Int, meta: Int): Boolean {
        main.post {
            val node = findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
            if (meta and KeyEvent.META_CTRL_ON != 0 && node != null) {
                val action = when (code) {
                    KeyEvent.KEYCODE_C -> AccessibilityNodeInfo.ACTION_COPY
                    KeyEvent.KEYCODE_X -> AccessibilityNodeInfo.ACTION_CUT
                    KeyEvent.KEYCODE_V -> AccessibilityNodeInfo.ACTION_PASTE
                    KeyEvent.KEYCODE_A -> -1
                    else -> 0
                }
                if (action == -1) {
                    select(node, 0, text(node).length)
                    return@post
                }
                if (action != 0) {
                    node.performAction(action)
                    return@post
                }
            }
            if (Build.VERSION.SDK_INT >= 33) {
                val connection = inputMethod?.currentInputConnection
                if (connection != null) {
                    val t = SystemClock.uptimeMillis()
                    connection.sendKeyEvent(KeyEvent(t, t, KeyEvent.ACTION_DOWN, code, 0, meta))
                    connection.sendKeyEvent(KeyEvent(t, t, KeyEvent.ACTION_UP, code, 0, meta))
                    return@post
                }
            }
            node ?: return@post
            when (code) {
                KeyEvent.KEYCODE_DEL -> edit(node, "", -1)
                KeyEvent.KEYCODE_FORWARD_DEL -> edit(node, "", 1)
                KeyEvent.KEYCODE_ENTER -> if (Build.VERSION.SDK_INT >= 30) {
                    node.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)
                }
                KeyEvent.KEYCODE_SPACE -> edit(node, " ", 0)
                else -> {}
            }
        }
        return true
    }

    /** Text into the field in focus, at its cursor. */
    fun typeText(text: String): Boolean {
        main.post {
            if (Build.VERSION.SDK_INT >= 33) {
                val connection = inputMethod?.currentInputConnection
                if (connection != null) {
                    connection.commitText(text, 1, null)
                    return@post
                }
            }
            findFocus(AccessibilityNodeInfo.FOCUS_INPUT)?.let { edit(it, text, 0) }
        }
        return true
    }

    private fun text(node: AccessibilityNodeInfo): String =
        if (Build.VERSION.SDK_INT >= 26 && node.isShowingHintText) "" else node.text?.toString() ?: ""

    /** The text of `node` with `insert` at its cursor - or, `delete` -1 or 1, the character before or after it gone. */
    private fun edit(node: AccessibilityNodeInfo, insert: String, delete: Int) {
        if (!node.isEditable) return
        val now = text(node)
        var start = minOf(node.textSelectionStart, node.textSelectionEnd)
        var end = maxOf(node.textSelectionStart, node.textSelectionEnd)
        if (start < 0 || end > now.length) {
            start = now.length
            end = now.length
        }
        if (start == end && delete < 0 && start > 0) start--
        if (start == end && delete > 0 && end < now.length) end++
        val next = now.substring(0, start) + insert + now.substring(end)
        val args = Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, next) }
        node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
        select(node, start + insert.length, start + insert.length)
    }

    private fun select(node: AccessibilityNodeInfo, start: Int, end: Int) {
        val args = Bundle().apply {
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, start)
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, end)
        }
        node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, args)
    }
}
