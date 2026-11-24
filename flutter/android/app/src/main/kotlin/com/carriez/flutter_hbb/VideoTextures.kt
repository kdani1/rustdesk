package com.carriez.flutter_hbb

import android.media.MediaCodecList
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import ffi.FFI
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.view.TextureRegistry

/**
 * The Flutter textures of one engine that the remote picture is decoded into by the device's
 * hardware decoder (MediaCodec, driven from Rust: libs/scrap/src/android/surface_decoder.rs).
 *
 * One texture per remote display the engine's view shows alone. Dart asks for it over
 * "mChannel" (`video_texture_create` / `video_texture_dispose`) and draws it with a `Texture`
 * widget; the texture's Surface goes to Rust with [FFI.setVideoSurface], keyed by the view's UI
 * session id and the display. Rust falls back to software decoding whenever there is no
 * Surface, so everything here may fail without losing the picture.
 *
 * Main thread only. Texture ids belong to their engine: never pass one to another engine.
 */
class VideoTextures(private val engine: FlutterEngine, private val who: String) {
    companion object {
        private const val TAG = "VideoTextures"

        // How often the rendered-frame count of a texture is looked at.
        private const val TICK_MS = 50L

        // While pictures keep coming, one extra engine frame at most this often.
        private const val KICK_MS = 250L

        private var picked = false
        private var hevc = ""
        private var avc = ""

        /**
         * The hardware decoders to use, once per process. Empty: Rust asks the system for its
         * default decoder of the type.
         */
        private fun pickDecoders() {
            if (picked) return
            picked = true
            // isHardwareAccelerated is API 29, like the textures themselves.
            if (Build.VERSION.SDK_INT < 29) return
            hevc = pick("video/hevc")
            avc = pick("video/avc")
        }

        private fun pick(mime: String): String = try {
            val found = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter {
                !it.isEncoder && it.supportedTypes.any { t -> t.equals(mime, true) } &&
                        it.isHardwareAccelerated &&
                        !it.name.contains("secure", true) && !it.name.contains("dolby", true)
            }
            for (c in found) {
                val caps = c.getCapabilitiesForType(mime)
                Log.d(
                    TAG, "decoder $mime=${c.name} lowLatency=" +
                            (Build.VERSION.SDK_INT >= 30 && caps.isFeatureSupported("low-latency")) +
                            " maxInstances=${caps.maxSupportedInstances}"
                )
            }
            if (found.isEmpty()) Log.d(TAG, "decoder $mime: no hardware decoder")
            found.firstOrNull()?.name ?: ""
        } catch (e: Throwable) {
            Log.e(TAG, "decoder $mime: $e")
            ""
        }
    }

    private class Slot(
        val sid: String,
        val display: Int,
        val producer: TextureRegistry.SurfaceProducer
    ) {
        // The Surface Rust has now, null when it has none.
        var surface: Surface? = null

        // Rust's count of buffers rendered to the Surface, as last seen.
        var rendered = -1L
        var dirty = false
        var kickAt = 0L
    }

    private val slots = HashMap<Int, Slot>()
    private val handler = Handler(Looper.getMainLooper())
    private var ticking = false

    /**
     * Flutter shows the oldest picture waiting in a texture's queue with each of its frames,
     * and only a new picture asks for a frame. Two pictures that arrive within one frame
     * leave the newer one waiting until the next picture: one frame late while the stream
     * runs, and not shown at all when it stops. So while pictures come one extra frame is
     * asked for now and then, and one more after the last picture.
     */
    private val tick = object : Runnable {
        override fun run() {
            if (slots.isEmpty()) {
                ticking = false
                return
            }
            val now = SystemClock.uptimeMillis()
            for (s in slots.values) {
                if (s.surface == null) continue
                val n = try {
                    FFI.videoSurfaceRendered(s.sid, s.display)
                } catch (e: Throwable) {
                    -1L
                }
                if (n < 0) continue
                if (n != s.rendered) {
                    s.rendered = n
                    s.dirty = true
                    if (now - s.kickAt >= KICK_MS) kick(s, now)
                } else if (s.dirty) {
                    s.dirty = false
                    kick(s, now)
                }
            }
            handler.postDelayed(this, TICK_MS)
        }
    }

    private fun kick(s: Slot, now: Long) {
        s.kickAt = now
        try {
            s.producer.scheduleFrame()
        } catch (e: Throwable) {
            Log.e(TAG, "$who scheduleFrame display=${s.display}: $e")
        }
    }

    /**
     * A texture of [w] x [h] for [display], shown by UI session [sid]. Returns its texture id
     * for the Dart `Texture` widget, or -1 when there is none (Android below 10, or any
     * failure): the picture then stays on the software path.
     */
    fun create(sid: String, display: Int, w: Int, h: Int): Long {
        if (Build.VERSION.SDK_INT < 29 || w <= 0 || h <= 0) return -1L
        dispose(display)
        return try {
            pickDecoders()
            val producer = engine.renderer.createSurfaceProducer()
            // Before the first getSurface(): the size of the buffers behind it.
            producer.setSize(w, h)
            val s = Slot(sid, display, producer)
            producer.setCallback(object : TextureRegistry.SurfaceProducer.Callback {
                // Called when the whole process resumes (no activity was resumed, now one
                // is), whether or not the Surface was lost. One activity of several that
                // stops and starts again gets no call: see reattach().
                override fun onSurfaceCreated() {
                    if (slots[display] === s) attach(s)
                }

                override fun onSurfaceDestroyed() {
                    if (slots[display] === s) detach(s)
                }
            })
            slots[display] = s
            if (!attach(s)) {
                // No Surface, or a native library without the entry point: no texture.
                dispose(display)
                return -1L
            }
            if (!ticking) {
                ticking = true
                handler.postDelayed(tick, TICK_MS)
            }
            Log.d(TAG, "$who create display=$display ${w}x$h texture=${producer.id()} sid=$sid")
            producer.id()
        } catch (e: Throwable) {
            Log.e(TAG, "$who create display=$display failed: $e")
            dispose(display)
            -1L
        }
    }

    // Gives Rust the texture's current Surface; does nothing when Rust already has it.
    // Returns whether Rust has the Surface now.
    private fun attach(s: Slot): Boolean {
        return try {
            val surface = s.producer.surface
            if (!surface.isValid) {
                detach(s)
                return false
            }
            if (surface === s.surface) return true
            FFI.setVideoSurface(
                s.sid, s.display, surface, s.producer.id(),
                s.producer.width, s.producer.height, hevc, avc
            )
            s.surface = surface
            s.rendered = -1L
            s.dirty = false
            true
        } catch (e: Throwable) {
            Log.e(TAG, "$who attach display=${s.display} failed: $e")
            detach(s)
            false
        }
    }

    private fun detach(s: Slot) {
        if (s.surface == null) return
        s.surface = null
        try {
            FFI.setVideoSurface(s.sid, s.display, null, -1L, 0, 0, "", "")
        } catch (e: Throwable) {
            Log.e(TAG, "$who detach display=${s.display} failed: $e")
        }
    }

    fun dispose(display: Int) {
        val s = slots.remove(display) ?: return
        detach(s)
        try {
            s.producer.release()
        } catch (e: Throwable) {
            Log.e(TAG, "$who release display=$display failed: $e")
        }
        Log.d(TAG, "$who dispose display=$display")
    }

    /**
     * Before the engine drops the textures' buffers (onTrimMemory at background level): Rust
     * must not render into them any more. The textures come back with onSurfaceCreated or
     * reattach().
     */
    fun beforeTrim() {
        slots.values.forEach { detach(it) }
    }

    /**
     * The activity starts (again). Every stop of a Flutter activity drops the buffers of its
     * engine's textures (FlutterActivityAndFragmentDelegate.onStop trims the renderer itself,
     * past the activity's onTrimMemory; the textures answer with onSurfaceDestroyed), and
     * the engine reports them back only when the whole process resumes. A panel that was
     * hidden and shown again while another window stayed resumed would keep no Surface: so
     * each start gives Rust the textures' current Surfaces. An unchanged Surface does
     * nothing; after a stop it is a new one, and its decoder starts with a key frame the
     * video thread asks for.
     */
    fun reattach() {
        slots.values.toList().forEach { attach(it) }
    }

    /** Before the engine goes: the textures must be released while it is still attached. */
    fun disposeAll() {
        slots.keys.toList().forEach { dispose(it) }
        handler.removeCallbacks(tick)
        ticking = false
    }
}
