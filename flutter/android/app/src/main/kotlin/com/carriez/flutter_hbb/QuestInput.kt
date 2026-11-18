package com.carriez.flutter_hbb

/**
 * Meta Quest controller input for the remote-control view.
 *
 * Flutter's Android embedder turns only pointer-class MotionEvents into PointerEvents. Controller
 * buttons arrive as gamepad KeyEvents and the thumbsticks/triggers as joystick MotionEvents, and
 * Flutter drops both, so the remote page never sees them. MainActivity hands every key and generic
 * motion event to this object first; while a remote session is open on a Quest ([active]), the
 * controller events are forwarded to Dart over the "mChannel" MethodChannel as "quest_input" and
 * consumed. Everything else (and everything on a phone, where [active] stays false) goes on to
 * Flutter unchanged.
 *
 * Payloads (Dart side: flutter/lib/models/quest_input_model.dart):
 *   {"k":"key",  "code":Int, "name":String, "down":Bool, "repeat":Int, "src":Int}
 *   {"k":"axes", "lx","ly","rx","ry","hx","hy","lt","rt": Double, "src":Int, "dev":Int}
 *   {"k":"dbg",  "t":String}   only while [debug] is on: a raw event description for the readout
 */

import android.os.Build
import android.os.SystemClock
import android.util.Log
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import io.flutter.plugin.common.MethodChannel
import kotlin.math.abs
import kotlin.math.max

object QuestInput {
    private const val TAG = "QuestInput"

    /** Set by Dart while the remote page is open on a Quest: controller events are consumed. */
    @Volatile
    var active = false

    /** Set by Dart: forward raw event descriptions for the on-screen debug readout. */
    @Volatile
    var debug = false

    val isQuest: Boolean by lazy {
        val manufacturer = Build.MANUFACTURER ?: ""
        val model = Build.MODEL ?: ""
        manufacturer.equals("Oculus", true) || manufacturer.equals("Meta", true) ||
            model.contains("Quest", true)
    }

    private const val STICK_DEADZONE = 0.12f
    private const val TRIGGER_DEADZONE = 0.05f
    private const val DEBUG_THROTTLE_MS = 120L

    // lx, ly, rx, ry, hx, hy, lt, rt of the last forwarded axes event.
    private val lastAxes = FloatArray(8)
    private var lastAxesDebugMs = 0L
    private var lastPointerDebugMs = 0L

    /** Keys the Dart side maps; any other key goes on to Flutter. */
    private val mappedKeys = setOf(
        KeyEvent.KEYCODE_BUTTON_A, KeyEvent.KEYCODE_BUTTON_B,
        KeyEvent.KEYCODE_BUTTON_X, KeyEvent.KEYCODE_BUTTON_Y,
        KeyEvent.KEYCODE_BUTTON_L1, KeyEvent.KEYCODE_BUTTON_R1,
        KeyEvent.KEYCODE_BUTTON_L2, KeyEvent.KEYCODE_BUTTON_R2,
        KeyEvent.KEYCODE_BUTTON_THUMBL, KeyEvent.KEYCODE_BUTTON_THUMBR,
    )
    private val dpadKeys = setOf(
        KeyEvent.KEYCODE_DPAD_UP, KeyEvent.KEYCODE_DPAD_DOWN,
        KeyEvent.KEYCODE_DPAD_LEFT, KeyEvent.KEYCODE_DPAD_RIGHT,
        KeyEvent.KEYCODE_DPAD_CENTER,
    )

    private fun hasSource(src: Int, s: Int) = (src and s) == s

    private fun isControllerSource(src: Int) =
        hasSource(src, InputDevice.SOURCE_GAMEPAD) || hasSource(src, InputDevice.SOURCE_JOYSTICK)

    private fun send(channel: MethodChannel?, args: Map<String, Any>) {
        channel?.invokeMethod("quest_input", args)
    }

    private fun debugText(channel: MethodChannel?, text: String) {
        Log.d(TAG, text)
        send(channel, mapOf("k" to "dbg", "t" to text))
    }

    /** Returns true when the key event was consumed. */
    fun onKey(channel: MethodChannel?, e: KeyEvent): Boolean {
        val code = e.keyCode
        val src = e.source
        val fromController = isControllerSource(src) || KeyEvent.isGamepadButton(code)
        val mapped = code in mappedKeys || (code in dpadKeys && fromController)
        if (debug && e.repeatCount == 0) {
            debugText(
                channel,
                "key ${KeyEvent.keyCodeToString(code).removePrefix("KEYCODE_")}($code) " +
                    "${if (e.action == KeyEvent.ACTION_DOWN) "down" else "up"} " +
                    "src=0x${Integer.toHexString(src)} dev=${e.deviceId}" +
                    if (mapped && active) " -> mapped" else ""
            )
        }
        if (!active || !mapped) return false
        if (e.action != KeyEvent.ACTION_DOWN && e.action != KeyEvent.ACTION_UP) return false
        send(
            channel, mapOf(
                "k" to "key",
                "code" to code,
                "name" to KeyEvent.keyCodeToString(code),
                "down" to (e.action == KeyEvent.ACTION_DOWN),
                "repeat" to e.repeatCount,
                "src" to src,
            )
        )
        return true
    }

    private fun stick(e: MotionEvent, axis: Int): Float {
        val v = e.getAxisValue(axis)
        val flat = e.device?.getMotionRange(axis, e.source)?.flat ?: 0f
        return if (abs(v) < max(flat, STICK_DEADZONE)) 0f else v
    }

    private fun trigger(e: MotionEvent, a: Int, b: Int): Float {
        val v = max(e.getAxisValue(a), e.getAxisValue(b))
        return if (v < TRIGGER_DEADZONE) 0f else v
    }

    /** Joystick/gamepad motion. Returns true when consumed. Pointer events only feed the readout. */
    fun onGenericMotion(channel: MethodChannel?, e: MotionEvent): Boolean {
        val src = e.source
        if (!isControllerSource(src)) {
            if (debug) debugPointer(channel, e, "gen")
            return false
        }
        if (e.actionMasked != MotionEvent.ACTION_MOVE) return active
        // Right stick: the Quest reports RX/RY; standard gamepads use Z/RZ.
        val rxRaw = stick(e, MotionEvent.AXIS_RX)
        val ryRaw = stick(e, MotionEvent.AXIS_RY)
        val useR = rxRaw != 0f || ryRaw != 0f
        val axes = floatArrayOf(
            stick(e, MotionEvent.AXIS_X),
            stick(e, MotionEvent.AXIS_Y),
            if (useR) rxRaw else stick(e, MotionEvent.AXIS_Z),
            if (useR) ryRaw else stick(e, MotionEvent.AXIS_RZ),
            e.getAxisValue(MotionEvent.AXIS_HAT_X),
            e.getAxisValue(MotionEvent.AXIS_HAT_Y),
            trigger(e, MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE),
            trigger(e, MotionEvent.AXIS_RTRIGGER, MotionEvent.AXIS_GAS),
        )
        var changed = false
        var edge = false // a value went to or from zero: always forward / always log
        for (i in axes.indices) {
            if (abs(axes[i] - lastAxes[i]) > 0.01f) changed = true
            if ((axes[i] == 0f) != (lastAxes[i] == 0f)) edge = true
        }
        if (debug && (edge || (changed &&
                SystemClock.uptimeMillis() - lastAxesDebugMs > DEBUG_THROTTLE_MS))) {
            lastAxesDebugMs = SystemClock.uptimeMillis()
            debugText(
                channel,
                "axes dev=${e.deviceId} src=0x${Integer.toHexString(src)} " +
                    "L(%.2f,%.2f) R(%.2f,%.2f) hat(%.0f,%.0f) trig(%.2f,%.2f)".format(
                        axes[0], axes[1], axes[2], axes[3], axes[4], axes[5], axes[6], axes[7]
                    ) + if (useR) " [RX/RY]" else ""
            )
        }
        if (!active) return false
        if (changed || edge) {
            axes.copyInto(lastAxes)
            send(
                channel, mapOf(
                    "k" to "axes",
                    "lx" to axes[0].toDouble(), "ly" to axes[1].toDouble(),
                    "rx" to axes[2].toDouble(), "ry" to axes[3].toDouble(),
                    "hx" to axes[4].toDouble(), "hy" to axes[5].toDouble(),
                    "lt" to axes[6].toDouble(), "rt" to axes[7].toDouble(),
                    "src" to src, "dev" to e.deviceId,
                )
            )
        }
        return true
    }

    /** Touch events: never consumed, only described for the readout. */
    fun onTouch(channel: MethodChannel?, e: MotionEvent) {
        if (debug) debugPointer(channel, e, "touch")
    }

    private fun toolName(t: Int) = when (t) {
        MotionEvent.TOOL_TYPE_FINGER -> "finger"
        MotionEvent.TOOL_TYPE_STYLUS -> "stylus"
        MotionEvent.TOOL_TYPE_MOUSE -> "mouse"
        MotionEvent.TOOL_TYPE_ERASER -> "eraser"
        5 -> "palm"
        else -> "unknown"
    }

    private fun debugPointer(channel: MethodChannel?, e: MotionEvent, via: String) {
        val a = e.actionMasked
        val isStream = a == MotionEvent.ACTION_MOVE || a == MotionEvent.ACTION_HOVER_MOVE
        val now = SystemClock.uptimeMillis()
        if (isStream && now - lastPointerDebugMs < DEBUG_THROTTLE_MS) return
        lastPointerDebugMs = now
        val extra = if (a == MotionEvent.ACTION_SCROLL)
            " v=%.1f h=%.1f".format(
                e.getAxisValue(MotionEvent.AXIS_VSCROLL), e.getAxisValue(MotionEvent.AXIS_HSCROLL)
            ) else ""
        debugText(
            channel,
            "$via ${MotionEvent.actionToString(a).removePrefix("ACTION_")} " +
                "tool=${toolName(e.getToolType(0))} src=0x${Integer.toHexString(e.source)} " +
                "btn=${e.buttonState} n=${e.pointerCount} (%.0f,%.0f)".format(e.x, e.y) + extra
        )
    }

    fun configure(args: Any?): Boolean {
        if (args is Map<*, *>) {
            (args["active"] as? Boolean)?.let { active = it && isQuest }
            (args["debug"] as? Boolean)?.let { debug = it }
            if (!active) lastAxes.fill(0f)
        }
        Log.d(TAG, "configure: active=$active debug=$debug isQuest=$isQuest")
        return isQuest
    }

    fun info(): Map<String, Any> = mapOf(
        "isQuest" to isQuest,
        "manufacturer" to (Build.MANUFACTURER ?: ""),
        "model" to (Build.MODEL ?: ""),
    )
}
