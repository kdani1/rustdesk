// AgentHandler: the `InvokeUiSession` implementation for the headless agent.
//
// RustDesk's client core (io_loop / Remote) drives a `Session<T>` and reports
// everything that happens -- connection state, directory listings, file-transfer
// job progress, terminal output -- by calling methods on `T: InvokeUiSession`.
// The flutter UI implements this trait to push events to Dart; we implement it
// to push events into plain shared state that the synchronous MCP tool code
// waits on with a Condvar.
//
// Every clone of an `AgentHandler` shares the same `Arc<HandlerState>`, so the
// clone that the io_loop thread holds and the clone a tool keeps both see the
// same mailbox.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use base::message_proto::{
    terminal_response::Union as TermUnion, CursorData, CursorPosition, DisplayInfo, FileEntry,
    PeerInfo, ReadEmptyDirsResponse, SwitchDisplay, TerminalResponse, WindowsSession,
};
use hbb_common::rendezvous_proto::ConnType;

use crate::client::{Data, QualityStatus};
use crate::ui_session_interface::InvokeUiSession;

#[cfg(all(feature = "vram", feature = "flutter"))]
use std::ffi::c_void;

/// Connection phase for one session.
#[derive(Clone, Debug)]
pub enum Conn {
    Connecting,
    Ready,
    Failed(String),
}

impl Default for Conn {
    fn default() -> Self {
        Conn::Connecting
    }
}

/// Outcome of one file-transfer job id.
#[derive(Clone, Debug)]
pub enum JobOutcome {
    Running { last_speed: f64, finished: f64 },
    Done,
    Error(String),
}

/// A plain, serializable view of a remote directory entry.
#[derive(Clone, Debug)]
pub struct FileEntryInfo {
    pub name: String,
    /// proto FileType value: Dir=0, DirLink=2, DirDrive=3, File=4, FileLink=5.
    pub entry_type: i32,
    pub size: u64,
    pub modified_time: u64,
    pub is_hidden: bool,
}

impl FileEntryInfo {
    fn from_proto(e: &FileEntry) -> Self {
        FileEntryInfo {
            name: e.name.clone(),
            entry_type: e.entry_type.value(),
            size: e.size,
            modified_time: e.modified_time,
            is_hidden: e.is_hidden,
        }
    }
    pub fn is_dir(&self) -> bool {
        // Dir, DirLink, DirDrive are the directory-ish types.
        matches!(self.entry_type, 0 | 2 | 3)
    }
}

/// Everything we have learned about one terminal id.
#[derive(Clone, Debug, Default)]
pub struct TermState {
    pub opened: bool,
    pub open_success: bool,
    pub open_message: String,
    pub pid: u32,
    pub output: Vec<u8>,
    pub closed: bool,
    pub exit_code: i32,
    pub error: Option<String>,
}

/// The shared mailbox. One per session.
#[derive(Default)]
pub struct Shared {
    pub conn: Conn,
    pub peer_info: bool,
    /// The most recent remote directory listing, plus a monotonic sequence
    /// number. read_remote_dir responses carry a path the host may have
    /// normalized (slashes, trailing separators), so matching on the requested
    /// path is unreliable; instead a waiter records the sequence before asking
    /// and waits for it to advance.
    pub last_dir: Option<(String, Vec<FileEntryInfo>)>,
    pub dir_seq: u64,
    pub jobs: HashMap<i32, JobOutcome>,
    pub terminals: HashMap<i32, TermState>,
    /// Human-readable errors reported via msgbox("error", ...).
    pub errors: Vec<String>,
}

pub struct HandlerState {
    pub m: Mutex<Shared>,
    pub cv: Condvar,
    /// A clone of the Session's own sender Arc. io_loop fills the inner Option
    /// once its channel exists; the handler reads it to answer prompts
    /// (continue insecure connection, confirm file overwrite).
    pub sender: RwLock<Option<Arc<RwLock<Option<hbb_common::tokio::sync::mpsc::UnboundedSender<Data>>>>>>,
}

impl Default for HandlerState {
    fn default() -> Self {
        HandlerState {
            m: Mutex::new(Shared::default()),
            cv: Condvar::new(),
            sender: RwLock::new(None),
        }
    }
}

#[derive(Clone, Default)]
pub struct AgentHandler {
    pub state: Arc<HandlerState>,
}

impl AgentHandler {
    /// Point this handler at the owning session's sender channel, so callbacks
    /// can send replies (e.g. ContinueInsecureConnection) back into io_loop.
    pub fn attach_sender(
        &self,
        sender: Arc<RwLock<Option<hbb_common::tokio::sync::mpsc::UnboundedSender<Data>>>>,
    ) {
        *self.state.sender.write().unwrap() = Some(sender);
    }

    fn send_data(&self, d: Data) {
        if let Some(arc) = self.state.sender.read().unwrap().as_ref() {
            if let Some(tx) = arc.read().unwrap().as_ref() {
                let _ = tx.send(d);
            }
        }
    }

    fn notify(&self) {
        self.state.cv.notify_all();
    }

    fn set_failed(&self, why: String) {
        let mut g = self.state.m.lock().unwrap();
        // Keep the first failure; later noise (e.g. a close after an error)
        // should not overwrite the real reason.
        if matches!(g.conn, Conn::Connecting) {
            g.conn = Conn::Failed(why);
        }
        drop(g);
        self.notify();
    }

    // ---- waiters used by the tool code (all time-bounded) ----

    pub fn wait_connected(&self, timeout: Duration) -> Result<(), String> {
        let g = self.state.m.lock().unwrap();
        let (g, res) = self
            .state
            .cv
            .wait_timeout_while(g, timeout, |s| matches!(s.conn, Conn::Connecting))
            .unwrap();
        match &g.conn {
            Conn::Ready => Ok(()),
            Conn::Failed(e) => Err(e.clone()),
            Conn::Connecting => {
                if res.timed_out() {
                    Err("timed out waiting for connection".into())
                } else {
                    Err("still connecting".into())
                }
            }
        }
    }

    pub fn wait_peer_info(&self, timeout: Duration) -> Result<(), String> {
        let g = self.state.m.lock().unwrap();
        let (g, res) = self
            .state
            .cv
            .wait_timeout_while(g, timeout, |s| {
                !s.peer_info && !matches!(s.conn, Conn::Failed(_))
            })
            .unwrap();
        if let Conn::Failed(e) = &g.conn {
            return Err(e.clone());
        }
        if res.timed_out() {
            return Err("timed out waiting for peer info".into());
        }
        Ok(())
    }

    /// Read the current directory-listing sequence number. Call before issuing
    /// read_remote_dir, then pass the value to `wait_dir`.
    pub fn dir_seq(&self) -> u64 {
        self.state.m.lock().unwrap().dir_seq
    }

    /// Wait for a directory listing newer than `since_seq`.
    pub fn wait_dir(
        &self,
        since_seq: u64,
        timeout: Duration,
    ) -> Result<(String, Vec<FileEntryInfo>), String> {
        let g = self.state.m.lock().unwrap();
        let (g, res) = self
            .state
            .cv
            .wait_timeout_while(g, timeout, |s| {
                s.dir_seq == since_seq && !matches!(s.conn, Conn::Failed(_))
            })
            .unwrap();
        if g.dir_seq != since_seq {
            if let Some((path, entries)) = &g.last_dir {
                return Ok((path.clone(), entries.clone()));
            }
        }
        if let Conn::Failed(e) = &g.conn {
            return Err(e.clone());
        }
        if res.timed_out() {
            return Err("timed out waiting for directory listing".into());
        }
        Err("directory listing not received".into())
    }

    pub fn wait_job(&self, id: i32, timeout: Duration) -> Result<(), String> {
        let g = self.state.m.lock().unwrap();
        let (g, res) = self
            .state
            .cv
            .wait_timeout_while(g, timeout, |s| {
                !matches!(s.jobs.get(&id), Some(JobOutcome::Done) | Some(JobOutcome::Error(_)))
                    && !matches!(s.conn, Conn::Failed(_))
            })
            .unwrap();
        match g.jobs.get(&id) {
            Some(JobOutcome::Done) => Ok(()),
            Some(JobOutcome::Error(e)) => Err(e.clone()),
            _ => {
                if let Conn::Failed(e) = &g.conn {
                    return Err(e.clone());
                }
                if res.timed_out() {
                    return Err(format!("timed out waiting for transfer job {}", id));
                }
                Err("transfer did not complete".into())
            }
        }
    }

    pub fn wait_terminal_opened(&self, tid: i32, timeout: Duration) -> Result<TermState, String> {
        let g = self.state.m.lock().unwrap();
        let (g, res) = self
            .state
            .cv
            .wait_timeout_while(g, timeout, |s| {
                !s.terminals.get(&tid).map(|t| t.opened).unwrap_or(false)
                    && !matches!(s.conn, Conn::Failed(_))
            })
            .unwrap();
        if let Some(t) = g.terminals.get(&tid) {
            if t.opened {
                if t.open_success {
                    return Ok(t.clone());
                }
                return Err(format!("terminal open failed: {}", t.open_message));
            }
        }
        if let Conn::Failed(e) = &g.conn {
            return Err(e.clone());
        }
        if res.timed_out() {
            return Err("timed out opening terminal".into());
        }
        Err("terminal not opened".into())
    }

    /// Snapshot a terminal's accumulated output plus terminal/connection errors.
    pub fn terminal_snapshot(&self, tid: i32) -> (Vec<u8>, bool, Option<String>) {
        let g = self.state.m.lock().unwrap();
        let conn_err = if let Conn::Failed(e) = &g.conn {
            Some(e.clone())
        } else {
            None
        };
        if let Some(t) = g.terminals.get(&tid) {
            let err = t.error.clone().or(conn_err);
            (t.output.clone(), t.closed, err)
        } else {
            (Vec::new(), false, conn_err)
        }
    }

    pub fn first_error(&self) -> Option<String> {
        let g = self.state.m.lock().unwrap();
        if let Conn::Failed(e) = &g.conn {
            return Some(e.clone());
        }
        g.errors.first().cloned()
    }
}

impl InvokeUiSession for AgentHandler {
    fn set_cursor_data(&self, _cd: CursorData) {}
    fn set_cursor_id(&self, _id: String) {}
    fn set_cursor_position(&self, _cp: CursorPosition) {}
    fn set_display(&self, _x: i32, _y: i32, _w: i32, _h: i32, _cursor_embedded: bool, _scale: f64) {}
    fn switch_display(&self, _display: &SwitchDisplay) {}

    fn set_peer_info(&self, _peer_info: &PeerInfo) {
        let mut g = self.state.m.lock().unwrap();
        g.peer_info = true;
        drop(g);
        self.notify();
    }

    fn set_displays(&self, _displays: &Vec<DisplayInfo>) {}
    fn set_platform_additions(&self, _data: &str) {}

    fn on_connected(&self, _conn_type: ConnType) {
        let mut g = self.state.m.lock().unwrap();
        g.conn = Conn::Ready;
        drop(g);
        self.notify();
    }

    fn update_privacy_mode(&self) {}
    fn set_permission(&self, _name: &str, _value: bool) {}

    fn close_success(&self) {
        // NOTE: despite the name, for a FILE_TRANSFER session the client core
        // calls this on SUCCESSFUL connect (handle_peer_info, just before
        // on_connected). It is not a disconnect signal, so we do nothing here
        // and rely on on_connected() -> Ready. Actual failures arrive via
        // msgbox("error", ...).
    }

    fn update_quality_status(&self, _qs: QualityStatus) {}
    fn set_connection_type(&self, _is_secured: bool, _direct: bool, _stream_type: &str) {}
    fn set_fingerprint(&self, _fingerprint: String) {}

    fn job_error(&self, id: i32, err: String, _file_num: i32) {
        let mut g = self.state.m.lock().unwrap();
        g.jobs.insert(id, JobOutcome::Error(err));
        drop(g);
        self.notify();
    }

    fn job_done(&self, id: i32, _file_num: i32) {
        let mut g = self.state.m.lock().unwrap();
        g.jobs.insert(id, JobOutcome::Done);
        drop(g);
        self.notify();
    }

    fn clear_all_jobs(&self) {}
    fn new_message(&self, _msg: String) {}
    fn update_transfer_list(&self) {}
    fn load_last_job(&self, _cnt: i32, _job_json: &str, _auto_start: bool) {}

    fn update_folder_files(
        &self,
        _id: i32,
        entries: &Vec<FileEntry>,
        path: String,
        is_local: bool,
        _only_count: bool,
    ) {
        if is_local {
            return;
        }
        let infos: Vec<FileEntryInfo> = entries.iter().map(FileEntryInfo::from_proto).collect();
        let mut g = self.state.m.lock().unwrap();
        g.last_dir = Some((path, infos));
        g.dir_seq = g.dir_seq.wrapping_add(1);
        drop(g);
        self.notify();
    }

    fn confirm_delete_files(&self, _id: i32, _i: i32, _name: String) {}

    fn override_file_confirm(
        &self,
        id: i32,
        file_num: i32,
        _to: String,
        is_upload: bool,
        _is_identical: bool,
    ) {
        // User policy: never prompt. Always overwrite.
        self.send_data(Data::SetConfirmOverrideFile((
            id, file_num, true, // need_override
            false, // remember
            is_upload,
        )));
    }

    fn update_block_input_state(&self, _on: bool) {}

    fn job_progress(&self, id: i32, _file_num: i32, speed: f64, finished_size: f64) {
        let mut g = self.state.m.lock().unwrap();
        g.jobs
            .entry(id)
            .and_modify(|o| {
                if let JobOutcome::Running { last_speed, finished } = o {
                    *last_speed = speed;
                    *finished = finished_size;
                }
            })
            .or_insert(JobOutcome::Running {
                last_speed: speed,
                finished: finished_size,
            });
        // No notify needed; progress is advisory.
    }

    fn adapt_size(&self) {}
    fn on_rgba(&self, _display: usize, _rgba: &mut scrap::ImageRgb) {}

    fn msgbox(&self, msgtype: &str, title: &str, text: &str, _link: &str, _retry: bool) {
        if msgtype.starts_with("insecure-connection") {
            // User policy: auto-accept the insecure (unverified key) prompt.
            self.send_data(Data::ContinueInsecureConnection);
            return;
        }
        if msgtype.starts_with("input-password")
            || msgtype.starts_with("re-input-password")
            || msgtype.starts_with("input-2fa")
        {
            // No usable saved password / 2FA required: treat as an auth failure
            // rather than hanging on a prompt no human will answer.
            self.set_failed(
                "authentication required but no saved password is available for this peer".into(),
            );
            return;
        }
        if msgtype.starts_with("terminal-admin-login") {
            // We never request an elevated terminal (IS_TERMINAL_ADMIN unset), so
            // this should not fire. If it does, we have no admin credentials.
            self.set_failed("terminal requested admin login, which the agent does not perform".into());
            return;
        }
        if msgtype == "error" || msgtype.starts_with("error") {
            let msg = if text.is_empty() { title } else { text };
            let msg = msg.to_string();
            let mut g = self.state.m.lock().unwrap();
            g.errors.push(msg.clone());
            if matches!(g.conn, Conn::Connecting) {
                g.conn = Conn::Failed(msg);
            }
            drop(g);
            self.notify();
            return;
        }
        // Everything else (loading, re-connecting, etc.): ignore.
    }

    fn cancel_msgbox(&self, _tag: &str) {}
    fn switch_back(&self, _id: &str) {}
    fn portable_service_running(&self, _running: bool) {}
    fn on_voice_call_started(&self) {}
    fn on_voice_call_closed(&self, _reason: &str) {}
    fn on_voice_call_waiting(&self) {}
    fn on_voice_call_incoming(&self) {}

    fn get_rgba(&self, _display: usize) -> *const u8 {
        std::ptr::null()
    }
    fn next_rgba(&self, _display: usize) {}

    #[cfg(all(feature = "vram", feature = "flutter"))]
    fn on_texture(&self, _display: usize, _texture: *mut c_void) {}

    fn set_multiple_windows_session(&self, _sessions: Vec<WindowsSession>) {}
    fn set_current_display(&self, _disp_idx: i32) {}

    #[cfg(feature = "flutter")]
    fn is_multi_ui_session(&self) -> bool {
        false
    }

    fn update_record_status(&self, _start: bool) {}
    fn update_empty_dirs(&self, _res: ReadEmptyDirsResponse) {}
    fn printer_request(&self, _id: i32, _path: String) {}

    fn handle_screenshot_resp(&self, _sid: String, _msg: String) {
        // PHASE 2: screenshot capture. See src/agent/input_stub.rs.
    }

    fn handle_terminal_response(&self, response: TerminalResponse) {
        let mut g = self.state.m.lock().unwrap();
        match response.union {
            Some(TermUnion::Opened(opened)) => {
                let t = g.terminals.entry(opened.terminal_id).or_default();
                t.opened = true;
                t.open_success = opened.success;
                t.open_message = opened.message.clone();
                t.pid = opened.pid;
            }
            Some(TermUnion::Data(data)) => {
                let bytes = if data.compressed {
                    hbb_common::compress::decompress(&data.data)
                } else {
                    data.data.to_vec()
                };
                let t = g.terminals.entry(data.terminal_id).or_default();
                t.output.extend_from_slice(&bytes);
            }
            Some(TermUnion::Closed(closed)) => {
                let t = g.terminals.entry(closed.terminal_id).or_default();
                t.closed = true;
                t.exit_code = closed.exit_code;
            }
            Some(TermUnion::Error(error)) => {
                // TerminalError carries terminal_id = 0, so route it to every
                // known terminal and also record it globally.
                let msg = error.message.clone();
                if g.terminals.is_empty() {
                    let t = g.terminals.entry(error.terminal_id).or_default();
                    t.error = Some(msg.clone());
                } else {
                    for (_, t) in g.terminals.iter_mut() {
                        if t.error.is_none() {
                            t.error = Some(msg.clone());
                        }
                    }
                }
                g.errors.push(msg);
            }
            _ => {}
        }
        drop(g);
        self.notify();
    }
}
