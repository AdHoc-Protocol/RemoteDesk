// The screen of the app. Before the start: the four ways as tiles, the fields of a meeting point for its way, one button. While the
// screen is shown: how it goes, the line to send with Copy and Share, what the host waits for, Stop; what the host says, folded under
// Details. What is not allowed yet - the control, the marks - in one card, gone once all of it is allowed.

package org.unirail.RemoteDesk

import android.Manifest
import android.animation.ObjectAnimator
import android.animation.ValueAnimator
import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ComponentName
import android.content.Intent
import android.content.res.Configuration
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.EditText
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast

class MainActivity : Activity(), Host.Watcher {
    /** The ways, as a computer has them: the sign, the name, when. */
    private val ways = arrayOf(
        Triple("🏠", "Same Wi-Fi", "home or office"),
        Triple("🌍", "Internet", "a public address"),
        Triple("🤝", "Meeting point", "SYN Mirror"),
        Triple("⚡", "Direct", "nothing to set up"),
    )

    // ── the colours: of the night or of the day, as the phone is ──

    private val night get() = (resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES
    private val paper get() = if (night) 0xFF0F1115.toInt() else 0xFFF4F5F8.toInt()
    private val surface get() = if (night) 0xFF1A1D23.toInt() else 0xFFFFFFFF.toInt()
    private val edge get() = if (night) 0xFF2A2F38.toInt() else 0xFFE1E4EA.toInt()
    private val ink get() = if (night) 0xFFE9ECF1.toInt() else 0xFF15181E.toInt()
    private val muted get() = if (night) 0xFF9AA3B2.toInt() else 0xFF5B6475.toInt()
    private val accent = 0xFF3B82F6.toInt()
    private val accentSoft get() = if (night) 0xFF1B2A44.toInt() else 0xFFE8F0FE.toInt()
    private val good = 0xFF2EC25A.toInt()
    private val warn = 0xFFF5A524.toInt()
    private val bad = 0xFFEF4444.toInt()

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun shape(fill: Int, radius: Int, stroke: Int = 0, strokeWidth: Int = 1) = GradientDrawable().apply {
        setColor(fill)
        cornerRadius = dp(radius).toFloat()
        if (stroke != 0) setStroke(dp(strokeWidth), stroke)
    }

    private fun text(value: String, size: Float, color: Int, bold: Boolean = false) = TextView(this).apply {
        text = value
        textSize = size
        setTextColor(color)
        if (bold) typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
    }

    private fun column(vararg children: View) = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        children.forEach { addView(it) }
    }

    /** A card: a rounded surface with a hairline. */
    private fun card(vararg children: View) = column(*children).apply {
        background = shape(surface, 18, edge)
        setPadding(dp(16), dp(14), dp(16), dp(14))
    }

    private fun gap(height: Int) = View(this).apply { layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, dp(height)) }

    /** The button that does the thing: wide, round, of the colour of `fill`. */
    private fun primary(label: String, fill: Int = accent, act: () -> Unit) = TextView(this).apply {
        text = label
        textSize = 17f
        setTextColor(0xFFFFFFFF.toInt())
        typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
        gravity = Gravity.CENTER
        background = shape(fill, 28)
        minHeight = dp(54)
        isClickable = true
        isFocusable = true
        setOnClickListener { act() }
    }

    /** A quieter button: an outline. */
    private fun secondary(label: String, color: Int = accent, act: () -> Unit) = TextView(this).apply {
        text = label
        textSize = 15f
        setTextColor(color)
        typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
        gravity = Gravity.CENTER
        background = shape(0, 22, edge)
        minHeight = dp(44)
        setPadding(dp(18), 0, dp(18), 0)
        isClickable = true
        setOnClickListener { act() }
    }

    /** A link: a word that does something, no frame. */
    private fun link(label: String, color: Int = accent, act: () -> Unit) = text(label, 14f, color, true).apply {
        setPadding(0, dp(6), 0, dp(6))
        isClickable = true
        setOnClickListener { act() }
    }

    private fun field(hint: String, value: String, kind: Int) = EditText(this).apply {
        this.hint = hint
        setText(value)
        inputType = kind
        textSize = 15f
        setTextColor(ink)
        setHintTextColor(muted)
        background = shape(paper, 12, edge)
        setPadding(dp(14), dp(12), dp(14), dp(12))
    }

    // ── the views that change ──

    private lateinit var choose: LinearLayout
    private val tiles = ArrayList<LinearLayout>()
    private lateinit var meet: LinearLayout
    private lateinit var point: EditText
    private lateinit var password: EditText
    private lateinit var hint: TextView
    private lateinit var running: LinearLayout
    private lateinit var dot: View
    private lateinit var status: TextView
    private lateinit var lineCard: LinearLayout
    private lateinit var lineText: TextView
    private lateinit var wait: LinearLayout
    private lateinit var answerField: EditText
    private lateinit var done: TextView
    private lateinit var details: TextView
    private lateinit var log: TextView
    private lateinit var setup: LinearLayout
    private lateinit var controlRow: LinearLayout
    private lateinit var marksRow: LinearLayout
    private var way = 0

    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)
        actionBar?.hide()
        window.decorView.setBackgroundColor(paper)
        val prefs = getPreferences(MODE_PRIVATE)
        way = prefs.getInt("way", 0)

        val root = column().apply { setPadding(dp(20), dp(12), dp(20), dp(24)) }
        val scroll = ScrollView(this).apply { addView(root); isFillViewport = true; setBackgroundColor(paper) }
        // the bars of the system over the screen of an app of Android 15: the content keeps clear of them
        scroll.setOnApplyWindowInsetsListener { v, insets ->
            @Suppress("DEPRECATION")
            v.setPadding(insets.systemWindowInsetLeft, insets.systemWindowInsetTop, insets.systemWindowInsetRight, insets.systemWindowInsetBottom)
            insets
        }

        // ── the head: the mark and the name ──
        val head = LinearLayout(this).apply { gravity = Gravity.CENTER_VERTICAL; setPadding(0, dp(12), 0, dp(20)) }
        head.addView(ImageView(this).apply { setImageResource(R.drawable.launcher) }, LinearLayout.LayoutParams(dp(48), dp(48)))
        head.addView(column(text("RemoteDesk", 26f, ink, true), text("Show this phone to a computer", 14f, muted)).apply { setPadding(dp(14), 0, 0, 0) })
        root.addView(head)

        // ── before the start ──
        choose = column()
        choose.addView(text("How will they reach you?", 13f, muted, true).apply { setPadding(dp(4), 0, 0, dp(8)) })
        for (row in 0..1) {
            val line = LinearLayout(this)
            for (col in 0..1) {
                val i = row * 2 + col
                val (sign, name, whenTo) = ways[i]
                val tile = column(text(sign, 24f, ink), text(name, 16f, ink, true).apply { setPadding(0, dp(6), 0, 0) }, text(whenTo, 12f, muted)).apply {
                    setPadding(dp(14), dp(12), dp(14), dp(12))
                    isClickable = true
                    setOnClickListener { pick(i) }
                }
                tiles += tile
                line.addView(tile, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply {
                    setMargins(if (col == 0) 0 else dp(6), if (row == 0) 0 else dp(6), if (col == 0) dp(6) else 0, 0)
                })
            }
            choose.addView(line)
        }
        point = field("Tr7kQ2xZ@router.example.org", prefs.getString("point", "") ?: "", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI)
        password = field("Its password: 8 characters", prefs.getString("password", "") ?: "", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD)
        // the meeting point in one line, as the page of SYN Mirror copies it - Tr7kQ2xZ@router.example.org: the password before @
        // goes to its own field
        point.addTextChangedListener(object : android.text.TextWatcher {
            override fun afterTextChanged(s: android.text.Editable) {
                val at = s.indexOf('@')
                if (at < 0) return
                password.setText(s.substring(0, at).trim())
                s.delete(0, at + 1)
            }
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
        })
        meet = column(gap(14), text("The meeting point, as its owner gave it", 13f, muted, true).apply { setPadding(dp(4), 0, 0, dp(6)) }, point, gap(8), password)
        choose.addView(meet)
        hint = text("", 13f, warn).apply { setPadding(dp(4), dp(10), 0, 0); visibility = View.GONE }
        choose.addView(hint)
        choose.addView(gap(20))
        choose.addView(primary("▶   Show this screen") { ask() })
        root.addView(choose)

        // ── while it is shown ──
        dot = View(this).apply { background = shape(good, 6) }
        status = text("Starting…", 14f, muted)
        val statusRow = LinearLayout(this).apply { gravity = Gravity.CENTER_VERTICAL }
        statusRow.addView(dot, LinearLayout.LayoutParams(dp(12), dp(12)))
        statusRow.addView(column(text("This screen is shown", 18f, ink, true), status).apply { setPadding(dp(14), 0, 0, 0) })
        ObjectAnimator.ofFloat(dot, "alpha", 1f, 0.25f).apply {
            duration = 900
            repeatCount = ValueAnimator.INFINITE
            repeatMode = ValueAnimator.REVERSE
            start()
        }

        lineText = text("", 16f, ink).apply { typeface = Typeface.MONOSPACE; setTextIsSelectable(true); setPadding(0, dp(8), 0, dp(14)) }
        val actions = LinearLayout(this)
        actions.addView(secondary("Copy") { copy() }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { setMargins(0, 0, dp(6), 0) })
        actions.addView(primary("Share") { share() }.apply { textSize = 15f; minHeight = dp(44); background = shape(accent, 22) }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply { setMargins(dp(6), 0, 0, 0) })
        answerField = field("Paste the answer of the page: rd1o.…", "", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS)
        done = secondary("") { answer() }
        wait = column(gap(14), answerField, gap(8), done).apply { visibility = View.GONE }
        lineCard = card(text("SEND THIS LINE TO WHOEVER CONNECTS", 11f, muted, true).apply { letterSpacing = 0.08f }, lineText, actions, wait).apply { visibility = View.GONE }

        log = text("", 12f, muted).apply { typeface = Typeface.MONOSPACE; setTextIsSelectable(true); visibility = View.GONE; setPadding(dp(4), dp(4), 0, 0) }
        details = link("Details  ▾", muted) {
            log.visibility = if (log.visibility == View.VISIBLE) View.GONE else View.VISIBLE
            details.text = if (log.visibility == View.VISIBLE) "Details  ▴" else "Details  ▾"
        }

        running = column(card(statusRow), gap(12), lineCard, gap(8), details, log, gap(12),
            secondary("Stop", bad) { startService(Intent(this, HostService::class.java).setAction(HostService.STOP)) }).apply { visibility = View.GONE }
        root.addView(running)

        // ── what is not allowed yet ──
        controlRow = permission(
            "Control from the page", "Taps, keys and the buttons of the phone.\nAccessibility › at the end: Downloaded apps › RemoteDesk › on",
            "Turn on", { openControl() }, "control",
            "Switch grey? Settings › Apps › RemoteDesk › ⋮ › Allow restricted settings", "restricted",
        )
        marksRow = permission(
            "Marks over the screen", "Lines and rings the viewer draws to show where to tap",
            "Allow", { startActivity(Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:$packageName"))) }, "over",
        )
        setup = column(gap(20), text("Finish the setup, once", 13f, muted, true).apply { setPadding(dp(4), 0, 0, dp(8)) }, controlRow, gap(8), marksRow)
        root.addView(setup)

        setContentView(scroll)
        pick(way)
    }

    /** A row of what is not allowed yet: what it is for, the button that opens its settings, how on this phone. */
    private fun permission(title: String, about: String, button: String, act: () -> Unit, how: String, tip: String? = null, tipHow: String? = null): LinearLayout {
        val head = LinearLayout(this).apply { gravity = Gravity.CENTER_VERTICAL }
        head.addView(column(text(title, 16f, ink, true), text(about, 13f, muted).apply { setPadding(0, dp(2), 0, 0) }), LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
        head.addView(secondary(button) { act() }.apply { minHeight = dp(38) }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { setMargins(dp(10), 0, 0, 0) })
        val box = card(head, link("How on this phone?") { search(how) })
        if (tip != null && tipHow != null) box.addView(link(tip, muted) { search(tipHow) })
        return box
    }

    override fun onResume() {
        super.onResume()
        Host.watcher = this
        // the service may not have started the host yet when this window comes back from the consent
        val on = Host.running || Host.starting
        choose.visibility = if (on) View.GONE else View.VISIBLE
        running.visibility = if (on) View.VISIBLE else View.GONE
        log.text = ""
        Host.news.forEach { news(it) }
        Host.line?.let { line(it) }
        waits()
        allowed()
    }

    override fun onPause() {
        super.onPause()
        if (Host.watcher === this) Host.watcher = null
    }

    /** What is allowed, as the settings have it - not whether the service has come up yet: it comes a moment after the app. */
    private fun allowed() {
        val mine = ComponentName(this, ControlService::class.java)
        val enabled = Settings.Secure.getString(contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES) ?: ""
        val control = enabled.split(':').any { ComponentName.unflattenFromString(it) == mine }
        val marks = Settings.canDrawOverlays(this)
        controlRow.visibility = if (control) View.GONE else View.VISIBLE
        marksRow.visibility = if (marks) View.GONE else View.VISIBLE
        setup.visibility = if (control && marks) View.GONE else View.VISIBLE
    }

    /** The way `i`: its tile lit, the fields of a meeting point for its way only. */
    private fun pick(i: Int) {
        way = i
        tiles.forEachIndexed { n, tile -> tile.background = if (n == i) shape(accentSoft, 16, accent, 2) else shape(surface, 16, edge) }
        meet.visibility = if (i == 2) View.VISIBLE else View.GONE
        hint.visibility = View.GONE
    }

    /** Where the host waits for its user: the meeting point, until the line reached them; directly, the answer of the page. */
    private fun waits() {
        val shown = Host.running && Host.line != null && (way == 2 || way == 3)
        wait.visibility = if (shown) View.VISIBLE else View.GONE
        answerField.visibility = if (way == 3) View.VISIBLE else View.GONE
        done.text = if (way == 3) "Connect" else "The line has reached them"
    }

    /** Opens the settings of accessibility: Android scrolls to the service and lights it up where it can. */
    private fun openControl() {
        val service = ComponentName(this, ControlService::class.java).flattenToString()
        startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)
            .putExtra(":settings:fragment_args_key", service)
            .putExtra(":settings:show_fragment_args", Bundle().apply { putString(":settings:fragment_args_key", service) }))
    }

    /**
     * Searches the web for how to do `what` on this phone: its maker and its Android in the words, in the language of the phone -
     * every maker names and places its settings its own way.
     */
    private fun search(what: String) {
        val ru = resources.configuration.locales[0].language == "ru"
        val phone = "${Build.MANUFACTURER} Android ${Build.VERSION.RELEASE}"
        val words = when (what) {
            "control" -> if (ru) "как включить специальные возможности для скачанного приложения" else "how to turn on accessibility for a downloaded app"
            "restricted" -> if (ru) "как разрешить ограниченные настройки приложению" else "how to allow restricted settings for an app"
            else -> if (ru) "как разрешить приложению показ поверх других приложений" else "how to allow an app to display over other apps"
        }
        val query = "$phone $words"
        try {
            startActivity(Intent(Intent.ACTION_WEB_SEARCH).putExtra(android.app.SearchManager.QUERY, query))
        } catch (e: android.content.ActivityNotFoundException) {
            startActivity(Intent(Intent.ACTION_VIEW, Uri.parse("https://www.google.com/search?q=" + Uri.encode(query))))
        }
    }

    // ── the start: the permissions, the consent of Android to show the screen, the service ──

    private fun ask() {
        if (way == 2 && (point.text.isBlank() || password.text.length != 8)) {
            hint.text = "The meeting point and its password of 8 characters, as its owner gave them"
            hint.visibility = View.VISIBLE
            return
        }
        getPreferences(MODE_PRIVATE).edit().putInt("way", way).putString("point", point.text.toString().trim()).putString("password", password.text.toString()).apply()
        // the sound of the phone needs the microphone permission; the notification its own on Android 13
        val wanted = mutableListOf(Manifest.permission.RECORD_AUDIO)
        if (Build.VERSION.SDK_INT >= 33) wanted += Manifest.permission.POST_NOTIFICATIONS
        val missing = wanted.filter { checkSelfPermission(it) != android.content.pm.PackageManager.PERMISSION_GRANTED }
        if (missing.isNotEmpty()) requestPermissions(missing.toTypedArray(), 1) else consent()
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        consent() // without them the screen is shown all the same
    }

    private fun consent() {
        val manager = getSystemService(MediaProjectionManager::class.java)
        // the whole screen: the viewer works with the phone, not one app of it
        val intent = if (Build.VERSION.SDK_INT >= 34) manager.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay()) else manager.createScreenCaptureIntent()
        @Suppress("DEPRECATION")
        startActivityForResult(intent, 1)
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (resultCode != RESULT_OK || data == null) {
            hint.text = "Android did not let the screen be shown: press it again, and pick the entire screen"
            hint.visibility = View.VISIBLE
            return
        }
        val arguments = when (way) {
            2 -> arrayOf("--meet", point.text.toString().trim(), "--meet-password", password.text.toString())
            3 -> arrayOf("--direct")
            else -> emptyArray()
        }
        Host.starting = true
        startForegroundService(
            Intent(this, HostService::class.java)
                .putExtra(HostService.CONSENT_CODE, resultCode)
                .putExtra(HostService.CONSENT_DATA, data)
                .putExtra(HostService.WAY, way)
                .putExtra(HostService.ARGUMENTS, arguments)
        )
        choose.visibility = View.GONE
        running.visibility = View.VISIBLE
    }

    // ── while it runs ──

    override fun news(news: Host.News) {
        if (log.text.isNotEmpty()) log.append("\n")
        log.append("${news.sign} ${news.text}")
        // the status: what happened last, of the news that tell how it goes
        if (news.tone != 5) status.text = news.text
        if (news.tone == 4) dot.background = shape(bad, 6)
    }

    override fun line(line: String) {
        lineText.text = line
        lineCard.visibility = View.VISIBLE
        waits()
    }

    private fun copy() {
        val line = Host.line ?: return
        getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("RemoteDesk", line))
        Toast.makeText(this, "The line is in the clipboard", Toast.LENGTH_SHORT).show()
    }

    private fun share() {
        val line = Host.line ?: return
        startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, line), "Send the line"))
    }

    private fun answer() {
        val text = if (way == 3) answerField.text.toString().trim() else ""
        if (way == 3 && text.isEmpty()) return
        Host.answer(text.toByteArray())
        answerField.setText("")
        if (way == 2) wait.visibility = View.GONE // the meeting point lets them in from now on
    }
}
