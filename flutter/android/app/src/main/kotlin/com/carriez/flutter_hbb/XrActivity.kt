package com.carriez.flutter_hbb

import android.app.Activity
import android.app.ActivityOptions
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.util.Log
import android.view.KeyEvent
import android.view.WindowManager
import androidx.annotation.Keep
import ffi.FFI
import io.flutter.plugin.common.MethodChannel
import java.lang.ref.WeakReference

/**
 * Quest 3 immersive "game mode", phase 1 (lifecycle spike, see C:\RustDeskBuild\gamemode-plan.md).
 *
 * Process-wide state. The Rust session (and the Dart remote page) must outlive the switch to
 * the immersive view: while [active], MainActivity.onDestroy neither closes the sessions nor
 * the display panels (critique #1). If the OS finishes MainActivity anyway, its teardown is
 * deferred to the end of game mode ([teardownDeferred]) and logged loudly: that is the T1 "stop
 * and redesign" case (cached FlutterEngine).
 *
 * All main thread.
 */
object GameMode {
    const val TAG = "RustDeskXr"

    /**
     * Leaving the immersive view: true = Meta's documented path (Home intent carrying a
     * PendingIntent for MainActivity), with a REORDER_TO_FRONT fallback if the panel is not
     * back within [RETURN_TIMEOUT_MS]. false = the fallback only. Test T2 records which works.
     */
    const val EXIT_VIA_HOME = true
    private const val RETURN_TIMEOUT_MS = 2500L

    /** From game_mode_enter until XrActivity is gone. */
    @Volatile
    var active = false
        private set

    /** MainActivity finished while [active]: close the sessions when game mode ends. */
    var teardownDeferred = false

    /** MainActivity is between onResume and onPause (set by MainActivity). */
    var mainResumed = false

    /** The XrActivity of the current game mode (set in its onCreate). */
    private var xr: WeakReference<XrActivity>? = null

    /** That XrActivity is between onResume and onPause; it was resumed at least once. */
    private var xrResumed = false
    private var xrResumedOnce = false

    /** Game mode ended; Dart hears it ("game_mode_exited") at MainActivity's next onResume. */
    private var exitPending = false
    private var exitReason = ""
    private var exitAt = 0L
    private var exitPath = ""
    private val handler = Handler(Looper.getMainLooper())
    private var watchdog: Runnable? = null

    /** game_mode_enter from Dart: starts XrActivity in its own task. MainActivity stays. */
    fun enter(from: Activity, sid: String, display: Int, w: Int, h: Int, pad: Int): Boolean {
        if (active) {
            Log.w(TAG, "enter: game mode is already on")
            return false
        }
        active = true
        teardownDeferred = false
        exitPending = false
        exitPath = ""
        xrResumed = false
        xrResumedOnce = false
        return try {
            from.startActivity(Intent(from, XrActivity::class.java).apply {
                putExtra(XrActivity.EXTRA_SID, sid)
                putExtra(XrActivity.EXTRA_DISPLAY, display)
                putExtra(XrActivity.EXTRA_W, w)
                putExtra(XrActivity.EXTRA_H, h)
                putExtra(XrActivity.EXTRA_PAD, pad)
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            })
            Log.i(TAG, "enter sid=$sid display=$display ${w}x$h pad=$pad")
            true
        } catch (e: Exception) {
            Log.e(TAG, "enter: XrActivity did not start: $e")
            active = false
            false
        }
    }

    /** XrActivity.onCreate of this game mode. */
    fun attach(a: XrActivity) {
        xr = WeakReference(a)
    }

    /** [a] is the XrActivity of the current (or last) game mode, not an older one. */
    fun isCurrent(a: XrActivity): Boolean = xr?.get() === a

    fun xrLifecycle(a: XrActivity, resumed: Boolean) {
        if (!isCurrent(a)) return
        xrResumed = resumed
        if (resumed) xrResumedOnce = true
    }

    /**
     * Game mode is over ([reason]: why). Called by XrActivity.leave BEFORE it brings the panel
     * back (MainActivity.onResume runs before XrActivity.onDestroy), by onMainResumed when the
     * user left through the system, and by XrActivity.onDestroy as the fallback. Idempotent.
     */
    fun end(reason: String) {
        if (!active) return
        active = false
        exitReason = reason
        exitAt = SystemClock.uptimeMillis()
        exitPending = true
        Log.i(TAG, "game mode over: $reason")
        if (teardownDeferred) {
            teardownDeferred = false
            Log.e(TAG, "MainActivity was finished during game mode: closing its sessions now")
            DisplayActivity.closeAll()
            FFI.closeAllSessions()
        }
    }

    /** XrActivity asked the system to bring the panel back by [path]. */
    fun returning(app: Context, path: String, fallback: Intent?) {
        exitPath = path
        watchdog?.let { handler.removeCallbacks(it) }
        watchdog = null
        if (fallback == null) return
        val w = Runnable {
            watchdog = null
            if (!exitPending) return@Runnable
            // From the background, Android 10+ blocks this start silently unless the app may
            // draw overlays ("Background activity launch blocked" from ActivityTaskManager).
            // A blocked start here says nothing about REORDER_TO_FRONT itself: test that path
            // with EXIT_VIA_HOME = false.
            val overlay = try {
                Settings.canDrawOverlays(app)
            } catch (e: Exception) {
                false
            }
            Log.w(
                TAG, "return via $path: no panel after $RETURN_TIMEOUT_MS ms, REORDER_TO_FRONT" +
                        " from the background (canDrawOverlays=$overlay)"
            )
            exitPath = "$path, then reorder-to-front"
            try {
                app.startActivity(fallback)
            } catch (e: Exception) {
                Log.e(TAG, "return fallback failed: $e")
            }
        }
        watchdog = w
        handler.postDelayed(w, RETURN_TIMEOUT_MS)
    }

    /**
     * MainActivity.onResume (after it set [mainResumed]): after game mode, the panel is back;
     * tells Dart once. While game mode is still on, a resumed panel means the user left the
     * immersive view through the system (Meta button, Home, the panel from the library): end
     * game mode here, or it would stay on until the process dies.
     */
    fun onMainResumed(channel: MethodChannel?) {
        if (active) {
            val a = xr?.get()
            if (a != null && xrResumedOnce && !xrResumed) {
                Log.w(TAG, "panel resumed during game mode: the immersive view was left through the system")
                a.leaveFromSystem("left via system (panel reopened)")
                end("left via system (panel reopened)")
                exitPath = "system: panel reopened"
            }
        }
        if (!exitPending) return
        exitPending = false
        watchdog?.let { handler.removeCallbacks(it) }
        watchdog = null
        Log.i(
            TAG, "panel back ${SystemClock.uptimeMillis() - exitAt} ms after the exit" +
                    " (path: ${exitPath.ifEmpty { "none: the user reopened it" }})"
        )
        channel?.invokeMethod("game_mode_exited", mapOf("reason" to exitReason))
    }
}

/**
 * The immersive activity: a plain Activity (no Flutter), category VR + IMMERSIVE_HMD in its
 * own task. The OpenXR session runs in Rust (src/client/xr) on a thread of its own; this
 * class only starts it, forwards the lifecycle and takes the user back to the 2D panel when
 * the session ends ([onXrExit]).
 */
class XrActivity : Activity() {
    companion object {
        const val EXTRA_SID = "xr_sid"
        const val EXTRA_DISPLAY = "xr_display"
        const val EXTRA_W = "xr_w"
        const val EXTRA_H = "xr_h"
        const val EXTRA_PAD = "xr_pad"
        private const val TAG = GameMode.TAG
    }

    private var started = false
    private var exitReason: String? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        val sid = intent.getStringExtra(EXTRA_SID) ?: ""
        val display = intent.getIntExtra(EXTRA_DISPLAY, -1)
        val w = intent.getIntExtra(EXTRA_W, 0)
        val h = intent.getIntExtra(EXTRA_H, 0)
        val pad = intent.getIntExtra(EXTRA_PAD, 0)
        Log.i(TAG, "XrActivity onCreate task=$taskId sid=$sid display=$display ${w}x$h pad=$pad")
        if (!GameMode.active || sid.isEmpty()) {
            // Restored by the system without a game mode to show (e.g. after the process died).
            Log.w(TAG, "XrActivity without game mode: finishing")
            finish()
            return
        }
        GameMode.attach(this)
        started = try {
            FFI.xrStart(this, sid, display, w, h, pad)
        } catch (e: UnsatisfiedLinkError) {
            Log.e(TAG, "this build has no game mode (Rust feature quest-xr): $e")
            false
        }
        if (!started) {
            Log.e(TAG, "xrStart failed")
            leave("error: the XR session did not start")
        }
    }

    override fun onStart() {
        super.onStart()
        Log.i(TAG, "XrActivity onStart")
    }

    override fun onResume() {
        super.onResume()
        Log.i(TAG, "XrActivity onResume")
        GameMode.xrLifecycle(this, true)
        if (started) FFI.xrLifecycle(true)
    }

    override fun onPause() {
        Log.i(TAG, "XrActivity onPause")
        GameMode.xrLifecycle(this, false)
        if (started) FFI.xrLifecycle(false)
        super.onPause()
    }

    override fun onStop() {
        Log.i(TAG, "XrActivity onStop")
        super.onStop()
    }

    // Back (if the system delivers one here): leave the normal way, through the session.
    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        Log.i(TAG, "XrActivity back")
        if (started) FFI.xrRequestExit() else super.onBackPressed()
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (event.action == KeyEvent.ACTION_DOWN) {
            Log.d(TAG, "XrActivity key ${KeyEvent.keyCodeToString(event.keyCode)}")
        }
        return super.dispatchKeyEvent(event)
    }

    /**
     * Called by Rust from the XR thread when the session is over ([reason]: "Menu held 1.5 s",
     * "session exited", "error: ..."). Posts to the UI thread and returns at once: it must never
     * block the XR thread (onDestroy never waits for it either).
     */
    @Keep
    fun onXrExit(reason: String) {
        Log.i(TAG, "onXrExit: $reason")
        runOnUiThread { leave(reason) }
    }

    private fun leave(reason: String) {
        if (exitReason != null) return
        exitReason = reason
        // First: MainActivity.onResume (where Dart hears of the end) runs before this
        // activity's onDestroy.
        if (GameMode.isCurrent(this)) GameMode.end(reason)
        if (isFinishing || isDestroyed) return
        if (GameMode.mainResumed) {
            // The panel is already in front (the user went there through the system): a Home
            // intent now would take them out of it again.
            Log.i(TAG, "return: the panel is already resumed")
        } else {
            returnToPanel()
        }
        finish()
    }

    /** GameMode.onMainResumed: the user is back in the panel without the exit chord. */
    fun leaveFromSystem(reason: String) {
        if (exitReason != null) return
        Log.i(TAG, "leave: $reason")
        if (started) FFI.xrRequestExit()
        leave(reason)
    }

    private fun returnToPanel() {
        val app = applicationContext
        val main = Intent(app, MainActivity::class.java).apply {
            action = Intent.ACTION_MAIN
            addCategory(Intent.CATEGORY_LAUNCHER)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_REORDER_TO_FRONT)
        }
        if (GameMode.EXIT_VIA_HOME) {
            try {
                val flags = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
                val pending = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    // Android 15+ with targetSdk 35+: the creator must opt in to the background
                    // start (critique #8). Harmless on 14.
                    @Suppress("DEPRECATION")
                    val opts = ActivityOptions.makeBasic()
                        .setPendingIntentCreatorBackgroundActivityStartMode(
                            ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED
                        )
                    PendingIntent.getActivity(app, 0, main, flags, opts.toBundle())
                } else {
                    PendingIntent.getActivity(app, 0, main, flags)
                }
                startActivity(Intent(Intent.ACTION_MAIN).apply {
                    addCategory(Intent.CATEGORY_HOME)
                    addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    putExtra("extra_launch_in_home_pending_intent", pending)
                })
                Log.i(TAG, "return: Home intent with a PendingIntent for MainActivity")
                GameMode.returning(app, "home-pending-intent", main)
                return
            } catch (e: Exception) {
                Log.w(TAG, "return via Home failed: $e")
            }
        }
        try {
            startActivity(main)
            Log.i(TAG, "return: MainActivity REORDER_TO_FRONT")
            GameMode.returning(app, "reorder-to-front", null)
        } catch (e: Exception) {
            Log.e(TAG, "return to the panel failed: $e")
        }
    }

    override fun onDestroy() {
        val current = GameMode.isCurrent(this)
        Log.i(TAG, "XrActivity onDestroy finishing=$isFinishing reason=$exitReason current=$current")
        // Never waits: the XR thread ends the session by itself and calls onXrExit, which is
        // ignored by then. An older XrActivity destroyed after a new game mode started must
        // touch neither the new XR thread nor the new game mode.
        if (current) {
            if (started) FFI.xrStop()
            GameMode.end(exitReason ?: "XrActivity destroyed (system)")
        }
        super.onDestroy()
    }
}
