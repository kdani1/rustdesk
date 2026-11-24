// ffi.kt

package ffi

import android.app.Activity
import android.content.Context
import android.view.Surface
import java.nio.ByteBuffer

import com.carriez.flutter_hbb.RdClipboardManager

object FFI {
    init {
        System.loadLibrary("rustdesk")
    }

    external fun init(ctx: Context)
    external fun onAppStart(ctx: Context)
    external fun setClipboardManager(clipboardManager: RdClipboardManager)
    external fun startServer(app_dir: String, home_dir: String, custom_client_config: String)
    external fun startService()
    external fun onVideoFrameUpdate(buf: ByteBuffer)
    external fun onAudioFrameUpdate(buf: ByteBuffer)
    external fun translateLocale(localeName: String, input: String): String
    external fun refreshScreen()
    external fun closeAllSessions()
    external fun setFrameRawEnable(name: String, value: Boolean)
    external fun setCodecInfo(info: String)
    external fun getLocalOption(key: String): String
    external fun getBuildinOption(key: String): String
    external fun onClipboardUpdate(clips: ByteBuffer)
    external fun isServiceClipboardEnabled(): Boolean
    // Hardware decoding into a Flutter texture (VideoTextures.kt): the Surface of UI session
    // [sid]'s texture [texture] for [display], null when it is gone; [hevc] / [avc]: decoder
    // names.
    external fun setVideoSurface(sid: String, display: Int, surface: Surface?, texture: Long, width: Int, height: Int, hevc: String, avc: String)
    // Buffers rendered to that Surface so far, -1 without one.
    external fun videoSurfaceRendered(sid: String, display: Int): Long
    // Quest game mode (Rust feature quest-xr, src/client/xr): absent from other builds, so
    // callers catch UnsatisfiedLinkError. xrStart starts the OpenXR thread for XrActivity
    // [activity] and returns at once; xrStop and xrRequestExit only raise flags (never block).
    external fun xrStart(activity: Activity, sid: String, display: Int, width: Int, height: Int, pad: Int): Boolean
    external fun xrStop()
    external fun xrRequestExit()
    external fun xrLifecycle(resumed: Boolean)
    // Quest 2D gamepad mode (QuestGamepadCapture.kt, Rust src/client/gamepad_jni.rs): one full
    // Misc.gamepad_state snapshot for UI session [sid] (XInput buttons, triggers 0-255, sticks
    // int16, up = +Y). Returns the host's Features.gamepad (sent only when 1), -2 before its
    // PeerInfo, -1 without such a session.
    external fun sendGamepad(sid: String, seq: Int, buttons: Int, lt: Int, rt: Int, lx: Int, ly: Int, rx: Int, ry: Int, off: Boolean): Int
}
