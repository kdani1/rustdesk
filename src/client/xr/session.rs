//! The XR thread: session, state machine and the (empty) frame loop.

use super::{egl::Egl, input::Input, instance, xlog_e, xlog_i, xlog_w, Control, StartArgs};
use jni::{
    objects::{GlobalRef, JObject, JValue},
    JNIEnv, JavaVM,
};
use openxr as xr;
use std::{
    os::raw::c_void,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

const REFRESH_HZ: f32 = 90.0;
/// After `onDestroy` asked to stop: how long the session may take to reach EXITING.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const STATS_EVERY: Duration = Duration::from_secs(10);

extern "C" {
    fn gettid() -> i32;
}

/// The thread body: attached to the JVM for its whole life (the loader, the runtime and
/// `onXrExit` all need it).
pub(super) fn thread_main(vm: JavaVM, activity: GlobalRef, args: StartArgs, ctl: Arc<Control>) {
    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(e) => {
            xlog_e(&format!("XR thread: attach to the JVM failed: {}", e));
            return;
        }
    };
    xlog_i(&format!("XR thread tid={}", unsafe { gettid() }));
    let reason = match run(&vm, &activity, &args, &ctl) {
        Ok(r) => r,
        Err(e) => {
            xlog_e(&format!("game mode failed: {}", e));
            format!("error: {}", e)
        }
    };
    xlog_i(&format!("XR session over: {}", reason));
    notify_exit(&mut env, &activity, &reason);
    drop(activity);
    drop(env);
}

/// `XrActivity.onXrExit(reason)`: posts to its UI thread and returns at once.
fn notify_exit(env: &mut JNIEnv, activity: &GlobalRef, reason: &str) {
    let r = (|| -> jni::errors::Result<()> {
        let s: JObject = env.new_string(reason)?.into();
        env.call_method(
            activity.as_obj(),
            "onXrExit",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&s)],
        )?;
        Ok(())
    })();
    if let Err(e) = r {
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        xlog_e(&format!("onXrExit: {}", e));
    }
}

fn err(what: &str, e: xr::sys::Result) -> String {
    format!("{}: {}", what, e)
}

fn run(
    vm: &JavaVM,
    activity: &GlobalRef,
    args: &StartArgs,
    ctl: &Control,
) -> Result<String, String> {
    let platform = unsafe {
        xr::AndroidPlatformInfo::new(
            vm.get_java_vm_pointer() as *mut c_void,
            activity.as_obj().as_raw() as *mut c_void,
        )
    };
    let xi = instance::create(&platform)?;
    let inst = &xi.instance;
    let system = instance::system(inst, &ctl.stop)?;
    // Before xrCreateSession, else XR_ERROR_GRAPHICS_REQUIREMENTS_CALL_MISSING (critique #9).
    let reqs = inst
        .graphics_requirements::<xr::OpenGlEs>(system)
        .map_err(|e| err("xrGetOpenGLESGraphicsRequirementsKHR", e))?;
    xlog_i(&format!(
        "GLES requirements: {} .. {}",
        reqs.min_api_version_supported, reqs.max_api_version_supported
    ));
    let egl = Egl::new()?;
    let (session, mut waiter, mut stream) = unsafe {
        inst.create_session::<xr::OpenGlEs>(
            system,
            &xr::opengles::SessionCreateInfo::Android {
                display: egl.display,
                config: egl.config,
                context: egl.context,
            },
        )
    }
    .map_err(|e| err("xrCreateSession", e))?;
    xlog_i(&format!(
        "session created for sid={} display={} {}x{} pad={}",
        args.sid, args.display, args.width, args.height, args.pad
    ));
    setup_session(&xi, &session);
    let space = session
        .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
        .map_err(|e| err("xrCreateReferenceSpace(LOCAL)", e))?;
    // Without the actions there is no exit chord, and Back may never reach an immersive
    // activity: fail now, back to the panel with an error, rather than trap the user in a
    // black view whose only way out is the system menu (risk 10).
    let mut input = Some(
        Input::new(inst)
            .and_then(|i| i.attach(&session).map(|_| i))
            .map_err(|e| err("controller actions", e))?,
    );

    let mut buf = xr::EventDataBuffer::new();
    let mut state = xr::SessionState::UNKNOWN;
    let mut running = false;
    let mut exit_requested = false;
    let mut stop_deadline: Option<Instant> = None;
    let mut reason = String::from("session exited");
    let mut frames = 0u64;
    let mut stats_at = Instant::now();

    'main: loop {
        // What the activity asked for.
        if ctl.stop.load(Ordering::SeqCst) && stop_deadline.is_none() {
            stop_deadline = Some(Instant::now() + STOP_TIMEOUT);
            reason = "activity destroyed".to_owned();
            if !running {
                break 'main;
            }
            if !exit_requested {
                exit_requested = true;
                if let Err(e) = session.request_exit() {
                    xlog_w(&err("xrRequestExitSession", e));
                    break 'main;
                }
            }
        }
        if ctl.exit.swap(false, Ordering::SeqCst) && !exit_requested {
            reason = "exit asked by the activity".to_owned();
            exit_requested = true;
            if !running {
                break 'main;
            }
            if let Err(e) = session.request_exit() {
                xlog_w(&err("xrRequestExitSession", e));
                break 'main;
            }
        }
        if let Some(d) = stop_deadline {
            if Instant::now() > d {
                xlog_w("the session did not reach EXITING in time: tearing down anyway");
                break 'main;
            }
        }

        while let Some(event) = inst
            .poll_event(&mut buf)
            .map_err(|e| err("xrPollEvent", e))?
        {
            use xr::Event::*;
            match event {
                SessionStateChanged(e) => {
                    state = e.state();
                    xlog_i(&format!(
                        "session state -> {:?} (activity {})",
                        state,
                        if ctl.resumed.load(Ordering::SeqCst) { "resumed" } else { "paused" }
                    ));
                    match state {
                        xr::SessionState::READY => {
                            session
                                .begin(xr::ViewConfigurationType::PRIMARY_STEREO)
                                .map_err(|e| err("xrBeginSession", e))?;
                            running = true;
                            perf_settings(&xi, &session);
                            if exit_requested {
                                if let Err(e) = session.request_exit() {
                                    xlog_w(&err("xrRequestExitSession", e));
                                }
                            }
                        }
                        xr::SessionState::STOPPING => {
                            if let Some(i) = input.as_mut() {
                                i.neutral("stopping");
                            }
                            session.end().map_err(|e| err("xrEndSession", e))?;
                            running = false;
                        }
                        xr::SessionState::EXITING => break 'main,
                        xr::SessionState::LOSS_PENDING => {
                            reason = "session loss pending".to_owned();
                            break 'main;
                        }
                        _ => {}
                    }
                }
                InstanceLossPending(_) => {
                    reason = "instance loss pending".to_owned();
                    break 'main;
                }
                EventsLost(e) => xlog_w(&format!("{} events lost", e.lost_event_count())),
                ReferenceSpaceChangePending(e) => xlog_i(&format!(
                    "reference space {:?} changes (recenter), pose valid {}",
                    e.reference_space_type(),
                    e.pose_valid()
                )),
                InteractionProfileChanged(_) => {
                    if let Some(i) = input.as_mut() {
                        i.profile_changed(&session);
                    }
                }
                DisplayRefreshRateChangedFB(e) => xlog_i(&format!(
                    "display refresh rate {} -> {} Hz",
                    e.from_display_refresh_rate(),
                    e.to_display_refresh_rate()
                )),
                PerfSettingsEXT(e) => xlog_w(&format!(
                    "perf notification {:?}/{:?}: {:?} -> {:?}",
                    e.domain(),
                    e.sub_domain(),
                    e.from_level(),
                    e.to_level()
                )),
                _ => {}
            }
        }

        if !running {
            // IDLE, or not started yet: no frame loop, xrWaitFrame would fail.
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }

        // Paced by the runtime (90 Hz asked). No layers yet: the user sees black.
        let fs = waiter.wait().map_err(|e| err("xrWaitFrame", e))?;
        stream.begin().map_err(|e| err("xrBeginFrame", e))?;
        let focused = state == xr::SessionState::FOCUSED;
        let chord_exit = input.as_mut().and_then(|i| i.frame(&session, focused));
        stream
            .end(
                fs.predicted_display_time,
                xr::EnvironmentBlendMode::OPAQUE,
                &[],
            )
            .map_err(|e| err("xrEndFrame", e))?;
        if let Some(why) = chord_exit {
            if !exit_requested {
                exit_requested = true;
                reason = why.to_owned();
                if let Err(e) = session.request_exit() {
                    xlog_w(&err("xrRequestExitSession", e));
                    break 'main;
                }
            }
        }
        frames += 1;
        let period_ns = fs.predicted_display_period.as_nanos();
        let elapsed = stats_at.elapsed();
        if elapsed >= STATS_EVERY {
            xlog_i(&format!(
                "frames {} in {:.1} s ({:.1} fps), period {:.2} ms, state {:?}",
                frames,
                elapsed.as_secs_f32(),
                frames as f32 / elapsed.as_secs_f32(),
                period_ns as f32 / 1e6,
                state
            ));
            frames = 0;
            stats_at = Instant::now();
        }
    }

    // Teardown, in order: actions, space, frame objects, session, EGL, instance.
    if let Some(mut i) = input.take() {
        i.neutral("exit");
    }
    drop(space);
    drop(stream);
    drop(waiter);
    drop(session);
    drop(egl);
    drop(xi);
    Ok(reason)
}

/// Thread priority, colour space and the 90 Hz request, right after xrCreateSession.
fn setup_session(xi: &instance::XrInstance, session: &xr::Session<xr::OpenGlEs>) {
    let inst = &xi.instance;
    if let Some(t) = inst.exts().khr_android_thread_settings.as_ref() {
        let tid = unsafe { gettid() } as u32;
        let r = unsafe {
            (t.set_android_application_thread)(
                session.as_raw(),
                xr::AndroidThreadTypeKHR::RENDERER_MAIN,
                tid,
            )
        };
        xlog_i(&format!("thread {} as RENDERER_MAIN: {}", tid, r));
    }
    if xi.enabled.fb_color_space {
        match session.enumerate_color_spaces() {
            Ok(list) => xlog_i(&format!("colour spaces: {:?}", list)),
            Err(e) => xlog_w(&err("xrEnumerateColorSpacesFB", e)),
        }
        match session.set_color_space(xr::ColorSpaceFB::REC709) {
            Ok(()) => xlog_i("colour space: Rec.709"),
            Err(e) => xlog_w(&err("xrSetColorSpaceFB(REC709)", e)),
        }
    }
    if xi.enabled.fb_display_refresh_rate {
        let rates = session.enumerate_display_refresh_rates().unwrap_or_default();
        let current = session.get_display_refresh_rate().unwrap_or(0.0);
        xlog_i(&format!("refresh rates {:?}, current {} Hz", rates, current));
        if rates.iter().any(|r| (r - REFRESH_HZ).abs() < 0.5) {
            match session.request_display_refresh_rate(REFRESH_HZ) {
                Ok(()) => xlog_i(&format!("refresh rate {} Hz requested", REFRESH_HZ)),
                Err(e) => xlog_w(&err("xrRequestDisplayRefreshRateFB", e)),
            }
        } else {
            xlog_w(&format!("{} Hz is not offered", REFRESH_HZ));
        }
    }
}

/// GPU at sustained-low; the CPU keeps the runtime's default, so the network and video
/// threads are not slowed down (critique #18). Measure in T7.
fn perf_settings(xi: &instance::XrInstance, session: &xr::Session<xr::OpenGlEs>) {
    if let Some(p) = xi.instance.exts().ext_performance_settings.as_ref() {
        let r = unsafe {
            (p.perf_settings_set_performance_level)(
                session.as_raw(),
                xr::PerfSettingsDomainEXT::GPU,
                xr::PerfSettingsLevelEXT::SUSTAINED_LOW,
            )
        };
        xlog_i(&format!("GPU performance level SUSTAINED_LOW: {}", r));
    }
}
