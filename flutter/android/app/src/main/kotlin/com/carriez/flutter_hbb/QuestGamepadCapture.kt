package com.carriez.flutter_hbb

/**
 * Quest "gamepad mode" in the normal 2D panel, the way the Xbox app (com.xbox.pwa) does it
 * (C:\RustDeskBuild\xbox-gamepad-re.md): Horizon OS's public, permission-free
 * `vros.spatial.input.SpatialInputManager.requestControllerCapture(true)`. The system's only
 * check is that our window has focus. On a grant it creates a virtual input device
 * "VR controller gamepad device" (vendor 0x2833, product 0x5024) and VrShell routes the Touch
 * buttons, sticks and triggers to the focused panel; the Meta button and any focus change
 * release it.
 *
 * While captured, the virtual pad's KeyEvents / MotionEvents are taken here (MainActivity hands
 * them over BEFORE QuestInput, so they never become mouse or scroll input), turned into one
 * XInput state and sent to the host as Misc.gamepad_state through FFI.sendGamepad: on every
 * change and at least every [HEARTBEAT_MS] (the host sets the pad neutral after 600 ms of
 * silence), and once with off = true when capture ends or the session closes.
 *
 * Re-request rule (same as Xbox's GamepadInputFocusManager): only when focus was lost while
 * captured and the user still wants the mode. The focus callback (input channel) and the capture
 * callback (binder, then main.post) are not ordered, so a release is decided [DECIDE_MS] after
 * it arrives: if one of our own windows (a DisplayActivity panel, XrActivity) took focus around
 * then, capture is requested again when the main window gets focus back; otherwise it was the
 * Meta button or the system UI, and it is never undone automatically (the user turns it on again
 * from the Quest menu). Until that decision the host pad only goes neutral (it stays plugged in);
 * off = true goes out on a Meta / system release, on disable(), or when a resume is not granted
 * within [RESUME_GRANT_MS].
 *
 * Escape that does not depend on any ordering: both stick clicks (L3 + R3), or START + SELECT,
 * held for [CHORD_MS] end gamepad mode like the menu toggle.
 *
 * Every key code and axis seen for the first time is logged (tag QuestGamepad), to pin down the
 * exact button map of the virtual device on the headset.
 */

import android.app.Activity
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import ffi.FFI
import java.lang.reflect.Method
import java.lang.reflect.Proxy
import java.util.concurrent.Executor
import kotlin.math.abs
import kotlin.math.max
import kotlin.math.roundToInt

object QuestGamepadCapture {
    private const val TAG = "QuestGamepadCapture"
    private const val PAD_TAG = "QuestGamepad"

    /** A full snapshot goes out at least this often while captured (host: neutral at 600 ms). */
    private const val HEARTBEAT_MS = 150L

    /** A release is judged (Meta vs. focus moved to our own window) this long after it came. */
    private const val DECIDE_MS = 300L

    /** A resume request without a grant within this long ends the mode (off = true). */
    private const val RESUME_GRANT_MS = 1000L

    /** disable(): no release callback within this long counts as released. */
    private const val RELEASE_CHECK_MS = 500L

    /** The escape chord must be held this long. */
    private const val CHORD_MS = 1000L

    /** The virtual device Horizon OS creates on a grant. */
    private const val PAD_VENDOR = 0x2833
    private const val PAD_PRODUCT = 0x5024

    // XINPUT_GAMEPAD_* (wButtons), as in src/client/xr/input.rs. Guide (0x0400) is never sent.
    private const val DPAD_UP = 0x0001
    private const val DPAD_DOWN = 0x0002
    private const val DPAD_LEFT = 0x0004
    private const val DPAD_RIGHT = 0x0008
    private const val START = 0x0010
    private const val BACK = 0x0020
    private const val LEFT_THUMB = 0x0040
    private const val RIGHT_THUMB = 0x0080
    private const val LEFT_SHOULDER = 0x0100
    private const val RIGHT_SHOULDER = 0x0200
    private const val BTN_A = 0x1000
    private const val BTN_B = 0x2000
    private const val BTN_X = 0x4000
    private const val BTN_Y = 0x8000

    private var mgr: Any? = null
    private var reqM: Method? = null
    private var unregM: Method? = null
    private var cb: Any? = null
    private var onChange: ((Boolean) -> Unit)? = null
    private var onHost: ((Int) -> Unit)? = null
    private val main = Handler(Looper.getMainLooper())

    /** The system routes the controllers to us (ControllerCaptureCallback). Main thread. */
    @Volatile
    var captured = false
        private set
    private var wanted = false          // the user turned gamepad mode on
    private var hasFocus = false
    private var resumeOnFocus = false   // focus went to our own window while captured (the Xbox rule)
    private var pending = false         // released while wanted, Meta vs. own focus not decided yet
    private var releasing = false       // we asked for the release, its callback has not come yet
    private var chordArmed = false
    private var releasedAt = 0L         // uptime of the last unasked release
    private var focusLostAt = 0L        // uptime the main window last lost focus
    private var ownFocusAt = -1_000_000L // uptime one of our other windows last took focus
    private var sid: String? = null     // UI session the state goes to

    // Pad state, from keys and axes.
    private var keyButtons = 0
    private var hatButtons = 0
    private var ltAxis = 0
    private var rtAxis = 0
    private var ltKey = false
    private var rtKey = false
    private var lx = 0
    private var ly = 0
    private var rx = 0
    private var ry = 0
    private var seq = 0
    private var sending = false         // a state went out since capture began: an off is owed
    private var last: IntArray? = null
    private var lastHost = Int.MIN_VALUE
    private val seenKeys = HashSet<Int>()
    private val seenAxes = HashSet<Int>()
    private val seenDevices = HashSet<Int>()

    private val heartbeat = Runnable { if (captured && !releasing) sendState(true) }
    private val decide = Runnable { decideRelease() }
    private val resumeTimeout = Runnable {
        if (captured) return@Runnable
        Log.i(TAG, "resume not granted within ${RESUME_GRANT_MS} ms")
        wanted = false
        resumeOnFocus = false
        stopSending("resume not granted")
    }
    private val releaseCheck = Runnable {
        if (!(captured && releasing)) return@Runnable
        Log.w(TAG, "no release callback within ${RELEASE_CHECK_MS} ms: treated as released")
        releasing = false
        captured = false
        onChange?.invoke(false)
    }
    private val chordCheck = Runnable {
        chordArmed = false
        if (!captured || releasing || !chordHeld()) return@Runnable
        Log.i(TAG, "escape chord held ${CHORD_MS} ms: gamepad mode off")
        disable()
        onChange?.invoke(false)
    }

    private fun now() = SystemClock.uptimeMillis()

    /**
     * Looks up the capture API and registers the callback. False when this Horizon OS has no
     * such API (or not a Quest); everything else then stays a no-op. [changed]: the capture
     * state; [host]: the host's Features.gamepad as FFI.sendGamepad returns it, on change.
     */
    fun init(a: Activity, changed: (Boolean) -> Unit, host: (Int) -> Unit): Boolean {
        onChange = changed
        onHost = host
        if (mgr != null) return true
        return try {
            val cls = Class.forName("vros.spatial.input.SpatialInputManager")
            @Suppress("UNCHECKED_CAST")
            val m = a.getSystemService(cls as Class<Any>) ?: return false
            val cbCls = Class.forName("vros.spatial.input.SpatialInputManager\$ControllerCaptureCallback")
            val proxy = Proxy.newProxyInstance(cbCls.classLoader, arrayOf(cbCls)) { p, meth, args ->
                when (meth.name) {
                    "onControllerCaptureChanged" -> {
                        onCaptureChanged(args!![0] as Boolean); null
                    }
                    "hashCode" -> System.identityHashCode(p)
                    "equals" -> p === args?.get(0)
                    "toString" -> TAG
                    else -> null
                }
            }
            val exec = Executor { r -> main.post(r) }
            cls.getMethod("registerControllerCaptureCallback", Executor::class.java, cbCls)
                .invoke(m, exec, proxy)
            reqM = cls.getMethod("requestControllerCapture", Boolean::class.javaPrimitiveType)
            unregM = cls.getMethod("unregisterControllerCaptureCallback", cbCls)
            mgr = m
            cb = proxy
            Log.i(TAG, "controller-capture API available")
            true
        } catch (t: Throwable) {
            Log.w(TAG, "controller-capture API unavailable: $t")
            false
        }
    }

    val available: Boolean get() = mgr != null

    private fun onCaptureChanged(c: Boolean) {
        val t = now()
        val was = captured
        captured = c
        Log.i(
            PAD_TAG, "capture callback captured=$c t=$t wanted=$wanted focus=$hasFocus " +
                "releasing=$releasing focusLost=${t - focusLostAt}ms ago ownFocus=${t - ownFocusAt}ms ago"
        )
        main.removeCallbacks(resumeTimeout)
        if (c) {
            releasing = false
            main.removeCallbacks(releaseCheck)
            pending = false
            main.removeCallbacks(decide)
            resumeOnFocus = false
            if (!was) startSending()
        } else if (was) {
            main.removeCallbacks(chordCheck)
            chordArmed = false
            if (releasing || !wanted) {
                releasing = false
                main.removeCallbacks(releaseCheck)
                stopSending("capture released")
            } else {
                // Meta / system UI, or focus moved to one of our windows: the focus callback may
                // come before or after this one, so judge it a little later. The host pad only
                // goes neutral meanwhile.
                releasedAt = t
                pending = true
                goNeutral()
                main.removeCallbacks(decide)
                main.postDelayed(decide, DECIDE_MS)
            }
        }
        onChange?.invoke(c)
    }

    private fun decideRelease() {
        pending = false
        if (!wanted || captured) return
        val t = now()
        // Our own window took focus shortly before, or since, the release.
        val ownMoved = ownFocusAt >= releasedAt - DECIDE_MS
        Log.i(
            PAD_TAG, "release judged: ${if (ownMoved) "focus moved to our window" else "Meta / system UI"} " +
                "release=${t - releasedAt}ms ago focusLost=${t - focusLostAt}ms ago " +
                "ownFocus=${t - ownFocusAt}ms ago focus=$hasFocus"
        )
        if (ownMoved) {
            resumeOnFocus = true
            if (hasFocus) resume()
        } else {
            // Never re-requested: the user turns it on again from the Quest menu.
            wanted = false
            resumeOnFocus = false
            stopSending("Meta / system release")
        }
    }

    private fun resume() {
        resumeOnFocus = false
        if (request(true)) {
            main.removeCallbacks(resumeTimeout)
            main.postDelayed(resumeTimeout, RESUME_GRANT_MS)
        } else {
            wanted = false
            stopSending("resume request failed")
        }
    }

    /**
     * One of our other windows (a DisplayActivity panel, XrActivity) took focus or is about to be
     * started: a capture release around now is a focus move, not the Meta button.
     */
    fun noteOwnFocus(who: String) {
        ownFocusAt = now()
        if (wanted) Log.i(PAD_TAG, "own focus: $who t=$ownFocusAt captured=$captured")
    }

    private fun request(on: Boolean): Boolean = try {
        if (reqM == null) false else {
            reqM?.invoke(mgr, on)
            Log.i(TAG, "requestControllerCapture($on) focus=$hasFocus")
            true
        }
    } catch (t: Throwable) {
        Log.w(TAG, "requestControllerCapture($on) failed: $t")
        false
    }

    /**
     * Quest menu toggle on, for UI session [session]. The system grants it only while our window
     * has focus; the answer comes as the capture callback (Dart treats no answer within 1 s as a
     * refusal). Returns false when the API is missing.
     */
    fun enable(session: String?): Boolean {
        if (!available) return false
        if (session != null && session != sid) {
            if (sending) stopSending("session changed")
            sid = session
        }
        wanted = true
        pending = false
        resumeOnFocus = false
        main.removeCallbacks(decide)
        if (releasing) {
            // Our release is still on its way: do not assume the capture is ours any more.
            releasing = false
            main.removeCallbacks(releaseCheck)
            captured = false
        }
        if (captured) {
            // Already ours (e.g. the shell chord): make sure this session gets the pad.
            if (!sending) sendState(true)
            onChange?.invoke(true)
            return true
        }
        return request(true)
    }

    /** Quest menu toggle off, or the remote page closed. Only the holder can release. */
    fun disable() {
        wanted = false
        resumeOnFocus = false
        pending = false
        chordArmed = false
        main.removeCallbacks(decide)
        main.removeCallbacks(resumeTimeout)
        main.removeCallbacks(chordCheck)
        if (captured && !releasing) {
            // Nothing goes out after the off until the system confirms the release.
            releasing = true
            request(false)
            main.removeCallbacks(releaseCheck)
            main.postDelayed(releaseCheck, RELEASE_CHECK_MS)
        }
        stopSending("disabled")
    }

    /** MainActivity.onWindowFocusChanged. */
    fun onWindowFocusChanged(focus: Boolean) {
        hasFocus = focus
        val t = now()
        if (!focus) focusLostAt = t
        if (wanted || captured) {
            Log.i(PAD_TAG, "main focus=$focus t=$t captured=$captured resume=$resumeOnFocus pending=$pending")
        }
        if (focus && wanted && resumeOnFocus && !captured && !pending) resume()
    }

    /** MainActivity.onDestroy: owed off, release, unregister. */
    fun release() {
        disable()
        main.removeCallbacks(releaseCheck)
        releasing = false
        try {
            unregM?.invoke(mgr, cb)
        } catch (_: Throwable) {
        }
        mgr = null
        cb = null
        reqM = null
        unregM = null
        captured = false
        onChange = null
        onHost = null
    }

    // ---- events ----

    private fun hasSource(src: Int, s: Int) = (src and s) == s

    private fun isPadDevice(d: InputDevice?) =
        d != null && d.vendorId == PAD_VENDOR && d.productId == PAD_PRODUCT

    private fun fromPad(d: InputDevice?, src: Int) =
        isPadDevice(d) || hasSource(src, InputDevice.SOURCE_GAMEPAD) ||
            hasSource(src, InputDevice.SOURCE_JOYSTICK)

    private fun logDevice(d: InputDevice?, id: Int) {
        if (!seenDevices.add(id)) return
        if (d == null) {
            Log.i(PAD_TAG, "device $id: unknown")
            return
        }
        Log.i(
            PAD_TAG, "device $id \"${d.name}\" vendor=0x${Integer.toHexString(d.vendorId)} " +
                "product=0x${Integer.toHexString(d.productId)} " +
                "sources=0x${Integer.toHexString(d.sources)} axes=" +
                d.motionRanges.joinToString(",") { MotionEvent.axisToString(it.axis).removePrefix("AXIS_") }
        )
    }

    private fun keyBit(code: Int): Int = when (code) {
        KeyEvent.KEYCODE_BUTTON_A -> BTN_A
        KeyEvent.KEYCODE_BUTTON_B -> BTN_B
        KeyEvent.KEYCODE_BUTTON_X -> BTN_X
        KeyEvent.KEYCODE_BUTTON_Y -> BTN_Y
        KeyEvent.KEYCODE_BUTTON_L1 -> LEFT_SHOULDER
        KeyEvent.KEYCODE_BUTTON_R1 -> RIGHT_SHOULDER
        KeyEvent.KEYCODE_BUTTON_THUMBL -> LEFT_THUMB
        KeyEvent.KEYCODE_BUTTON_THUMBR -> RIGHT_THUMB
        KeyEvent.KEYCODE_BUTTON_START -> START
        KeyEvent.KEYCODE_BUTTON_SELECT -> BACK
        KeyEvent.KEYCODE_DPAD_UP -> DPAD_UP
        KeyEvent.KEYCODE_DPAD_DOWN -> DPAD_DOWN
        KeyEvent.KEYCODE_DPAD_LEFT -> DPAD_LEFT
        KeyEvent.KEYCODE_DPAD_RIGHT -> DPAD_RIGHT
        else -> 0 // KEYCODE_BUTTON_MODE (Guide) is never sent
    }

    /** MainActivity.dispatchKeyEvent, before QuestInput. True when consumed. */
    fun onKey(e: KeyEvent): Boolean {
        if (!captured) return false
        val d = e.device
        val code = e.keyCode
        if (!fromPad(d, e.source) && !KeyEvent.isGamepadButton(code)) return false
        if (seenKeys.add(code)) {
            logDevice(d, e.deviceId)
            Log.i(
                PAD_TAG, "key ${KeyEvent.keyCodeToString(code)}($code) first seen " +
                    "src=0x${Integer.toHexString(e.source)} dev=${e.deviceId} " +
                    "-> 0x${Integer.toHexString(keyBit(code))}" +
                    when (code) {
                        KeyEvent.KEYCODE_BUTTON_L2 -> " (LT)"
                        KeyEvent.KEYCODE_BUTTON_R2 -> " (RT)"
                        else -> ""
                    }
            )
        }
        val down = when (e.action) {
            KeyEvent.ACTION_DOWN -> true
            KeyEvent.ACTION_UP -> false
            else -> return true
        }
        when (code) {
            KeyEvent.KEYCODE_BUTTON_L2 -> ltKey = down
            KeyEvent.KEYCODE_BUTTON_R2 -> rtKey = down
            else -> {
                val bit = keyBit(code)
                if (bit != 0) keyButtons = if (down) keyButtons or bit else keyButtons and bit.inv()
            }
        }
        if (e.repeatCount == 0) sendState(false)
        val chord = chordHeld()
        if (chord && !chordArmed) {
            chordArmed = true
            main.postDelayed(chordCheck, CHORD_MS)
        } else if (!chord && chordArmed) {
            chordArmed = false
            main.removeCallbacks(chordCheck)
        }
        return true
    }

    /** L3 + R3, or START + SELECT: the escape chord. */
    private fun chordHeld(): Boolean {
        val thumbs = LEFT_THUMB or RIGHT_THUMB
        val menu = START or BACK
        return (keyButtons and thumbs) == thumbs || (keyButtons and menu) == menu
    }

    private fun stick(e: MotionEvent, axis: Int, invert: Boolean): Int {
        val v = e.getAxisValue(axis)
        val flat = e.device?.getMotionRange(axis, e.source)?.flat ?: 0f
        if (abs(v) <= flat) return 0
        val s = if (invert) -v else v
        return (s * 32767f).roundToInt().coerceIn(-32768, 32767)
    }

    private fun trigger(e: MotionEvent, a: Int, b: Int): Int {
        val v = max(e.getAxisValue(a), e.getAxisValue(b)).coerceIn(0f, 1f)
        return (v * 255f).roundToInt()
    }

    private fun hasAxis(e: MotionEvent, axis: Int): Boolean =
        e.device?.getMotionRange(axis, e.source) != null

    private fun logAxes(e: MotionEvent) {
        val d = e.device ?: return
        for (r in d.motionRanges) {
            val a = r.axis
            if (a in seenAxes) continue
            val v = e.getAxisValue(a)
            if (abs(v) < 0.3f) continue
            seenAxes.add(a)
            logDevice(d, e.deviceId)
            Log.i(
                PAD_TAG, "axis ${MotionEvent.axisToString(a)}($a) first seen value=%.2f ".format(v) +
                    "src=0x${Integer.toHexString(e.source)} dev=${e.deviceId}"
            )
        }
    }

    /** MainActivity.dispatchGenericMotionEvent, before QuestInput. True when consumed. */
    fun onGenericMotion(e: MotionEvent): Boolean {
        if (!captured) return false
        if (!fromPad(e.device, e.source)) return false
        if (e.actionMasked != MotionEvent.ACTION_MOVE) return true
        logAxes(e)
        lx = stick(e, MotionEvent.AXIS_X, false)
        // Android: down is +Y; XInput: up is +Y.
        ly = stick(e, MotionEvent.AXIS_Y, true)
        // The Quest pad reports RX/RY; a standard gamepad uses Z/RZ.
        val useR = hasAxis(e, MotionEvent.AXIS_RX) || e.device == null
        rx = stick(e, if (useR) MotionEvent.AXIS_RX else MotionEvent.AXIS_Z, false)
        ry = stick(e, if (useR) MotionEvent.AXIS_RY else MotionEvent.AXIS_RZ, true)
        ltAxis = trigger(e, MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE)
        rtAxis = trigger(e, MotionEvent.AXIS_RTRIGGER, MotionEvent.AXIS_GAS)
        val hx = e.getAxisValue(MotionEvent.AXIS_HAT_X)
        val hy = e.getAxisValue(MotionEvent.AXIS_HAT_Y)
        var hat = 0
        if (hx < -0.5f) hat = hat or DPAD_LEFT
        if (hx > 0.5f) hat = hat or DPAD_RIGHT
        if (hy < -0.5f) hat = hat or DPAD_UP
        if (hy > 0.5f) hat = hat or DPAD_DOWN
        hatButtons = hat
        sendState(false)
        return true
    }

    // ---- sending ----

    private fun resetPad() {
        if (chordArmed) main.removeCallbacks(chordCheck)
        chordArmed = false
        keyButtons = 0; hatButtons = 0
        ltAxis = 0; rtAxis = 0; ltKey = false; rtKey = false
        lx = 0; ly = 0; rx = 0; ry = 0
        last = null
    }

    private fun startSending() {
        resetPad()
        seq = 0
        lastHost = Int.MIN_VALUE
        // A neutral first state plugs the host pad in right away.
        sendState(true)
    }

    /** Capture lost but maybe coming back: the host pad goes neutral and stays plugged in. */
    private fun goNeutral() {
        main.removeCallbacks(heartbeat)
        resetPad()
        if (sending) {
            push(intArrayOf(0, 0, 0, 0, 0, 0, 0), false)
            Log.i(TAG, "neutral sent (capture lost, maybe resumed)")
        }
    }

    private fun stopSending(reason: String) {
        main.removeCallbacks(heartbeat)
        if (sending) {
            resetPad()
            push(intArrayOf(0, 0, 0, 0, 0, 0, 0), true)
            Log.i(TAG, "off sent ($reason)")
        }
        sending = false
        seq = 0
        resetPad()
    }

    private fun sendState(force: Boolean) {
        // Our release is on its way: an off went out, do not plug the host pad in again.
        if (releasing) return
        val st = intArrayOf(
            keyButtons or hatButtons,
            max(ltAxis, if (ltKey) 255 else 0),
            max(rtAxis, if (rtKey) 255 else 0),
            lx, ly, rx, ry,
        )
        if (!force && last?.contentEquals(st) == true) return
        last = st
        push(st, false)
        main.removeCallbacks(heartbeat)
        if (captured && !releasing) main.postDelayed(heartbeat, HEARTBEAT_MS)
    }

    private fun push(st: IntArray, off: Boolean) {
        val s = sid ?: return
        val r = try {
            FFI.sendGamepad(s, seq, st[0], st[1], st[2], st[3], st[4], st[5], st[6], off)
        } catch (t: UnsatisfiedLinkError) {
            -3
        }
        seq++
        sending = true
        if (r != lastHost) {
            lastHost = r
            Log.i(TAG, "host Features.gamepad=$r (1 = sent; -1 no session, -2 not known yet)")
            onHost?.invoke(r)
        }
    }
}
