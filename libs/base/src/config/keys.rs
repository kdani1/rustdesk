//! Option keys shared across the app.
//!
//! The handful that `hbb_common` itself reads stay defined there and are
//! re-exported here, so callers always use this one path.

pub use hbb_common::config::keys::*;

pub const OPTION_VIEW_ONLY: &str = "view_only";
pub const OPTION_SHOW_MONITORS_TOOLBAR: &str = "show_monitors_toolbar";
pub const OPTION_SHOW_REMOTE_CURSOR: &str = "show_remote_cursor";
pub const OPTION_FOLLOW_REMOTE_CURSOR: &str = "follow_remote_cursor";
pub const OPTION_FOLLOW_REMOTE_WINDOW: &str = "follow_remote_window";
pub const OPTION_SHOW_QUALITY_MONITOR: &str = "show_quality_monitor";
pub const OPTION_DISABLE_AUDIO: &str = "disable_audio";
pub const OPTION_ENABLE_REMOTE_PRINTER: &str = "enable-remote-printer";
pub const OPTION_DISABLE_CLIPBOARD: &str = "disable_clipboard";
pub const OPTION_LOCK_AFTER_SESSION_END: &str = "lock_after_session_end";
pub const OPTION_PRIVACY_MODE: &str = "privacy_mode";
pub const OPTION_TOUCH_MODE: &str = "touch-mode";
pub const OPTION_SYNC_INIT_CLIPBOARD: &str = "sync-init-clipboard";
pub const OPTION_THEME: &str = "theme";
pub const OPTION_REMOTE_MENUBAR_DRAG_LEFT: &str = "remote-menubar-drag-left";
pub const OPTION_REMOTE_MENUBAR_DRAG_RIGHT: &str = "remote-menubar-drag-right";
pub const OPTION_HIDE_AB_TAGS_PANEL: &str = "hideAbTagsPanel";
pub const OPTION_ENABLE_CONFIRM_CLOSING_TABS: &str = "enable-confirm-closing-tabs";
pub const OPTION_ENABLE_OPEN_NEW_CONNECTIONS_IN_TABS: &str = "enable-open-new-connections-in-tabs";
pub const OPTION_TEXTURE_RENDER: &str = "use-texture-render";
// Internal health record written by the texture-render watchdog/probe;
// "failed-*" flips the texture-render default to opt-in on this machine.
pub const OPTION_TEXTURE_RENDER_HEALTH: &str = "texture-render-health";
pub const OPTION_ALLOW_D3D_RENDER: &str = "allow-d3d-render";
pub const OPTION_ENABLE_CHECK_UPDATE: &str = "enable-check-update";
pub const OPTION_ALLOW_AUTO_UPDATE: &str = "allow-auto-update";
pub const OPTION_SYNC_AB_WITH_RECENT_SESSIONS: &str = "sync-ab-with-recent-sessions";
pub const OPTION_SYNC_AB_TAGS: &str = "sync-ab-tags";
pub const OPTION_FILTER_AB_BY_INTERSECTION: &str = "filter-ab-by-intersection";
pub const OPTION_ACCESS_MODE: &str = "access-mode";
pub const OPTION_ENABLE_KEYBOARD: &str = "enable-keyboard";
pub const OPTION_ENABLE_CLIPBOARD: &str = "enable-clipboard";
pub const OPTION_ENABLE_FILE_TRANSFER: &str = "enable-file-transfer";
pub const OPTION_ENABLE_CAMERA: &str = "enable-camera";
pub const OPTION_ENABLE_TERMINAL: &str = "enable-terminal";
pub const OPTION_TERMINAL_PERSISTENT: &str = "terminal-persistent";
pub const OPTION_ENABLE_AUDIO: &str = "enable-audio";
pub const OPTION_ENABLE_TUNNEL: &str = "enable-tunnel";
pub const OPTION_ENABLE_REMOTE_RESTART: &str = "enable-remote-restart";
pub const OPTION_ENABLE_RECORD_SESSION: &str = "enable-record-session";
pub const OPTION_ENABLE_BLOCK_INPUT: &str = "enable-block-input";
pub const OPTION_ENABLE_PRIVACY_MODE: &str = "enable-privacy-mode";
pub const OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW: &str = "enable-perm-change-in-accept-window";
pub const OPTION_HIDE_ELEVATE_BUTTON_IN_ACCEPT_WINDOW: &str =
    "hide-elevate-button-in-accept-window";
pub const OPTION_ALLOW_SCOPE_VIOLATION_CLOSE: &str = "allow-scope-violation-close";
pub const OPTION_ALLOW_SCOPE_VIOLATION_ALARM: &str = "allow-scope-violation-alarm";
pub const OPTION_ALLOW_REMOTE_CONFIG_MODIFICATION: &str = "allow-remote-config-modification";
pub const OPTION_ENABLE_LAN_DISCOVERY: &str = "enable-lan-discovery";
pub const OPTION_DIRECT_ACCESS_PORT: &str = "direct-access-port";
pub const OPTION_WHITELIST: &str = "whitelist";
pub const OPTION_ID_WHITELIST: &str = "id-whitelist";
pub const OPTION_ALLOW_AUTO_DISCONNECT: &str = "allow-auto-disconnect";
pub const OPTION_AUTO_DISCONNECT_TIMEOUT: &str = "auto-disconnect-timeout";
pub const OPTION_ALLOW_ONLY_CONN_WINDOW_OPEN: &str = "allow-only-conn-window-open";
pub const OPTION_ALLOW_AUTO_RECORD_INCOMING: &str = "allow-auto-record-incoming";
pub const OPTION_ALLOW_AUTO_RECORD_OUTGOING: &str = "allow-auto-record-outgoing";
pub const OPTION_HIDE_RECORDING_BUTTON: &str = "hide-recording-button";
pub const OPTION_WINDOWS_SERVICE_VIDEO_SAVE_DIRECTORY: &str =
    "windows-service-video-save-directory";
pub const OPTION_VIDEO_SAVE_DIRECTORY: &str = "video-save-directory";
pub const OPTION_ENABLE_ABR: &str = "enable-abr";
pub const OPTION_ALLOW_REMOVE_WALLPAPER: &str = "allow-remove-wallpaper";
pub const OPTION_ALLOW_ALWAYS_SOFTWARE_RENDER: &str = "allow-always-software-render";
pub const OPTION_ENABLE_HWCODEC: &str = "enable-hwcodec";
pub const OPTION_APPROVE_MODE: &str = "approve-mode";
pub const OPTION_VERIFICATION_METHOD: &str = "verification-method";
pub const OPTION_TEMPORARY_PASSWORD_LENGTH: &str = "temporary-password-length";
pub const OPTION_CUSTOM_RENDEZVOUS_SERVER: &str = "custom-rendezvous-server";
pub const OPTION_API_SERVER: &str = "api-server";
pub const OPTION_KEY: &str = "key";
pub const OPTION_PRESET_ADDRESS_BOOK_NAME: &str = "preset-address-book-name";
pub const OPTION_PRESET_ADDRESS_BOOK_TAG: &str = "preset-address-book-tag";
pub const OPTION_PRESET_ADDRESS_BOOK_ALIAS: &str = "preset-address-book-alias";
pub const OPTION_PRESET_ADDRESS_BOOK_PASSWORD: &str = "preset-address-book-password";
pub const OPTION_PRESET_ADDRESS_BOOK_NOTE: &str = "preset-address-book-note";
pub const OPTION_PRESET_DEVICE_USERNAME: &str = "preset-device-username";
pub const OPTION_PRESET_DEVICE_NAME: &str = "preset-device-name";
pub const OPTION_PRESET_NOTE: &str = "preset-note";
pub const OPTION_ENABLE_DIRECTX_CAPTURE: &str = "enable-directx-capture";
pub const OPTION_ENABLE_ANDROID_SOFTWARE_ENCODING_HALF_SCALE: &str =
    "enable-android-software-encoding-half-scale";
pub const OPTION_ENABLE_TRUSTED_DEVICES: &str = "enable-trusted-devices";
pub const OPTION_AV1_TEST: &str = "av1-test";
/// Maximum number of files allowed during a single file transfer request.
///
/// Key: `file-transfer-max-files`.
/// Unit: number of files (not bytes).
///
/// Behaviour:
/// - If set to a positive integer N, at most N files are allowed.
/// - If set to 0, a safe built-in default is used (see DEFAULT_MAX_VALIDATED_FILES).
/// - If unset, negative, or non-integer, no explicit limit is enforced for backward compatibility.
pub const OPTION_FILE_TRANSFER_MAX_FILES: &str = "file-transfer-max-files";
pub const OPTION_DISABLE_UDP: &str = "disable-udp";
pub const OPTION_SHOW_VIRTUAL_MOUSE: &str = "show-virtual-mouse";
// joystick is the virtual mouse.
// So `OPTION_SHOW_VIRTUAL_MOUSE` should also be set if `OPTION_SHOW_VIRTUAL_JOYSTICK` is set.
pub const OPTION_SHOW_VIRTUAL_JOYSTICK: &str = "show-virtual-joystick";
pub const OPTION_ENABLE_FLUTTER_HTTP_ON_RUST: &str = "enable-flutter-http-on-rust";
pub const OPTION_ALLOW_ASK_FOR_NOTE: &str = "allow-ask-for-note";

// built-in options
pub const OPTION_DISPLAY_NAME: &str = "display-name";
pub const OPTION_AVATAR: &str = "avatar";
pub const OPTION_PRESET_DEVICE_GROUP_NAME: &str = "preset-device-group-name";
pub const OPTION_PRESET_USERNAME: &str = "preset-user-name";
pub const OPTION_PRESET_STRATEGY_NAME: &str = "preset-strategy-name";
pub const OPTION_REMOVE_PRESET_PASSWORD_WARNING: &str = "remove-preset-password-warning";
pub const OPTION_HIDE_GENERAL_SETTINGS: &str = "hide-general-settings";
pub const OPTION_HIDE_SECURITY_SETTINGS: &str = "hide-security-settings";
pub const OPTION_HIDE_NETWORK_SETTINGS: &str = "hide-network-settings";
pub const OPTION_HIDE_SERVER_SETTINGS: &str = "hide-server-settings";
pub const OPTION_HIDE_PROXY_SETTINGS: &str = "hide-proxy-settings";
pub const OPTION_HIDE_REMOTE_PRINTER_SETTINGS: &str = "hide-remote-printer-settings";
pub const OPTION_HIDE_WEBSOCKET_SETTINGS: &str = "hide-websocket-settings";
pub const OPTION_HIDE_STOP_SERVICE: &str = "hide-stop-service";
pub const OPTION_ALLOW_COMMAND_LINE_SETTINGS_WHEN_SETTINGS_DISABLED: &str =
    "allow-command-line-settings-when-settings-disabled";

// Connection punch-through / port-forward options
pub const OPTION_ENABLE_TCP_PUNCH: &str = "enable-tcp-punch";
pub const OPTION_ENABLE_UDP_PUNCH: &str = "enable-udp-punch";
pub const OPTION_ENABLE_IPV6_PUNCH: &str = "enable-ipv6-punch";
pub const OPTION_ENABLE_PORT_FORWARD_MUX: &str = "enable-port-forward-mux";
pub const OPTION_ENABLE_WEBRTC: &str = "enable-webrtc";
pub const OPTION_RELAY_FALLBACK_DELAY: &str = "relay-fallback-delay";
/// Viewer (M8 2a): hold a relay result back for `relay-fallback-delay` while a direct attempt is
/// still racing, also when no WebRTC offer is in the race. On by default; "N" restores the plain
/// first-result-wins race.
pub const OPTION_ENABLE_RELAY_HOLD: &str = "enable-relay-hold";
/// Viewer (M8 2b): "udp" or "tcp" holds a direct result of the other kind for 300 ms while the
/// preferred kind is still racing. Empty (default) keeps the first-direct-wins race.
pub const OPTION_DIRECT_TRANSPORT_PREFERENCE: &str = "direct-transport-preference";
pub const OPTION_ALLOW_KCP_CC: &str = "allow-kcp-congestion-control";
pub const OPTION_HIDE_USERNAME_ON_CARD: &str = "hide-username-on-card";
pub const OPTION_HIDE_HELP_CARDS: &str = "hide-help-cards";
pub const OPTION_DEFAULT_CONNECT_PASSWORD: &str = "default-connect-password";
pub const OPTION_HIDE_TRAY: &str = "hide-tray";
pub const OPTION_ONE_WAY_CLIPBOARD_REDIRECTION: &str = "one-way-clipboard-redirection";
pub const OPTION_ALLOW_LOGON_SCREEN_PASSWORD: &str = "allow-logon-screen-password";
pub const OPTION_ALLOW_DEEP_LINK_PASSWORD: &str = "allow-deep-link-password";
pub const OPTION_ALLOW_DEEP_LINK_SERVER_SETTINGS: &str = "allow-deep-link-server-settings";
pub const OPTION_ONE_WAY_FILE_TRANSFER: &str = "one-way-file-transfer";
pub const OPTION_ALLOW_HTTPS_21114: &str = "allow-https-21114";
pub const OPTION_USE_RAW_TCP_FOR_API: &str = "use-raw-tcp-for-api";
pub const OPTION_HIDE_POWERED_BY_ME: &str = "hide-powered-by-me";
pub const OPTION_MAIN_WINDOW_ALWAYS_ON_TOP: &str = "main-window-always-on-top";

// flutter local options
pub const OPTION_FLUTTER_REMOTE_MENUBAR_STATE: &str = "remoteMenubarState";
pub const OPTION_FLUTTER_PEER_SORTING: &str = "peer-sorting";
pub const OPTION_FLUTTER_PEER_TAB_INDEX: &str = "peer-tab-index";
pub const OPTION_FLUTTER_PEER_TAB_ORDER: &str = "peer-tab-order";
pub const OPTION_FLUTTER_PEER_TAB_VISIBLE: &str = "peer-tab-visible";
pub const OPTION_FLUTTER_PEER_CARD_UI_TYLE: &str = "peer-card-ui-type";
pub const OPTION_FLUTTER_CURRENT_AB_NAME: &str = "current-ab-name";
pub const OPTION_ALLOW_REMOTE_CM_MODIFICATION: &str = "allow-remote-cm-modification";
pub const OPTION_ALLOW_SYNC_CLIPBOARD_BETWEEN_SESSIONS: &str =
    "allow-sync-clipboard-between-sessions";

pub const OPTION_PRINTER_INCOMING_JOB_ACTION: &str = "printer-incomming-job-action";
pub const OPTION_PRINTER_ALLOW_AUTO_PRINT: &str = "allow-printer-auto-print";
pub const OPTION_PRINTER_SELECTED_NAME: &str = "printer-selected-name";

// android floating window options
pub const OPTION_DISABLE_FLOATING_WINDOW: &str = "disable-floating-window";
pub const OPTION_FLOATING_WINDOW_SIZE: &str = "floating-window-size";
pub const OPTION_FLOATING_WINDOW_UNTOUCHABLE: &str = "floating-window-untouchable";
pub const OPTION_FLOATING_WINDOW_TRANSPARENCY: &str = "floating-window-transparency";
pub const OPTION_FLOATING_WINDOW_SVG: &str = "floating-window-svg";

// android keep screen on
pub const OPTION_KEEP_SCREEN_ON: &str = "keep-screen-on";

// Server-side: keep host system awake during incoming sessions (Security setting)
pub const OPTION_KEEP_AWAKE_DURING_INCOMING_SESSIONS: &str = "keep-awake-during-incoming-sessions";

// Client-side: keep client system awake during outgoing sessions (General setting)
pub const OPTION_KEEP_AWAKE_DURING_OUTGOING_SESSIONS: &str = "keep-awake-during-outgoing-sessions";

pub const OPTION_DISABLE_GROUP_PANEL: &str = "disable-group-panel";
pub const OPTION_DISABLE_DISCOVERY_PANEL: &str = "disable-discovery-panel";
pub const OPTION_PRE_ELEVATE_SERVICE: &str = "pre-elevate-service";
// "Y" turns on the LATENCY log lines: local option on the viewer, server option on the host.
// Compared with "Y" directly because option2bool() treats "enable-*" as on by default.
pub const OPTION_ENABLE_LATENCY_LOG: &str = "enable-latency-log";
// Windows viewer: "N" keeps the Flutter-based relative mouse instead of raw input (on by default).
pub const OPTION_ENABLE_RAW_RELATIVE_MOUSE: &str = "enable-raw-relative-mouse";
// Windows viewer: enter relative mouse mode when the peer's game takes the pointer (on by default).
pub const OPTION_ENABLE_AUTO_GAME_MODE: &str = "enable-auto-game-mode";
// Windows host, server option: "N" keeps the server process and the capture thread at normal
// priority (on by default: ABOVE_NORMAL process, MMCSS "Capture" for a hardware-encoder video
// thread). A change applies from the next capture restart.
pub const OPTION_ENABLE_HOST_PRIORITY_BOOST: &str = "enable-host-priority-boost";
// Windows, on by default: "N" leaves the Wi-Fi driver out of media streaming mode (fewer background
// scans and power-save stalls). Server option on the host (while video streams), local option on
// the viewer (while a remote session is open).
pub const OPTION_ENABLE_WLAN_STREAMING_MODE: &str = "enable-wlan-streaming-mode";
// Windows host, server option, LAN peers only: "N" leaves the host's session TCP socket unmarked (on
// by default: qWAVE AudioVideo, DSCP 40 / WMM video on Wi-Fi). The socket is marked at accept time,
// before login, so file-transfer, port-forward and terminal connections from LAN peers are marked
// too (known limit). Applies from the next connection.
pub const OPTION_ENABLE_LAN_QOS: &str = "enable-lan-qos";
// Windows host, server option: "N" restores the relative frame pacing of the capture loop (sleep
// for what is left of the frame period, so every iteration's oversleep adds up). On by default:
// absolute deadlines (next tick += frame period, a new schedule from now when more than one
// period late, no catch-up burst), and DXGI waits ceil(period) + 2 ms for a new image instead of
// the period truncated to ms. A change applies from the next capture restart. See
// docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_CAPTURE_DEADLINE_PACING: &str = "enable-capture-deadline-pacing";
// Windows host, server option (Config), default on: "N" restores upstream's GDI fallback. A new
// DXGI capturer gets 2 s (not 4 empty waits) for its first image before falling back to GDI, and
// after a fallback to GDI the capturer tries DXGI again after 3 s, 10 s, 30 s, then every 60 s
// (reset after 30 s on DXGI). Fullscreen games switching display modes otherwise leave the session
// on GDI capture (~25 fps at 1440p) until it restarts.
pub const OPTION_ENABLE_DXGI_RETRY: &str = "enable-dxgi-retry";
// Windows host, server option (Config), off by default ("Y" turns it on, any other value keeps
// the legacy restart). A viewer's RefreshVideo then forces a key frame (IDR) on the running
// encoder instead of restarting capturer and encoder (`SWITCH`). On a static screen the key frame
// is encoded from the kept last image. Refreshes are merged into a pending key frame, at most one
// key frame per 200 ms; the legacy SWITCH stays for screenshots, display, codec or resolution
// changes, encoders without a forced key frame, a key frame that does not come, and 3 refreshes
// within 3 s already served by key frames. Read on every refresh. See docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_REFRESH_KEYFRAME: &str = "enable-refresh-keyframe";
// Windows host, server option (Config): VBV (rate-control buffer) of the nvenc encoders in frames,
// rc_buffer_size = N * bitrate / encoder frame rate, kept on every bitrate change. Empty or "0"
// (default) keeps FFmpeg's default VBV; "1" is Sunshine's one-frame VBV, clamped to 60. Smaller
// key and scene-change frames, less sharp until static refine (below) improves them. Read when an
// encoder is created. Only together with static refine: enable-static-refine=N turns the VBV off
// too. See docs/LATENCY-METRICS.md.
pub const OPTION_NVENC_VBV_FRAMES: &str = "nvenc-vbv-frames";
// Windows host, server option (Config): static refine. After a key frame or a large frame, when
// the screen then stays unchanged for 150 ms, the last image is encoded again up to 3 times so a
// small VBV does not leave text blurry. On exactly when nvenc-vbv-frames > 0 (roadmap M14: the two
// only go on together); "N" turns both off, other values change nothing. Read when the capture
// loop starts. See docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_STATIC_REFINE: &str = "enable-static-refine";
// Viewer, local option: present pacing, "always show the newest frame". Every frame is still
// decoded, but a stale one is not handed to the UI while a newer one waits; the soft renderer
// replaces a buffer Flutter has not fetched yet instead of dropping the newer frame. On by
// default on Windows ("N" turns it off); off by default elsewhere ("Y" turns it on). Read when a
// display's video thread starts. See docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_VIEWER_PRESENT_PACING: &str = "enable-viewer-present-pacing";
// Viewer, local option: gentle fps control. The fps the viewer asks the host for follows the
// decoder thread's measured load with hysteresis, steps of x0.85 down and +max(3, 10%) up, and
// a floor of min(custom fps, 24) (10 only after 10 s of measured capacity below it), instead of
// the legacy controller, which halves the fps down to 1 on queue growth. Same message to the
// host (custom_fps). On by default on Windows ("N" restores the legacy controller); off by
// default elsewhere ("Y" turns it on). Read when a connection starts. See
// docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_GENTLE_FPS_CONTROL: &str = "enable-gentle-fps-control";
// Windows viewer, local option: "N" leaves the viewer at normal priority (on by default). While a
// remote-control or camera session is open, its io_loop thread and each display's decoder thread
// run at THREAD_PRIORITY_ABOVE_NORMAL, and the process at ABOVE_NORMAL_PRIORITY_CLASS (only raised
// from NORMAL or lower, restored when the last such session ends) with EcoQoS/power throttling
// turned off. Read when a session round or a video thread starts. See docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_VIEWER_PRIORITY_BOOST: &str = "enable-viewer-priority-boost";
// Viewer, local option: decode without output. A frame present pacing will not show is decoded
// without its YUV->RGBA conversion (VP8/VP9/AV1, hardware decoding to RAM, Android MediaCodec)
// or, on the FFmpeg D3D11 GPU decoder, without the NV12->BGRA shader and texture hand-off (one GPU
// sync query is kept). Only acts while enable-viewer-present-pacing is on. On by default ("N"
// converts every frame, as before). Read when a display's video thread starts. See
// docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_VIEWER_DECODE_NO_OUTPUT: &str = "enable-viewer-decode-no-output";
// Windows viewer, local option: a display's decoder thread registers with the Multimedia Class
// Scheduler as a "Playback" task while it decodes on the GPU (D3D11 texture output), without
// changing the process priority class. On by default ("N" turns it off). Read when a display's
// video thread starts. See docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_VIEWER_MMCSS: &str = "enable-viewer-mmcss";
// Viewer, local option: playback buffer target for received audio, in ms. Empty: adaptive target
// clamp(p95 of the audio arrival lateness over 20 s + 20 ms, 60, 300) ms, raised by 20 ms per
// output underrun during a network stall (decays 10 ms per 15 s without one); buffered audio that
// stays above target + 20 ms for 2 s is trimmed down to the target. A number N > 0: the same with
// N ms as the lower bound. "0": the legacy rule (trim at most every 12 s, nothing below 600 ms).
// Default adaptive on Windows; elsewhere empty means "0" (set a number to turn it on). Read when
// the audio format of a session starts. See docs/LATENCY-METRICS.md.
pub const OPTION_AUDIO_TARGET_MS: &str = "audio-target-ms";
// Windows host, server option, off by default: TCP meter on the host's session socket. "Y" for
// LAN peers, "all" for every peer, on the paths that accept the session socket themselves
// (intranet, direct access, accepted TCP punch; not relay, dialed punch, KCP or WebRTC). A
// pass-through stream wrapper counts the bytes the session wrote and samples SIO_TCP_INFO;
// with enable-latency-log=Y it logs one `LATENCY tcp` line per connection every 5 s. Any
// error turns the sampling off, never the connection. Applies from the next connection. See
// docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_TCP_METER: &str = "enable-tcp-meter";
// Windows host, server option, empty by default (the socket keeps Windows' dynamic send
// buffering): a number of KB sets SO_SNDBUF of the session socket once a LAN peer has logged in
// to remote control or camera view (never for file transfer, terminal or port forward).
// Accepted paths only, as for enable-tcp-meter. An experiment (A/B 256 and 512); applies from
// the next connection. See docs/LATENCY-METRICS.md.
pub const OPTION_VIDEO_SNDBUF_KB: &str = "video-sndbuf-kb";
// Windows host, server option, off by default: "Y" splits an authorized remote-control or
// camera session over TCP or KCP into a read half that stays in the connection loop and one
// writer task that writes control, video and bulk messages (src/session_io.rs), so input is read
// while a video write waits on the socket. Wire bytes, encryption and per-source order are
// unchanged. WebSocket and WebRTC sessions stay whole. Two split sessions ending early (under 60
// s, not closed by the user or the CM) turn it off until the service restarts. Applies from the
// next connection; `rustdesk --option enable-split-session-io N` from a terminal session (never
// split) turns it off. See docs/LATENCY-METRICS.md.
pub const OPTION_ENABLE_SPLIT_SESSION_IO: &str = "enable-split-session-io";
// UDP video lane (roadmap M19 shadow probe, M20 live, docs/UDP-VIDEO.md). The same key on both
// sides:
// - Windows host, server option (Config), off by default. Offered to a fork viewer that
//   advertised a lane, after a remote-control login over direct, encrypted TCP with both
//   addresses on the LAN:
//   "live": live (M20): video frames go over UDP, everything else stays on TCP; falls back to
//        TCP video in the same session on loss, socket errors or silence. Needs a viewer of
//        lane v3; an older fork viewer gets no offer.
//   "shadow" or "Y" (M19's value, still shadow): video stays on TCP; a copy of every Nth frame
//        also goes over UDP to be measured.
//   Read at each login; `rustdesk --option enable-udp-video N` turns it off.
// - Windows viewer, local option (LocalConfig), on by default: "N" stops advertising the lane
//   (and refuses an offer), so a reconnect gives a plain TCP session without touching the host.
pub const OPTION_ENABLE_UDP_VIDEO: &str = "enable-udp-video";
// Windows host, server option: shadow mode copies one frame in this many to UDP (1..=60). Empty
// or invalid: 2. Read at each offer.
pub const OPTION_UDP_VIDEO_SHADOW_EVERY: &str = "udp-video-shadow-every";
// Windows host, server option, live mode: datagrams the sender puts on the wire back to back
// before it waits for the pacer (1..=64, "0" = unpaced, every frame at once). Empty or invalid:
// 8 (about 11 KB). Read at each offer.
pub const OPTION_UDP_VIDEO_BURST: &str = "udp-video-burst";
// Windows host, server option, live mode: no authenticated datagram from the other end for this
// many milliseconds ends the lane and video goes back to TCP (200..=10000). Empty or invalid:
// 1000. Sent to the viewer in the offer, which uses it the same way. Read at each offer.
pub const OPTION_UDP_VIDEO_TIMEOUT_MS: &str = "udp-video-timeout-ms";
// Windows host, server option (roadmap M21): Reed-Solomon FEC parity of the UDP video lane, in
// percent of each block's data shards (Sunshine style: at least 2 parity shards per block, at
// most 4 blocks of at most 200 data shards per frame). "0" sends no parity; empty or invalid
// (above 100): 20. Applies to live and shadow lanes, only for a viewer of lane v4 (older fork
// viewers get lanes without parity). Read at each offer. See docs/UDP-VIDEO.md, "FEC".
pub const OPTION_UDP_VIDEO_FEC_PERCENT: &str = "udp-video-fec-percent";
// Per-frame video feedback (roadmap M23, docs/UDP-VIDEO.md "Per-frame feedback"). The same key
// on both sides:
// - Windows host, server option (Config), off by default: "Y" advertises
//   Features.video_feedback to remote-control viewers, records each video frame's send time
//   and size, and takes the viewer's feedback batches: one-way queueing delay (`q_owd`), GCC
//   trendline, jitter, the viewer's own delay, UDP loss. Measurement only unless
//   video-feedback-qos is "Y". Read at each login.
// - Windows viewer, local option (LocalConfig), on by default: "N" sends no feedback even to a
//   host that asks for it. Read at each login.
pub const OPTION_ENABLE_VIDEO_FEEDBACK: &str = "enable-video-feedback";
// Windows host, server option, off by default (M23): "Y" lets the viewer's feedback steer the
// host: the queueing delay and UDP datagram loss it reports go into video_qos's delay sample,
// and a live UDP lane whose frames keep getting lost after FEC goes back to TCP. Needs
// enable-video-feedback=Y. Read every second.
pub const OPTION_VIDEO_FEEDBACK_QOS: &str = "video-feedback-qos";
// Windows host, server option, off by default (M23): "Y" adapts the FEC parity of a live UDP
// lane to the datagram loss the viewer's feedback reports: clamp(10, 50, 2 x loss% + 5),
// raised at once, lowered after 2 s. Needs enable-video-feedback=Y and a lane with parity
// (viewer lane v4, udp-video-fec-percent not "0"). Read every second.
pub const OPTION_UDP_VIDEO_FEC_ADAPTIVE: &str = "udp-video-fec-adaptive";

// DEFAULT_DISPLAY_SETTINGS, OVERWRITE_DISPLAY_SETTINGS
pub const OPTION_AUTO_FORWARD_MICROPHONE: &str = "allow-auto-forward-microphone";
pub const OPTION_ALLOW_FORWARDED_MICROPHONE: &str = "allow-forwarded-microphone";

pub const KEYS_DISPLAY_SETTINGS: &[&str] = &[
    OPTION_VIEW_ONLY,
    OPTION_SHOW_MONITORS_TOOLBAR,
    OPTION_COLLAPSE_TOOLBAR,
    OPTION_SHOW_REMOTE_CURSOR,
    OPTION_FOLLOW_REMOTE_CURSOR,
    OPTION_FOLLOW_REMOTE_WINDOW,
    OPTION_ZOOM_CURSOR,
    OPTION_SHOW_QUALITY_MONITOR,
    OPTION_DISABLE_AUDIO,
    OPTION_ENABLE_FILE_COPY_PASTE,
    OPTION_DISABLE_CLIPBOARD,
    OPTION_LOCK_AFTER_SESSION_END,
    OPTION_PRIVACY_MODE,
    OPTION_TOUCH_MODE,
    OPTION_I444,
    OPTION_REVERSE_MOUSE_WHEEL,
    OPTION_SWAP_LEFT_RIGHT_MOUSE,
    OPTION_DISPLAYS_AS_INDIVIDUAL_WINDOWS,
    OPTION_USE_ALL_MY_DISPLAYS_FOR_THE_REMOTE_SESSION,
    OPTION_VIEW_STYLE,
    OPTION_TERMINAL_PERSISTENT,
    OPTION_SCROLL_STYLE,
    OPTION_EDGE_SCROLL_EDGE_THICKNESS,
    OPTION_IMAGE_QUALITY,
    OPTION_CUSTOM_IMAGE_QUALITY,
    OPTION_CUSTOM_FPS,
    OPTION_CODEC_PREFERENCE,
    OPTION_SYNC_INIT_CLIPBOARD,
    OPTION_TRACKPAD_SPEED,
];
// DEFAULT_LOCAL_SETTINGS, OVERWRITE_LOCAL_SETTINGS
pub const KEYS_LOCAL_SETTINGS: &[&str] = &[
    OPTION_AUTO_FORWARD_MICROPHONE,
    OPTION_THEME,
    OPTION_LANGUAGE,
    OPTION_ENABLE_CONFIRM_CLOSING_TABS,
    OPTION_ENABLE_OPEN_NEW_CONNECTIONS_IN_TABS,
    OPTION_TEXTURE_RENDER,
    OPTION_ALLOW_D3D_RENDER,
    OPTION_SYNC_AB_WITH_RECENT_SESSIONS,
    OPTION_SYNC_AB_TAGS,
    OPTION_FILTER_AB_BY_INTERSECTION,
    OPTION_REMOTE_MENUBAR_DRAG_LEFT,
    OPTION_REMOTE_MENUBAR_DRAG_RIGHT,
    OPTION_HIDE_AB_TAGS_PANEL,
    OPTION_FLUTTER_REMOTE_MENUBAR_STATE,
    OPTION_FLUTTER_PEER_SORTING,
    OPTION_FLUTTER_PEER_TAB_INDEX,
    OPTION_FLUTTER_PEER_TAB_ORDER,
    OPTION_FLUTTER_PEER_TAB_VISIBLE,
    OPTION_FLUTTER_PEER_CARD_UI_TYLE,
    OPTION_FLUTTER_CURRENT_AB_NAME,
    OPTION_DISABLE_FLOATING_WINDOW,
    OPTION_FLOATING_WINDOW_SIZE,
    OPTION_FLOATING_WINDOW_UNTOUCHABLE,
    OPTION_FLOATING_WINDOW_TRANSPARENCY,
    OPTION_FLOATING_WINDOW_SVG,
    OPTION_KEEP_SCREEN_ON,
    // Client-side: keep client system awake during outgoing sessions (General setting)
    OPTION_KEEP_AWAKE_DURING_OUTGOING_SESSIONS,
    OPTION_ENABLE_RAW_RELATIVE_MOUSE,
    OPTION_ENABLE_AUTO_GAME_MODE,
    OPTION_ENABLE_VIEWER_PRESENT_PACING,
    OPTION_ENABLE_GENTLE_FPS_CONTROL,
    OPTION_ENABLE_VIEWER_PRIORITY_BOOST,
    OPTION_ENABLE_VIEWER_DECODE_NO_OUTPUT,
    OPTION_ENABLE_VIEWER_MMCSS,
    OPTION_AUDIO_TARGET_MS,
    OPTION_ENABLE_UDP_VIDEO,
    OPTION_ENABLE_VIDEO_FEEDBACK,
    OPTION_DISABLE_GROUP_PANEL,
    OPTION_DISABLE_DISCOVERY_PANEL,
    OPTION_PRE_ELEVATE_SERVICE,
    OPTION_ALLOW_REMOTE_CM_MODIFICATION,
    OPTION_ALLOW_SYNC_CLIPBOARD_BETWEEN_SESSIONS,
    OPTION_ENABLE_CHECK_UPDATE,
    OPTION_PRINTER_INCOMING_JOB_ACTION,
    OPTION_PRINTER_ALLOW_AUTO_PRINT,
    OPTION_PRINTER_SELECTED_NAME,
    OPTION_ALLOW_AUTO_RECORD_OUTGOING,
    OPTION_HIDE_RECORDING_BUTTON,
    OPTION_VIDEO_SAVE_DIRECTORY,
    OPTION_ENABLE_TCP_PUNCH,
    OPTION_ENABLE_UDP_PUNCH,
    OPTION_ENABLE_IPV6_PUNCH,
    OPTION_ENABLE_PORT_FORWARD_MUX,
    OPTION_ENABLE_WEBRTC,
    OPTION_RELAY_FALLBACK_DELAY,
    OPTION_ENABLE_RELAY_HOLD,
    OPTION_DIRECT_TRANSPORT_PREFERENCE,
    OPTION_TOUCH_MODE,
    OPTION_SHOW_VIRTUAL_MOUSE,
    OPTION_SHOW_VIRTUAL_JOYSTICK,
    OPTION_ENABLE_FLUTTER_HTTP_ON_RUST,
    OPTION_ALLOW_ASK_FOR_NOTE,
];
// DEFAULT_SETTINGS, OVERWRITE_SETTINGS
pub const KEYS_SETTINGS: &[&str] = &[
    OPTION_ALLOW_FORWARDED_MICROPHONE,
    OPTION_ACCESS_MODE,
    OPTION_ENABLE_KEYBOARD,
    OPTION_ENABLE_CLIPBOARD,
    OPTION_ENABLE_FILE_TRANSFER,
    OPTION_ENABLE_CAMERA,
    OPTION_ENABLE_TERMINAL,
    OPTION_ENABLE_REMOTE_PRINTER,
    OPTION_ENABLE_AUDIO,
    OPTION_ENABLE_TUNNEL,
    OPTION_ENABLE_REMOTE_RESTART,
    OPTION_ENABLE_RECORD_SESSION,
    OPTION_ENABLE_BLOCK_INPUT,
    OPTION_ENABLE_PRIVACY_MODE,
    OPTION_ALLOW_SCOPE_VIOLATION_CLOSE,
    OPTION_ALLOW_SCOPE_VIOLATION_ALARM,
    OPTION_ALLOW_REMOTE_CONFIG_MODIFICATION,
    OPTION_ALLOW_NUMERNIC_ONE_TIME_PASSWORD,
    OPTION_ENABLE_LAN_DISCOVERY,
    OPTION_DIRECT_SERVER,
    OPTION_DIRECT_ACCESS_PORT,
    OPTION_WHITELIST,
    OPTION_ID_WHITELIST,
    OPTION_ALLOW_AUTO_DISCONNECT,
    OPTION_AUTO_DISCONNECT_TIMEOUT,
    OPTION_ALLOW_ONLY_CONN_WINDOW_OPEN,
    OPTION_ALLOW_AUTO_RECORD_INCOMING,
    OPTION_WINDOWS_SERVICE_VIDEO_SAVE_DIRECTORY,
    OPTION_ENABLE_ABR,
    OPTION_ALLOW_REMOVE_WALLPAPER,
    OPTION_ALLOW_ALWAYS_SOFTWARE_RENDER,
    OPTION_ENABLE_HWCODEC,
    OPTION_APPROVE_MODE,
    OPTION_VERIFICATION_METHOD,
    OPTION_TEMPORARY_PASSWORD_LENGTH,
    OPTION_PROXY_URL,
    OPTION_PROXY_USERNAME,
    OPTION_PROXY_PASSWORD,
    OPTION_CUSTOM_RENDEZVOUS_SERVER,
    OPTION_API_SERVER,
    OPTION_KEY,
    OPTION_ALLOW_WEBSOCKET,
    OPTION_PRESET_ADDRESS_BOOK_NAME,
    OPTION_PRESET_ADDRESS_BOOK_TAG,
    OPTION_PRESET_ADDRESS_BOOK_ALIAS,
    OPTION_PRESET_ADDRESS_BOOK_PASSWORD,
    OPTION_PRESET_ADDRESS_BOOK_NOTE,
    OPTION_PRESET_DEVICE_USERNAME,
    OPTION_PRESET_DEVICE_NAME,
    OPTION_PRESET_NOTE,
    OPTION_ENABLE_DIRECTX_CAPTURE,
    OPTION_ENABLE_ANDROID_SOFTWARE_ENCODING_HALF_SCALE,
    OPTION_ENABLE_TRUSTED_DEVICES,
    OPTION_RELAY_SERVER,
    OPTION_ICE_SERVERS,
    OPTION_DISABLE_UDP,
    OPTION_ALLOW_INSECURE_TLS_FALLBACK,
    OPTION_KEEP_AWAKE_DURING_INCOMING_SESSIONS,
    OPTION_ALLOW_AUTO_UPDATE,
    OPTION_ALLOW_KCP_CC,
    OPTION_ALLOW_WEBRTC_CC,
];

// BUILDIN_SETTINGS
pub const KEYS_BUILDIN_SETTINGS: &[&str] = &[
    OPTION_DISPLAY_NAME,
    OPTION_AVATAR,
    OPTION_PRESET_DEVICE_GROUP_NAME,
    OPTION_PRESET_USERNAME,
    OPTION_PRESET_STRATEGY_NAME,
    OPTION_REMOVE_PRESET_PASSWORD_WARNING,
    OPTION_HIDE_GENERAL_SETTINGS,
    OPTION_HIDE_SECURITY_SETTINGS,
    OPTION_HIDE_NETWORK_SETTINGS,
    OPTION_HIDE_SERVER_SETTINGS,
    OPTION_HIDE_PROXY_SETTINGS,
    OPTION_HIDE_REMOTE_PRINTER_SETTINGS,
    OPTION_HIDE_WEBSOCKET_SETTINGS,
    OPTION_HIDE_STOP_SERVICE,
    OPTION_HIDE_USERNAME_ON_CARD,
    OPTION_HIDE_HELP_CARDS,
    OPTION_DEFAULT_CONNECT_PASSWORD,
    OPTION_HIDE_TRAY,
    OPTION_ONE_WAY_CLIPBOARD_REDIRECTION,
    OPTION_ALLOW_LOGON_SCREEN_PASSWORD,
    OPTION_ALLOW_DEEP_LINK_PASSWORD,
    OPTION_ALLOW_DEEP_LINK_SERVER_SETTINGS,
    OPTION_ONE_WAY_FILE_TRANSFER,
    OPTION_ALLOW_HTTPS_21114,
    OPTION_ALLOW_HOSTNAME_AS_ID,
    OPTION_REGISTER_DEVICE,
    OPTION_HIDE_POWERED_BY_ME,
    OPTION_MAIN_WINDOW_ALWAYS_ON_TOP,
    OPTION_FILE_TRANSFER_MAX_FILES,
    OPTION_DISABLE_CHANGE_PERMANENT_PASSWORD,
    OPTION_DISABLE_CHANGE_ID,
    OPTION_DISABLE_UNLOCK_PIN,
    OPTION_USE_RAW_TCP_FOR_API,
    OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW,
    OPTION_HIDE_ELEVATE_BUTTON_IN_ACCEPT_WINDOW,
    OPTION_ALLOW_COMMAND_LINE_SETTINGS_WHEN_SETTINGS_DISABLED,
];

#[cfg(test)]
mod tests {
    /// The glob above and the constants below share one namespace, and Rust
    /// silently prefers the explicit item over a glob import. A key defined on
    /// both sides would therefore compile, with the client and the server
    /// disagreeing about its string value and nothing to signal it. Keep the
    /// two sets apart.
    #[test]
    fn key_names_do_not_collide_with_hbb_common() {
        fn names(src: &str) -> Vec<&str> {
            src.lines()
                .filter_map(|l| l.trim().strip_prefix("pub const "))
                .filter_map(|l| l.split(':').next())
                .map(str::trim)
                .filter(|n| n.starts_with("OPTION_") || n.starts_with("KEYS_"))
                .collect()
        }

        let here = names(include_str!("keys.rs"));
        let there = names(include_str!("../../../hbb_common/src/config.rs"));
        assert!(
            !here.is_empty() && !there.is_empty(),
            "key parsing found nothing"
        );

        let both: Vec<_> = here.iter().filter(|n| there.contains(n)).collect();
        assert!(
            both.is_empty(),
            "defined in both crates, so the local one shadows hbb_common's \
             with no diagnostic: {:?}",
            both
        );
    }
}
