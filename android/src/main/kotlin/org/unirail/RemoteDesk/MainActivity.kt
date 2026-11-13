// The screen of the app: how the viewer will reach this phone - the four ways of a computer -, the start, and then what the host says,
// the line to send with its button to share it, and where the host waits for its user: the line reached them, the answer of the page.

package org.unirail.RemoteDesk

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.graphics.Typeface
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.net.Uri
import android.os.Bundle
import android.provider.Settings
import android.text.InputType
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.RadioButton
import android.widget.RadioGroup
import android.widget.ScrollView
import android.widget.TextView

class MainActivity : Activity(), Host.Watcher {
    private val ways = arrayOf(
        "🏠  From this network" to "the same home or office: Wi-Fi",
        "🌍  From the internet" to "this phone or its router has a public address",
        "🤝  Meeting point" to "SYN Mirror on a router of yours",
        "⚡  Directly" to "nothing to set up",
    )

    private lateinit var choose: LinearLayout
    private lateinit var group: RadioGroup
    private lateinit var point: EditText
    private lateinit var password: EditText
    private lateinit var running: LinearLayout
    private lateinit var lineBox: LinearLayout
    private lateinit var lineText: TextView
    private lateinit var waitBox: LinearLayout
    private lateinit var answerField: EditText
    private lateinit var done: Button
    private lateinit var log: TextView
    private lateinit var logScroll: ScrollView
    private lateinit var overApps: Button
    private lateinit var overAppsHow: Button
    private lateinit var control: LinearLayout
    private var way = 0

    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)
        actionBar?.hide() // the title is on the screen itself
        val prefs = getPreferences(MODE_PRIVATE)
        way = prefs.getInt("way", 0)
        val pad = (16 * resources.displayMetrics.density).toInt()
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(pad, pad, pad, pad) }
        // the bars of the system over the screen of an app of Android 15: the content keeps clear of them
        root.setOnApplyWindowInsetsListener { v, insets ->
            @Suppress("DEPRECATION")
            v.setPadding(pad + insets.systemWindowInsetLeft, pad + insets.systemWindowInsetTop, pad + insets.systemWindowInsetRight, pad + insets.systemWindowInsetBottom)
            insets
        }
        root.addView(TextView(this).apply { text = "RemoteDesk"; textSize = 26f; typeface = Typeface.DEFAULT_BOLD })
        root.addView(TextView(this).apply { text = "Show this phone to a computer: its browser sees the screen, right from a page."; setPadding(0, 0, 0, pad) })

        // ── before the start ──
        choose = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        choose.addView(TextView(this).apply { text = "How will the viewer reach this phone?"; typeface = Typeface.DEFAULT_BOLD })
        group = RadioGroup(this)
        ways.forEachIndexed { i, (name, whenTo) ->
            group.addView(RadioButton(this).apply { id = 100 + i; text = "$name\n$whenTo"; setPadding(0, pad / 3, 0, pad / 3) })
        }
        group.check(100 + way)
        choose.addView(group)
        point = EditText(this).apply { hint = "The meeting point, as its owner gave it: router.example.org:40000"; setText(prefs.getString("point", "")); inputType = InputType.TYPE_TEXT_VARIATION_URI }
        password = EditText(this).apply { hint = "Its password: 8 characters"; setText(prefs.getString("password", "")); inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD }
        choose.addView(point)
        choose.addView(password)
        group.setOnCheckedChangeListener { _, id -> way = id - 100; fields() }
        fields()
        choose.addView(Button(this).apply { text = "Show this screen"; setOnClickListener { ask() } })
        root.addView(choose)

        // ── after it ──
        running = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; visibility = View.GONE }
        lineBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; visibility = View.GONE }
        lineBox.addView(TextView(this).apply { text = "📨 Send this line to whoever connects. It is in the clipboard already."; typeface = Typeface.DEFAULT_BOLD })
        lineText = TextView(this).apply { textSize = 16f; typeface = Typeface.MONOSPACE; setTextIsSelectable(true); setPadding(0, pad / 2, 0, pad / 2) }
        lineBox.addView(lineText)
        lineBox.addView(Button(this).apply { text = "Share the line"; setOnClickListener { share() } })
        running.addView(lineBox)
        waitBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; visibility = View.GONE; setPadding(0, pad / 2, 0, pad / 2) }
        answerField = EditText(this).apply { hint = "Paste here the line from the page ( rd1o.… )"; inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS }
        waitBox.addView(answerField)
        done = Button(this).apply { setOnClickListener { answer() } }
        waitBox.addView(done)
        running.addView(waitBox)
        running.addView(Button(this).apply { text = "Stop"; setOnClickListener { startService(Intent(this@MainActivity, HostService::class.java).setAction(HostService.STOP)) } })
        root.addView(running)

        // the marks the viewer draws: Android asks once to let the app display over the others
        overApps = Button(this).apply {
            text = "Let the viewer draw over the screen"
            setOnClickListener { startActivity(Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:" + packageName))) }
        }
        root.addView(overApps)
        overAppsHow = how("How on this phone: display over other apps", "over")
        root.addView(overAppsHow)
        // the control from the page: Android asks once to turn the service of accessibility of the app on
        control = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        control.addView(Button(this).apply {
            text = "Let the viewer control this phone"
            setOnClickListener {
                // Android scrolls to the service and lights it up where it can ( the key of the fragment of the settings )
                val service = android.content.ComponentName(this@MainActivity, ControlService::class.java).flattenToString()
                startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)
                    .putExtra(":settings:fragment_args_key", service)
                    .putExtra(":settings:show_fragment_args", Bundle().apply { putString(":settings:fragment_args_key", service) }))
            }
        })
        control.addView(TextView(this).apply {
            text = "1. In Accessibility, scroll to the end: Downloaded apps ( on other phones: Installed apps, or Installed services ).
" +
                "2. Tap RemoteDesk: control from the page.
" +
                "3. Turn it on, and allow.
" +
                "The switch is grey: Settings, Apps, RemoteDesk, ⋮ at the top right, Allow restricted settings - then again from 1."
            textSize = 14f
        })
        control.addView(how("How on this phone: the control", "control"))
        control.addView(how("How on this phone: the switch is grey", "restricted"))
        root.addView(control)

        log = TextView(this).apply { setTextIsSelectable(true); setPadding(0, pad / 2, 0, 0) }
        logScroll = ScrollView(this).apply { addView(log) }
        root.addView(logScroll, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        setContentView(root)
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
        overApps.visibility = if (Settings.canDrawOverlays(this)) View.GONE else View.VISIBLE
        overAppsHow.visibility = overApps.visibility
        control.visibility = if (ControlService.running != null) View.GONE else View.VISIBLE
    }

    override fun onPause() {
        super.onPause()
        if (Host.watcher === this) Host.watcher = null
    }

    /** The fields of the meeting point: for its way only. */
    private fun fields() {
        val meet = if (way == 2) View.VISIBLE else View.GONE
        point.visibility = meet
        password.visibility = meet
    }

    /** Where the host waits for its user: the meeting point, until the line reached them; directly, the answer of the page. */
    private fun waits() {
        val shown = Host.running && Host.line != null && (way == 2 || way == 3)
        waitBox.visibility = if (shown) View.VISIBLE else View.GONE
        answerField.visibility = if (way == 3) View.VISIBLE else View.GONE
        done.text = if (way == 3) "Connect" else "The line has reached them"
    }

    /**
     * A button that searches the web for how to do `what` on this phone: its maker and its Android in the words, in the language of
     * the phone - every maker names and places its settings its own way.
     */
    private fun how(label: String, what: String): Button = Button(this).apply {
        text = label
        isAllCaps = false
        textSize = 13f
        setOnClickListener {
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
    }

    // ── the start: the permissions, the consent of Android to show the screen, the service ──

    private fun ask() {
        if (way == 2 && (point.text.isBlank() || password.text.length != 8)) {
            news(Host.News("💡", 3, "The meeting point and its password of 8 characters, as its owner gave them"))
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
            news(Host.News("✋", 3, "Android did not let the screen be shown"))
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
        logScroll.post { logScroll.fullScroll(View.FOCUS_DOWN) }
    }

    override fun line(line: String) {
        lineText.text = line
        lineBox.visibility = View.VISIBLE
        waits()
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
        if (way == 2) waitBox.visibility = View.GONE // the meeting point lets them in from now on
    }
}
