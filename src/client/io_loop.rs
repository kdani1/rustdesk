#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::clipboard::{clipboard_listener, update_clipboard, ClipboardSide};
#[cfg(not(any(target_os = "ios")))]
use crate::{audio_service, clipboard::CLIPBOARD_INTERVAL, ConnInner, CLIENT_SERVER};
use crate::{
    client::{
        self, new_voice_call_request, Client, Data, Interface, MediaData, MediaSender,
        QualityStatus, MILLI1, SEC30,
    },
    common::get_default_sound_input,
    ui_session_interface::{InvokeUiSession, Session},
};

// Empirical no-data window before exposing the restart reconnect state to the UI.
// Restart msgbox text is kept as a legacy UI fallback; Flutter handles the type as a control event.
const RESTART_REMOTE_DEVICE_NO_DATA_TIMEOUT: Duration = Duration::from_secs(5);
const KCP_CLOSE_REASON_FLUSH_DELAY: Duration = Duration::from_millis(30);
// Deadline for the parting close-reason send once the peer is presumed gone; KCP waits for send
// capacity with no deadline of its own.
const KCP_CLOSE_REASON_GONE_DEADLINE: Duration = Duration::from_millis(500);
// Grace after ICE reports Disconnected, which it does ~5s after it stops hearing from the peer,
// for ~8s in total. Disconnected is transient by design, so this waits out a Wi-Fi roam or a
// sleep/wake rather than acting on the first hint.
const WEBRTC_SUSPECT_GRACE: Duration = Duration::from_secs(3);
// KCP gets no such hint, only how long since a packet arrived; its endpoint pings an idle peer
// about every 2s, so this is several missed pings, and matches the 8s WebRTC arrives at.
const KCP_PEER_SILENCE_LIMIT: Duration = Duration::from_secs(8);
#[cfg(feature = "unix-file-copy-paste")]
use crate::{clipboard::try_empty_clipboard_files, clipboard_file::unix_file_clip};
use base::{
    config::keys,
    fs::{
        self, can_enable_overwrite_detection, get_job, get_string, new_send_confirm,
        DigestCheckResult, RemoveJobMeta,
    },
    message_proto::{permission_info::Permission, *},
};
#[cfg(any(
    target_os = "windows",
    all(target_os = "macos", feature = "unix-file-copy-paste")
))]
use clipboard::ContextSend;
use crossbeam_queue::ArrayQueue;
#[cfg(not(target_os = "ios"))]
use hbb_common::tokio::sync::mpsc::error::TryRecvError;
use hbb_common::{
    allow_err,
    config::{self, LocalConfig, PeerConfig, TransferSerde},
    get_time, log,
    protobuf::Message as _,
    rendezvous_proto::ConnType,
    timeout,
    tokio::{
        self,
        sync::mpsc,
        time::{self, Duration, Instant},
    },
    Stream,
};
#[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
use hbb_common::{tokio::sync::Mutex as TokioMutex, ResultType};
use scrap::CodecFormat;
use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    num::NonZeroI64,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
};

pub struct Remote<T: InvokeUiSession> {
    handler: Session<T>,
    audio_sender: MediaSender,
    receiver: mpsc::UnboundedReceiver<Data>,
    sender: mpsc::UnboundedSender<Data>,
    // Stop sending local audio to remote client.
    stop_voice_call_sender: Option<std::sync::mpsc::Sender<()>>,
    microphone_request: Option<(i64, Instant)>,
    microphone_active: bool,
    microphone_id: Option<i64>,
    voice_call_request_timestamp: Option<NonZeroI64>,
    read_jobs: Vec<fs::TransferJob>,
    write_jobs: Vec<fs::TransferJob>,
    remove_jobs: HashMap<i32, RemoveJob>,
    timer: crate::RustDeskInterval,
    last_update_jobs_status: (Instant, HashMap<i32, u64>),
    // Set after PeerInfo for this round, not when the transport connects.
    is_connected: bool,
    // Whether the scheduled initial snapshot may still be sent.
    // The connection loop clears this when handling its result or a live clipboard update.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    initial_clipboard_pending: bool,
    first_frame: bool,
    #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
    client_conn_id: i32, // used for file clipboard
    data_count: Arc<AtomicUsize>,
    video_format: CodecFormat,
    elevation_requested: bool,
    peer_info: ParsedPeerInfo,
    video_threads: HashMap<usize, VideoThread>,
    // Gentle fps control (`enable-gentle-fps-control`); when off, `fps_control` is the legacy one.
    gentle_fps: GentleFpsControl,
    chroma: Arc<RwLock<Option<Chroma>>>,
    last_record_state: bool,
    sent_close_reason: bool,
    cursor_dedupe: CursorDedupe,
    // Fork-only UDP video lane, shadow probe (docs/UDP-VIDEO.md).
    udp_video: crate::udp_video::viewer::ViewerLane,
}

#[derive(Default)]
struct ParsedPeerInfo {
    platform: String,
    is_installed: bool,
    idd_impl: String,
    support_view_camera: bool,
    support_terminal: bool,
}

impl ParsedPeerInfo {
    fn is_support_virtual_display(&self) -> bool {
        self.is_installed
            && self.platform == "Windows"
            && (self.idd_impl == "rustdesk_idd" || self.idd_impl == "amyuni_idd")
    }
}

impl<T: InvokeUiSession> Remote<T> {
    pub fn new(
        handler: Session<T>,
        receiver: mpsc::UnboundedReceiver<Data>,
        sender: mpsc::UnboundedSender<Data>,
    ) -> Self {
        Self {
            handler,
            audio_sender: crate::client::start_audio_thread(),
            receiver,
            sender,
            read_jobs: Vec::new(),
            write_jobs: Vec::new(),
            remove_jobs: Default::default(),
            timer: crate::rustdesk_interval(time::interval(SEC30)),
            last_update_jobs_status: (Instant::now(), Default::default()),
            is_connected: false,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            initial_clipboard_pending: false,
            first_frame: false,
            #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
            client_conn_id: 0,
            data_count: Arc::new(AtomicUsize::new(0)),
            video_format: CodecFormat::Unknown,
            stop_voice_call_sender: None,
            microphone_request: None,
            microphone_active: false,
            microphone_id: None,
            voice_call_request_timestamp: None,
            elevation_requested: false,
            peer_info: Default::default(),
            video_threads: Default::default(),
            gentle_fps: GentleFpsControl::new(gentle_fps_control_enabled()),
            chroma: Default::default(),
            last_record_state: false,
            sent_close_reason: false,
            cursor_dedupe: Default::default(),
            udp_video: Default::default(),
        }
    }

    pub async fn io_loop(&mut self, key: &str, token: &str, round: u32) {
        #[cfg(target_os = "windows")]
        let _file_clip_context_holder = {
            // `is_port_forward()` will not reach here, but we still check it for clarity.
            if self.handler.is_default() {
                // It is ok to call this function multiple times.
                ContextSend::enable(true);
                Some(crate::SimpleCallOnReturn {
                    b: true,
                    f: Box::new(|| {
                        // No need to call `enable(false)` for sciter version, because each client of sciter version is a new process.
                        // It's better to check if the peers are windows(support file copy&paste), but it's not necessary.
                        #[cfg(feature = "flutter")]
                        if !crate::flutter::sessions::has_sessions_running(ConnType::DEFAULT_CONN) {
                            ContextSend::enable(false);
                        };
                    }),
                })
            } else {
                None
            }
        };

        // Wi-Fi media streaming mode for this session round (released when io_loop returns).
        #[cfg(target_os = "windows")]
        let _wlan_hold = (self.handler.is_default() || self.handler.is_view_camera())
            .then(|| crate::platform::windows_wlan::hold(crate::platform::windows_wlan::Side::Viewer));
        // Viewer priority boost for this session round: this thread (it runs only this io_loop,
        // whichever path spawned it) and the counted process boost, both undone on return.
        #[cfg(target_os = "windows")]
        let _priority_boost =
            (self.handler.is_default() || self.handler.is_view_camera()).then(|| {
                (
                    crate::platform::windows::viewer_thread_boost("io_loop", None),
                    crate::platform::windows::viewer_process_boost(),
                )
            });
        let mut received = false;
        let conn_type = if self.handler.is_file_transfer() {
            ConnType::FILE_TRANSFER
        } else if self.handler.is_view_camera() {
            ConnType::VIEW_CAMERA
        } else if self.handler.is_terminal() {
            ConnType::TERMINAL
        } else {
            ConnType::default()
        };

        let connect_start = Instant::now();
        match Client::start(
            &self.handler.get_id(),
            key,
            token,
            conn_type,
            self.handler.clone(),
        )
        .await
        {
            Ok(((mut peer, direct, pk, kcp, stream_type), (feedback, rendezvous_server))) => {
                self.handler
                    .connection_round_state
                    .lock()
                    .unwrap()
                    .set_connected();
                let is_secured = peer.is_secured();
                // Only WebRTC needs refining: its label names the transport that won the race,
                // not the family ICE ended up nominating, and it is the one path where the two
                // can disagree with the address the rendezvous observed.
                let stream_type = if peer.webrtc_remote_ipv6().await.unwrap_or(false) {
                    "WebRTC/IPv6"
                } else {
                    stream_type
                };
                self.handler
                    .set_connection_type(is_secured, direct, stream_type); // flutter -> connection_ready
                log::info!(
                    "TRANSPORT viewer peer={} type={} direct={} secured={} kcp={} rendezvous={} connect_ms={}",
                    self.handler.get_id(),
                    stream_type,
                    direct,
                    is_secured,
                    kcp.is_some(),
                    rendezvous_server,
                    connect_start.elapsed().as_millis()
                );
                if !is_secured
                    && !crate::common::is_direct_ip_access(&self.handler.get_id())
                    && !client::confirm_insecure_connection(&self.handler, &mut self.receiver).await
                {
                    self.send_close_reason(&mut peer, "").await;
                    if kcp.is_some() {
                        tokio::time::sleep(KCP_CLOSE_REASON_FLUSH_DELAY).await;
                    }
                    self.handle_disconnected(round);
                    return;
                }
                self.handler.update_direct(Some(direct));
                if conn_type == ConnType::DEFAULT_CONN || conn_type == ConnType::VIEW_CAMERA {
                    self.handler
                        .set_fingerprint(crate::common::pk_to_fingerprint(pk.unwrap_or_default()));
                }

                // just build for now
                #[cfg(not(any(target_os = "windows", feature = "unix-file-copy-paste")))]
                let (_tx_holder, mut rx_clip_client) = mpsc::unbounded_channel::<i32>();

                #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
                let (_tx_holder, rx) = mpsc::unbounded_channel();
                #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
                let mut rx_clip_client_holder = (Arc::new(TokioMutex::new(rx)), None);
                #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
                {
                    if self.handler.is_default() {
                        (self.client_conn_id, rx_clip_client_holder.0) =
                            clipboard::get_rx_cliprdr_client(&self.handler.get_id());
                        log::debug!("get cliprdr client for conn_id {}", self.client_conn_id);
                        let client_conn_id = self.client_conn_id;
                        rx_clip_client_holder.1 = Some(crate::SimpleCallOnReturn {
                            b: true,
                            f: Box::new(move || {
                                clipboard::remove_channel_by_conn_id(client_conn_id);
                            }),
                        });
                    };
                }
                #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
                let mut rx_clip_client = rx_clip_client_holder.0.lock().await;

                let mut status_timer =
                    crate::rustdesk_interval(time::interval(Duration::new(1, 0)));
                let mut fps_instant = Instant::now();
                // A UDP video lane may be offered only on a direct, encrypted TCP session.
                let udp_local = (conn_type == ConnType::DEFAULT_CONN
                    && direct
                    && is_secured
                    && kcp.is_none()
                    && !peer.is_webrtc())
                .then(|| peer.local_addr());
                let mut udp_rx = self.udp_video.begin_round(&self.handler.get_id(), udp_local);

                let _keep_it = client::hc_connection(feedback, rendezvous_server, token).await;
                let mut last_recv_time = Instant::now();
                let mut webrtc_suspect_since: Option<Instant> = None;
                let mut last_rx_progress = peer.rx_progress();
                let mut peer_gone = false;

                loop {
                    tokio::select! {
                        res = peer.next() => {
                            if let Some(res) = res {
                                match res {
                                    Err(err) => {
                                        self.handler.on_establish_connection_error(err.to_string());
                                        break;
                                    }
                                    Ok(ref bytes) => {
                                        last_recv_time = Instant::now();
                                        // The UDP lane stamps a TCP video copy here, before it
                                        // is parsed, as it stamps a UDP copy before any parse.
                                        self.udp_video.note_tcp_rx(last_recv_time.into_std());
                                        if !received {
                                            received = true;
                                            self.handler.update_received(true);
                                        }
                                        self.data_count.fetch_add(bytes.len(), Ordering::Relaxed);
                                        if !self.handle_msg_from_peer(bytes, &mut peer).await {
                                            break
                                        }
                                        // An ACTIVE marker or a SwitchDisplay may have released
                                        // held UDP frames.
                                        self.feed_udp_video(&mut peer).await;
                                    }
                                }
                            } else {
                                if self.handler.is_restarting_remote_device() {
                                    log::info!("Restart remote device");
                                    self.handler.msgbox("restarting", "Restarting remote device", "Connection in progress. Please wait.", "");
                                } else {
                                    log::info!("Reset by the peer");
                                    self.handler.msgbox("error", "Connection Error", "Reset by the peer", "");
                                }
                                break;
                            }
                        }
                        d = self.receiver.recv() => {
                            if let Some(d) = d {
                                if !self.handle_msg_from_ui(d, &mut peer).await {
                                    break;
                                }
                            }
                        }
                        Some(ev) = udp_rx.recv() => {
                            if let Some(msg) = self.udp_video.on_event(ev) {
                                allow_err!(peer.send(&msg).await);
                            }
                            self.feed_udp_video(&mut peer).await;
                        }
                        _msg = rx_clip_client.recv() => {
                            #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
                            self.handle_local_clipboard_msg(&mut peer, _msg).await;
                        }
                        _ = self.timer.tick() => {
                            if last_recv_time.elapsed() >= SEC30 {
                                self.handler.msgbox("error", "Connection Error", "Timeout", "");
                                break;
                            }
                            if !self.read_jobs.is_empty() {
                                if let Err(err) = fs::handle_read_jobs(&mut self.read_jobs, &mut peer).await {
                                    self.handler.msgbox("error", "Connection Error", &err.to_string(), "");
                                    break;
                                }
                                self.update_jobs_status();
                            } else {
                                self.timer = crate::rustdesk_interval(time::interval_at(Instant::now() + SEC30, SEC30));
                            }
                        }
                        _ = status_timer.tick() => {
                            if self.microphone_request.as_ref().map(|(_, since)| {
                                let seconds = if self.peer_info.platform == "Windows" { 30 } else { 10 };
                                since.elapsed() > Duration::from_secs(seconds)
                            }).unwrap_or(false) {
                                self.microphone_request = None;
                                self.microphone_id = None;
                                self.stop_voice_call();
                                self.handler.on_microphone_forwarding("error", "Microphone startup timed out: check peer support and microphone permission");
                                allow_err!(peer.send(&crate::microphone_forwarding::message(0, false, false, "")).await);
                            }
                            if self.handler.is_restarting_remote_device()
                                && last_recv_time.elapsed() >= RESTART_REMOTE_DEVICE_NO_DATA_TIMEOUT
                            {
                                self.handler.msgbox("restarting-show", "Restarting remote device", "Connection in progress. Please wait.", "");
                                break;
                            }
                            let rx_progress = peer.rx_progress();
                            // `None` for transports that report none, and it never changes for a
                            // given one, so they are inert here.
                            let progressed = rx_progress != last_rx_progress;
                            last_rx_progress = rx_progress;
                            if peer.webrtc_disconnected() && !progressed {
                                webrtc_suspect_since.get_or_insert_with(Instant::now);
                            } else {
                                webrtc_suspect_since = None;
                            }
                            // Neither limit is a hard upper bound. A send is awaited inline in
                            // this loop, so one in progress delays this tick - bounded on WebRTC
                            // by the timeout the stream was built with, not bounded at all on
                            // KCP. The 30s watchdog above shares the loop and the same delay.
                            peer_gone = webrtc_suspect_since
                                .map_or(false, |since| since.elapsed() >= WEBRTC_SUSPECT_GRACE)
                                || kcp
                                    .as_ref()
                                    .and_then(|k| k.peer_silent_for())
                                    .map_or(false, |silent| silent >= KCP_PEER_SILENCE_LIMIT);
                            if peer_gone {
                                log::info!("Peer stopped answering, reconnecting");
                                #[cfg(feature = "flutter")]
                                self.handler.msgbox("restarting-show", "Connecting...", "Connection in progress. Please wait.", "");
                                // Sciter knows no `restarting-show` and would show a dialog that
                                // waits for a click, where the timeout this arrives ahead of is
                                // retryable and reconnects on its own. Keep that message for it.
                                #[cfg(not(feature = "flutter"))]
                                self.handler.msgbox("error", "Connection Error", "Timeout", "");
                                break;
                            }
                            if let Some(msg) = self.udp_video.tick() {
                                allow_err!(peer.send(&msg).await);
                            }
                            let elapsed = fps_instant.elapsed().as_millis();
                            if elapsed < 1000 {
                                continue;
                            }
                            fps_instant = Instant::now();
                            let mut speed = self.data_count.swap(0, Ordering::Relaxed);
                            speed = speed * 1000 / elapsed as usize;
                            let speed = format!("{:.2}kB/s", speed as f32 / 1024 as f32);

                            let fps = self.video_threads.iter().map(|(k, v)| {
                                // Correcting the inaccuracy of status_timer
                                (k.clone(), (*v.frame_count.read().unwrap() as i32) * 1000 / elapsed as i32)
                            }).collect::<HashMap<usize, i32>>();
                            self.video_threads.iter().for_each(|(_, v)| {
                                *v.frame_count.write().unwrap() = 0;
                            });
                            let show_quality_monitor = self.handler.lc.read().unwrap().get_toggle_option("show-quality-monitor");
                            let measuring = client::latency::is_measuring(show_quality_monitor);
                            // FPSCTL every tick only with enable-latency-log, not for the overlay alone.
                            let latency_log = measuring && client::latency::is_log_enabled();
                            let counts = self.video_threads.iter().map(|(k, v)| (*k, v.counters.take())).collect::<HashMap<_, _>>();
                            self.fps_control(direct, fps.clone(), &counts, elapsed, latency_log);
                            let chroma = self.chroma.read().unwrap().clone();
                            let chroma = match chroma {
                                Some(Chroma::I444) => "4:4:4",
                                Some(Chroma::I420) => "4:2:0",
                                None => "-",
                            };
                            let chroma = Some(chroma.to_string());
                            let codec_format = if self.video_format == CodecFormat::Unknown {
                                None
                            } else {
                                Some(self.video_format.clone())
                            };
                            let latency = client::latency::refresh_streams(
                                self.video_threads.iter().map(|(display, v)| (*display, client::latency::stream_key(&v.video_queue), counts.get(display).copied())),
                                Duration::from_millis(elapsed as u64),
                                stream_type,
                                measuring,
                            );
                            client::latency::refresh_input(client::latency::stream_key(&self.handler.sender), measuring, stream_type);
                            self.handler.update_quality_status(QualityStatus {
                                speed: Some(speed),
                                fps,
                                chroma,
                                codec_format,
                                latency,
                                ..Default::default()
                            });
                        }
                    }
                }
                log::debug!("Exit io_loop of id={}", self.handler.get_id());
                self.udp_video.end_round();
                client::latency::remove_input(client::latency::stream_key(&self.handler.sender));
                self.microphone_request = None;
                self.microphone_active = false;
                self.microphone_id = None;
                self.handler.on_microphone_forwarding("off", "");
                // Stop client audio server.
                if let Some(s) = self.stop_voice_call_sender.take() {
                    s.send(()).ok();
                }
                if kcp.is_some() {
                    // Attempted rather than skipped even here: if the loss was one-way the peer
                    // does get it, and drops its side instead of waiting out its own timeout.
                    if peer_gone {
                        peer.set_send_timeout(KCP_CLOSE_REASON_GONE_DEADLINE.as_millis() as u64);
                    }
                    // Send the close reason if it hasn't been sent yet, as KCP cannot detect the socket close event.
                    self.send_close_reason(&mut peer, "kcp").await;
                    // KCP does not send messages immediately, so wait to ensure the last message is sent.
                    // 1ms works in my test, but 30ms is more reliable.
                    tokio::time::sleep(KCP_CLOSE_REASON_FLUSH_DELAY).await;
                }
            }
            Err(err) => {
                self.handler.on_establish_connection_error(err.to_string());
            }
        }
        self.handle_disconnected(round);
    }

    fn handle_disconnected(&self, round: u32) {
        // set_disconnected_ok is used to check if new connection round is started.
        let _set_disconnected_ok = self
            .handler
            .connection_round_state
            .lock()
            .unwrap()
            .set_disconnected(round);

        #[cfg(not(target_os = "ios"))]
        if self.handler.is_default() && _set_disconnected_ok {
            // Other sessions may keep the listener running after this one disconnects.
            #[cfg(feature = "flutter")]
            crate::flutter::update_text_clipboard_required();
            #[cfg(all(feature = "flutter", feature = "unix-file-copy-paste"))]
            crate::flutter::update_file_clipboard_required();
            Client::try_stop_clipboard();
        }

        #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
        if self.handler.is_default() && _set_disconnected_ok {
            // Linux client cleanup runs synchronously in try_stop_clipboard() before FUSE is
            // unmounted. Keep this async path for other file-clipboard platforms.
            crate::clipboard::try_empty_clipboard_files(ClipboardSide::Client, self.client_conn_id);
        }
    }

    #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
    async fn handle_local_clipboard_msg(
        &mut self,
        peer: &mut Stream,
        msg: Option<clipboard::ClipboardFile>,
    ) {
        match msg {
            Some(clip) => match clip {
                clipboard::ClipboardFile::NotifyCallback {
                    r#type,
                    title,
                    text,
                } => {
                    self.handler.msgbox(&r#type, &title, &text, "");
                }
                // File-data responses bypass is_stopping_allowed, but still require login.
                _ if !self.is_connected => {
                    log::debug!("Discarding local file clipboard message before login");
                }
                _ => {
                    let is_stopping_allowed = clip.is_stopping_allowed();
                    let server_file_transfer_enabled =
                        *self.handler.server_file_transfer_enabled.read().unwrap();
                    let file_transfer_enabled =
                        self.handler.lc.read().unwrap().enable_file_copy_paste.v;
                    let view_only = self.handler.lc.read().unwrap().view_only.v;
                    let stop = is_stopping_allowed
                        && (view_only || !(server_file_transfer_enabled && file_transfer_enabled));
                    log::debug!(
                        "Process clipboard message from system, view_only: {}, stop: {}, is_stopping_allowed: {}, server_file_transfer_enabled: {}, file_transfer_enabled: {}",
                        view_only, stop, is_stopping_allowed, server_file_transfer_enabled, file_transfer_enabled
                    );
                    if stop {
                        #[cfg(target_os = "windows")]
                        {
                            ContextSend::set_is_stopped();
                        }
                    } else {
                        #[cfg(target_os = "windows")]
                        if let Err(e) = ContextSend::make_sure_enabled() {
                            log::error!("failed to restart clipboard context: {}", e);
                            // to-do: Show msgbox with "Don't show again" option
                        };
                        log::debug!("Send system clipboard message to remote");
                        #[cfg(not(any(target_os = "android", target_os = "ios")))]
                        if matches!(&clip, clipboard::ClipboardFile::FormatList { .. }) {
                            self.initial_clipboard_pending = false;
                        }
                        let msg = crate::clipboard_file::clip_2_msg(clip);
                        allow_err!(peer.send(&msg).await);
                    }
                }
            },
            None => {
                // unreachable!()
            }
        }
    }

    fn handle_job_status(&mut self, id: i32, file_num: i32, err: Option<String>) {
        if let Some(job) = self.remove_jobs.get_mut(&id) {
            if job.no_confirm {
                let file_num = (file_num + 1) as usize;
                if file_num < job.files.len() {
                    let path = format!("{}{}{}", job.path, job.sep, job.files[file_num].name);
                    self.sender
                        .send(Data::RemoveFile((id, path, file_num as i32, job.is_remote)))
                        .ok();
                    let elapsed = job.last_update_job_status.elapsed().as_millis() as i32;
                    if elapsed >= 1000 {
                        job.last_update_job_status = Instant::now();
                    } else {
                        return;
                    }
                } else {
                    self.remove_jobs.remove(&id);
                }
            }
        }
        if let Some(err) = err {
            self.handler.job_error(id, err, file_num);
        } else {
            self.handler.job_done(id, file_num);
        }
    }

    fn stop_voice_call(&mut self) {
        let voice_call_sender = std::mem::replace(&mut self.stop_voice_call_sender, None);
        if let Some(stopper) = voice_call_sender {
            let _ = stopper.send(());
        }
    }

    // Start a voice call recorder, records audio and send to remote
    fn start_voice_call(&mut self) -> Option<std::sync::mpsc::Sender<()>> {
        if self.handler.is_file_transfer()
            || self.handler.is_port_forward()
            || self.handler.is_terminal()
        {
            return None;
        }
        // iOS does not have this server.
        #[cfg(not(any(target_os = "ios")))]
        {
            // NOTE:
            // The client server and --server both use the same sound input device.
            // It's better to distinguish the server side and client side.
            // But it' not necessary for now, because it's not a common case.
            // And it is immediately known when the input device is changed.
            crate::audio_service::set_voice_call_input_device(get_default_sound_input(), false);
            // Create a channel to receive error or closed message
            let (tx, rx) = std::sync::mpsc::channel();
            let (tx_audio_data, mut rx_audio_data) =
                hbb_common::tokio::sync::mpsc::unbounded_channel();
            // Create a stand-alone inner, add subscribe to audio service
            let conn_id = CLIENT_SERVER.write().unwrap().get_new_id();
            let client_conn_inner = ConnInner::new(conn_id.clone(), Some(tx_audio_data), None);
            // now we subscribe
            CLIENT_SERVER.write().unwrap().subscribe(
                audio_service::NAME,
                client_conn_inner.clone(),
                true,
            );
            let tx_audio = self.sender.clone();
            std::thread::spawn(move || {
                loop {
                    // check if client is closed
                    match rx.try_recv() {
                        Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            log::debug!("Exit voice call audio service of client");
                            // unsubscribe
                            CLIENT_SERVER.write().unwrap().subscribe(
                                audio_service::NAME,
                                client_conn_inner,
                                false,
                            );
                            crate::audio_service::set_voice_call_input_device(None, true);
                            break;
                        }
                        _ => {}
                    }
                    match rx_audio_data.try_recv() {
                        Ok((_instant, msg)) => match &msg.union {
                            Some(message::Union::AudioFrame(frame)) => {
                                let mut msg = Message::new();
                                msg.set_audio_frame(frame.clone());
                                tx_audio.send(Data::Message(msg)).ok();
                            }
                            Some(message::Union::Misc(misc)) => {
                                let mut msg = Message::new();
                                msg.set_misc(misc.clone());
                                tx_audio.send(Data::Message(msg)).ok();
                            }
                            _ => {}
                        },
                        Err(err) => {
                            if err == TryRecvError::Empty {
                                // ignore
                            } else {
                                log::debug!("Failed to record local audio channel: {}", err);
                            }
                            // Both arms fall through with nothing else in this loop blocking, so
                            // without a pause the thread spun a core for the whole voice call.
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                }
            });
            return Some(tx);
        }
        #[cfg(target_os = "ios")]
        {
            None
        }
    }

    fn start_microphone_capture(&mut self, request_id: i64) -> Option<std::sync::mpsc::Sender<()>> {
        if self.handler.is_file_transfer()
            || self.handler.is_port_forward()
            || self.handler.is_terminal()
        {
            return None;
        }
        // iOS does not have this server.
        #[cfg(not(any(target_os = "ios")))]
        {
            // NOTE:
            // The client server and --server both use the same sound input device.
            // It's better to distinguish the server side and client side.
            // But it' not necessary for now, because it's not a common case.
            // And it is immediately known when the input device is changed.
            let input = get_default_sound_input();
            #[cfg(not(target_os = "android"))]
            if input.is_none() { return None; }
            let capture_lease = crate::microphone_forwarding::CaptureLease::acquire(input)?;
            // Create a channel to receive error or closed message
            let (tx, rx) = std::sync::mpsc::channel();
            let (tx_audio_data, mut rx_audio_data) =
                hbb_common::tokio::sync::mpsc::unbounded_channel();
            // Create a stand-alone inner, add subscribe to audio service
            let conn_id = CLIENT_SERVER.write().unwrap().get_new_id();
            let client_conn_inner = ConnInner::new(conn_id.clone(), Some(tx_audio_data), None);
            // now we subscribe
            CLIENT_SERVER.write().unwrap().subscribe(
                audio_service::NAME,
                client_conn_inner.clone(),
                true,
            );
            let tx_audio = self.sender.clone();
            std::thread::spawn(move || {
                let _capture_lease = capture_lease;
                loop {
                    // check if client is closed
                    match rx.try_recv() {
                        Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            log::debug!("Exit voice call audio service of client");
                            // unsubscribe
                            CLIENT_SERVER.write().unwrap().subscribe(
                                audio_service::NAME,
                                client_conn_inner,
                                false,
                            );
                            break;
                        }
                        _ => {}
                    }
                    match rx_audio_data.try_recv() {
                        Ok((_instant, msg)) => match &msg.union {
                            Some(message::Union::AudioFrame(frame)) => {
                                let mut msg = Message::new();
                                msg.set_audio_frame(frame.clone());
                                tx_audio.send(Data::MicrophoneMessage(request_id, msg)).ok();
                            }
                            Some(message::Union::Misc(misc)) => {
                                let mut msg = Message::new();
                                msg.set_misc(misc.clone());
                                tx_audio.send(Data::MicrophoneMessage(request_id, msg)).ok();
                            }
                            _ => {}
                        },
                        Err(err) => {
                            if err == TryRecvError::Empty {
                                // ignore
                            } else {
                                log::debug!("Failed to record local audio channel: {}", err);
                            }
                            // Both arms fall through with nothing else in this loop blocking, so
                            // without a pause the thread spun a core for the whole voice call.
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                }
            });
            return Some(tx);
        }
        #[cfg(target_os = "ios")]
        {
            None
        }
    }

    async fn send_close_reason(&mut self, peer: &mut Stream, reason: &str) {
        if self.sent_close_reason {
            return;
        }
        let mut misc = Misc::new();
        misc.set_close_reason(reason.to_owned());
        let mut msg = Message::new();
        msg.set_misc(misc);
        allow_err!(peer.send(&msg).await);
        self.sent_close_reason = true;
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn start_initial_clipboard_sync(&mut self) {
        let peer_info = {
            let lc = self.handler.lc.read().unwrap();
            lc.peer_info
                .as_ref()
                .map(|pi| (pi.version.clone(), pi.platform.clone()))
        };
        let Some((peer_version, peer_platform)) = peer_info else {
            log::error!("Cannot start initial clipboard sync without peer information");
            return;
        };

        self.initial_clipboard_pending = true;
        let sender = self.sender.clone();
        let permission_config = self.handler.get_permission_config();
        // Clipboard access and encoding must not block the connection loop.
        let read_clipboard = move || {
            // Capture after readiness, before reading; later changes invalidate this snapshot.
            let generation = clipboard_listener::current_generation();
            let msg_out = if permission_config.is_text_clipboard_required() {
                crate::clipboard::get_current_clipboard_msg(
                    &peer_version,
                    &peer_platform,
                    crate::clipboard::ClipboardSide::Client,
                )
            } else {
                None
            };
            let msg_out = msg_out.filter(|_| permission_config.is_text_clipboard_required());
            // Empty or failed reads must also finish the pending initial-sync attempt.
            if let Err(err) = sender.send(Data::InitialClipboard(generation, msg_out)) {
                log::debug!("Failed to send initial clipboard result: {}", err);
            }
        };
        #[cfg(target_os = "linux")]
        self.spawn_initial_clipboard_read_after_ready(read_clipboard);
        #[cfg(not(target_os = "linux"))]
        tokio::task::spawn_blocking(read_clipboard);
    }

    // Linux listener creation returns before X11/Wayland has subscribed to changes.
    // Capture after readiness so changes during the read can invalidate the snapshot.
    // A separate task keeps a stalled backend from blocking the connection loop.
    #[cfg(target_os = "linux")]
    fn spawn_initial_clipboard_read_after_ready(&self, read_clipboard: impl FnOnce() + Send + 'static) {
        // Initial-sync wait budget, not a protocol-defined startup deadline.
        const CLIPBOARD_READY_TIMEOUT: Duration = Duration::from_secs(5);

        let sender = self.sender.clone();
        tokio::spawn(async move {
            let ready = tokio::select! {
                result = time::timeout(CLIPBOARD_READY_TIMEOUT, clipboard_listener::wait_for_ready()) => {
                    match result {
                        Ok(ready) => ready,
                        Err(err) => Err(err.into()),
                    }
                }
                _ = sender.closed() => return,
            };
            if let Err(err) = ready {
                log::error!(
                    "Failed to wait for clipboard listener readiness (limit {:?}): {}",
                    CLIPBOARD_READY_TIMEOUT,
                    err,
                );
                let generation = clipboard_listener::current_generation();
                if let Err(err) = sender.send(Data::InitialClipboard(generation, None)) {
                    log::debug!("Failed to send initial clipboard result: {}", err);
                }
                return;
            }
            tokio::task::spawn_blocking(read_clipboard);
        });
    }

    async fn handle_msg_from_ui(&mut self, data: Data, peer: &mut Stream) -> bool {
        match data {
            Data::Close => {
                self.send_close_reason(peer, "").await;
                return false;
            }
            Data::Login((os_username, os_password, password, remember)) => {
                self.handler
                    .handle_login_from_ui(os_username, os_password, password, remember, peer)
                    .await;
            }
            #[cfg(all(target_os = "windows", not(feature = "flutter")))]
            Data::ToggleClipboardFile => {
                self.check_clipboard_file_context();
            }
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            Data::InitialClipboard(generation, msg) => {
                // A live update supersedes any initial snapshot still being prepared.
                if !self.initial_clipboard_pending {
                    return true;
                }
                self.initial_clipboard_pending = false;
                if !self.handler.is_text_clipboard_required() {
                    return true;
                }
                if generation != clipboard_listener::current_generation() {
                    // A clipboard change or listener restart can invalidate the snapshot
                    // without sending a live update that supersedes it.
                    drop(msg);
                    self.start_initial_clipboard_sync();
                    return true;
                }
                if let Some(msg) = msg {
                    allow_err!(peer.send(&msg).await);
                }
            }
            Data::Message(msg) => {
                // The Flutter clipboard broadcast is process-wide, so a clipboard can reach this
                // round's queue before the round has logged in; it is dropped here, on the round
                // itself.
                #[cfg(feature = "flutter")]
                if !self.is_connected
                    && matches!(
                        msg.union.as_ref(),
                        Some(message::Union::Clipboard(_))
                            | Some(message::Union::MultiClipboards(_))
                    )
                {
                    return true;
                }
                match &msg.union {
                    Some(message::Union::Cliprdr(_)) if !self.is_connected => {
                        log::debug!("Discarding outgoing file clipboard message before login");
                        return true;
                    }
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    Some(message::Union::Clipboard(_)) | Some(message::Union::MultiClipboards(_)) => {
                        self.initial_clipboard_pending = false;
                    }
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    Some(message::Union::Cliprdr(clip))
                        if matches!(clip.union.as_ref(), Some(cliprdr::Union::FormatList(_))) =>
                    {
                        self.initial_clipboard_pending = false;
                    }
                    Some(message::Union::Misc(misc)) => match misc.union {
                        Some(misc::Union::RefreshVideo(_)) => {
                            self.video_threads.iter().for_each(|(_, v)| {
                                *v.discard_queue.write().unwrap() = true;
                            });
                        }
                        Some(misc::Union::RefreshVideoDisplay(display)) => {
                            if let Some(v) = self.video_threads.get_mut(&(display as usize)) {
                                *v.discard_queue.write().unwrap() = true;
                            }
                        }
                        Some(misc::Union::Option(ref o)) if o.custom_fps > 0 => {
                            if self.gentle_fps.on_custom_fps_sent(o.custom_fps as usize) {
                                self.handler.lc.write().unwrap().last_auto_fps = None;
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
                allow_err!(peer.send(&msg).await);
                client::latency::on_input_written(
                    client::latency::stream_key(&self.handler.sender),
                    &msg,
                );
            }
            Data::SendFiles((id, r#type, path, to, file_num, include_hidden, is_remote)) => {
                log::info!("send files, is remote {}", is_remote);
                let od = can_enable_overwrite_detection(self.handler.lc.read().unwrap().version);
                if is_remote {
                    log::debug!("New job {}, write to {} from remote {}", id, to, path);
                    let to = match r#type {
                        fs::JobType::Generic => fs::DataSource::FilePath(PathBuf::from(&to)),
                        fs::JobType::Printer => {
                            fs::DataSource::MemoryCursor(std::io::Cursor::new(Vec::new()))
                        }
                    };
                    self.write_jobs.push(fs::TransferJob::new_write(
                        id,
                        r#type,
                        path.clone(),
                        to,
                        file_num,
                        include_hidden,
                        is_remote,
                        od,
                    ));
                    allow_err!(
                        peer.send(&fs::new_send(id, r#type, path, file_num, include_hidden))
                            .await
                    );
                } else {
                    match fs::TransferJob::new_read(
                        id,
                        r#type,
                        to.clone(),
                        fs::DataSource::FilePath(PathBuf::from(&path)),
                        file_num,
                        include_hidden,
                        is_remote,
                        od,
                    ) {
                        Err(err) => {
                            self.handle_job_status(id, -1, Some(err.to_string()));
                        }
                        Ok(job) => {
                            log::debug!(
                                "New job {}, read {} to remote {}, {} files",
                                id,
                                path,
                                to,
                                job.files().len()
                            );
                            self.handler.update_folder_files(
                                job.id(),
                                job.files(),
                                path,
                                !is_remote,
                                true,
                            );
                            #[cfg(not(windows))]
                            let files = job.files().clone();
                            #[cfg(windows)]
                            let mut files = job.files().clone();
                            #[cfg(windows)]
                            if self.handler.peer_platform() != "Windows" {
                                // peer is not windows, need transform \ to /
                                fs::transform_windows_path(&mut files);
                            }
                            let total_size = job.total_size();
                            self.read_jobs.push(job);
                            self.timer = crate::rustdesk_interval(time::interval(MILLI1));
                            allow_err!(
                                peer.send(&fs::new_receive(id, to, file_num, files, total_size))
                                    .await
                            );
                        }
                    }
                }
            }
            Data::AddJob((id, r#type, path, to, file_num, include_hidden, is_remote)) => {
                let od = can_enable_overwrite_detection(self.handler.lc.read().unwrap().version);
                if is_remote {
                    log::debug!(
                        "new write waiting job {}, write to {} from remote {}",
                        id,
                        to,
                        path
                    );
                    let mut job = fs::TransferJob::new_write(
                        id,
                        r#type,
                        path.clone(),
                        fs::DataSource::FilePath(PathBuf::from(&to)),
                        file_num,
                        include_hidden,
                        is_remote,
                        od,
                    );
                    job.is_last_job = true;
                    self.write_jobs.push(job);
                } else {
                    match fs::TransferJob::new_read(
                        id,
                        r#type,
                        to.clone(),
                        fs::DataSource::FilePath(PathBuf::from(&path)),
                        file_num,
                        include_hidden,
                        is_remote,
                        od,
                    ) {
                        Err(err) => {
                            self.handle_job_status(id, -1, Some(err.to_string()));
                        }
                        Ok(mut job) => {
                            log::debug!(
                                "new read waiting job {}, read {} to remote {}, {} files",
                                id,
                                path,
                                to,
                                job.files().len()
                            );
                            self.handler.update_folder_files(
                                job.id(),
                                job.files(),
                                path,
                                !is_remote,
                                true,
                            );
                            job.is_last_job = true;
                            self.read_jobs.push(job);
                            self.timer = crate::rustdesk_interval(time::interval(MILLI1));
                        }
                    }
                }
            }
            Data::ResumeJob((id, is_remote)) => {
                if is_remote {
                    if let Some(job) = get_job(id, &mut self.write_jobs) {
                        job.is_last_job = false;
                        job.is_resume = true;
                        allow_err!(
                            peer.send(&fs::new_send(
                                id,
                                fs::JobType::Generic,
                                job.remote.clone(),
                                job.file_num,
                                job.show_hidden
                            ))
                            .await
                        );
                    }
                } else {
                    if let Some(job) = get_job(id, &mut self.read_jobs) {
                        match &job.data_source {
                            fs::DataSource::FilePath(_p) => {
                                job.is_last_job = false;
                                job.is_resume = true;
                                job.set_finished_size_on_resume();
                                #[cfg(not(windows))]
                                let files = job.files().clone();
                                #[cfg(windows)]
                                let mut files = job.files().clone();
                                #[cfg(windows)]
                                if self.handler.peer_platform() != "Windows" {
                                    // peer is not windows, need transform \ to /
                                    fs::transform_windows_path(&mut files);
                                }
                                allow_err!(
                                    peer.send(&fs::new_receive(
                                        id,
                                        job.remote.clone(),
                                        job.file_num,
                                        files,
                                        job.total_size(),
                                    ))
                                    .await
                                );
                            }
                            fs::DataSource::MemoryCursor(_) => {
                                // unreachable!()
                                log::error!("Resume job with memory cursor");
                            }
                        }
                    }
                }
            }
            Data::SetNoConfirm(id) => {
                if let Some(job) = self.remove_jobs.get_mut(&id) {
                    job.no_confirm = true;
                }
            }
            Data::ConfirmDeleteFiles((id, file_num)) => {
                if let Some(job) = self.remove_jobs.get_mut(&id) {
                    let i = file_num as usize;
                    if i < job.files.len() {
                        self.handler.ui_handler.confirm_delete_files(
                            id,
                            file_num,
                            job.files[i].name.clone(),
                        );
                    }
                }
            }
            Data::SetConfirmOverrideFile((id, file_num, need_override, remember, is_upload)) => {
                if is_upload {
                    if let Some(job) = fs::get_job(id, &mut self.read_jobs) {
                        if remember {
                            job.set_overwrite_strategy(Some(need_override));
                        }
                        job.confirm(&FileTransferSendConfirmRequest {
                            id,
                            file_num,
                            union: if need_override {
                                Some(file_transfer_send_confirm_request::Union::OffsetBlk(0))
                            } else {
                                Some(file_transfer_send_confirm_request::Union::Skip(true))
                            },
                            ..Default::default()
                        })
                        .await;
                    }
                } else {
                    if let Some(job) = fs::get_job(id, &mut self.write_jobs) {
                        if remember {
                            job.set_overwrite_strategy(Some(need_override));
                        }
                        let mut msg = Message::new();
                        let mut file_action = FileAction::new();
                        let req = FileTransferSendConfirmRequest {
                            id,
                            file_num,
                            union: if need_override {
                                Some(file_transfer_send_confirm_request::Union::OffsetBlk(0))
                            } else {
                                Some(file_transfer_send_confirm_request::Union::Skip(true))
                            },
                            ..Default::default()
                        };
                        job.confirm(&req).await;
                        file_action.set_send_confirm(req);
                        msg.set_file_action(file_action);
                        allow_err!(peer.send(&msg).await);
                    }
                }
            }
            Data::RemoveDirAll((id, path, is_remote, include_hidden)) => {
                let sep = self.handler.get_path_sep(is_remote);
                if is_remote {
                    let mut msg_out = Message::new();
                    let mut file_action = FileAction::new();
                    file_action.set_all_files(ReadAllFiles {
                        id,
                        path: path.clone(),
                        include_hidden,
                        ..Default::default()
                    });
                    msg_out.set_file_action(file_action);
                    allow_err!(peer.send(&msg_out).await);
                    self.remove_jobs
                        .insert(id, RemoveJob::new(Vec::new(), path, sep, is_remote));
                } else {
                    match fs::get_recursive_files(&path, include_hidden) {
                        Ok(entries) => {
                            self.handler.update_folder_files(
                                id,
                                &entries,
                                path.clone(),
                                !is_remote,
                                false,
                            );
                            self.remove_jobs
                                .insert(id, RemoveJob::new(entries, path, sep, is_remote));
                        }
                        Err(err) => {
                            self.handle_job_status(id, -1, Some(err.to_string()));
                        }
                    }
                }
            }
            Data::CancelJob(id) => {
                self.cancel_transfer_job(id, peer).await;
            }
            Data::RemoveDir((id, path)) => {
                let mut msg_out = Message::new();
                let mut file_action = FileAction::new();
                file_action.set_remove_dir(FileRemoveDir {
                    id,
                    path,
                    recursive: true,
                    ..Default::default()
                });
                msg_out.set_file_action(file_action);
                allow_err!(peer.send(&msg_out).await);
            }
            Data::RemoveFile((id, path, file_num, is_remote)) => {
                if is_remote {
                    let mut msg_out = Message::new();
                    let mut file_action = FileAction::new();
                    file_action.set_remove_file(FileRemoveFile {
                        id,
                        path,
                        file_num,
                        ..Default::default()
                    });
                    msg_out.set_file_action(file_action);
                    allow_err!(peer.send(&msg_out).await);
                } else {
                    match fs::remove_file(&path) {
                        Err(err) => {
                            self.handle_job_status(id, file_num, Some(err.to_string()));
                        }
                        Ok(()) => {
                            self.handle_job_status(id, file_num, None);
                        }
                    }
                }
            }
            Data::CreateDir((id, path, is_remote)) => {
                if is_remote {
                    let mut msg_out = Message::new();
                    let mut file_action = FileAction::new();
                    file_action.set_create(FileDirCreate {
                        id,
                        path,
                        ..Default::default()
                    });
                    msg_out.set_file_action(file_action);
                    allow_err!(peer.send(&msg_out).await);
                } else {
                    match fs::create_dir(&path) {
                        Err(err) => {
                            self.handle_job_status(id, -1, Some(err.to_string()));
                        }
                        Ok(()) => {
                            self.handle_job_status(id, -1, None);
                        }
                    }
                }
            }
            Data::RenameFile((id, path, new_name, is_remote)) => {
                if is_remote {
                    let mut msg_out = Message::new();
                    let mut file_action = FileAction::new();
                    file_action.set_rename(FileRename {
                        id,
                        path,
                        new_name,
                        ..Default::default()
                    });
                    msg_out.set_file_action(file_action);
                    allow_err!(peer.send(&msg_out).await);
                } else {
                    let err = fs::rename_file(&path, &new_name)
                        .err()
                        .map(|e| e.to_string());
                    self.handle_job_status(id, -1, err);
                }
            }
            Data::RecordScreen(start) => {
                self.handler.lc.write().unwrap().record_state = start;
                self.update_record_state();
            }
            Data::ElevateDirect => {
                let mut request = ElevationRequest::new();
                request.set_direct(true);
                let mut misc = Misc::new();
                misc.set_elevation_request(request);
                let mut msg = Message::new();
                msg.set_misc(misc);
                allow_err!(peer.send(&msg).await);
                self.elevation_requested = true;
            }
            Data::ElevateWithLogon(username, password) => {
                let mut request = ElevationRequest::new();
                request.set_logon(ElevationRequestWithLogon {
                    username,
                    password,
                    ..Default::default()
                });
                let mut misc = Misc::new();
                misc.set_elevation_request(request);
                let mut msg = Message::new();
                msg.set_misc(misc);
                allow_err!(peer.send(&msg).await);
                self.elevation_requested = true;
            }
            Data::MicrophoneMessage(id, data) => {
                if self.microphone_id == Some(id) && self.stop_voice_call_sender.is_some() {
                    let mut packet = MicrophonePacket { request_id: id, ..Default::default() };
                    match data.union {
                        Some(message::Union::AudioFrame(frame)) => packet.set_frame(frame),
                        Some(message::Union::Misc(misc)) => match misc.union {
                            Some(misc::Union::AudioFormat(format)) => packet.set_format(format),
                            _ => return true,
                        },
                        _ => return true,
                    }
                    let mut message = Message::new();
                    message.set_microphone_packet(packet);
                    allow_err!(peer.send(&message).await);
                }
            }
            Data::ToggleMicrophone => {
                if self.microphone_active || self.microphone_request.is_some() {
                    self.stop_voice_call();
                    self.microphone_active = false;
                    self.microphone_id = None;
                    self.microphone_request = None;
                    allow_err!(peer.send(&crate::microphone_forwarding::message(0, false, false, "")).await);
                    self.handler.on_microphone_forwarding("off", "");
                } else if self.stop_voice_call_sender.is_some() || self.voice_call_request_timestamp.is_some() {
                    self.handler.on_microphone_forwarding("error", "End the voice call before forwarding the microphone");
                } else if self.handler.is_default() {
                    let id = get_time();
                    self.microphone_request = Some((id, Instant::now()));
                    self.microphone_id = Some(id);
                    self.handler.on_microphone_forwarding("pending", "");
                    allow_err!(peer.send(&crate::microphone_forwarding::message(id, true, false, "")).await);
                }
            }
            Data::NewVoiceCall => {
                if crate::microphone_forwarding::capture_in_use() || self.microphone_request.is_some() {
                    self.handler.on_voice_call_closed("Stop microphone forwarding first");
                    return true;
                }
                let msg = new_voice_call_request(true);
                // Save the voice call request timestamp for the further validation.
                self.voice_call_request_timestamp = Some(
                    NonZeroI64::new(msg.voice_call_request().req_timestamp)
                        .unwrap_or(NonZeroI64::new(get_time()).unwrap()),
                );
                allow_err!(peer.send(&msg).await);
                self.handler.on_voice_call_waiting();
            }
            Data::CloseVoiceCall => {
                if self.microphone_id.is_some() { return true; }
                self.stop_voice_call();
                let msg = new_voice_call_request(false);
                self.handler
                    .on_voice_call_closed("Closed manually by the peer");
                allow_err!(peer.send(&msg).await);
            }
            Data::ResetDecoder(display) => match display {
                Some(display) => {
                    if let Some(v) = self.video_threads.get_mut(&display) {
                        v.video_sender.send(MediaData::Reset).ok();
                    }
                }
                None => {
                    for (_, v) in self.video_threads.iter_mut() {
                        v.video_sender.send(MediaData::Reset).ok();
                    }
                }
            },
            Data::TakeScreenshot((display, sid)) => {
                let mut msg = Message::new();
                msg.set_screenshot_request(ScreenshotRequest {
                    display,
                    sid,
                    ..Default::default()
                });
                allow_err!(peer.send(&msg).await);
            }
            _ => {}
        }
        true
    }

    #[inline]
    fn update_job_status(
        job: &fs::TransferJob,
        elapsed: i32,
        last_update_jobs_status: &mut (Instant, HashMap<i32, u64>),
        handler: &Session<T>,
    ) {
        if elapsed <= 0 {
            return;
        }
        let transferred = job.transferred();
        let last_transferred = {
            if let Some(v) = last_update_jobs_status.1.get(&job.id()) {
                v.to_owned()
            } else {
                0
            }
        };
        last_update_jobs_status.1.insert(job.id(), transferred);
        let speed = (transferred - last_transferred) as f64 / (elapsed as f64 / 1000.);
        let file_num = job.file_num() - 1;
        handler.job_progress(job.id(), file_num, speed, job.finished_size() as f64);
    }

    fn update_jobs_status(&mut self) {
        let elapsed = self.last_update_jobs_status.0.elapsed().as_millis() as i32;
        if elapsed >= 1000 {
            for job in self.read_jobs.iter() {
                Self::update_job_status(
                    job,
                    elapsed,
                    &mut self.last_update_jobs_status,
                    &self.handler,
                );
            }
            for job in self.write_jobs.iter() {
                Self::update_job_status(
                    job,
                    elapsed,
                    &mut self.last_update_jobs_status,
                    &mut self.handler,
                );
            }
            self.last_update_jobs_status.0 = Instant::now();
        }
    }

    async fn cancel_transfer_job(&mut self, id: i32, peer: &mut Stream) {
        let mut msg_out = Message::new();
        let mut file_action = FileAction::new();
        file_action.set_cancel(FileTransferCancel {
            id,
            ..Default::default()
        });
        msg_out.set_file_action(file_action);
        allow_err!(peer.send(&msg_out).await);
        if let Some(job) = fs::remove_job(id, &mut self.write_jobs) {
            job.remove_download_file();
        }
        let _ = fs::remove_job(id, &mut self.read_jobs);
        self.remove_jobs.remove(&id);
    }

    pub async fn sync_jobs_status_to_local(&mut self) -> bool {
        if !self.is_connected {
            return false;
        }
        let mut config: PeerConfig = self.handler.load_config();
        let mut transfer_metas = TransferSerde::default();
        for job in self.read_jobs.iter() {
            let json_str = serde_json::to_string(&job.gen_meta()).unwrap_or_default();
            transfer_metas.read_jobs.push(json_str);
        }
        for job in self.write_jobs.iter() {
            let json_str = serde_json::to_string(&job.gen_meta()).unwrap_or_default();
            transfer_metas.write_jobs.push(json_str);
        }
        log::info!("meta: {:?}", transfer_metas);
        if config.transfer != transfer_metas {
            config.transfer = transfer_metas;
            self.handler.save_config(config);
        }
        true
    }

    async fn send_toggle_virtual_display_msg(&self, peer: &mut Stream) {
        if self.handler.is_view_camera() {
            return;
        }
        if !self.peer_info.is_support_virtual_display() {
            return;
        }
        let lc = self.handler.lc.read().unwrap();
        let displays = lc.get_option("virtual-display");
        for d in displays.split(',') {
            if let Ok(index) = d.parse::<i32>() {
                let mut misc = Misc::new();
                misc.set_toggle_virtual_display(ToggleVirtualDisplay {
                    display: index,
                    on: true,
                    ..Default::default()
                });
                let mut msg_out = Message::new();
                msg_out.set_misc(misc);
                allow_err!(peer.send(&msg_out).await);
            }
        }
    }

    async fn send_toggle_privacy_mode_msg(&self, peer: &mut Stream) {
        if self.handler.is_view_camera() {
            return;
        }
        let lc = self.handler.lc.read().unwrap();
        if lc.version >= hbb_common::get_version_number("1.2.4")
            && lc.get_toggle_option("privacy-mode")
        {
            let impl_key = lc.get_option("privacy-mode-impl-key");
            if impl_key == crate::privacy_mode::PRIVACY_MODE_IMPL_WIN_VIRTUAL_DISPLAY
                && !self.peer_info.is_support_virtual_display()
            {
                return;
            }
            let mut misc = Misc::new();
            misc.set_toggle_privacy_mode(TogglePrivacyMode {
                impl_key,
                on: true,
                ..Default::default()
            });
            let mut msg_out = Message::new();
            msg_out.set_misc(misc);
            allow_err!(peer.send(&msg_out).await);
        }
    }

    fn contains_key_frame(vf: &VideoFrame) -> bool {
        use video_frame::Union::*;
        match &vf.union {
            Some(vf) => match vf {
                Vp8s(f) | Vp9s(f) | Av1s(f) | H264s(f) | H265s(f) => f.frames.iter().any(|e| e.key),
                _ => false,
            },
            None => false,
        }
    }

    // Currently, this function only considers decoding speed and queue length, not network delay.
    // The controlled end can consider auto fps as the maximum decoding fps.
    //
    // `counts` (this tick's `VideoCounters`, `tick_ms` long) and `log` (`enable-latency-log` on:
    // an FPSCTL line every tick) only feed the FPSCTL / "Set fps" log lines of this legacy
    // controller (measurement, see docs/LATENCY-METRICS.md); its decision does not read them.
    // With `enable-gentle-fps-control` on, `fps_control_gentle` decides instead.
    #[inline]
    fn fps_control(
        &mut self,
        direct: bool,
        real_fps_map: HashMap<usize, i32>,
        counts: &HashMap<usize, client::VideoCountersTick>,
        tick_ms: u128,
        log: bool,
    ) {
        if self.gentle_fps.enabled {
            self.fps_control_gentle(direct, counts, tick_ms, log);
            return;
        }
        let log_tick_ms = log.then_some(tick_ms);
        self.video_threads.iter_mut().for_each(|(k, v)| {
            let real_fps = real_fps_map.get(k).cloned().unwrap_or_default();
            if real_fps == 0 {
                v.fps_control.inactive_counter += 1;
            } else {
                v.fps_control.inactive_counter = 0;
            }
        });
        let custom_fps = self.handler.lc.read().unwrap().custom_fps.clone();
        let custom_fps = custom_fps.lock().unwrap().clone();
        let mut custom_fps = custom_fps.unwrap_or(30);
        if custom_fps < 5 || custom_fps > 120 {
            custom_fps = 30;
        }
        let inactive_threshold = 15;
        let max_queue_len = self
            .video_threads
            .iter()
            .map(|v| v.1.video_queue.read().unwrap().len())
            .max()
            .unwrap_or_default();
        let min_decode_fps = self
            .video_threads
            .iter()
            .filter(|v| v.1.fps_control.inactive_counter < inactive_threshold)
            .map(|v| *v.1.decode_fps.read().unwrap())
            .min()
            .flatten();
        let Some(min_decode_fps) = min_decode_fps else {
            if let Some(tick_ms) = log_tick_ms {
                log::info!(
                    "FPSCTL tick_ms={} direct={} custom={} decision=none (no decode_fps yet) |{}",
                    tick_ms,
                    direct,
                    custom_fps,
                    self.fps_control_displays(counts, &HashMap::new())
                );
            }
            return;
        };
        let mut limited_fps = if direct {
            min_decode_fps * 9 / 10 // 30 got 27
        } else {
            min_decode_fps * 4 / 5 // 30 got 24
        };
        if limited_fps > custom_fps {
            limited_fps = custom_fps;
        }
        let last_auto_fps = self.handler.lc.read().unwrap().last_auto_fps.clone();
        let displays = self.video_threads.keys().cloned().collect::<Vec<_>>();
        // What the trend check below saw per display, for the log lines: (len, decode_fps).
        let mut seen: HashMap<usize, (usize, Option<usize>)> = HashMap::new();
        let mut fps_trending = |display: usize| {
            let thread = self.video_threads.get_mut(&display)?;
            let ctl = &mut thread.fps_control;
            let len = thread.video_queue.read().unwrap().len();
            seen.insert(display, (len, *thread.decode_fps.read().unwrap()));
            let decode_fps = thread.decode_fps.read().unwrap().clone()?;
            let last_auto_fps = last_auto_fps.clone().unwrap_or(custom_fps as _);
            if ctl.inactive_counter > inactive_threshold {
                return None;
            }
            if len > 1 && last_auto_fps > limited_fps || len > std::cmp::max(1, decode_fps / 2) {
                ctl.idle_counter = 0;
                return Some(false);
            }
            if len <= 1 {
                ctl.idle_counter += 1;
                if ctl.idle_counter > 3 && last_auto_fps + 3 <= limited_fps {
                    return Some(true);
                }
            }
            if len > 1 {
                ctl.idle_counter = 0;
            }
            None
        };
        let trendings: Vec<_> = displays.iter().map(|k| fps_trending(*k)).collect();
        let trend_of = displays
            .iter()
            .zip(trendings.iter())
            .filter_map(|(d, t)| seen.get(d).map(|(len, fps)| (*d, (*len, *fps, *t))))
            .collect::<HashMap<_, _>>();
        let should_decrease = trendings.iter().any(|v| *v == Some(false));
        let should_increase = !should_decrease && trendings.iter().any(|v| *v == Some(true));
        let mut decision = "keep";
        let mut sent = None;
        if last_auto_fps.is_none() || should_decrease || should_increase {
            decision = if should_decrease {
                "decrease"
            } else if should_increase {
                "increase"
            } else {
                "init"
            };
            // limited_fps to ensure decoding is faster than encoding
            let mut auto_fps = limited_fps;
            if should_decrease && limited_fps < max_queue_len {
                auto_fps = limited_fps / 2;
                decision = "halve";
            }
            if auto_fps < 1 {
                auto_fps = 1;
            }
            if Some(auto_fps) != last_auto_fps {
                let mut misc = Misc::new();
                misc.set_option(OptionMessage {
                    custom_fps: auto_fps as _,
                    ..Default::default()
                });
                let mut msg = Message::new();
                msg.set_misc(misc);
                self.sender.send(Data::Message(msg)).ok();
                log::info!(
                    "Set fps to {} (reason={} last_auto={:?} custom={} limited={} min_decode_fps={} max_queue_len={} direct={} |{})",
                    auto_fps,
                    decision,
                    last_auto_fps,
                    custom_fps,
                    limited_fps,
                    min_decode_fps,
                    max_queue_len,
                    direct,
                    self.fps_control_displays(counts, &trend_of)
                );
                self.handler.lc.write().unwrap().last_auto_fps = Some(auto_fps);
                sent = Some(auto_fps);
            }
        }
        if let Some(tick_ms) = log_tick_ms {
            log::info!(
                "FPSCTL tick_ms={} direct={} custom={} last_auto={} limited={} min_decode_fps={} max_queue_len={} decision={} sent={} |{}",
                tick_ms,
                direct,
                custom_fps,
                last_auto_fps.map_or("-".to_owned(), |f| f.to_string()),
                limited_fps,
                min_decode_fps,
                max_queue_len,
                decision,
                sent.map_or("-".to_owned(), |f| f.to_string()),
                self.fps_control_displays(counts, &trend_of)
            );
        }
        // send refresh
        for (display, thread) in self.video_threads.iter_mut() {
            let ctl = &mut thread.fps_control;
            let video_queue = thread.video_queue.read().unwrap();
            let tolerable = std::cmp::min(min_decode_fps, video_queue.capacity() / 2);
            if ctl.refresh_times < 20 // enough
                    && (video_queue.len() > tolerable
                            && (ctl.refresh_times == 0 || ctl.last_refresh_instant.map(|t|t.elapsed().as_secs() > 10).unwrap_or(false)))
            {
                // Refresh causes client set_display, left frames cause flickering.
                let queue_len = video_queue.len();
                drop(video_queue);
                self.handler.refresh_video(*display as _);
                log::info!(
                    "Refresh display {} to reduce delay (queue_len={} tolerable={} refresh_times={})",
                    display,
                    queue_len,
                    tolerable,
                    ctl.refresh_times
                );
                ctl.refresh_times += 1;
                ctl.last_refresh_instant = Some(Instant::now());
            }
        }
    }

    /// Per-display inputs of `fps_control` for its log lines: the counters of the last tick,
    /// the legacy `decode_fps` split into decode, present and the rest, the queue, the
    /// controller state and trend, and the current `net+server(est)` when it is measured.
    fn fps_control_displays(
        &self,
        counts: &HashMap<usize, client::VideoCountersTick>,
        // What the trend check saw: (queue len, decode_fps, trend). Missing: read them now.
        seen: &HashMap<usize, (usize, Option<usize>, Option<bool>)>,
    ) -> String {
        let mut displays = self.video_threads.keys().cloned().collect::<Vec<_>>();
        displays.sort_unstable();
        let avg_ms = |us: u64, n: usize| {
            if n == 0 {
                "n/a".to_owned()
            } else {
                format!("{:.1}", us as f64 / n as f64 / 1000.0)
            }
        };
        let mut out = String::new();
        for display in displays {
            let Some(thread) = self.video_threads.get(&display) else {
                continue;
            };
            let c = counts.get(&display).copied().unwrap_or_default();
            let (queue_len, decode_fps, trend) = match seen.get(&display) {
                Some((len, fps, trend)) => (
                    *len,
                    *fps,
                    match trend {
                        Some(true) => "up",
                        Some(false) => "down",
                        None => "-",
                    },
                ),
                None => (
                    thread.video_queue.read().unwrap().len(),
                    *thread.decode_fps.read().unwrap(),
                    "?",
                ),
            };
            out += &format!(
                " d{} rx={} dec={} shown={} ui_lost={} skip={} noout={} discard={} fail={} queue_len={} queue_max={} decode_fps={} dec_ms={} present_ms={} other_ms={} idle={} inactive={} trend={} net+server={};",
                display,
                c.received,
                c.decoded,
                c.shown,
                c.ui_lost,
                c.decoded.saturating_sub(c.shown),
                c.no_output,
                c.discarded,
                c.failed,
                queue_len,
                c.queue_max,
                decode_fps.map_or("-".to_owned(), |f| f.to_string()),
                avg_ms(c.decode_us, c.decoded),
                avg_ms(c.present_us, c.shown),
                avg_ms(
                    c.busy_us.saturating_sub(c.decode_us + c.present_us),
                    c.decoded
                ),
                thread.fps_control.idle_counter,
                thread.fps_control.inactive_counter,
                trend,
                client::latency::net_srv_now(client::latency::stream_key(&thread.video_queue))
                    .unwrap_or_else(|| "?".to_owned()),
            );
        }
        out
    }

    /// Gentle fps control (`enable-gentle-fps-control`, docs/LATENCY-METRICS.md): asks the host
    /// for less only while the decoder thread is measurably overloaded, with hysteresis and a
    /// floor, and refreshes only as a last resort. Same message to the host as the legacy one.
    fn fps_control_gentle(
        &mut self,
        direct: bool,
        counts: &HashMap<usize, client::VideoCountersTick>,
        tick_ms: u128,
        log: bool,
    ) {
        let custom_fps = self.handler.lc.read().unwrap().custom_fps.clone();
        let custom_fps = custom_fps.lock().unwrap().clone();
        let mut custom_fps = custom_fps.unwrap_or(30);
        if custom_fps < 5 || custom_fps > 120 {
            custom_fps = 30;
        }
        if self.gentle_fps.ticks == 0 {
            // The host starts at the custom fps sent at login, whatever an earlier connection
            // of this session asked for.
            self.handler.lc.write().unwrap().last_auto_fps = None;
        }
        let mut ticks = self
            .video_threads
            .keys()
            .map(|d| (*d, counts.get(d).copied().unwrap_or_default()))
            .collect::<Vec<_>>();
        ticks.sort_unstable_by_key(|(d, _)| *d);
        let tick = self
            .gentle_fps
            .tick(custom_fps, tick_ms.min(u64::MAX as u128) as u64, &ticks);
        if let Some(fps) = tick.send {
            let mut misc = Misc::new();
            misc.set_option(OptionMessage {
                custom_fps: fps as _,
                ..Default::default()
            });
            let mut msg = Message::new();
            msg.set_misc(misc);
            self.sender.send(Data::Message(msg)).ok();
            self.handler.lc.write().unwrap().last_auto_fps = Some(fps);
            log::info!(
                "Set fps to {} (gentle {} direct={} |{}|{})",
                fps,
                tick.summary(),
                direct,
                self.fps_control_displays(counts, &HashMap::new()),
                self.gentle_fps.describe_displays()
            );
        }
        if log {
            log::info!(
                "FPSCTL mode=gentle tick_ms={} direct={} {} |{}|{}",
                tick_ms,
                direct,
                tick.summary(),
                self.fps_control_displays(counts, &HashMap::new()),
                self.gentle_fps.describe_displays()
            );
        }
        // A display whose overflow refresh was deferred (`key_frame_pending`) gets it as soon as
        // its limit allows.
        let mut pending = self
            .video_threads
            .iter()
            .filter(|(d, v)| v.fps_control.key_frame_pending && !tick.refresh.contains(*d))
            .map(|(d, _)| *d)
            .collect::<Vec<_>>();
        pending.sort_unstable();
        for display in pending {
            let Some(thread) = self.video_threads.get_mut(&display) else {
                continue;
            };
            if !thread.fps_control.key_frame_refresh_allowed() {
                continue;
            }
            self.handler.refresh_video(display as _);
            log::info!(
                "Refresh display {} after queue overflow (deferred, refresh_times={})",
                display,
                thread.fps_control.refresh_times
            );
            thread.fps_control.on_refresh();
        }
        for display in tick.refresh {
            let Some(thread) = self.video_threads.get_mut(&display) else {
                continue;
            };
            let allowed = if thread.fps_control.key_frame_pending {
                thread.fps_control.key_frame_refresh_allowed()
            } else {
                thread.fps_control.refresh_allowed()
            };
            if !allowed {
                continue;
            }
            let queue_len = thread.video_queue.read().unwrap().len();
            self.handler.refresh_video(display as _);
            log::info!(
                "Refresh display {} to reduce delay (gentle: queue_max above {} for {} ticks, queue_len={} refresh_times={})",
                display,
                gentle_refresh_queue(),
                GENTLE_REFRESH_TICKS,
                queue_len,
                thread.fps_control.refresh_times
            );
            thread.fps_control.on_refresh();
        }
    }

    fn check_view_camera_support(&self, peer_version: &str, peer_platform: &str) -> bool {
        if self.peer_info.support_view_camera {
            return true;
        }
        if hbb_common::get_version_number(&peer_version) < hbb_common::get_version_number("1.3.9")
            && (peer_platform == "Windows" || peer_platform == "Linux")
        {
            self.handler.msgbox(
                "error",
                "Download new version",
                "upgrade_remote_rustdesk_client_to_{1.3.9}_tip",
                "",
            );
        } else {
            self.handler.on_error("view_camera_unsupported_tip");
        }
        return false;
    }

    fn check_terminal_support(&self, peer_version: &str) -> bool {
        if self.peer_info.support_terminal {
            return true;
        }
        if hbb_common::get_version_number(&peer_version) < hbb_common::get_version_number("1.4.1") {
            self.handler.msgbox(
                "error",
                "Remote terminal not supported",
                "Remote terminal is not supported by the remote side. Please upgrade to version 1.4.1 or higher.",
                "",
            );
        } else {
            self.handler
                .on_error("Remote terminal is not supported by the remote side");
        }
        return false;
    }

    /// A video frame from the session stream or, in live mode, from the UDP video lane: the
    /// same decoder queue, pacer, fps control and latency stats either way.
    async fn handle_video_frame(&mut self, vf: VideoFrame, peer: &mut Stream) {
        if !self.first_frame {
            self.first_frame = true;
            self.handler.close_success();
            self.handler.adapt_size();
            self.send_toggle_virtual_display_msg(peer).await;
            self.send_toggle_privacy_mode_msg(peer).await;
        }
        self.video_format = CodecFormat::from(&vf);

        let display = vf.display as usize;
        if !self.video_threads.contains_key(&display) {
            self.new_video_thread(display);
        }
        let Some(thread) = self.video_threads.get_mut(&display) else {
            return;
        };
        client::latency::on_received(client::latency::stream_key(&thread.video_queue), &vf);
        thread.counters.on_received();
        let key_frame = Self::contains_key_frame(&vf);
        thread.pacing.on_received(&vf, key_frame);
        if key_frame {
            thread.fps_control.key_frame_pending = false;
            thread
                .video_sender
                .send(MediaData::VideoFrame(Box::new(vf)))
                .ok();
        } else {
            let video_queue = thread.video_queue.read().unwrap();
            if video_queue.force_push(vf).is_some() {
                drop(video_queue);
                thread.counters.on_discarded();
                if !self.gentle_fps.enabled {
                    self.handler.refresh_video(display as _);
                } else if thread.fps_control.key_frame_refresh_allowed() {
                    // Gentle fps control: a refresh restarts the host's capturer
                    // and encoder for every viewer, so overflow refreshes are
                    // spaced (`KEY_FRAME_REFRESH_SPACING`), not capped: without a
                    // key frame the stream cannot be decoded again.
                    self.handler.refresh_video(display as _);
                    log::info!(
                        "Refresh display {} after queue overflow (refresh_times={})",
                        display,
                        thread.fps_control.refresh_times
                    );
                    thread.fps_control.on_refresh();
                } else if !thread.fps_control.key_frame_pending {
                    // The evicted frame is a reference of the frames after it:
                    // freeze (drop the queue until a key frame) rather than decode
                    // across the hole, and refresh once the limit allows
                    // (`fps_control_gentle`, every status tick).
                    *thread.discard_queue.write().unwrap() = true;
                    thread.fps_control.key_frame_pending = true;
                    log::info!(
                        "Queue overflow on display {}: refresh deferred by the limit, queue discarded until a key frame (refresh_times={})",
                        display,
                        thread.fps_control.refresh_times
                    );
                }
            } else {
                thread.video_sender.send(MediaData::VideoQueue).ok();
            }
        }
    }

    /// Feeds the live UDP frames the lane has ready (after a UDP event, or after an ACTIVE
    /// marker or SwitchDisplay read on TCP released held ones).
    async fn feed_udp_video(&mut self, peer: &mut Stream) {
        let bytes = self.udp_video.take_udp_bytes();
        if bytes > 0 {
            self.data_count.fetch_add(bytes, Ordering::Relaxed);
        }
        while let Some(vf) = self.udp_video.next_ready() {
            self.handle_video_frame(vf, peer).await;
        }
    }

    async fn handle_msg_from_peer(&mut self, data: &[u8], peer: &mut Stream) -> bool {
        if let Ok(msg_in) = Message::parse_from_bytes(&data) {
            match msg_in.union {
                Some(message::Union::VideoFrame(vf)) => {
                    // After a live UDP video lane, a display whose UDP frames did not all
                    // arrive restarts on TCP from a key frame.
                    if self.udp_video.on_tcp_frame(&vf) {
                        self.handle_video_frame(vf, peer).await;
                    }
                }
                Some(message::Union::Hash(hash)) => {
                    if !self
                        .handler
                        .handle_hash(&self.handler.password.clone(), hash, peer)
                        .await
                    {
                        return false;
                    }
                }
                Some(message::Union::LoginResponse(lr)) => match lr.union {
                    Some(login_response::Union::Error(err)) => {
                        if err == client::REQUIRE_2FA {
                            self.handler.lc.write().unwrap().enable_trusted_devices =
                                lr.enable_trusted_devices;
                        }
                        if !self.handler.handle_login_error(&err) {
                            return false;
                        }
                    }
                    Some(login_response::Union::PeerInfo(pi)) => {
                        let peer_version = pi.version.clone();
                        let peer_platform = pi.platform.clone();
                        self.set_peer_info(&pi);
                        if self.handler.is_view_camera() {
                            if !self.check_view_camera_support(&peer_version, &peer_platform) {
                                self.handler.lc.write().unwrap().handle_peer_info(&pi);
                                return false;
                            }
                        }
                        if self.handler.is_terminal() {
                            if !self.check_terminal_support(&peer_version) {
                                self.handler.lc.write().unwrap().handle_peer_info(&pi);
                                return false;
                            }
                        }
                        self.handler.handle_peer_info(pi);
                        #[cfg(all(target_os = "windows", not(feature = "flutter")))]
                        self.check_clipboard_file_context();
                        if self.handler.is_default() {
                            // Startup notifications must see the current sessions' clipboard requirements.
                            #[cfg(feature = "flutter")]
                            #[cfg(not(target_os = "ios"))]
                            crate::flutter::update_text_clipboard_required();

                            #[cfg(all(feature = "flutter", feature = "unix-file-copy-paste"))]
                            crate::flutter::update_file_clipboard_required();

                            #[cfg(feature = "flutter")]
                            #[cfg(not(target_os = "ios"))]
                            let rx = Client::try_start_clipboard(None);
                            #[cfg(not(feature = "flutter"))]
                            #[cfg(not(any(target_os = "android", target_os = "ios")))]
                            let rx = Client::try_start_clipboard(Some(
                                crate::client::ClientClipboardContext {
                                    cfg: self.handler.get_permission_config(),
                                    tx: self.sender.clone(),
                                    #[cfg(feature = "unix-file-copy-paste")]
                                    is_file_supported: crate::is_support_file_copy_paste(
                                        &peer_version,
                                    ),
                                },
                            ));
                            // To make sure current text clipboard data is updated.
                            #[cfg(not(target_os = "ios"))]
                            if let Some(mut rx) = rx {
                                timeout(CLIPBOARD_INTERVAL, rx.recv()).await.ok();
                            }

                            // `is_connected`` becomes true after the first PeerInfo.
                            // Reconnects create a new Remote. Refreshes after a generation change
                            // use a separate entry point and are not blocked by this guard.
                            #[cfg(not(any(target_os = "android", target_os = "ios")))]
                            if !self.is_connected
                                && self.handler.is_text_clipboard_required()
                                && self.handler.lc.read().unwrap().sync_init_clipboard.v
                            {
                                self.start_initial_clipboard_sync();
                            }
                            // to-do: Android, is `sync_init_clipboard` really needed?
                            // https://github.com/rustdesk/rustdesk/discussions/9010
                        }

                        if self.handler.is_file_transfer() {
                            self.handler.load_last_jobs();
                        }

                        self.is_connected = true;
                    }
                    _ => {}
                },
                Some(message::Union::CursorData(cd)) if self.dedupes_cursors() => {
                    self.set_cursor_data_by_content(cd);
                }
                Some(message::Union::CursorId(id)) if self.dedupes_cursors() => {
                    self.handler
                        .set_cursor_id(self.cursor_dedupe.id(id).to_string());
                }
                Some(message::Union::CursorData(cd)) => {
                    let id = cd.id;
                    #[cfg(feature = "flutter")]
                    let compressed = cd.colors.clone();
                    match decode_cursor_data(cd) {
                        Ok(cd) => {
                            #[cfg(feature = "flutter")]
                            self.keep_cursor_shape(cursor_shape(id, &cd, &compressed));
                            self.handler.set_cursor_data(cd)
                        }
                        Err(err) => {
                            log::warn!("Rejected cursor {id}: {err}");
                            // The peer shows it now: selected, the UI treats it as a shape it
                            // lacks, as it will when the peer selects it again.
                            self.handler.set_cursor_id(id.to_string());
                        }
                    }
                }
                Some(message::Union::CursorId(id)) => {
                    self.handler.set_cursor_id(id.to_string());
                }
                Some(message::Union::CursorPosition(cp)) => {
                    self.handler.set_cursor_position(cp);
                }
                Some(message::Union::Clipboard(cb)) => {
                    let clipboard_allowed = {
                        let lc = self.handler.lc.read().unwrap();
                        !lc.disable_clipboard.v && !lc.view_only.v
                    };
                    if clipboard_allowed {
                        #[cfg(all(
                            feature = "flutter",
                            not(any(target_os = "android", target_os = "ios"))
                        ))]
                        if self.handler.is_text_clipboard_required()
                            && crate::clipboard::is_sync_clipboard_between_sessions_enabled()
                        {
                            let mut msg = Message::new();
                            msg.set_clipboard(cb.clone());
                            let session_id = self.handler.lc.read().unwrap().session_id;
                            crate::flutter::send_clipboard_msg_to_other_sessions(msg, session_id);
                        }
                        #[cfg(not(any(target_os = "android", target_os = "ios")))]
                        {
                            self.initial_clipboard_pending = false;
                            update_clipboard(vec![cb], ClipboardSide::Client);
                        }
                        #[cfg(target_os = "ios")]
                        {
                            let content = if cb.compress {
                                hbb_common::compress::decompress(&cb.content)
                            } else {
                                cb.content.into()
                            };
                            if let Ok(content) = String::from_utf8(content) {
                                self.handler.clipboard(content);
                            }
                        }
                        #[cfg(target_os = "android")]
                        crate::clipboard::handle_msg_clipboard(cb);
                    }
                }
                Some(message::Union::MultiClipboards(_mcb)) => {
                    let clipboard_allowed = {
                        let lc = self.handler.lc.read().unwrap();
                        !lc.disable_clipboard.v && !lc.view_only.v
                    };
                    if clipboard_allowed {
                        #[cfg(all(
                            feature = "flutter",
                            not(any(target_os = "android", target_os = "ios"))
                        ))]
                        if self.handler.is_text_clipboard_required()
                            && crate::clipboard::is_sync_clipboard_between_sessions_enabled()
                        {
                            let mut msg = Message::new();
                            msg.set_multi_clipboards(_mcb.clone());
                            let session_id = self.handler.lc.read().unwrap().session_id;
                            crate::flutter::send_clipboard_msg_to_other_sessions(msg, session_id);
                        }
                        #[cfg(not(any(target_os = "android", target_os = "ios")))]
                        {
                            self.initial_clipboard_pending = false;
                            update_clipboard(_mcb.clipboards, ClipboardSide::Client);
                        }
                        #[cfg(target_os = "ios")]
                        {
                            if let Some(cb) = _mcb
                                .clipboards
                                .iter()
                                .find(|c| c.format.enum_value() == Ok(ClipboardFormat::Text))
                            {
                                let content = if cb.compress {
                                    hbb_common::compress::decompress(&cb.content)
                                } else {
                                    cb.content.to_vec()
                                };
                                if let Ok(content) = String::from_utf8(content) {
                                    self.handler.clipboard(content);
                                }
                            }
                        }
                        #[cfg(target_os = "android")]
                        crate::clipboard::handle_msg_multi_clipboards(_mcb);
                    }
                }
                #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
                Some(message::Union::Cliprdr(clip)) => {
                    self.handle_cliprdr_msg(clip, peer).await;
                }
                Some(message::Union::FileResponse(fr)) => {
                    match fr.union {
                        Some(file_response::Union::EmptyDirs(res)) => {
                            self.handler.update_empty_dirs(res);
                        }
                        Some(file_response::Union::Dir(fd)) => {
                            #[cfg(windows)]
                            let entries = fd.entries.to_vec();
                            #[cfg(not(windows))]
                            let mut entries = fd.entries.to_vec();
                            #[cfg(not(windows))]
                            {
                                if self.handler.peer_platform() == "Windows" {
                                    fs::transform_windows_path(&mut entries);
                                }
                            }
                            // We cannot call cancel_transfer_job/handle_job_status while holding
                            // a mutable borrow from fs::get_job(&mut self.write_jobs), so defer
                            // the error handling until after the borrow scope ends.
                            let mut set_files_err = None;
                            if let Some(job) = fs::get_job(fd.id, &mut self.write_jobs) {
                                log::info!("job set_files: {:?}", entries);
                                if let Err(err) = job.set_files(entries) {
                                    set_files_err = Some(err.to_string());
                                } else {
                                    job.set_finished_size_on_resume();
                                    self.handler.update_folder_files(
                                        fd.id,
                                        job.files(),
                                        fd.path,
                                        false,
                                        false,
                                    );
                                }
                            } else if let Some(job) = self.remove_jobs.get_mut(&fd.id) {
                                // Intentionally keep raw entries here:
                                // - remote remove flow executes deletions on peer side;
                                // - local remove flow is populated from local get_recursive_files().
                                job.files = entries;
                                self.handler
                                    .update_folder_files(fd.id, &job.files, fd.path, false, false);
                            } else {
                                self.handler
                                    .update_folder_files(fd.id, &entries, fd.path, false, false);
                            }
                            if let Some(err) = set_files_err {
                                log::warn!(
                                    "Rejected unsafe file list from remote peer for job {}: {}",
                                    fd.id,
                                    err
                                );
                                self.cancel_transfer_job(fd.id, peer).await;
                                self.handle_job_status(fd.id, -1, Some(err));
                            }
                        }
                        Some(file_response::Union::Digest(digest)) => {
                            if digest.is_upload {
                                if let Some(job) = fs::get_job(digest.id, &mut self.read_jobs) {
                                    if let Some(file) = job.files().get(digest.file_num as usize) {
                                        if let fs::DataSource::FilePath(p) = &job.data_source {
                                            let read_path =
                                                get_string(&fs::TransferJob::join(p, &file.name));
                                            let mut overwrite_strategy =
                                                job.default_overwrite_strategy();
                                            let mut offset = 0;
                                            if digest.is_identical && job.is_resume {
                                                if digest.transferred_size > 0 {
                                                    overwrite_strategy = Some(true);
                                                    offset = digest.transferred_size as _;
                                                }
                                            }
                                            if let Some(overwrite) = overwrite_strategy {
                                                let req = FileTransferSendConfirmRequest {
                                                    id: digest.id,
                                                    file_num: digest.file_num,
                                                    union: Some(if overwrite {
                                                        file_transfer_send_confirm_request::Union::OffsetBlk(offset)
                                                    } else {
                                                        file_transfer_send_confirm_request::Union::Skip(
                                                            true,
                                                        )
                                                    }),
                                                    ..Default::default()
                                                };
                                                job.confirm(&req).await;
                                                let msg = new_send_confirm(req);
                                                allow_err!(peer.send(&msg).await);
                                            } else {
                                                self.handler.override_file_confirm(
                                                    digest.id,
                                                    digest.file_num,
                                                    read_path,
                                                    true,
                                                    digest.is_identical,
                                                );
                                            }
                                        }
                                    }
                                }
                            } else {
                                if let Some(job) = fs::get_job(digest.id, &mut self.write_jobs) {
                                    if let Some(file) = job.files().get(digest.file_num as usize) {
                                        if let fs::DataSource::FilePath(p) = &job.data_source {
                                            let write_path =
                                                get_string(&fs::TransferJob::join(p, &file.name));
                                            job.set_digest(digest.file_size, digest.last_modified);
                                            let peer_ver = self.handler.lc.read().unwrap().version;
                                            let is_support_resume =
                                                crate::is_support_file_transfer_resume_num(
                                                    peer_ver,
                                                );
                                            match fs::is_write_need_confirmation(
                                                is_support_resume && job.is_resume,
                                                &write_path,
                                                &digest,
                                            ) {
                                                Ok(res) => match res {
                                                    DigestCheckResult::IsSame => {
                                                        let req = FileTransferSendConfirmRequest {
                                                            id: digest.id,
                                                            file_num: digest.file_num,
                                                            union: Some(file_transfer_send_confirm_request::Union::Skip(true)),
                                                            ..Default::default()
                                                        };
                                                        job.confirm(&req).await;
                                                        let msg = new_send_confirm(req);
                                                        allow_err!(peer.send(&msg).await);
                                                    }
                                                    DigestCheckResult::NeedConfirm(digest) => {
                                                        let mut overwrite_strategy =
                                                            job.default_overwrite_strategy();
                                                        let mut offset = 0;
                                                        if digest.is_identical
                                                            && job.is_resume
                                                            && digest.transferred_size > 0
                                                        {
                                                            overwrite_strategy = Some(true);
                                                            offset = digest.transferred_size as _;
                                                        }
                                                        if let Some(overwrite) = overwrite_strategy
                                                        {
                                                            let req =
                                                                FileTransferSendConfirmRequest {
                                                                    id: digest.id,
                                                                    file_num: digest.file_num,
                                                                    union: Some(if overwrite {
                                                                        file_transfer_send_confirm_request::Union::OffsetBlk(offset)
                                                                    } else {
                                                                        file_transfer_send_confirm_request::Union::Skip(true)
                                                                    }),
                                                                    ..Default::default()
                                                                };
                                                            job.confirm(&req).await;
                                                            let msg = new_send_confirm(req);
                                                            allow_err!(peer.send(&msg).await);
                                                        } else {
                                                            self.handler.override_file_confirm(
                                                                digest.id,
                                                                digest.file_num,
                                                                write_path,
                                                                false,
                                                                digest.is_identical,
                                                            );
                                                        }
                                                    }
                                                    DigestCheckResult::NoSuchFile => {
                                                        let req = FileTransferSendConfirmRequest {
                                                        id: digest.id,
                                                        file_num: digest.file_num,
                                                        union: Some(file_transfer_send_confirm_request::Union::OffsetBlk(0)),
                                                        ..Default::default()
                                                    };
                                                        job.confirm(&req).await;
                                                        let msg = new_send_confirm(req);
                                                        allow_err!(peer.send(&msg).await);
                                                    }
                                                },
                                                Err(err) => {
                                                    println!("error receiving digest: {}", err);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some(file_response::Union::Block(block)) => {
                            if let Some(job) = fs::get_job(block.id, &mut self.write_jobs) {
                                if let Err(_err) = job.write(block).await {
                                    // to-do: add "skip" for writing job
                                }
                                if job.r#type == fs::JobType::Generic {
                                    self.update_jobs_status();
                                }
                            }
                        }
                        Some(file_response::Union::Done(d)) => {
                            let mut err: Option<String> = None;
                            let mut job_type = fs::JobType::Generic;
                            let mut printer_data = None;
                            if let Some(job) = fs::remove_job(d.id, &mut self.write_jobs) {
                                job.modify_time();
                                err = job.job_error();
                                job_type = job.r#type;
                                printer_data = match job.get_buf_data().await {
                                    Ok(d) => d,
                                    Err(e) => {
                                        log::error!("Failed to get the printer data: {}", e);
                                        None
                                    }
                                };
                            }
                            match job_type {
                                fs::JobType::Generic => {
                                    self.handle_job_status(d.id, d.file_num, err);
                                }
                                fs::JobType::Printer => {
                                    if let Some(err) = err {
                                        log::error!("Receive print job failed, error {err}");
                                    } else {
                                        log::info!(
                                            "Receive print job done, data len: {:?}",
                                            printer_data.as_ref().map(|d| d.len()).unwrap_or(0)
                                        );
                                        #[cfg(target_os = "windows")]
                                        if let Some(data) = printer_data {
                                            let printer_name = self
                                                .handler
                                                .printer_names
                                                .write()
                                                .unwrap()
                                                .remove(&d.id);
                                            // Spawn a new thread to handle the print job.
                                            // Or print job will block the ui thread.
                                            std::thread::spawn(move || {
                                                if let Err(e) =
                                                    crate::platform::send_raw_data_to_printer(
                                                        printer_name,
                                                        data,
                                                    )
                                                {
                                                    log::error!("Print job error: {}", e);
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        Some(file_response::Union::Error(e)) => {
                            let job_type = fs::remove_job(e.id, &mut self.write_jobs)
                                .or_else(|| fs::remove_job(e.id, &mut self.read_jobs))
                                .map(|j| j.r#type)
                                .unwrap_or(fs::JobType::Generic);
                            match job_type {
                                fs::JobType::Generic => {
                                    self.handle_job_status(e.id, e.file_num, Some(e.error));
                                }
                                fs::JobType::Printer => {
                                    log::error!("Printer job error: {}", e.error);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Some(message::Union::Misc(misc)) => match misc.union {
                    Some(misc::Union::PointerCaptured(captured)) => {
                        self.handler.on_pointer_captured(captured);
                    }
                    Some(misc::Union::UdpVideoOffer(offer)) => {
                        if let Some(msg) = self.udp_video.on_offer(&offer) {
                            allow_err!(peer.send(&msg).await);
                        }
                    }
                    Some(misc::Union::UdpVideoControl(c)) => self.udp_video.on_control(&c),
                    Some(misc::Union::MicrophoneForwarding(response)) => {
                        if response.response && self.microphone_id == Some(response.request_id) {
                            if !response.enabled {
                                self.stop_voice_call();
                                self.microphone_active = false;
                                self.microphone_request = None;
                                self.microphone_id = None;
                                self.handler.on_microphone_forwarding("error", &response.error);
                            } else if response.ready {
                                self.microphone_request = None;
                                self.microphone_active = true;
                                self.handler.on_microphone_forwarding("active", "");
                            } else if self.stop_voice_call_sender.is_none() {
                                self.stop_voice_call_sender = self.start_microphone_capture(response.request_id);
                                if self.stop_voice_call_sender.is_some() {
                                    self.handler.on_microphone_forwarding("capturing", "");
                                } else {
                                    self.microphone_request = None;
                                    self.microphone_id = None;
                                    self.handler.on_microphone_forwarding("error", "Microphone capture is busy or unavailable on this platform");
                                    allow_err!(peer.send(&crate::microphone_forwarding::message(0, false, false, "")).await);
                                }
                            }
                        }
                    }
                    Some(misc::Union::AudioFormat(f)) => {
                        self.audio_sender.send(MediaData::AudioFormat(f)).ok();
                    }
                    Some(misc::Union::ChatMessage(c)) => {
                        self.handler.new_message(c.text);
                    }
                    Some(misc::Union::PermissionInfo(p)) => {
                        log::info!("Change permission {:?} -> {}", p.permission, p.enabled);
                        // https://github.com/rustdesk/rustdesk/issues/3703#issuecomment-1474734754
                        match p.permission.enum_value() {
                            Ok(Permission::Keyboard) => {
                                *self.handler.server_keyboard_enabled.write().unwrap() = p.enabled;
                                #[cfg(feature = "flutter")]
                                #[cfg(not(target_os = "ios"))]
                                crate::flutter::update_text_clipboard_required();
                                #[cfg(all(feature = "flutter", feature = "unix-file-copy-paste"))]
                                crate::flutter::update_file_clipboard_required();
                                self.handler.set_permission("keyboard", p.enabled);
                            }
                            Ok(Permission::Clipboard) => {
                                *self.handler.server_clipboard_enabled.write().unwrap() = p.enabled;
                                #[cfg(feature = "flutter")]
                                #[cfg(not(target_os = "ios"))]
                                crate::flutter::update_text_clipboard_required();
                                self.handler.set_permission("clipboard", p.enabled);
                            }
                            Ok(Permission::Audio) => {
                                self.handler.set_permission("audio", p.enabled);
                            }
                            Ok(Permission::File) => {
                                *self.handler.server_file_transfer_enabled.write().unwrap() =
                                    p.enabled;
                                if !p.enabled && self.handler.is_file_transfer() {
                                    return true;
                                }
                                #[cfg(all(feature = "flutter", feature = "unix-file-copy-paste"))]
                                crate::flutter::update_file_clipboard_required();
                                self.handler.set_permission("file", p.enabled);
                                #[cfg(feature = "unix-file-copy-paste")]
                                if !p.enabled {
                                    try_empty_clipboard_files(
                                        ClipboardSide::Client,
                                        self.client_conn_id,
                                    );
                                }
                            }
                            Ok(Permission::Restart) => {
                                self.handler.set_permission("restart", p.enabled);
                            }
                            Ok(Permission::Recording) => {
                                self.handler.lc.write().unwrap().record_permission = p.enabled;
                                self.update_record_state();
                                self.handler.set_permission("recording", p.enabled);
                            }
                            Ok(Permission::BlockInput) => {
                                self.handler.set_permission("block_input", p.enabled);
                            }
                            Ok(Permission::PrivacyMode) => {
                                self.handler.set_permission("privacy_mode", p.enabled);
                            }
                            _ => {}
                        }
                    }
                    Some(misc::Union::SwitchDisplay(s)) => {
                        // Live UDP frames of the next epoch may be shown after this.
                        self.udp_video.on_switch_display();
                        self.handler.handle_peer_switch_display(&s);
                        if let Some(thread) = self.video_threads.get_mut(&(s.display as usize)) {
                            thread.video_sender.send(MediaData::Reset).ok();
                        }

                        let mut scale = 1.0;
                        if let Some(pi) = &self.handler.lc.read().unwrap().peer_info {
                            if let Some(d) = pi.displays.get(s.display as usize) {
                                scale = d.scale;
                            }
                        }

                        if s.width > 0 && s.height > 0 {
                            self.handler.set_display(
                                s.x,
                                s.y,
                                s.width,
                                s.height,
                                s.cursor_embedded,
                                scale,
                            );
                        }
                    }
                    Some(misc::Union::CloseReason(c)) => {
                        self.sent_close_reason = true; // The controlled end will close, no need to send close reason
                        self.handler.msgbox("error", "Connection Error", &c, "");
                        return false;
                    }
                    Some(misc::Union::BackNotification(notification)) => {
                        if !self.handle_back_notification(notification).await {
                            return false;
                        }
                    }
                    Some(misc::Union::Uac(uac)) => {
                        let keyboard = self.handler.server_keyboard_enabled.read().unwrap().clone();
                        #[cfg(feature = "flutter")]
                        {
                            if uac && keyboard {
                                self.handler.msgbox(
                                    "on-uac",
                                    "Prompt",
                                    "Please wait for confirmation of UAC...",
                                    "",
                                );
                            } else {
                                self.handler.cancel_msgbox("on-uac");
                                self.handler.cancel_msgbox("wait-uac");
                                self.handler.cancel_msgbox("elevation-error");
                            }
                        }
                        #[cfg(not(feature = "flutter"))]
                        {
                            let msgtype = "custom-uac-nocancel";
                            let title = "Prompt";
                            let text = "Please wait for confirmation of UAC...";
                            let link = "";
                            if uac && keyboard {
                                self.handler.msgbox(msgtype, title, text, link);
                            } else {
                                self.handler.cancel_msgbox(&format!(
                                    "{}-{}-{}-{}",
                                    msgtype, title, text, link,
                                ));
                            }
                        }
                    }
                    Some(misc::Union::ForegroundWindowElevated(elevated)) => {
                        let keyboard = self.handler.server_keyboard_enabled.read().unwrap().clone();
                        #[cfg(feature = "flutter")]
                        {
                            if elevated && keyboard {
                                self.handler.msgbox(
                                    "on-foreground-elevated",
                                    "Prompt",
                                    "elevated_foreground_window_tip",
                                    "",
                                );
                            } else {
                                self.handler.cancel_msgbox("on-foreground-elevated");
                                self.handler.cancel_msgbox("wait-uac");
                                self.handler.cancel_msgbox("elevation-error");
                            }
                        }
                        #[cfg(not(feature = "flutter"))]
                        {
                            let msgtype = "custom-elevated-foreground-nocancel";
                            let title = "Prompt";
                            let text = "elevated_foreground_window_tip";
                            let link = "";
                            if elevated && keyboard {
                                self.handler.msgbox(msgtype, title, text, link);
                            } else {
                                self.handler.cancel_msgbox(&format!(
                                    "{}-{}-{}-{}",
                                    msgtype, title, text, link,
                                ));
                            }
                        }
                    }
                    Some(misc::Union::ElevationResponse(err)) => {
                        if err.is_empty() {
                            self.handler.msgbox("wait-uac", "", "", "");
                        } else {
                            self.handler.cancel_msgbox("wait-uac");
                            self.handler
                                .msgbox("elevation-error", "Elevation Error", &err, "");
                        }
                    }
                    Some(misc::Union::PortableServiceRunning(b)) => {
                        self.handler.portable_service_running(b);
                        if self.elevation_requested && b {
                            self.handler.msgbox(
                                "custom-nocancel-success",
                                "Successful",
                                "Elevate successfully",
                                "",
                            );
                        }
                    }
                    #[cfg(feature = "flutter")]
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    Some(misc::Union::SwitchBack(_)) => {
                        let allow_switch_back = self
                            .handler
                            .lc
                            .write()
                            .unwrap()
                            .consume_switch_back_permission();
                        if allow_switch_back {
                            self.handler.switch_back(&self.handler.get_id());
                        } else {
                            log::warn!(
                                "Ignored unsolicited SwitchBack from {}",
                                self.handler.get_id()
                            );
                        }
                    }
                    Some(misc::Union::SupportedEncoding(e)) => {
                        log::info!("update supported encoding:{:?}", e);
                        self.handler.lc.write().unwrap().supported_encoding = e;
                    }
                    Some(misc::Union::FollowCurrentDisplay(d_idx)) => {
                        self.handler.set_current_display(d_idx);
                    }
                    _ => {}
                },
                Some(message::Union::TestDelay(t)) => {
                    client::latency::on_test_delay(
                        self.video_threads.values().map(|v| client::latency::stream_key(&v.video_queue)),
                        &t,
                    );
                    self.handler.handle_test_delay(t, peer).await;
                }
                Some(message::Union::AudioFrame(frame)) => {
                    if !self.handler.lc.read().unwrap().disable_audio.v {
                        self.audio_sender
                            .send(MediaData::AudioFrame(Box::new(frame)))
                            .ok();
                    }
                }
                Some(message::Union::FileAction(action)) => match action.union {
                    Some(file_action::Union::Send(_s)) => match _s.file_type.enum_value() {
                        #[cfg(target_os = "windows")]
                        Ok(file_transfer_send_request::FileType::Printer) => {
                            #[cfg(feature = "flutter")]
                            let action =
                                LocalConfig::get_option(keys::OPTION_PRINTER_INCOMING_JOB_ACTION);
                            #[cfg(not(feature = "flutter"))]
                            let action = "";
                            if action == "dismiss" {
                                // Just ignore the incoming print job.
                            } else {
                                let id = fs::get_next_job_id();
                                #[cfg(feature = "flutter")]
                                let allow_auto_print = LocalConfig::get_bool_option(
                                    keys::OPTION_PRINTER_ALLOW_AUTO_PRINT,
                                );
                                #[cfg(not(feature = "flutter"))]
                                let allow_auto_print = false;
                                if allow_auto_print {
                                    let printer_name = if action == "" {
                                        "".to_string()
                                    } else {
                                        LocalConfig::get_option(keys::OPTION_PRINTER_SELECTED_NAME)
                                    };
                                    self.handler.printer_response(id, _s.path, printer_name);
                                } else {
                                    self.handler.printer_request(id, _s.path);
                                }
                            }
                        }
                        _ => {}
                    },
                    Some(file_action::Union::SendConfirm(c)) => {
                        if let Some(job) = fs::get_job(c.id, &mut self.read_jobs) {
                            job.confirm(&c).await;
                        }
                    }
                    _ => {}
                },
                Some(message::Union::MessageBox(msgbox)) => {
                    let mut link = msgbox.link;
                    if let Some(v) = config::HELPER_URL.get(&link as &str) {
                        link = v.to_string();
                    } else {
                        log::warn!("Message box ignore link {} for security", &link);
                        link = "".to_string();
                    }
                    self.handler
                        .msgbox(&msgbox.msgtype, &msgbox.title, &msgbox.text, &link);
                }
                Some(message::Union::VoiceCallRequest(request)) => {
                    if self.microphone_id.is_some() { return true; }
                    if request.is_connect {
                        // TODO: maybe we will do a voice call from the peer in the future.
                    } else {
                        log::debug!("The remote has requested to close the voice call");
                        if let Some(sender) = self.stop_voice_call_sender.take() {
                            allow_err!(sender.send(()));
                            self.handler.on_voice_call_closed("");
                        }
                    }
                }
                Some(message::Union::VoiceCallResponse(response)) => {
                    let ts = std::mem::replace(&mut self.voice_call_request_timestamp, None);
                    if let Some(ts) = ts {
                        if response.req_timestamp != ts.get() {
                            log::debug!("Possible encountering a voice call attack.");
                        } else {
                            if response.accepted {
                                // The peer accepted the voice call.
                                self.handler.on_voice_call_started();
                                self.stop_voice_call_sender = self.start_voice_call();
                            } else {
                                // The peer refused the voice call.
                                self.handler.on_voice_call_closed("");
                            }
                        }
                    }
                }
                Some(message::Union::PeerInfo(pi)) => {
                    self.handler.set_displays(&pi.displays);
                    self.handler.set_platform_additions(&pi.platform_additions);
                }
                Some(message::Union::ScreenshotResponse(response)) => {
                    crate::client::screenshot::set_screenshot(response.data);
                    self.handler
                        .handle_screenshot_resp(response.sid, response.msg);
                }
                Some(message::Union::TerminalResponse(response)) => {
                    use base::message_proto::terminal_response::Union;
                    if let Some(Union::Opened(opened)) = &response.union {
                        if opened.success && !opened.service_id.is_empty() {
                            let mut lc = self.handler.lc.write().unwrap();
                            let key = lc.get_key_terminal_service_id().to_owned();
                            lc.set_option(key, opened.service_id.clone());
                        }
                    }
                    self.handler.handle_terminal_response(response);
                }
                _ => {}
            }
        }
        true
    }

    fn set_peer_info(&mut self, pi: &PeerInfo) {
        self.peer_info.platform = pi.platform.clone();

        // Check features field for terminal support
        if let Some(features) = pi.features.as_ref() {
            self.peer_info.support_terminal = features.terminal;
        }

        if let Ok(platform_additions) =
            serde_json::from_str::<HashMap<String, serde_json::Value>>(&pi.platform_additions)
        {
            self.peer_info.is_installed = platform_additions
                .get("is_installed")
                .map(|v| v.as_bool())
                .flatten()
                .unwrap_or(false);
            self.peer_info.idd_impl = platform_additions
                .get("idd_impl")
                .map(|v| v.as_str())
                .flatten()
                .unwrap_or_default()
                .to_string();
            self.peer_info.support_view_camera = platform_additions
                .get("support_view_camera")
                .map(|v| v.as_bool())
                .flatten()
                .unwrap_or(false);
        }
    }

    async fn handle_back_notification(&mut self, notification: BackNotification) -> bool {
        match notification.union {
            Some(back_notification::Union::BlockInputState(state)) => {
                self.handle_back_msg_block_input(
                    state.enum_value_or(back_notification::BlockInputState::BlkStateUnknown),
                    notification.details,
                )
                .await;
            }
            Some(back_notification::Union::PrivacyModeState(state)) => {
                if !self
                    .handle_back_msg_privacy_mode(
                        state.enum_value_or(back_notification::PrivacyModeState::PrvStateUnknown),
                        notification.details,
                        notification.impl_key,
                    )
                    .await
                {
                    return false;
                }
            }
            _ => {}
        }
        true
    }

    #[inline(always)]
    fn update_block_input_state(&mut self, on: bool) {
        self.handler.update_block_input_state(on);
    }

    async fn handle_back_msg_block_input(
        &mut self,
        state: back_notification::BlockInputState,
        details: String,
    ) {
        match state {
            back_notification::BlockInputState::BlkOnSucceeded => {
                self.update_block_input_state(true);
            }
            back_notification::BlockInputState::BlkOnFailed => {
                self.handler.msgbox(
                    "custom-error",
                    "Block user input",
                    if details.is_empty() {
                        "Failed"
                    } else {
                        &details
                    },
                    "",
                );
                self.update_block_input_state(false);
            }
            back_notification::BlockInputState::BlkOffSucceeded => {
                self.update_block_input_state(false);
            }
            back_notification::BlockInputState::BlkOffFailed => {
                self.handler.msgbox(
                    "custom-error",
                    "Unblock user input",
                    if details.is_empty() {
                        "Failed"
                    } else {
                        &details
                    },
                    "",
                );
            }
            _ => {}
        }
    }

    #[inline(always)]
    fn update_privacy_mode(&mut self, impl_key: String, on: bool) {
        let mut config = self.handler.load_config();
        config.privacy_mode.v = on;
        if on {
            // For compatibility, version < 1.2.4, the default value is 'privacy_mode_impl_mag'.
            let impl_key = if impl_key.is_empty() {
                "privacy_mode_impl_mag".to_string()
            } else {
                impl_key
            };
            config
                .options
                .insert("privacy-mode-impl-key".to_string(), impl_key);
        }
        self.handler.save_config(config);

        self.handler.update_privacy_mode();
    }

    async fn handle_back_msg_privacy_mode(
        &mut self,
        state: back_notification::PrivacyModeState,
        details: String,
        impl_key: String,
    ) -> bool {
        match state {
            back_notification::PrivacyModeState::PrvOnByOther => {
                self.handler.msgbox(
                    "error",
                    "Connecting...",
                    "Someone turns on privacy mode, exit",
                    "",
                );
                return false;
            }
            back_notification::PrivacyModeState::PrvNotSupported => {
                self.handler
                    .msgbox("custom-error", "Privacy mode", "Unsupported", "");
                self.update_privacy_mode(impl_key, false);
            }
            back_notification::PrivacyModeState::PrvOnSucceeded => {
                self.handler
                    .msgbox("custom-nocancel", "Privacy mode", "Enter privacy mode", "");
                self.update_privacy_mode(impl_key, true);
            }
            back_notification::PrivacyModeState::PrvOnFailedDenied => {
                self.handler
                    .msgbox("custom-error", "Privacy mode", "Peer denied", "");
                self.update_privacy_mode(impl_key, false);
            }
            back_notification::PrivacyModeState::PrvOnFailedPlugin
            | back_notification::PrivacyModeState::PrvOnFailed => {
                self.handler.msgbox(
                    "custom-error",
                    "Privacy mode",
                    if details.is_empty() {
                        "Failed"
                    } else {
                        &details
                    },
                    "",
                );
                self.update_privacy_mode(impl_key, false);
            }
            back_notification::PrivacyModeState::PrvOffSucceeded => {
                self.handler
                    .msgbox("custom-nocancel", "Privacy mode", "Exit privacy mode", "");
                self.update_privacy_mode(impl_key, false);
            }
            back_notification::PrivacyModeState::PrvOffByPeer => {
                self.handler
                    .msgbox("custom-error", "Privacy mode", "Peer exit", "");
                self.update_privacy_mode(impl_key, false);
            }
            back_notification::PrivacyModeState::PrvOffFailed => {
                self.handler.msgbox(
                    "custom-error",
                    "Privacy mode",
                    if details.is_empty() {
                        "Failed to turn off"
                    } else {
                        &details
                    },
                    "",
                );
            }
            back_notification::PrivacyModeState::PrvOffUnknown => {
                self.handler
                    .msgbox("custom-error", "Privacy mode", "Turned off", "");
                // log::error!("Privacy mode is turned off with unknown reason");
                self.update_privacy_mode(impl_key, false);
            }
            _ => {}
        }
        true
    }

    #[cfg(all(target_os = "windows", not(feature = "flutter")))]
    fn check_clipboard_file_context(&self) {
        let enabled = *self.handler.server_file_transfer_enabled.read().unwrap()
            && self.handler.lc.read().unwrap().enable_file_copy_paste.v;
        ContextSend::enable(enabled);
    }

    #[cfg(any(target_os = "windows", feature = "unix-file-copy-paste"))]
    async fn handle_cliprdr_msg(&mut self, clip: base::message_proto::Cliprdr, _peer: &mut Stream) {
        if !self.is_connected {
            log::debug!("Discarding incoming file clipboard message before login");
            return;
        }
        log::debug!("handling cliprdr msg from server peer");
        #[cfg(feature = "flutter")]
        if let Some(base::message_proto::cliprdr::Union::FormatList(_)) = &clip.union {
            if self.client_conn_id
                != clipboard::get_client_conn_id(&crate::flutter::get_cur_peer_id()).unwrap_or(0)
            {
                return;
            }
        }

        let Some(clip) = crate::clipboard_file::msg_2_clip(clip) else {
            log::warn!("failed to decode cliprdr msg from server peer");
            return;
        };

        let is_stopping_allowed = clip.is_beginning_message();
        let file_transfer_enabled = self.handler.is_file_clipboard_required();
        let stop = is_stopping_allowed && !file_transfer_enabled;
        log::debug!(
                "Process clipboard message from server peer, stop: {}, is_stopping_allowed: {}, file_transfer_enabled: {}",
                stop, is_stopping_allowed, file_transfer_enabled);
        if !stop {
            #[cfg(any(
                target_os = "windows",
                all(target_os = "macos", feature = "unix-file-copy-paste")
            ))]
            if let Err(e) = ContextSend::make_sure_enabled() {
                log::error!("failed to restart clipboard context: {}", e);
            };
            #[cfg(target_os = "windows")]
            {
                if matches!(&clip, clipboard::ClipboardFile::FormatList { .. }) {
                    self.initial_clipboard_pending = false;
                }
                let _ = ContextSend::proc(|context| -> ResultType<()> {
                    context
                        .server_clip_file(self.client_conn_id, clip)
                        .map_err(|e| e.into())
                });
            }
            #[cfg(feature = "unix-file-copy-paste")]
            if crate::is_support_file_copy_paste_num(self.handler.lc.read().unwrap().version) {
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                if matches!(&clip, clipboard::ClipboardFile::FormatList { .. }) {
                    self.initial_clipboard_pending = false;
                }
                let mut out_msgs = vec![];

                #[cfg(target_os = "macos")]
                if clipboard::platform::unix::macos::should_handle_msg(&clip) {
                    if let Err(e) = ContextSend::proc(|context| -> ResultType<()> {
                        context
                            .server_clip_file(self.client_conn_id, clip)
                            .map_err(|e| e.into())
                    }) {
                        log::error!("failed to handle cliprdr msg: {}", e);
                    }
                } else {
                    out_msgs = unix_file_clip::serve_clip_messages(
                        ClipboardSide::Client,
                        clip,
                        self.client_conn_id,
                    );
                }

                #[cfg(not(target_os = "macos"))]
                {
                    out_msgs = unix_file_clip::serve_clip_messages(
                        ClipboardSide::Client,
                        clip,
                        self.client_conn_id,
                    );
                }

                for msg in out_msgs.into_iter() {
                    allow_err!(_peer.send(&msg).await);
                }
            }
        }
    }

    fn new_video_thread(&mut self, display: usize) {
        let video_queue = Arc::new(RwLock::new(ArrayQueue::new(client::VIDEO_QUEUE_SIZE)));
        let (video_sender, video_receiver) = std::sync::mpsc::channel::<MediaData>();
        let decode_fps = Arc::new(RwLock::new(None));
        let frame_count = Arc::new(RwLock::new(0));
        let discard_queue = Arc::new(RwLock::new(false));
        let counters: Arc<client::VideoCounters> = Default::default();
        let pacing = Arc::new(client::PresentPacing::new());
        let video_thread = VideoThread {
            video_queue: video_queue.clone(),
            video_sender,
            decode_fps: decode_fps.clone(),
            frame_count: frame_count.clone(),
            fps_control: Default::default(),
            discard_queue: discard_queue.clone(),
            counters: counters.clone(),
            pacing: pacing.clone(),
        };
        let handler = self.handler.ui_handler.clone();
        crate::client::start_video_thread(
            self.handler.clone(),
            display,
            video_receiver,
            video_queue,
            decode_fps,
            self.chroma.clone(),
            discard_queue,
            counters,
            pacing,
            move |display: usize,
                  data: &mut scrap::ImageRgb,
                  _texture: *mut c_void,
                  pixelbuffer: bool| {
                *frame_count.write().unwrap() += 1;
                if pixelbuffer {
                    handler.on_rgba(display, data);
                } else {
                    #[cfg(all(feature = "vram", feature = "flutter"))]
                    handler.on_texture(display, _texture);
                }
            },
        );
        self.video_threads.insert(display, video_thread);
        if self.video_threads.len() == 1 {
            let auto_record = LocalConfig::get_bool_option(keys::OPTION_ALLOW_AUTO_RECORD_OUTGOING);
            self.handler.lc.write().unwrap().record_state = auto_record;
            self.update_record_state();
        }
    }

    fn update_record_state(&mut self) {
        // state
        let permission = self.handler.lc.read().unwrap().record_permission;
        if !permission {
            self.handler.lc.write().unwrap().record_state = false;
        }
        let state = self.handler.lc.read().unwrap().record_state;
        let start = state && permission;
        if self.last_record_state == start {
            return;
        }
        self.last_record_state = start;
        log::info!("record screen start: {start}");
        // update local
        for (_, v) in self.video_threads.iter_mut() {
            v.video_sender.send(MediaData::RecordScreen(start)).ok();
        }
        self.handler.update_record_status(start);
        // update remote
        let mut misc = Misc::new();
        misc.set_client_record_status(start);
        let mut msg = Message::new();
        msg.set_misc(misc);
        self.sender.send(Data::Message(msg)).ok();
    }

    /// A shape that decoded, for the UI to ask for again; see `Session::cursor_shapes`.
    #[cfg(feature = "flutter")]
    fn keep_cursor_shape(&self, shape: CursorData) {
        self.handler
            .cursor_shapes
            .write()
            .unwrap()
            .insert(shape.id, shape);
    }

    fn dedupes_cursors(&self) -> bool {
        !crate::is_peer_naming_cursors_by_content(self.handler.lc.read().unwrap().version)
    }

    /// A shape the UI already has, under whatever handle, is only selected.
    fn set_cursor_data_by_content(&mut self, cd: CursorData) {
        let peer_id = cd.id;
        let id = CursorDedupe::name(&cd);
        if self.cursor_dedupe.has_decoded(id) {
            self.cursor_dedupe.record(peer_id, id);
            self.handler.set_cursor_id(id.to_string());
            return;
        }
        #[cfg(feature = "flutter")]
        let compressed = cd.colors.clone();
        match decode_cursor_data(cd) {
            Ok(mut cd) => {
                cd.id = id;
                self.cursor_dedupe.record(peer_id, id);
                #[cfg(feature = "flutter")]
                self.keep_cursor_shape(cursor_shape(id, &cd, &compressed));
                self.handler.set_cursor_data(cd);
            }
            Err(err) => {
                log::warn!("Rejected cursor {peer_id}: {err}");
                self.handler
                    .set_cursor_id(self.cursor_dedupe.rejected(peer_id, id).to_string());
            }
        }
    }
}

/// A shape that decoded, still compressed, under the id the UI knows it by. Copied once it
/// decoded: the message's colors may be a slice of a larger received buffer, which a clone
/// would keep alive.
#[cfg(any(feature = "flutter", test))]
fn cursor_shape(id: u64, cd: &CursorData, compressed: &[u8]) -> CursorData {
    CursorData {
        id,
        hotx: cd.hotx,
        hoty: cd.hoty,
        width: cd.width,
        height: cd.height,
        colors: compressed.to_vec().into(),
        ..Default::default()
    }
}

/// The RGBA of a shape kept by `Session::cursor_shapes`, for the UI to draw it again.
#[cfg(any(feature = "flutter", test))]
pub(crate) fn kept_cursor_rgba(shape: CursorData) -> hbb_common::ResultType<CursorData> {
    decode_cursor_data(shape)
}

// Both UI handlers receive validated, uncompressed RGBA from the receive loop.
fn decode_cursor_data(data: CursorData) -> hbb_common::ResultType<CursorData> {
    use hbb_common::{anyhow::anyhow, bail};

    // Limit decoded cursor data to 1 MiB before JSON serialization.
    const MAX_CURSOR_SIZE: i32 = 512;
    const RGBA_CHANNELS: usize = 4;

    let mut cd = data;
    if !(1..=MAX_CURSOR_SIZE).contains(&cd.width) || !(1..=MAX_CURSOR_SIZE).contains(&cd.height) {
        bail!("invalid source size {}x{}", cd.width, cd.height);
    }
    if !(0..cd.width).contains(&cd.hotx) || !(0..cd.height).contains(&cd.hoty) {
        bail!(
            "hotspot ({},{}) is outside the cursor image",
            cd.hotx,
            cd.hoty
        );
    }
    let expected = (cd.width as usize)
        .checked_mul(cd.height as usize)
        .and_then(|pixels| pixels.checked_mul(RGBA_CHANNELS))
        .ok_or_else(|| anyhow!("cursor RGBA size overflow"))?;
    let max_compressed_size = zstd::zstd_safe::compress_bound(expected);
    if cd.colors.len() > max_compressed_size {
        bail!(
            "compressed cursor data too large: {} bytes (limit {max_compressed_size})",
            cd.colors.len()
        );
    }
    let colors = zstd::bulk::decompress(&cd.colors, expected)?;
    if colors.len() != expected {
        bail!(
            "invalid RGBA length: expected {expected}, got {}",
            colors.len()
        );
    }
    cd.colors = colors.into();
    Ok(cd)
}

/// Gives the UI one id per shape for a peer that names shapes by handle, so the UI decodes and
/// keeps a shape once however many handles it arrives under. Nothing is dropped for the
/// connection: the peer sends a shape once and may select any handle it named again.
#[derive(Default)]
struct CursorDedupe {
    ids: HashMap<u64, u64>,
    // The shapes given to the UI, by content id: whether one that arrives needs decoding.
    decoded: HashSet<u64>,
}

impl CursorDedupe {
    /// Hashes the compressed colors: one peer compresses the same pixels to the same bytes, and
    /// a shape seen before is then never decompressed again.
    fn name(cd: &CursorData) -> u64 {
        crate::cursor_content_id(cd.width, cd.height, cd.hotx, cd.hoty, &cd.colors)
    }

    fn id(&self, peer_id: u64) -> u64 {
        self.ids.get(&peer_id).copied().unwrap_or(peer_id)
    }

    fn has_decoded(&self, id: u64) -> bool {
        self.decoded.contains(&id)
    }

    /// Names the handle once its shape decoded, now or before.
    fn record(&mut self, peer_id: u64, id: u64) {
        self.ids.insert(peer_id, id);
        self.decoded.insert(id);
    }

    /// Names a known handle by the content of a shape that did not decode, which is never
    /// marked decoded: the UI has no shape under it, now or when the handle is selected again,
    /// and the shape the handle named before is not shown in its place. A handle never seen is
    /// not filed, so what a peer sends that does not decode takes no room.
    fn rejected(&mut self, peer_id: u64, id: u64) -> u64 {
        if let Some(named) = self.ids.get_mut(&peer_id) {
            *named = id;
        }
        id
    }
}

struct RemoveJob {
    files: Vec<FileEntry>,
    path: String,
    sep: &'static str,
    is_remote: bool,
    no_confirm: bool,
    last_update_job_status: Instant,
}

impl RemoveJob {
    fn new(files: Vec<FileEntry>, path: String, sep: &'static str, is_remote: bool) -> Self {
        Self {
            files,
            path,
            sep,
            is_remote,
            no_confirm: false,
            last_update_job_status: Instant::now(),
        }
    }

    pub fn _gen_meta(&self) -> RemoveJobMeta {
        RemoveJobMeta {
            path: self.path.clone(),
            is_remote: self.is_remote,
            no_confirm: self.no_confirm,
        }
    }
}

/// Shortest gap between two key-frame refreshes after queue overflows (gentle fps control). A
/// refresh restarts the host encoder for every viewer, so not every overflow gets one; a longer
/// gap would freeze the display for that long.
const KEY_FRAME_REFRESH_SPACING: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
struct FpsControl {
    refresh_times: usize,
    last_refresh_instant: Option<Instant>,
    idle_counter: usize,
    inactive_counter: usize,
    // Gentle fps control: a queue overflow evicted a frame while `refresh_allowed` refused the
    // refresh. The queue is discarded until a key frame arrives (cleared then), and the refresh
    // goes out once `key_frame_refresh_allowed`.
    key_frame_pending: bool,
}

impl FpsControl {
    /// The legacy controller's refresh limit: at most 20 per display, 10 s apart.
    fn refresh_allowed(&self) -> bool {
        self.refresh_times < 20
            && (self.refresh_times == 0
                || self
                    .last_refresh_instant
                    .map_or(false, |t| t.elapsed().as_secs() > 10))
    }

    /// The limit for a refresh that a frame evicted by a queue overflow needs: spaced by
    /// `KEY_FRAME_REFRESH_SPACING`, not capped at 20, as the stream cannot be decoded again
    /// without a key frame. In between, the display freezes (queue discarded).
    fn key_frame_refresh_allowed(&self) -> bool {
        self.last_refresh_instant
            .map_or(true, |t| t.elapsed() >= KEY_FRAME_REFRESH_SPACING)
    }

    fn on_refresh(&mut self) {
        self.refresh_times += 1;
        self.last_refresh_instant = Some(Instant::now());
    }
}

/// Whether gentle fps control is on (`enable-gentle-fps-control`): by default on Windows ("N"
/// turns it off), off elsewhere unless "Y". Read once per connection.
fn gentle_fps_control_enabled() -> bool {
    let option = LocalConfig::get_option(keys::OPTION_ENABLE_GENTLE_FPS_CONTROL);
    if cfg!(windows) {
        option != "N"
    } else {
        option == "Y"
    }
}

// Gentle fps control constants (docs/LATENCY-METRICS.md, "Viewer: fps control").
// The soft floor is min(custom fps, GENTLE_FLOOR); below it, down to min(custom fps,
// GENTLE_HARD_FLOOR), only after GENTLE_BELOW_FLOOR_TICKS ticks of capacity under it.
const GENTLE_FLOOR: usize = 24;
const GENTLE_HARD_FLOOR: usize = 10;
const GENTLE_BELOW_FLOOR_TICKS: usize = 10;
// Decoder thread busy fraction (dequeue to after the UI callback, over the tick) above which it
// is overloaded, and below which it is calm.
const GENTLE_BUSY_OVERLOAD: f64 = 0.9;
const GENTLE_BUSY_CALM: f64 = 0.75;
// Overload: GENTLE_DECREASE_TICKS overloaded ticks in a row and GENTLE_DECREASE_GAP_MS since
// the last change, then x GENTLE_DECREASE_FACTOR.
const GENTLE_DECREASE_TICKS: usize = 2;
const GENTLE_DECREASE_GAP_MS: u64 = 2000;
const GENTLE_DECREASE_FACTOR: f64 = 0.85;
// Backlog: fewer frames taken from the queue than received, and the median queue length
// growing, for this many ticks in a row.
const GENTLE_BACKLOG_TICKS: usize = 3;
// Calm: GENTLE_INCREASE_TICKS calm ticks in a row and GENTLE_INCREASE_GAP_MS since the last
// change, then + max(GENTLE_INCREASE_MIN, 10%), at most the custom fps. A calm display also has
// capacity for GENTLE_CALM_HEADROOM x the current fps.
const GENTLE_INCREASE_TICKS: usize = 5;
const GENTLE_INCREASE_GAP_MS: u64 = 3000;
const GENTLE_INCREASE_MIN: usize = 3;
const GENTLE_CALM_HEADROOM: f64 = 1.2;
// Lasting present overload: shown fps under GENTLE_PRESENT_LOW x min(current fps, received
// fps) for GENTLE_PRESENT_LOW_TICKS ticks, then ask for GENTLE_PRESENT_HEADROOM x shown fps.
const GENTLE_PRESENT_LOW: f64 = 0.7;
const GENTLE_PRESENT_LOW_TICKS: usize = 10;
const GENTLE_PRESENT_HEADROOM: f64 = 1.2;
// Start-up: no request before this many ticks and steady (non-key, past warm-up) frames.
const GENTLE_MIN_TICKS: usize = 3;
const GENTLE_MIN_SAMPLES: usize = 30;
// A display without decoded frames for this many ticks is ignored (as the legacy controller).
const GENTLE_INACTIVE_TICKS: usize = 15;
// Capacity: EWMA over ticks with at least GENTLE_CAP_MIN_FRAMES steady frames.
const GENTLE_CAP_ALPHA: f64 = 0.3;
const GENTLE_CAP_MIN_FRAMES: usize = 5;
// Last-resort refresh: queue_max above gentle_refresh_queue() for this many ticks in a row.
const GENTLE_REFRESH_TICKS: usize = 3;

fn gentle_refresh_queue() -> usize {
    std::cmp::max(30, client::VIDEO_QUEUE_SIZE / 4)
}

/// Per-display state of `GentleFpsControl`.
#[derive(Debug, Default)]
struct GentleDisplay {
    ticks: usize,
    samples: usize,
    inactive_ticks: usize,
    // Frames per busy second of steady frames, EWMA over ticks.
    cap: Option<f64>,
    busy: f64,
    queue_p50: usize,
    // Ticks in a row with a growing backlog, and the median queue length before them.
    growth_ticks: usize,
    growth_from: usize,
    big_queue_ticks: usize,
    ready: bool,
    overloaded: bool,
    present_low: bool,
}

/// Gentle fps control: the fps the viewer asks the host for (`custom_fps` option message, as
/// the legacy controller) follows the decoder thread's measured load. A state machine fed once
/// per status tick with each display's `VideoCountersTick`.
#[derive(Debug)]
struct GentleFpsControl {
    enabled: bool,
    displays: HashMap<usize, GentleDisplay>,
    ticks: usize,
    custom: Option<usize>,
    // The fps last asked for; None while the host runs at the custom fps.
    target: Option<usize>,
    since_change_ms: u64,
    overload_ticks: usize,
    calm_ticks: usize,
    below_floor_ticks: usize,
    present_low_ticks: usize,
    present_low_shown_sum: f64,
}

/// What one `GentleFpsControl::tick` saw and decided.
#[derive(Debug, Default)]
struct GentleTick {
    custom: usize,
    target: usize,
    floor: usize,
    cap: Option<f64>,
    overload_ticks: usize,
    calm_ticks: usize,
    below_floor_ticks: usize,
    present_low_ticks: usize,
    since_change_ms: u64,
    decision: &'static str,
    send: Option<usize>,
    refresh: Vec<usize>,
}

impl GentleTick {
    fn summary(&self) -> String {
        format!(
            "custom={} target={} floor={} cap={} overload={}/{} calm={}/{} below_floor={}/{} present_low={}/{} since_change_ms={} decision={} sent={}",
            self.custom,
            self.target,
            self.floor,
            self.cap.map_or("-".to_owned(), |c| format!("{:.1}", c)),
            self.overload_ticks,
            GENTLE_DECREASE_TICKS,
            self.calm_ticks,
            GENTLE_INCREASE_TICKS,
            self.below_floor_ticks,
            GENTLE_BELOW_FLOOR_TICKS,
            self.present_low_ticks,
            GENTLE_PRESENT_LOW_TICKS,
            self.since_change_ms,
            self.decision,
            self.send.map_or("-".to_owned(), |f| f.to_string()),
        )
    }
}

impl GentleFpsControl {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            displays: Default::default(),
            ticks: 0,
            custom: None,
            target: None,
            since_change_ms: 0,
            overload_ticks: 0,
            calm_ticks: 0,
            below_floor_ticks: 0,
            present_low_ticks: 0,
            present_low_shown_sum: 0.0,
        }
    }

    /// A `custom_fps` option message on its way to the host: the controller's own request, or
    /// the user's custom fps (sent again even when unchanged), after which the host runs at the
    /// latter. Returns true when it was the user's, while the controller had asked for less.
    fn on_custom_fps_sent(&mut self, fps: usize) -> bool {
        if !self.enabled || self.target.map_or(true, |target| target == fps) {
            return false;
        }
        self.target = None;
        self.since_change_ms = 0;
        self.reset_streaks();
        true
    }

    fn reset_streaks(&mut self) {
        self.overload_ticks = 0;
        self.calm_ticks = 0;
        self.below_floor_ticks = 0;
        self.present_low_ticks = 0;
        self.present_low_shown_sum = 0.0;
    }

    /// One status tick of `tick_ms` with each display's counters. `custom` is the user's fps
    /// (already defaulted to 30 like the legacy controller).
    fn tick(
        &mut self,
        custom: usize,
        tick_ms: u64,
        counts: &[(usize, client::VideoCountersTick)],
    ) -> GentleTick {
        self.ticks += 1;
        self.since_change_ms = self.since_change_ms.saturating_add(tick_ms);
        if self.custom != Some(custom) {
            if self.custom.is_some() {
                // The user changed it, and the host was just told: it runs at `custom` now.
                self.target = None;
                self.since_change_ms = 0;
                self.reset_streaks();
            }
            self.custom = Some(custom);
        }
        let target = self.target.unwrap_or(custom).min(custom);
        let floor = GENTLE_FLOOR.min(custom);
        let hard_floor = GENTLE_HARD_FLOOR.min(custom);
        let secs = tick_ms.max(1) as f64 / 1000.0;
        let target_f = target as f64;
        let refresh_queue = gentle_refresh_queue();
        self.displays
            .retain(|d, _| counts.iter().any(|(display, _)| display == d));
        let mut any_ready = false;
        let mut overloaded = false;
        // Calm: every display with enough frames to tell is calm, and there is one.
        let mut calm_all = true;
        let mut calm_some = false;
        let mut cap_min: Option<f64> = None;
        // The smallest shown fps over displays with a present overload.
        let mut shown_min: Option<f64> = None;
        let mut refresh = Vec::new();
        for (display, c) in counts {
            let st = self.displays.entry(*display).or_default();
            st.ticks += 1;
            st.samples = st.samples.saturating_add(c.steady_decoded);
            if c.decoded == 0 {
                st.inactive_ticks += 1;
            } else {
                st.inactive_ticks = 0;
            }
            if c.steady_decoded >= GENTLE_CAP_MIN_FRAMES && c.steady_busy_us > 0 {
                let cap = c.steady_decoded as f64 * 1e6 / c.steady_busy_us as f64;
                st.cap = Some(st.cap.map_or(cap, |e| e + GENTLE_CAP_ALPHA * (cap - e)));
            }
            st.busy = c.busy_us as f64 / (secs * 1e6);
            // The queue grows when fewer frames leave it than arrive; its median (not its
            // maximum, which Wi-Fi bursts make large) must grow too.
            let taken = c.decoded + c.failed + c.discarded;
            if taken < c.received && c.queue_p50 > 0 && c.queue_p50 >= st.queue_p50 {
                if st.growth_ticks == 0 {
                    st.growth_from = st.queue_p50;
                }
                st.growth_ticks += 1;
            } else {
                st.growth_ticks = 0;
            }
            st.queue_p50 = c.queue_p50;
            let backlog =
                st.growth_ticks >= GENTLE_BACKLOG_TICKS && c.queue_p50 > st.growth_from;
            if c.queue_max > refresh_queue {
                st.big_queue_ticks += 1;
            } else {
                st.big_queue_ticks = 0;
            }
            if st.big_queue_ticks >= GENTLE_REFRESH_TICKS && self.ticks >= GENTLE_MIN_TICKS {
                st.big_queue_ticks = 0;
                refresh.push(*display);
            }
            st.ready = st.ticks >= GENTLE_MIN_TICKS
                && st.samples >= GENTLE_MIN_SAMPLES
                && st.inactive_ticks < GENTLE_INACTIVE_TICKS;
            if !st.ready {
                st.overloaded = false;
                st.present_low = false;
                continue;
            }
            any_ready = true;
            let rx = c.received as f64 / secs;
            let dec = c.decoded as f64 / secs;
            // Frames that reached the screen (soft-render drops and replacements excluded).
            let shown = c.displayed() as f64 / secs;
            st.overloaded = c.decoded >= 2 && (st.busy > GENTLE_BUSY_OVERLOAD || backlog);
            // Frames arrive (not a static screen) but most of them are not shown.
            st.present_low =
                rx >= target_f * 0.5 && shown < GENTLE_PRESENT_LOW * target_f.min(rx);
            overloaded |= st.overloaded;
            if st.present_low {
                shown_min = Some(shown_min.map_or(shown, |m: f64| m.min(shown)));
            }
            if let Some(cap) = st.cap {
                cap_min = Some(cap_min.map_or(cap, |m: f64| m.min(cap)));
            }
            if dec >= target_f * 0.5 {
                let calm = !st.overloaded
                    && !st.present_low
                    && st.busy < GENTLE_BUSY_CALM
                    && c.queue_p50 <= 1
                    && shown >= rx * 0.9
                    && st
                        .cap
                        .map_or(false, |cap| cap >= target_f * GENTLE_CALM_HEADROOM);
                calm_all &= calm;
                calm_some |= calm;
            }
        }
        let calm = calm_all && calm_some && !overloaded && shown_min.is_none();
        let mut out = GentleTick {
            custom,
            target,
            floor,
            cap: cap_min,
            decision: "keep",
            refresh,
            ..Default::default()
        };
        if self.ticks < GENTLE_MIN_TICKS || !any_ready {
            self.reset_streaks();
            out.decision = "warmup";
            out.since_change_ms = self.since_change_ms;
            return out;
        }
        self.overload_ticks = if overloaded { self.overload_ticks + 1 } else { 0 };
        self.calm_ticks = if calm { self.calm_ticks + 1 } else { 0 };
        self.below_floor_ticks = match cap_min {
            Some(cap) if cap < floor as f64 => self.below_floor_ticks + 1,
            _ => 0,
        };
        match shown_min {
            Some(shown) => {
                self.present_low_ticks += 1;
                self.present_low_shown_sum += shown;
            }
            None => {
                self.present_low_ticks = 0;
                self.present_low_shown_sum = 0.0;
            }
        }
        out.overload_ticks = self.overload_ticks;
        out.calm_ticks = self.calm_ticks;
        out.below_floor_ticks = self.below_floor_ticks;
        out.present_low_ticks = self.present_low_ticks;
        let mut new = None;
        if self.overload_ticks >= GENTLE_DECREASE_TICKS
            && self.since_change_ms >= GENTLE_DECREASE_GAP_MS
        {
            self.overload_ticks = 0;
            let mut lowest = floor;
            if self.below_floor_ticks >= GENTLE_BELOW_FLOOR_TICKS {
                if let Some(cap) = cap_min {
                    lowest = ((cap * 0.9).round() as usize).clamp(hard_floor, floor);
                }
            }
            let fps = lowest.max((target_f * GENTLE_DECREASE_FACTOR).round() as usize);
            if fps < target {
                new = Some(fps);
                out.decision = if fps < floor {
                    "decrease-below-floor"
                } else {
                    "decrease"
                };
            } else {
                out.decision = "at-floor";
            }
        } else if self.present_low_ticks >= GENTLE_PRESENT_LOW_TICKS
            && self.since_change_ms >= GENTLE_DECREASE_GAP_MS
        {
            let shown = self.present_low_shown_sum / self.present_low_ticks as f64;
            self.present_low_ticks = 0;
            self.present_low_shown_sum = 0.0;
            let fps = floor.max((shown * GENTLE_PRESENT_HEADROOM).round() as usize);
            if fps < target {
                new = Some(fps);
                out.decision = "present";
            } else {
                out.decision = "present-at-floor";
            }
        } else if self.calm_ticks >= GENTLE_INCREASE_TICKS
            && self.since_change_ms >= GENTLE_INCREASE_GAP_MS
            && target < custom
        {
            self.calm_ticks = 0;
            new = Some(custom.min(target + GENTLE_INCREASE_MIN.max(target / 10)));
            out.decision = "increase";
        }
        if let Some(fps) = new {
            self.target = Some(fps);
            self.since_change_ms = 0;
            out.send = Some(fps);
        }
        out.since_change_ms = self.since_change_ms;
        out
    }

    /// The per-display state for the log lines.
    fn describe_displays(&self) -> String {
        let mut displays = self.displays.iter().collect::<Vec<_>>();
        displays.sort_unstable_by_key(|(d, _)| **d);
        let mut out = String::new();
        for (display, st) in displays {
            out += &format!(
                " d{} ready={} busy={:.2} cap={} queue_p50={} grow={} big_queue={} overload={} present_low={};",
                display,
                st.ready as u8,
                st.busy,
                st.cap.map_or("-".to_owned(), |c| format!("{:.1}", c)),
                st.queue_p50,
                st.growth_ticks,
                st.big_queue_ticks,
                st.overloaded as u8,
                st.present_low as u8,
            );
        }
        out
    }
}

struct VideoThread {
    video_queue: Arc<RwLock<ArrayQueue<VideoFrame>>>,
    video_sender: MediaSender,
    decode_fps: Arc<RwLock<Option<usize>>>,
    frame_count: Arc<RwLock<usize>>,
    discard_queue: Arc<RwLock<bool>>,
    fps_control: FpsControl,
    counters: Arc<client::VideoCounters>,
    pacing: Arc<client::PresentPacing>,
}

impl Drop for VideoThread {
    fn drop(&mut self) {
        // since channels are buffered, messages sent before the disconnect will still be properly received.
        *self.discard_queue.write().unwrap() = true;
    }
}

#[cfg(test)]
#[cfg(feature = "flutter")]
mod tests {
    use super::*;
    use crate::flutter::FlutterHandler;

    /// A round's `Remote` over a loopback pair, before any login: what it sends to `peer`
    /// arrives at `far`.
    async fn remote_and_peer() -> (Remote<FlutterHandler>, Stream, Stream) {
        let listener = hbb_common::tcp::new_listener("127.0.0.1:0", false)
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (peer, accepted) = tokio::join!(
            hbb_common::socket_client::connect_tcp(addr.to_string(), 3000),
            listener.accept()
        );
        let (accepted, far_addr) = accepted.unwrap();
        let far = Stream::Tcp(hbb_common::tcp::FramedStream::from(accepted, far_addr));
        let (sender, receiver) = mpsc::unbounded_channel::<Data>();
        let remote = Remote::new(Session::<FlutterHandler>::default(), receiver, sender);
        (remote, peer.unwrap(), far)
    }

    async fn arrives(far: &mut Stream) -> bool {
        matches!(hbb_common::timeout(300, far.next()).await, Ok(Some(Ok(_))))
    }

    fn clipboard() -> Data {
        let mut msg = Message::new();
        msg.set_clipboard(Clipboard {
            content: b"copied while this login was pending".to_vec().into(),
            ..Default::default()
        });
        Data::Message(msg)
    }

    fn auth_2fa() -> Data {
        let mut msg = Message::new();
        msg.set_auth_2fa(Auth2FA {
            code: "123456".to_owned(),
            ..Default::default()
        });
        Data::Message(msg)
    }

    // A clipboard queued before this round's login stays here; what the login itself sends
    // through the same queue does not.
    #[tokio::test]
    async fn a_clipboard_queued_before_this_rounds_login_is_dropped() {
        let (mut remote, mut peer, mut far) = remote_and_peer().await;
        assert!(!remote.is_connected);
        assert!(remote.handle_msg_from_ui(clipboard(), &mut peer).await);
        assert!(
            !arrives(&mut far).await,
            "a clipboard went out before the login"
        );
        assert!(remote.handle_msg_from_ui(auth_2fa(), &mut peer).await);
        assert!(arrives(&mut far).await, "the 2FA code was held back");

        remote.is_connected = true;
        assert!(remote.handle_msg_from_ui(clipboard(), &mut peer).await);
        assert!(
            arrives(&mut far).await,
            "a clipboard after the login was held back"
        );
    }
}

#[cfg(test)]
mod cursor_dedupe_tests {
    use super::*;

    fn shape(id: u64, hotx: i32, colors: &[u8]) -> CursorData {
        CursorData {
            id,
            hotx,
            width: 2,
            height: 2,
            colors: colors.to_vec().into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_shape_sent_under_a_new_handle_keeps_its_id() {
        let mut dedupe = CursorDedupe::default();
        let first = CursorDedupe::name(&shape(1, 0, b"arrow"));
        assert!(!dedupe.has_decoded(first));
        dedupe.record(1, first);

        let again = CursorDedupe::name(&shape(2, 0, b"arrow"));
        assert_eq!(again, first);
        assert!(dedupe.has_decoded(again));
        dedupe.record(2, again);
        assert_eq!(dedupe.id(2), first);
        assert_eq!(dedupe.id(1), first);
    }

    #[test]
    fn the_hotspot_is_part_of_the_shape() {
        let a = CursorDedupe::name(&shape(1, 0, b"arrow"));
        let b = CursorDedupe::name(&shape(2, 1, b"arrow"));
        assert_ne!(a, b);
    }

    #[test]
    fn a_reused_handle_names_its_latest_shape() {
        let mut dedupe = CursorDedupe::default();
        dedupe.record(1, CursorDedupe::name(&shape(1, 0, b"arrow")));
        let beam = CursorDedupe::name(&shape(1, 0, b"beam"));
        dedupe.record(1, beam);
        assert_eq!(dedupe.id(1), beam);
    }

    #[test]
    fn a_rejected_shape_names_its_handle_and_is_never_decoded() {
        let mut dedupe = CursorDedupe::default();
        let arrow = CursorDedupe::name(&shape(7, 0, b"arrow"));
        dedupe.record(7, arrow);
        let beam = CursorDedupe::name(&shape(8, 0, b"beam"));
        dedupe.record(8, beam);
        let broken = CursorDedupe::name(&shape(7, 0, b"broken"));
        assert_eq!(
            dedupe.rejected(7, broken),
            broken,
            "not the shape it named before"
        );
        assert_eq!(
            dedupe.id(7),
            broken,
            "nor when the handle is selected again"
        );
        assert_eq!(dedupe.id(8), beam, "another handle keeps its shape");
        assert!(!dedupe.has_decoded(broken));
        assert!(
            dedupe.has_decoded(arrow),
            "kept for a handle that brings it again"
        );
        dedupe.record(7, arrow);
        assert_eq!(dedupe.id(7), arrow);
    }

    #[test]
    fn a_shape_rejected_under_handles_never_seen_files_nothing() {
        let mut dedupe = CursorDedupe::default();
        let broken = CursorDedupe::name(&shape(0, 0, b"broken"));
        for handle in 0..1000 {
            assert_eq!(dedupe.rejected(handle, broken), broken);
        }
        assert!(dedupe.ids.is_empty());
        assert!(!dedupe.has_decoded(broken));
    }

    #[test]
    fn an_id_the_peer_never_sent_is_passed_on() {
        assert_eq!(CursorDedupe::default().id(7), 7);
    }

    #[test]
    fn peers_from_1_5_0_name_cursors_by_content() {
        use hbb_common::get_version_number as v;
        assert!(!crate::is_peer_naming_cursors_by_content(v("1.4.9")));
        assert!(crate::is_peer_naming_cursors_by_content(v("1.5.0")));
        assert!(crate::is_peer_naming_cursors_by_content(v("1.5.1")));
    }
}

#[cfg(test)]
mod kept_cursor_tests {
    use super::*;

    fn compressed(width: i32, height: i32) -> CursorData {
        let rgba = vec![7u8; (width * height * 4) as usize];
        CursorData {
            id: 1,
            width,
            height,
            colors: hbb_common::compress::compress(&rgba).into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_kept_shape_is_a_copy_under_the_ui_id() {
        let mut cd = compressed(4, 4);
        cd.hotx = 1;
        let kept = cursor_shape(42, &cd, &cd.colors);
        assert_eq!(kept.id, 42);
        assert_eq!((kept.hotx, kept.width, kept.height), (1, 4, 4));
        assert_eq!(kept.colors, cd.colors);
        assert_ne!(
            kept.colors.as_ptr(),
            cd.colors.as_ptr(),
            "not a view of the received buffer"
        );
    }

    #[test]
    fn a_kept_shape_is_checked_again_when_it_is_read() {
        assert_eq!(kept_cursor_rgba(compressed(4, 4)).unwrap().colors.len(), 64);
        assert!(
            kept_cursor_rgba(compressed(513, 1)).is_err(),
            "over the size cap"
        );
        let mut bomb = compressed(4, 4);
        bomb.colors = hbb_common::compress::compress(&vec![0u8; 1 << 20]).into();
        assert!(kept_cursor_rgba(bomb).is_err(), "more pixels than its size");
    }
}

#[cfg(test)]
mod gentle_fps_tests {
    use super::*;

    // A 1 s tick in which the host sent `rx` frames and the viewer decoded `dec` (all steady)
    // in `busy_ms`, showed `shown`, with the given median and largest queue length.
    fn tick(
        rx: usize,
        dec: usize,
        shown: usize,
        busy_ms: u64,
        queue_p50: usize,
        queue_max: usize,
    ) -> client::VideoCountersTick {
        client::VideoCountersTick {
            received: rx,
            decoded: dec,
            shown,
            busy_us: busy_ms * 1000,
            steady_decoded: dec,
            steady_busy_us: busy_ms * 1000,
            queue_p50,
            queue_max,
            ..Default::default()
        }
    }

    // Runs `n` ticks; `f(fps the host sends)` gives each tick's counters. Returns the requests.
    fn run(
        ctl: &mut GentleFpsControl,
        custom: usize,
        n: usize,
        mut f: impl FnMut(usize) -> client::VideoCountersTick,
    ) -> Vec<(usize, usize)> {
        let mut sent = Vec::new();
        for i in 0..n {
            let fps = ctl.target.unwrap_or(custom);
            let out = ctl.tick(custom, 1000, &[(0, f(fps))]);
            if let Some(fps) = out.send {
                sent.push((i, fps));
            }
        }
        sent
    }

    // A viewer that decodes at most `cap` fps; the host sends `fps`.
    fn limited(cap: usize, fps: usize) -> client::VideoCountersTick {
        let dec = fps.min(cap);
        let busy_ms = (dec as u64 * 1000 / cap as u64).min(1000);
        tick(fps, dec, dec, busy_ms, 0, 1)
    }

    #[test]
    fn gentle_fps_latency_waits_for_warmup_then_steps_down() {
        let mut ctl = GentleFpsControl::new(true);
        let sent = run(&mut ctl, 60, 4, |fps| tick(fps, fps, fps, 950, 0, 1));
        // Ticks 0-1 warm up, 2-3 are the two overloaded ticks.
        assert_eq!(sent, vec![(3, 51)]);
    }

    #[test]
    fn gentle_fps_latency_floor_holds_while_capacity_is_above_it() {
        let mut ctl = GentleFpsControl::new(true);
        let sent = run(&mut ctl, 60, 60, |fps| limited(26, fps));
        let fps = sent.iter().map(|(_, f)| *f).collect::<Vec<_>>();
        assert_eq!(fps, vec![51, 43, 37, 31, 26, 24]);
        // Each step waited at least 2 ticks and 2 s.
        assert!(sent.windows(2).all(|w| w[1].0 - w[0].0 >= 2));
    }

    #[test]
    fn gentle_fps_latency_below_floor_only_after_10s_of_low_capacity() {
        let mut ctl = GentleFpsControl::new(true);
        // Custom 24 is the floor: overloaded from tick 2 (ready), but the first request waits
        // until capacity (15) has been below the floor for 10 ticks: 2..=11.
        let sent = run(&mut ctl, 24, 40, |fps| limited(15, fps));
        assert_eq!(sent.first(), Some(&(11, 20)), "{:?}", sent);
        // Then down in x0.85 steps to 0.9 x capacity, not lower.
        let fps = sent.iter().map(|(_, f)| *f).collect::<Vec<_>>();
        assert_eq!(fps, vec![20, 17, 14]);
        // From custom 60 it goes through the floor the same way, never under 0.9 x capacity.
        let mut ctl = GentleFpsControl::new(true);
        let sent = run(&mut ctl, 60, 80, |fps| limited(15, fps));
        assert_eq!(sent.last().unwrap().1, 14, "{:?}", sent);
        assert!(sent.iter().all(|(_, f)| *f >= 14));
    }

    #[test]
    fn gentle_fps_latency_ignores_wifi_bursts() {
        let mut ctl = GentleFpsControl::new(true);
        // Bursts: a big queue maximum, the decoder briefly behind, but it keeps up overall.
        let mut i = 0;
        let sent = run(&mut ctl, 60, 60, |_| {
            i += 1;
            if i % 2 == 0 {
                tick(66, 55, 50, 450, 1, 12)
            } else {
                tick(54, 65, 60, 500, 0, 3)
            }
        });
        assert!(sent.is_empty(), "{:?}", sent);
    }

    #[test]
    fn gentle_fps_latency_growing_backlog_is_overload() {
        let mut ctl = GentleFpsControl::new(true);
        let mut p50 = 0;
        let sent = run(&mut ctl, 60, 8, |_| {
            p50 += 4;
            tick(60, 50, 50, 600, p50, p50 + 5)
        });
        assert_eq!(sent.first().map(|(_, f)| *f), Some(51));
    }

    #[test]
    fn gentle_fps_latency_climbs_back_when_calm() {
        let mut ctl = GentleFpsControl::new(true);
        let mut loaded = 6;
        let sent = run(&mut ctl, 60, 40, |fps| {
            if loaded > 0 {
                loaded -= 1;
                tick(fps, fps, fps, 950, 0, 1)
            } else {
                tick(fps, fps, fps, 300, 0, 1)
            }
        });
        let fps = sent.iter().map(|(_, f)| *f).collect::<Vec<_>>();
        assert_eq!(fps.first(), Some(&51));
        assert_eq!(fps.last(), Some(&60));
        assert!(fps.iter().all(|f| *f <= 60));
        // Up in steps of max(3, 10%), each after 5 calm ticks.
        assert_eq!(&fps[fps.len() - 2..], &[56, 60]);
        assert!(sent.windows(2).skip(1).all(|w| w[1].0 - w[0].0 >= 5));
    }

    #[test]
    fn gentle_fps_latency_present_overload_needs_10s() {
        let mut ctl = GentleFpsControl::new(true);
        let sent = run(&mut ctl, 60, 30, |fps| tick(fps, fps, 25, 500, 0, 1));
        // Ready at tick 2, ten present-low ticks: 2..=11.
        assert_eq!(sent, vec![(11, 30)]);
        let mut ctl = GentleFpsControl::new(true);
        let sent = run(&mut ctl, 60, 30, |fps| tick(fps, fps, 10, 500, 0, 1));
        assert_eq!(sent.first(), Some(&(11, 24)));
    }

    #[test]
    fn gentle_fps_latency_refresh_is_last_resort() {
        let mut ctl = GentleFpsControl::new(true);
        let mut refreshes = Vec::new();
        for i in 0..6 {
            let qmax = if i < 4 { 100 } else { 10 };
            let out = ctl.tick(60, 1000, &[(0, tick(60, 60, 60, 500, 0, qmax))]);
            refreshes.push(out.refresh.len());
        }
        // Three ticks in a row above the threshold, then the count starts again.
        assert_eq!(refreshes, vec![0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn gentle_fps_latency_custom_change_resets_target() {
        let mut ctl = GentleFpsControl::new(true);
        run(&mut ctl, 60, 4, |fps| tick(fps, fps, fps, 950, 0, 1));
        assert_eq!(ctl.target, Some(51));
        let out = ctl.tick(30, 1000, &[(0, tick(30, 30, 30, 300, 0, 1))]);
        assert_eq!(ctl.target, None);
        assert_eq!(out.target, 30);
        assert_eq!(out.send, None);
    }

    #[test]
    fn gentle_fps_latency_custom_resend_resets_target() {
        let mut ctl = GentleFpsControl::new(true);
        run(&mut ctl, 60, 4, |fps| tick(fps, fps, fps, 950, 0, 1));
        assert_eq!(ctl.target, Some(51));
        // The controller's own request passes through io_loop too.
        assert!(!ctl.on_custom_fps_sent(51));
        assert_eq!(ctl.target, Some(51));
        // The user applies the unchanged custom fps again: the host is back at 60.
        assert!(ctl.on_custom_fps_sent(60));
        assert_eq!(ctl.target, None);
        assert_eq!(ctl.since_change_ms, 0);
    }

    #[test]
    fn gentle_fps_latency_deferred_overflow_refresh_ignores_the_cap() {
        let mut ctl = FpsControl::default();
        assert!(ctl.key_frame_refresh_allowed());
        ctl.refresh_times = 20;
        ctl.last_refresh_instant = Some(Instant::now() - Duration::from_secs(11));
        assert!(!ctl.refresh_allowed());
        assert!(ctl.key_frame_refresh_allowed());
        ctl.on_refresh();
        assert!(!ctl.key_frame_refresh_allowed());
    }

    #[test]
    fn gentle_fps_latency_static_screen_sends_nothing() {
        let mut ctl = GentleFpsControl::new(true);
        let sent = run(&mut ctl, 60, 40, |_| tick(2, 2, 2, 30, 0, 1));
        assert!(sent.is_empty(), "{:?}", sent);
    }
}
