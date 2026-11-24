//! The smallest EGL setup an OpenXR OpenGL ES session accepts: the default display, an ES 3
//! context and a 1x1 pbuffer, current on the XR thread. Nothing is drawn with it in phase 1
//! (the frame loop submits no layers); the composited picture comes from a surface swapchain
//! in phase 2, which needs no GL either. Hand-written externs instead of a crate: eight
//! functions.
//!
//! The display is never terminated: it is the process-wide default display that Flutter and
//! the video decoders share. Only this context and its pbuffer are destroyed.

use std::os::raw::c_void;

type EGLDisplay = *mut c_void;
type EGLConfig = *mut c_void;
type EGLContext = *mut c_void;
type EGLSurface = *mut c_void;
type EGLint = i32;
type EGLBoolean = u32;

const EGL_DEFAULT_DISPLAY: *mut c_void = std::ptr::null_mut();
const EGL_NO_CONTEXT: EGLContext = std::ptr::null_mut();
const EGL_NO_SURFACE: EGLSurface = std::ptr::null_mut();
const EGL_TRUE: EGLBoolean = 1;
const EGL_NONE: EGLint = 0x3038;
const EGL_ALPHA_SIZE: EGLint = 0x3021;
const EGL_BLUE_SIZE: EGLint = 0x3022;
const EGL_GREEN_SIZE: EGLint = 0x3023;
const EGL_RED_SIZE: EGLint = 0x3024;
const EGL_SURFACE_TYPE: EGLint = 0x3033;
const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
const EGL_HEIGHT: EGLint = 0x3056;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
const EGL_PBUFFER_BIT: EGLint = 0x0001;
const EGL_OPENGL_ES3_BIT_KHR: EGLint = 0x0040;

#[link(name = "EGL")]
extern "C" {
    fn eglGetDisplay(display_id: *mut c_void) -> EGLDisplay;
    fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean;
    fn eglChooseConfig(
        dpy: EGLDisplay,
        attrib_list: *const EGLint,
        configs: *mut EGLConfig,
        config_size: EGLint,
        num_config: *mut EGLint,
    ) -> EGLBoolean;
    fn eglCreateContext(
        dpy: EGLDisplay,
        config: EGLConfig,
        share_context: EGLContext,
        attrib_list: *const EGLint,
    ) -> EGLContext;
    fn eglCreatePbufferSurface(
        dpy: EGLDisplay,
        config: EGLConfig,
        attrib_list: *const EGLint,
    ) -> EGLSurface;
    fn eglMakeCurrent(
        dpy: EGLDisplay,
        draw: EGLSurface,
        read: EGLSurface,
        ctx: EGLContext,
    ) -> EGLBoolean;
    fn eglDestroySurface(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
    fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean;
    fn eglGetError() -> EGLint;
}

/// An ES 3 context with a 1x1 pbuffer, current on the thread that created it. Must be dropped
/// on that thread, after the OpenXR session that uses it.
pub struct Egl {
    pub display: EGLDisplay,
    pub config: EGLConfig,
    pub context: EGLContext,
    surface: EGLSurface,
}

impl Egl {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
            if display.is_null() {
                return Err(format!("eglGetDisplay: 0x{:x}", eglGetError()));
            }
            let (mut major, mut minor) = (0, 0);
            if eglInitialize(display, &mut major, &mut minor) != EGL_TRUE {
                return Err(format!("eglInitialize: 0x{:x}", eglGetError()));
            }
            let attribs = [
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_ES3_BIT_KHR,
                EGL_SURFACE_TYPE,
                EGL_PBUFFER_BIT,
                EGL_RED_SIZE,
                8,
                EGL_GREEN_SIZE,
                8,
                EGL_BLUE_SIZE,
                8,
                EGL_ALPHA_SIZE,
                8,
                EGL_NONE,
            ];
            let mut config: EGLConfig = std::ptr::null_mut();
            let mut n = 0;
            if eglChooseConfig(display, attribs.as_ptr(), &mut config, 1, &mut n) != EGL_TRUE
                || n < 1
            {
                return Err(format!("eglChooseConfig: n={} 0x{:x}", n, eglGetError()));
            }
            let ctx_attribs = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
            let context = eglCreateContext(display, config, EGL_NO_CONTEXT, ctx_attribs.as_ptr());
            if context.is_null() {
                return Err(format!("eglCreateContext: 0x{:x}", eglGetError()));
            }
            let pb_attribs = [EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE];
            let surface = eglCreatePbufferSurface(display, config, pb_attribs.as_ptr());
            if surface.is_null() {
                let e = eglGetError();
                eglDestroyContext(display, context);
                return Err(format!("eglCreatePbufferSurface: 0x{:x}", e));
            }
            if eglMakeCurrent(display, surface, surface, context) != EGL_TRUE {
                let e = eglGetError();
                eglDestroySurface(display, surface);
                eglDestroyContext(display, context);
                return Err(format!("eglMakeCurrent: 0x{:x}", e));
            }
            super::xlog_i(&format!("EGL {}.{}: ES 3 context, 1x1 pbuffer", major, minor));
            Ok(Self {
                display,
                config,
                context,
                surface,
            })
        }
    }
}

impl Drop for Egl {
    fn drop(&mut self) {
        unsafe {
            eglMakeCurrent(self.display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
            eglDestroySurface(self.display, self.surface);
            eglDestroyContext(self.display, self.context);
        }
    }
}
