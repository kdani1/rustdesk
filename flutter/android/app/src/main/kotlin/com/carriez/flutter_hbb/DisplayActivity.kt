package com.carriez.flutter_hbb

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.util.Log
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.WindowManager
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

/**
 * One extra Quest panel that shows one remote display of the main panel's connection.
 *
 * It runs its own Flutter engine in the same process (same librustdesk.so, same Rust session
 * table). Its Dart side attaches a second UI session to the connection MainActivity opened; it
 * never connects, logs in or closes the connection itself. See `runDisplayPanel` in main.dart
 * and `AndroidDisplayPanels` in common.dart.
 */
class DisplayActivity : FlutterActivity() {
    companion object {
        private const val TAG = "DisplayPanel"
        const val EXTRA_DISPLAY = "panel_display"
        const val EXTRA_SID = "panel_sid"

        // All main thread only.
        // Panels asked for and not closed since: UI session id -> Dart arguments (JSON). The
        // arguments stay here instead of the Intent: the cached peer data can be large.
        private val wanted = HashMap<String, String>()
        private val live = HashMap<String, DisplayActivity>()

        fun open(from: Activity, display: Int, sid: String, args: String): Boolean {
            if (wanted.containsKey(sid)) return false
            wanted[sid] = args
            return try {
                from.startActivity(Intent(from, DisplayActivity::class.java).apply {
                    putExtra(EXTRA_DISPLAY, display)
                    putExtra(EXTRA_SID, sid)
                    addFlags(
                        Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_MULTIPLE_TASK or
                                Intent.FLAG_ACTIVITY_LAUNCH_ADJACENT
                    )
                })
                Log.d(TAG, "open display=$display sid=$sid")
                true
            } catch (e: Exception) {
                Log.e(TAG, "open display=$display failed: $e")
                wanted.remove(sid)
                false
            }
        }

        fun close(sid: String) {
            wanted.remove(sid)
            live[sid]?.finishAndRemoveTask()
        }

        fun closeAll() {
            wanted.clear()
            live.values.toList().forEach { it.finishAndRemoveTask() }
        }

        // The window the laser left last, on which side, and when.
        private var exitId = Int.MIN_VALUE
        private var exitSide = 0
        private var exitAt = 0L

        /**
         * Hover events of every window of the app ([id]: the display a panel shows, -1 for the
         * main window). The laser leaving one window on its right edge and entering another
         * on its left edge right after means the second stands to the right of the first:
         * the only way an app can learn how its windows are placed.
         */
        fun onHover(id: Int, ev: MotionEvent, width: Int) {
            if (width <= 0 || live.isEmpty()) return
            val f = ev.x / width
            when (ev.actionMasked) {
                MotionEvent.ACTION_HOVER_EXIT -> {
                    exitId = id
                    exitAt = ev.eventTime
                    exitSide = if (f >= 0.8f) 1 else if (f <= 0.2f) -1 else 0
                }
                MotionEvent.ACTION_HOVER_ENTER -> {
                    if (exitSide != 0 && exitId != id && exitId != Int.MIN_VALUE &&
                        ev.eventTime - exitAt in 0L..700L
                    ) {
                        if (exitSide == 1 && f <= 0.3f) neighbor(exitId, id)
                        else if (exitSide == -1 && f >= 0.7f) neighbor(id, exitId)
                    }
                    exitSide = 0
                }
            }
        }

        private var poseLoggedAt = 0L
        private var apiLogged = false
        private var poseFailLogged = false

        /**
         * Where this window of the app stands in space, as Horizon OS reports it (the same
         * calls the system's remote desktop app uses; they answer an ordinary app too):
         * position x,y,z in metres, orientation x,y,z,w, then the window's width and height
         * in metres. Null when it cannot be read or the headset is not worn (all zero).
         */
        fun readPose(activity: Activity): DoubleArray? {
            try {
                val ext = Class.forName("horizonos.app.ActivityExt")
                val token = ext.methods.first { it.name == "getVolumetricWindowToken" }
                    .invoke(null, activity) ?: return null
                val mgrClass =
                    Class.forName("horizonos.app.volumetricwindow.VolumetricWindowManager")
                val mgr = activity.getSystemService(mgrClass) ?: return null
                val info = mgrClass.methods
                    .first {
                        it.name == "getWindowInfo" && it.parameterTypes.size == 1 &&
                                it.parameterTypes[0].isInstance(token)
                    }
                    .invoke(mgr, token) ?: return null
                val pose = call(info, "getPose") ?: return null
                val pos = call(pose, "getPosition")
                val ori = call(pose, "getOrientation")
                val b = call(info, "getBounds")
                val v = doubleArrayOf(
                    num(pos, "getX"), num(pos, "getY"), num(pos, "getZ"),
                    num(ori, "getX"), num(ori, "getY"), num(ori, "getZ"), num(ori, "getW"),
                    num(b, "getWidth"), num(b, "getHeight")
                )
                return if (v[0] == 0.0 && v[1] == 0.0 && v[2] == 0.0) null else v
            } catch (e: Throwable) {
                if (!poseFailLogged) {
                    poseFailLogged = true
                    Log.d(TAG, "pose read failed: $e")
                }
                return null
            }
        }

        private fun call(o: Any?, m: String): Any? = o?.javaClass?.getMethod(m)?.invoke(o)
        private fun num(o: Any?, m: String): Double = (call(o, m) as? Number)?.toDouble() ?: 0.0

        /**
         * Test: can an ordinary app read where its own window stands in space, with the call
         * the system's remote desktop app uses? Logs what comes back (at most every 2 s).
         */
        fun logWindowPose(activity: Activity, id: Int) {
            val now = android.os.SystemClock.uptimeMillis()
            if (now - poseLoggedAt < 2000) return
            poseLoggedAt = now
            val text = try {
                val ext = Class.forName("horizonos.app.ActivityExt")
                val tokenMethod = ext.methods.first { it.name == "getVolumetricWindowToken" }
                val token = tokenMethod.invoke(null, activity)
                val mgrClass =
                    Class.forName("horizonos.app.volumetricwindow.VolumetricWindowManager")
                val mgr = activity.getSystemService(mgrClass)
                if (mgr == null) {
                    "no manager, token=$token"
                } else {
                    if (!apiLogged) {
                        apiLogged = true
                        Log.d(TAG, "pose api: " + mgrClass.methods
                            .filter { it.declaringClass == mgrClass }
                            .joinToString("; ") { m ->
                                m.name + "(" + m.parameterTypes.joinToString(",") { it.simpleName } + "):" + m.returnType.simpleName
                            })
                    }
                    val info = mgrClass.methods
                        .first {
                            it.name == "getWindowInfo" && it.parameterTypes.size == 1 &&
                                    it.parameterTypes[0].isInstance(token)
                        }
                        .invoke(mgr, token)
                    "info=" + describe(info, 0)
                }
            } catch (e: Throwable) {
                "failed: " + ((e as? java.lang.reflect.InvocationTargetException)?.targetException ?: e)
            }
            Log.d(TAG, "pose[$id] $text")
        }

        private fun describe(o: Any?, depth: Int): String {
            if (o == null) return "null"
            if (depth > 2 || o is Number || o is CharSequence || o is Boolean) return o.toString()
            val parts = o.javaClass.methods
                .filter {
                    it.parameterTypes.isEmpty() && it.name != "getClass" &&
                            (it.name.startsWith("get") || it.name.startsWith("is"))
                }
                .sortedBy { it.name }.take(12)
                .map { m ->
                    m.name + "=" + try { describe(m.invoke(o), depth + 1) } catch (e: Throwable) { "!" }
                }
            return o.javaClass.simpleName + "{" + parts.joinToString(",") + "}"
        }

        private fun neighbor(left: Int, right: Int) {
            Log.d(TAG, "neighbor left=$left right=$right")
            MainActivity.flutterMethodChannel?.invokeMethod(
                "panel_neighbor", mapOf("left" to left, "right" to right)
            )
        }
    }

    private var channel: MethodChannel? = null

    // Hardware decoding into Flutter textures of this panel's engine (see VideoTextures).
    private var videoTextures: VideoTextures? = null
    private val display get() = intent.getIntExtra(EXTRA_DISPLAY, -1)
    private val poseWatcher = PoseWatcher(this) { display }

    override fun onStart() {
        super.onStart()
        // The stop before this start took the textures' Surfaces (see VideoTextures.reattach).
        videoTextures?.reattach()
        poseWatcher.start()
    }

    private val sid get() = intent.getStringExtra(EXTRA_SID) ?: ""

    // Read before the engine starts: decides whether there is anything to show.
    private var args: String? = null

    // Why the panel closed itself ("user", "gone": its display left the host's list, "conn":
    // the session ended), for the main panel. A close by the system's own button is "user".
    private var finishReason = "user"

    override fun getDartEntrypointArgs(): List<String> =
        listOf("display_panel", args ?: "{}")

    override fun shouldRestoreAndSaveState() = false

    override fun onCreate(savedInstanceState: Bundle?) {
        args = wanted[sid]
        super.onCreate(savedInstanceState)
        Log.d(TAG, "onCreate display=$display sid=$sid wanted=${args != null}")
        // Closed again before it started, or restored by the system without its connection
        // (the arguments live in this process only).
        if (args == null || MainActivity.flutterMethodChannel == null) {
            finishAndRemoveTask()
            return
        }
        live[sid] = this
    }

    // No super: it would register every plugin of the app a second time, and several of them
    // keep process-wide state. Only what the remote page uses.
    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        flutterEngine.plugins.add(io.flutter.plugins.pathprovider.PathProviderPlugin())
        flutterEngine.plugins.add(dev.fluttercommunity.plus.device_info.DeviceInfoPlusPlugin())
        flutterEngine.plugins.add(dev.fluttercommunity.plus.packageinfo.PackageInfoPlugin())
        flutterEngine.plugins.add(dev.fluttercommunity.plus.wakelock.WakelockPlusPlugin())
        flutterEngine.plugins.add(com.jrai.flutter_keyboard_visibility.FlutterKeyboardVisibilityPlugin())
        videoTextures = VideoTextures(flutterEngine, "panel$display")
        channel = MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "mChannel").also {
            it.setMethodCallHandler { call, result ->
                when (call.method) {
                    "enable_soft_keyboard" -> {
                        if (call.arguments as? Boolean == true) {
                            window.clearFlags(WindowManager.LayoutParams.FLAG_ALT_FOCUSABLE_IM)
                        } else {
                            window.addFlags(WindowManager.LayoutParams.FLAG_ALT_FOCUSABLE_IM)
                        }
                        result.success(true)
                    }
                    "try_sync_clipboard" -> {
                        MainActivity.rdClipboardManager?.syncClipboard(true)
                        result.success(true)
                    }
                    "quest_get_info" -> result.success(QuestInput.info())
                    "quest_input_config" -> {
                        // QuestInput.active is process-wide: a panel may switch it on, never
                        // off (the main panel and the other panels still use it).
                        val a = call.arguments as? Map<*, *>
                        if (a?.get("active") == false) result.success(QuestInput.isQuest)
                        else result.success(QuestInput.configure(call.arguments))
                    }
                    "panel_finish" -> {
                        ((call.arguments as? Map<*, *>)?.get("reason") as? String)?.let {
                            finishReason = it
                        }
                        result.success(true)
                        wanted.remove(sid)
                        finishAndRemoveTask()
                    }
                    "video_texture_create" -> {
                        val a = call.arguments as? Map<*, *>
                        val s = a?.get("sid") as? String
                        val d = a?.get("display") as? Int
                        val w = a?.get("w") as? Int
                        val h = a?.get("h") as? Int
                        result.success(
                            if (s == null || d == null || w == null || h == null) -1L
                            else videoTextures?.create(s, d, w, h) ?: -1L
                        )
                    }
                    "video_texture_dispose" -> {
                        (call.arguments as? Int)?.let { videoTextures?.dispose(it) }
                        result.success(true)
                    }
                    else -> result.success(false)
                }
            }
        }
    }

    // At background level the engine closes the buffers behind its textures: the decoders
    // must have let go of them first.
    override fun onTrimMemory(level: Int) {
        if (level >= android.content.ComponentCallbacks2.TRIM_MEMORY_BACKGROUND) {
            videoTextures?.beforeTrim()
        }
        super.onTrimMemory(level)
    }

    override fun cleanUpFlutterEngine(flutterEngine: FlutterEngine) {
        videoTextures?.disposeAll()
        videoTextures = null
        super.cleanUpFlutterEngine(flutterEngine)
    }

    override fun onResume() {
        super.onResume()
        Log.d(TAG, "onResume display=$display")
    }

    override fun onPause() {
        Log.d(TAG, "onPause display=$display")
        super.onPause()
    }

    override fun onStop() {
        poseWatcher.stop()
        Log.d(TAG, "onStop display=$display")
        super.onStop()
    }

    override fun onDestroy() {
        Log.d(TAG, "onDestroy display=$display finishing=$isFinishing")
        if (live[sid] === this) live.remove(sid)
        if (isFinishing) {
            wanted.remove(sid)
            // The main panel detaches this panel's UI session if the panel itself did not get
            // to it (closed with the system's own close button).
            MainActivity.flutterMethodChannel?.invokeMethod(
                "panel_closed",
                mapOf("display" to display, "sid" to sid, "reason" to finishReason)
            )
        }
        super.onDestroy()
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (QuestInput.onKey(channel, event)) return true
        return super.dispatchKeyEvent(event)
    }

    override fun dispatchGenericMotionEvent(ev: MotionEvent): Boolean {
        onHover(display, ev, window.decorView.width)
        if (QuestInput.onGenericMotion(channel, ev)) return true
        return super.dispatchGenericMotionEvent(ev)
    }

    override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
        QuestInput.onTouch(channel, ev)
        return super.dispatchTouchEvent(ev)
    }
}

/**
 * Tells the main engine where a window of the app stands in space: twice a second while the
 * window is shown, when it moved, and every few seconds anyway. [id]: the display a panel
 * shows, -1 for the main window. See AndroidDisplayPanels.onPose in common.dart.
 */
class PoseWatcher(private val activity: Activity, private val id: () -> Int) {
    private val handler = android.os.Handler(android.os.Looper.getMainLooper())
    private var last: DoubleArray? = null
    private var lastSentAt = 0L
    private var running = false
    private val tick = object : Runnable {
        override fun run() {
            if (!running) return
            poll()
            handler.postDelayed(this, 500)
        }
    }

    fun start() {
        if (running) return
        running = true
        last = null
        handler.post(tick)
    }

    fun stop() {
        running = false
        handler.removeCallbacks(tick)
    }

    private fun poll() {
        val v = DisplayActivity.readPose(activity) ?: return
        val l = last
        val now = android.os.SystemClock.uptimeMillis()
        val same = l != null && v.indices.all { Math.abs(v[it] - l[it]) < 0.002 }
        if (same && now - lastSentAt < 5000) return
        last = v
        lastSentAt = now
        MainActivity.flutterMethodChannel?.invokeMethod(
            "panel_pose", mapOf("id" to id(), "v" to v.toList())
        )
    }
}
