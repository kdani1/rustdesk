//! The host's session stream, whole or split (roadmap M12).
//!
//! Whole: every call goes to the [`Stream`] exactly as before, so with the option off nothing
//! changes. Split (option `enable-split-session-io`, only authorized remote-control and camera
//! sessions over TCP or KCP): the connection loop keeps the read half, and one writer task per
//! connection owns the write half. The writer takes a control FIFO first, then the video channel,
//! then a bulk FIFO, and gives bulk a slot at least every [`BULK_EVERY`] video writes. While a
//! video write waits on a full socket, the loop still reads the viewer's input.
//!
//! The wire does not change: the same framing, the same secretbox encryption with its nonce
//! counters (the writer is the only one that encrypts, the reader the only one that decrypts:
//! each holds a copy of the key state and uses its own counter), the same messages. Each source
//! stays in order: control (replies, the rx service channel), video (frames and SwitchDisplay),
//! bulk (CM raw messages, file-clipboard messages, file read jobs, and rx messages over
//! [`DEMOTE_BYTES`]). The rx channel keeps its order across the two queues: once an rx message
//! went to bulk, the rx messages after it follow it there until the writer has taken the last
//! of them.
//!
//! WebSocket (private fields in hbb_common) and WebRTC stay whole.

use crate::client::latency::Samples;
use base::message_proto::{message, Message};
use hbb_common::{
    anyhow::anyhow,
    bail,
    bytes::{Bytes, BytesMut},
    bytes_codec::BytesCodec,
    config::Config,
    futures::{SinkExt, StreamExt},
    log,
    protobuf::Message as _,
    tcp::{DynTcpStream, Encrypt, FramedStream},
    tokio::{
        self,
        io::{ReadHalf, WriteHalf},
        sync::{mpsc, Notify},
        task::JoinHandle,
        time::Instant,
    },
    tokio_util::codec::{Decoder, FramedRead, FramedWrite},
    ResultType, Stream,
};
use std::{
    collections::VecDeque,
    io,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// Bulk gets a write at least after this many video writes in a row.
pub const BULK_EVERY: usize = 4;
/// File read jobs pause while bulk holds this much (about four 128 KB blocks).
const BULK_ROOM: usize = 512 * 1024;
/// The reader pauses while control and bulk hold this much: the backpressure an inline write
/// gave before (a reply waited for the socket, and nothing was read meanwhile).
const READ_CAP: usize = 16 * 1024 * 1024;
/// rx messages larger than this (a clipboard image) go to bulk instead of control, and the rx
/// messages after them follow while one is still queued there (rx order is kept).
pub const DEMOTE_BYTES: usize = 256 * 1024;
/// Audio older than this when the writer takes it is dropped, as the loop did before.
const AUDIO_MAX_AGE: Duration = Duration::from_millis(1000);
/// The writer counts as hung when it has work and no progress for the send timeout plus this.
const WATCHDOG_GRACE: Duration = Duration::from_secs(5);
/// How long `finish` and `flush` wait for the writer.
pub const FINISH_TIMEOUT: Duration = Duration::from_millis(1500);
// Sticky fallback: split sessions ending earlier than this count, unless the user or the CM
// closed them; this many turn split off until the process restarts; a split session that lasted
// longer than RESET_AFTER clears the count.
const EARLY_END: Duration = Duration::from_secs(60);
const RESET_AFTER: Duration = Duration::from_secs(300);
const MAX_EARLY_ENDS: usize = 2;

/// A video channel item: when it was queued, and the message (VideoFrame or SwitchDisplay).
pub type VideoItem = (Instant, Arc<Message>);
pub type VideoRx = mpsc::UnboundedReceiver<VideoItem>;
/// What the writer task returns: Err with the write error that ended it.
pub type WriterHandle = JoinHandle<Result<(), String>>;

/// The connection's part of video writes; run by the writer task.
pub trait WriterHooks: Send + 'static {
    /// A video item was taken off the channel, before its write ("fetched"). The shared
    /// message, so a hook can keep it past the write (the UDP shadow copy). Returns where it
    /// goes: markers to write first, and whether to write the item itself (a live UDP video
    /// lane takes video frames off TCP).
    fn video_dequeued(
        &mut self,
        msg: &Arc<Message>,
        queued_at: Instant,
    ) -> crate::udp_video::host::Route;
    /// The write of a video item completed.
    fn video_sent(&mut self, msg: &Message, send: Duration);
    /// The writer ended, however: return, error, panic or abort.
    fn writer_exited(&mut self);
}

struct ExitGuard(Box<dyn WriterHooks>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.writer_exited();
    }
}

enum Out {
    Shared(Arc<Message>),
    Owned(Message),
    Raw(Vec<u8>),
}

struct Item {
    out: Out,
    // When the message was made or queued at its source (the rx channel): the audio age.
    at: Instant,
    // When it entered the writer's queue: the `ctrlq` wait.
    enq_at: Instant,
    bytes: usize,
    // Control items are numbered for `flush`; bulk items are 0.
    seq: u64,
    bulk: bool,
    // From the rx service channel.
    rx: bool,
}

impl Item {
    fn is_audio(&self) -> bool {
        let msg = match &self.out {
            Out::Shared(m) => m.as_ref(),
            Out::Owned(m) => m,
            Out::Raw(_) => return false,
        };
        matches!(msg.union, Some(message::Union::AudioFrame(_)))
    }

    fn into_bytes(self) -> ResultType<Vec<u8>> {
        Ok(match self.out {
            Out::Shared(m) => m.write_to_bytes()?,
            Out::Owned(m) => m.write_to_bytes()?,
            Out::Raw(v) => v,
        })
    }
}

/// Writer counters of one report window (`LATENCY conn` line).
#[derive(Default)]
pub struct WriterWindow {
    /// How long control items waited in the queue before their write started.
    pub ctrlq: Samples,
    pub ctrlq_max: Duration,
    /// Most bytes control and bulk held at once.
    pub queued_max: usize,
    pub audio_dropped: usize,
    pub bulk_writes: usize,
}

/// State the connection loop and the writer task share.
pub struct Shared {
    epoch: std::time::Instant,
    send_timeout_ms: AtomicU64,
    queued_bytes: AtomicUsize,
    queued_max: AtomicUsize,
    bulk_bytes: AtomicUsize,
    // rx messages queued in bulk: while any is, later rx messages go to bulk too.
    rx_in_bulk: AtomicUsize,
    queued_items: AtomicUsize,
    ctrl_done: AtomicU64,
    // Since `epoch`: the last dequeue or write completion.
    progress_us: AtomicU64,
    in_write: AtomicBool,
    // The loop waits on this while file reads or the reader are paused.
    room: Notify,
    ctrl_progress: Notify,
    window: Mutex<WriterWindow>,
}

impl Shared {
    fn new(send_timeout_ms: u64) -> Self {
        Self {
            epoch: std::time::Instant::now(),
            send_timeout_ms: AtomicU64::new(send_timeout_ms),
            queued_bytes: AtomicUsize::new(0),
            queued_max: AtomicUsize::new(0),
            bulk_bytes: AtomicUsize::new(0),
            rx_in_bulk: AtomicUsize::new(0),
            queued_items: AtomicUsize::new(0),
            ctrl_done: AtomicU64::new(0),
            progress_us: AtomicU64::new(0),
            in_write: AtomicBool::new(false),
            room: Notify::new(),
            ctrl_progress: Notify::new(),
            window: Mutex::new(Default::default()),
        }
    }

    fn mark_progress(&self) {
        self.progress_us
            .store(self.epoch.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    fn dequeued(&self, item: &Item) {
        self.queued_bytes.fetch_sub(item.bytes, Ordering::Relaxed);
        if item.bulk {
            self.bulk_bytes.fetch_sub(item.bytes, Ordering::Relaxed);
            if item.rx {
                self.rx_in_bulk.fetch_sub(1, Ordering::Relaxed);
            }
        }
        self.queued_items.fetch_sub(1, Ordering::Relaxed);
        self.mark_progress();
        // A stored permit at most wakes the loop once for nothing.
        self.room.notify_one();
    }

    fn ctrl_done(&self, seq: u64) {
        self.ctrl_done.fetch_max(seq, Ordering::Relaxed);
        self.ctrl_progress.notify_waiters();
    }

    fn with_window(&self, f: impl FnOnce(&mut WriterWindow)) {
        if let Ok(mut w) = self.window.lock() {
            f(&mut w);
        }
    }

    /// The writer counters since the last call.
    pub fn take_window(&self) -> WriterWindow {
        let mut w = self
            .window
            .lock()
            .map(|mut w| std::mem::take(&mut *w))
            .unwrap_or_default();
        w.queued_max = self.queued_max.swap(
            self.queued_bytes.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        w
    }

    /// Resolves when the writer took something off a queue (or at once after an earlier one).
    pub async fn room(&self) {
        self.room.notified().await
    }
}

struct Reader {
    framed: FramedRead<ReadHalf<DynTcpStream>, BytesCodec>,
    enc: Option<Encrypt>,
    // Complete frames that were already buffered when the stream was split.
    pending: VecDeque<BytesMut>,
    pending_err: Option<io::Error>,
}

impl Reader {
    async fn next(&mut self) -> Option<Result<BytesMut, io::Error>> {
        let mut res = if let Some(frame) = self.pending.pop_front() {
            Some(Ok(frame))
        } else if let Some(err) = self.pending_err.take() {
            Some(Err(err))
        } else {
            self.framed.next().await
        };
        // As FramedStream::next.
        if let Some(Ok(bytes)) = res.as_mut() {
            if let Some(key) = self.enc.as_mut() {
                if let Err(err) = key.dec(bytes) {
                    return Some(Err(err));
                }
            }
        }
        res
    }
}

struct Writer {
    sink: FramedWrite<WriteHalf<DynTcpStream>, BytesCodec>,
    enc: Option<Encrypt>,
    shared: Arc<Shared>,
}

impl Writer {
    // As FramedStream::send_raw + send_bytes: encrypt, then write under the send timeout.
    async fn write(&mut self, bytes: Vec<u8>) -> ResultType<()> {
        let bytes = match self.enc.as_mut() {
            Some(key) => key.enc(&bytes),
            None => bytes,
        };
        let ms = self.shared.send_timeout_ms.load(Ordering::Relaxed);
        self.shared.in_write.store(true, Ordering::Relaxed);
        let res: ResultType<()> = if ms > 0 {
            match hbb_common::timeout(ms, self.sink.send(Bytes::from(bytes))).await {
                Ok(r) => r.map_err(Into::into),
                Err(e) => Err(e.into()),
            }
        } else {
            self.sink.send(Bytes::from(bytes)).await.map_err(Into::into)
        };
        self.shared.in_write.store(false, Ordering::Relaxed);
        self.shared.mark_progress();
        res
    }
}

// A bulk item: stale audio (rx order sent it here) is dropped as in control.
async fn write_bulk(w: &mut Writer, shared: &Shared, item: Item) -> Result<(), String> {
    shared.dequeued(&item);
    if item.at.elapsed() > AUDIO_MAX_AGE && item.is_audio() {
        shared.with_window(|win| win.audio_dropped += 1);
        return Ok(());
    }
    shared.with_window(|win| win.bulk_writes += 1);
    let bytes = item.into_bytes().map_err(|e| e.to_string())?;
    w.write(bytes).await.map_err(|e| e.to_string())
}

async fn run_writer(
    mut w: Writer,
    mut ctrl: mpsc::UnboundedReceiver<Item>,
    mut bulk: mpsc::UnboundedReceiver<Item>,
    mut video: VideoRx,
    hooks: Box<dyn WriterHooks>,
) -> Result<(), String> {
    let mut hooks = ExitGuard(hooks);
    let shared = w.shared.clone();
    let mut video_run = 0usize;
    loop {
        // Bulk's guaranteed slot: a file block or cliprdr reply is not starved by a frame
        // that is always ready when the link is slower than the encoder.
        if video_run >= BULK_EVERY {
            if let Ok(item) = bulk.try_recv() {
                video_run = 0;
                write_bulk(&mut w, &shared, item).await?;
                continue;
            }
        }
        tokio::select! {
            biased;
            item = ctrl.recv() => {
                // Senders dropped: `finish`. Everything queued before has been written.
                let Some(item) = item else { return Ok(()) };
                shared.dequeued(&item);
                let seq = item.seq;
                if item.at.elapsed() > AUDIO_MAX_AGE && item.is_audio() {
                    shared.with_window(|win| win.audio_dropped += 1);
                    shared.ctrl_done(seq);
                    continue;
                }
                // The wait in this queue only, not the rx channel's before it.
                let waited = item.enq_at.elapsed();
                shared.with_window(|win| {
                    win.ctrlq.push(waited);
                    win.ctrlq_max = win.ctrlq_max.max(waited);
                });
                let res = match item.into_bytes() {
                    Ok(bytes) => w.write(bytes).await,
                    Err(e) => Err(e),
                };
                shared.ctrl_done(seq);
                res.map_err(|e| e.to_string())?;
            }
            Some((at, msg)) = video.recv() => {
                let route = hooks.0.video_dequeued(&msg, at);
                for marker in route.before {
                    let bytes = marker.write_to_bytes().map_err(|e| e.to_string())?;
                    w.write(bytes).await.map_err(|e| e.to_string())?;
                }
                if route.tcp {
                    let begin = std::time::Instant::now();
                    let bytes = msg.write_to_bytes().map_err(|e| e.to_string())?;
                    w.write(bytes).await.map_err(|e| e.to_string())?;
                    hooks.0.video_sent(&msg, begin.elapsed());
                    video_run += 1;
                }
            }
            Some(item) = bulk.recv() => {
                video_run = 0;
                write_bulk(&mut w, &shared, item).await?;
            }
        }
    }
}

/// The split stream: the reader stays in the connection loop, the writer is a task.
pub struct SplitIo {
    reader: Reader,
    ctrl: Option<mpsc::UnboundedSender<Item>>,
    bulk: Option<mpsc::UnboundedSender<Item>>,
    shared: Arc<Shared>,
    ctrl_seq: u64,
    since: std::time::Instant,
    carried: usize,
}

/// How a split session ended, for the sticky fallback.
pub struct SplitEnd {
    pub lived: Duration,
    pub panicked: bool,
    pub aborted: bool,
}

pub enum SessionIo {
    Whole(Stream),
    Split(Box<SplitIo>),
}

impl SessionIo {
    pub fn new(stream: Stream) -> Self {
        Self::Whole(stream)
    }

    #[inline]
    pub fn is_split(&self) -> bool {
        matches!(self, Self::Split(_))
    }

    /// The stream itself, only while whole (port forward, packet size cap).
    pub fn as_whole_mut(&mut self) -> Option<&mut Stream> {
        match self {
            Self::Whole(s) => Some(s),
            Self::Split(_) => None,
        }
    }

    pub fn shared(&self) -> Option<Arc<Shared>> {
        match self {
            Self::Whole(_) => None,
            Self::Split(s) => Some(s.shared.clone()),
        }
    }

    /// Complete frames that were in the read buffer when the stream was split.
    pub fn carried_frames(&self) -> usize {
        match self {
            Self::Whole(_) => 0,
            Self::Split(s) => s.carried,
        }
    }

    fn enqueue(
        &mut self,
        bulk: bool,
        rx: bool,
        out: Out,
        at: Instant,
        bytes: usize,
    ) -> ResultType<()> {
        let Self::Split(s) = self else {
            bail!("session stream is not split");
        };
        let tx = if bulk { &s.bulk } else { &s.ctrl };
        let Some(tx) = tx else {
            bail!("session writer closed");
        };
        let seq = if bulk {
            0
        } else {
            s.ctrl_seq += 1;
            s.ctrl_seq
        };
        let sh = &s.shared;
        let total = sh.queued_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        sh.queued_max.fetch_max(total, Ordering::Relaxed);
        if bulk {
            sh.bulk_bytes.fetch_add(bytes, Ordering::Relaxed);
            if rx {
                sh.rx_in_bulk.fetch_add(1, Ordering::Relaxed);
            }
        }
        sh.queued_items.fetch_add(1, Ordering::Relaxed);
        tx.send(Item {
            out,
            at,
            enq_at: Instant::now(),
            bytes,
            seq,
            bulk,
            rx,
        })
        .map_err(|_| anyhow!("session writer gone"))
    }

    /// A message, by reference: serialized here (the call sites that only have a borrow).
    pub async fn send(&mut self, msg: &impl hbb_common::protobuf::Message) -> ResultType<()> {
        match self {
            Self::Whole(s) => s.send(msg).await,
            Self::Split(_) => {
                let bytes = msg.write_to_bytes()?;
                let n = bytes.len();
                self.enqueue(false, false, Out::Raw(bytes), Instant::now(), n)
            }
        }
    }

    /// A message the caller gives away: serialized by the writer.
    pub async fn send_msg(&mut self, msg: Message) -> ResultType<()> {
        match self {
            Self::Whole(s) => s.send(&msg).await,
            Self::Split(_) => {
                let n = msg.compute_size() as usize;
                self.enqueue(false, false, Out::Owned(msg), Instant::now(), n)
            }
        }
    }

    /// A message from the rx service channel, with the time it was queued there (the writer
    /// drops audio older than 1 s by it). Very large ones go to bulk, and the rx messages after
    /// them follow while one is still queued there, so the rx order holds.
    pub async fn send_shared(&mut self, at: Instant, msg: Arc<Message>) -> ResultType<()> {
        match self {
            Self::Whole(s) => s.send(msg.as_ref()).await,
            Self::Split(s) => {
                let n = msg.compute_size() as usize;
                let bulk = n > DEMOTE_BYTES || s.shared.rx_in_bulk.load(Ordering::Relaxed) > 0;
                self.enqueue(bulk, true, Out::Shared(msg), at, n)
            }
        }
    }

    /// A bulk message (file clipboard, file read jobs).
    pub async fn send_bulk(&mut self, msg: &Message) -> ResultType<()> {
        match self {
            Self::Whole(s) => s.send(msg).await,
            Self::Split(_) => {
                let bytes = msg.write_to_bytes()?;
                let n = bytes.len();
                self.enqueue(true, false, Out::Raw(bytes), Instant::now(), n)
            }
        }
    }

    /// A serialized message from the CM (bulk when split).
    pub async fn send_raw(&mut self, bytes: Vec<u8>) -> ResultType<()> {
        match self {
            Self::Whole(s) => s.send_raw(bytes).await,
            Self::Split(_) => {
                let n = bytes.len();
                self.enqueue(true, false, Out::Raw(bytes), Instant::now(), n)
            }
        }
    }

    /// Unencrypted bytes (port forward): whole only.
    pub async fn send_bytes(&mut self, bytes: Bytes) -> ResultType<()> {
        match self {
            Self::Whole(s) => s.send_bytes(bytes).await,
            Self::Split(_) => bail!("raw bytes on a split session stream"),
        }
    }

    pub async fn next(&mut self) -> Option<Result<BytesMut, io::Error>> {
        match self {
            Self::Whole(s) => s.next().await,
            Self::Split(s) => s.reader.next().await,
        }
    }

    pub fn set_send_timeout(&mut self, ms: u64) {
        match self {
            Self::Whole(s) => s.set_send_timeout(ms),
            Self::Split(s) => s.shared.send_timeout_ms.store(ms, Ordering::Relaxed),
        }
    }

    pub fn set_raw(&mut self) {
        match self {
            Self::Whole(s) => s.set_raw(),
            Self::Split(_) => log::error!("set_raw on a split session stream ignored"),
        }
    }

    pub fn set_max_packet_length(&mut self, n: usize) {
        match self {
            Self::Whole(s) => s.set_max_packet_length(n),
            Self::Split(s) => s.reader.framed.decoder_mut().set_max_packet_length(n),
        }
    }

    /// Whether the loop may read: false while control and bulk hold more than the cap.
    #[inline]
    pub fn read_allowed(&self) -> bool {
        match self {
            Self::Whole(_) => true,
            Self::Split(s) => s.shared.queued_bytes.load(Ordering::Relaxed) < READ_CAP,
        }
    }

    /// Whether the loop may read the next file block.
    #[inline]
    pub fn bulk_has_room(&self) -> bool {
        match self {
            Self::Whole(_) => true,
            Self::Split(s) => s.shared.bulk_bytes.load(Ordering::Relaxed) < BULK_ROOM,
        }
    }

    /// Some(time without progress) when the writer has work and made no progress for longer
    /// than its send timeout plus a grace: it is hung, not just slow.
    pub fn writer_stalled(&self) -> Option<Duration> {
        let Self::Split(s) = self else {
            return None;
        };
        let sh = &s.shared;
        if sh.queued_items.load(Ordering::Relaxed) == 0 && !sh.in_write.load(Ordering::Relaxed) {
            return None;
        }
        let now = sh.epoch.elapsed();
        let last = Duration::from_micros(sh.progress_us.load(Ordering::Relaxed));
        let idle = now.saturating_sub(last);
        let limit =
            Duration::from_millis(sh.send_timeout_ms.load(Ordering::Relaxed)) + WATCHDOG_GRACE;
        (idle > limit).then_some(idle)
    }

    /// Splits a whole TCP or KCP stream; the writer task starts at once and owns `video`. On
    /// Err nothing changed: the stream stays whole and `video` is handed back.
    pub fn split(
        &mut self,
        video: VideoRx,
        hooks: Box<dyn WriterHooks>,
    ) -> Result<WriterHandle, (VideoRx, &'static str)> {
        let Self::Whole(stream) = self else {
            return Err((video, "already split"));
        };
        let fs = match stream {
            Stream::Tcp(fs) => fs,
            #[allow(unreachable_patterns)]
            _ => return Err((video, "transport")),
        };
        // Every earlier send completed, so nothing is half written; checked, not assumed.
        if !fs.0.write_buffer().is_empty() {
            return Err((video, "unflushed write"));
        }
        let addr = fs.1;
        // `Stream` implements Drop, so its variant cannot be moved out: swap in a husk.
        let husk = FramedStream::from(tokio::io::duplex(1).0, addr);
        let FramedStream(framed, _, enc, send_timeout) = std::mem::replace(fs, husk);
        let parts = framed.into_parts();
        let (rh, wh) = tokio::io::split(parts.io);
        let mut codec = parts.codec;
        let mut buf = parts.read_buf;
        // A new FramedRead reads from the socket before it decodes, so complete frames already
        // buffered would wait for the peer's next bytes. Take them out now; what stays is at
        // most one partial frame, with the codec state that goes with it.
        let mut pending = VecDeque::new();
        let mut pending_err = None;
        loop {
            match codec.decode(&mut buf) {
                Ok(Some(frame)) => pending.push_back(frame),
                Ok(None) => break,
                Err(e) => {
                    pending_err = Some(e);
                    break;
                }
            }
        }
        let carried = pending.len();
        let mut framed_read = FramedRead::new(rh, codec);
        framed_read.read_buffer_mut().extend_from_slice(&buf);
        let shared = Arc::new(Shared::new(send_timeout));
        shared.mark_progress();
        let writer = Writer {
            sink: FramedWrite::new(wh, codec),
            enc: enc.clone(),
            shared: shared.clone(),
        };
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let (bulk_tx, bulk_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(run_writer(writer, ctrl_rx, bulk_rx, video, hooks));
        // Drops the husk; `Stream::drop` only closes WebRTC.
        *self = Self::Split(Box::new(SplitIo {
            reader: Reader {
                framed: framed_read,
                enc,
                pending,
                pending_err,
            },
            ctrl: Some(ctrl_tx),
            bulk: Some(bulk_tx),
            shared,
            ctrl_seq: 0,
            since: std::time::Instant::now(),
            carried,
        }));
        Ok(handle)
    }

    /// Waits until the writer has written every control message queued so far (the close
    /// reason before `on_close`), at most `timeout`. Whole: nothing to wait for.
    pub async fn flush(&mut self, timeout: Duration) {
        let Self::Split(s) = self else {
            return;
        };
        let target = s.ctrl_seq;
        let sh = s.shared.clone();
        let Some(tx) = s.ctrl.clone() else {
            return;
        };
        let deadline = Instant::now() + timeout;
        loop {
            let notified = sh.ctrl_progress.notified();
            if sh.ctrl_done.load(Ordering::Relaxed) >= target {
                return;
            }
            tokio::select! {
                _ = notified => {}
                // The writer ended (write error, send timeout): nothing more will be written.
                _ = tx.closed() => return,
                _ = tokio::time::sleep_until(deadline) => {
                    log::warn!("session writer did not flush in {:?}", timeout);
                    return;
                }
            }
        }
    }

    /// How long an explicit close waits for its reason to be written: the frame in flight and
    /// then the reason, each bounded by the writer's send timeout, as the inline send was.
    /// Never less than [`FINISH_TIMEOUT`]. Zero when whole (nothing to flush).
    pub fn close_flush_timeout(&self) -> Duration {
        let Self::Split(s) = self else {
            return Duration::ZERO;
        };
        let send = Duration::from_millis(s.shared.send_timeout_ms.load(Ordering::Relaxed));
        (send * 2).max(FINISH_TIMEOUT)
    }

    /// Ends a split session: no more sends, the writer writes what control holds (the close
    /// reason) and returns; after `timeout` it is aborted. `writer` is the handle unless the
    /// loop already saw the writer end. None when whole.
    pub async fn finish(
        &mut self,
        writer: Option<WriterHandle>,
        timeout: Duration,
    ) -> Option<SplitEnd> {
        let Self::Split(s) = self else {
            return None;
        };
        s.ctrl.take();
        s.bulk.take();
        let mut end = SplitEnd {
            lived: s.since.elapsed(),
            panicked: false,
            aborted: false,
        };
        if let Some(mut handle) = writer {
            match hbb_common::timeout(timeout.as_millis() as u64, &mut handle).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => end.panicked = e.is_panic(),
                Err(_) => {
                    handle.abort();
                    end.aborted = true;
                }
            }
        }
        Some(end)
    }
}

struct Sticky {
    early_ends: AtomicUsize,
    tripped: AtomicBool,
}

impl Sticky {
    const fn new() -> Self {
        Self {
            early_ends: AtomicUsize::new(0),
            tripped: AtomicBool::new(false),
        }
    }

    /// Returns whether this end counted against split.
    fn on_end(&self, lived: Duration, user_close: bool, failure: bool) -> bool {
        let counts = failure || (lived < EARLY_END && !user_close);
        if counts {
            let n = self.early_ends.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= MAX_EARLY_ENDS && !self.tripped.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "SPLIT host: {} split sessions ended early, enable-split-session-io is off until the service restarts",
                    n
                );
            }
        } else if lived >= RESET_AFTER {
            self.early_ends.store(0, Ordering::Relaxed);
        }
        counts
    }
}

static STICKY: Sticky = Sticky::new();

/// Whether new sessions may split: Windows, option `enable-split-session-io` = Y, and the
/// sticky fallback has not tripped.
pub fn split_allowed() -> bool {
    cfg!(windows)
        && Config::get_option(base::config::keys::OPTION_ENABLE_SPLIT_SESSION_IO) == "Y"
        && !STICKY.tripped.load(Ordering::Relaxed)
}

/// A split session ended: `user_close` when the user or the CM closed it, `failure` for a
/// writer panic or a watchdog trip (always counted). Returns whether it counted.
pub fn on_split_end(lived: Duration, user_close: bool, failure: bool) -> bool {
    STICKY.on_end(lived, user_close, failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::message_proto::{AudioFrame, ChatMessage, Misc, TestDelay, VideoFrame};
    use hbb_common::{
        sodiumoxide::crypto::secretbox,
        tcp::KxTranscript,
        tokio::net::{TcpListener, TcpStream, UdpSocket},
    };

    struct TestHooks(Arc<AtomicBool>);

    impl WriterHooks for TestHooks {
        fn video_dequeued(
            &mut self,
            _: &Arc<Message>,
            _: Instant,
        ) -> crate::udp_video::host::Route {
            crate::udp_video::host::Route::TCP
        }
        fn video_sent(&mut self, _: &Message, _: Duration) {}
        fn writer_exited(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn hooks() -> (Box<dyn WriterHooks>, Arc<AtomicBool>) {
        let exited = Arc::new(AtomicBool::new(false));
        (Box::new(TestHooks(exited.clone())), exited)
    }

    fn msg(i: i64) -> Message {
        let mut m = Message::new();
        m.set_test_delay(TestDelay {
            time: i,
            ..Default::default()
        });
        m
    }

    fn video(i: i64) -> Arc<Message> {
        let mut vf = VideoFrame::new();
        vf.display = i as i32;
        let mut m = Message::new();
        m.set_video_frame(vf);
        Arc::new(m)
    }

    fn time_of(bytes: &[u8]) -> i64 {
        let m = Message::parse_from_bytes(bytes).unwrap();
        match m.union {
            Some(message::Union::TestDelay(t)) => t.time,
            Some(message::Union::VideoFrame(vf)) => -1000 - vf.display as i64,
            Some(message::Union::Misc(m)) => m
                .chat_message()
                .text
                .split(':')
                .next()
                .unwrap()
                .parse()
                .unwrap(),
            other => panic!("unexpected message {:?}", other.is_some()),
        }
    }

    async fn next_time(s: &mut Stream) -> i64 {
        time_of(&s.next_timeout(5000).await.expect("frame in time").unwrap())
    }

    async fn io_next_time(s: &mut SessionIo) -> i64 {
        let got = hbb_common::timeout(5000, s.next()).await.expect("frame in time");
        time_of(&got.unwrap().unwrap())
    }

    /// (host, peer) over loopback TCP.
    async fn tcp_pair() -> (Stream, Stream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (peer, host) = tokio::join!(TcpStream::connect(addr), listener.accept());
        (
            Stream::from(host.unwrap().0, addr),
            Stream::from(peer.unwrap(), addr),
        )
    }

    fn set_keys(host: &mut Stream, peer: &mut Stream, version: u32) {
        let key = secretbox::gen_key();
        let t = KxTranscript {
            initiator_pk: &[3u8; 32],
            responder_pk: &[4u8; 32],
            advertised: version,
            picked: version,
        };
        // The viewer seals the key (initiator), the host is the responder.
        host.set_negotiated_key(key.clone(), false, &t).unwrap();
        peer.set_negotiated_key(key, true, &t).unwrap();
    }

    // Key exchange v0 and v1: traffic before the split advances both nonce counters; after
    // the split two complete frames that sat in the read buffer arrive although the peer then
    // stays silent; concurrent traffic in both directions keeps order and decrypts; `finish`
    // writes what control holds before the writer ends.
    async fn split_round(version: u32) {
        let (host, mut peer) = tcp_pair().await;
        let mut host = host;
        set_keys(&mut host, &mut peer, version);
        let mut host = SessionIo::new(host);
        host.send_msg(msg(1)).await.unwrap();
        assert_eq!(next_time(&mut peer).await, 1);
        for i in 10..13 {
            peer.send(&msg(i)).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(io_next_time(&mut host).await, 10);
        let (video_tx, video_rx) = mpsc::unbounded_channel();
        let (h, exited) = hooks();
        let writer = host.split(video_rx, h).map_err(|(_, why)| why).unwrap();
        assert_eq!(host.carried_frames(), 2, "both buffered frames carried over");
        // The peer sends nothing more: both must come from the buffer.
        assert_eq!(io_next_time(&mut host).await, 11);
        assert_eq!(io_next_time(&mut host).await, 12);

        const N: i64 = 300;
        let reader = tokio::spawn(async move {
            let mut host = host;
            for i in 0..N {
                assert_eq!(io_next_time(&mut host).await, 1000 + i);
            }
            host
        });
        for i in 0..N {
            peer.send(&msg(1000 + i)).await.unwrap();
        }
        let mut host = reader.await.unwrap();
        for i in 0..N {
            host.send_msg(msg(2000 + i)).await.unwrap();
            if i % 50 == 0 {
                video_tx.send((Instant::now(), video(i))).unwrap();
            }
        }
        let mut ctrl = Vec::new();
        let mut frames = 0;
        while ctrl.len() < N as usize || frames < (N / 50) as usize {
            let t = next_time(&mut peer).await;
            if t <= -1000 {
                frames += 1;
            } else {
                ctrl.push(t);
            }
        }
        assert_eq!(ctrl, (2000..2000 + N).collect::<Vec<_>>(), "control stays FIFO");
        host.send_msg(msg(7)).await.unwrap();
        let end = host.finish(Some(writer), FINISH_TIMEOUT).await.unwrap();
        assert!(!end.panicked && !end.aborted);
        assert_eq!(next_time(&mut peer).await, 7, "finish drains control");
        assert!(exited.load(Ordering::SeqCst), "the exit hook ran");
        assert!(host.send_msg(msg(8)).await.is_err(), "no sends after finish");
    }

    #[tokio::test]
    async fn latency_split_buffered_frames_and_key_exchange_v0() {
        split_round(0).await;
    }

    #[tokio::test]
    async fn latency_split_buffered_frames_and_key_exchange_v1() {
        split_round(1).await;
    }

    // The point of the split: a writer stuck on a peer that does not read does not stop the
    // reader. The send timeout then ends the writer with an error, and the exit hook runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latency_split_blocked_writer_reader_still_reads() {
        let (mut host, mut peer) = tcp_pair().await;
        set_keys(&mut host, &mut peer, 1);
        let mut host = SessionIo::new(host);
        host.set_send_timeout(1500);
        let (video_tx, video_rx) = mpsc::unbounded_channel();
        let (h, exited) = hooks();
        let writer = host.split(video_rx, h).map_err(|(_, why)| why).unwrap();
        // Far more than the socket buffers hold; the peer reads none of it.
        let big = vec![9u8; 4 * 1024 * 1024];
        for _ in 0..8 {
            host.send_raw(big.clone()).await.unwrap();
        }
        let _ = video_tx;
        tokio::time::sleep(Duration::from_millis(200)).await;
        for i in 0..20 {
            peer.send(&msg(i)).await.unwrap();
        }
        for i in 0..20 {
            assert_eq!(io_next_time(&mut host).await, i);
        }
        let res = hbb_common::timeout(10_000, writer).await.expect("writer ends");
        assert!(matches!(res, Ok(Err(_))), "the send timeout ends the writer");
        assert!(exited.load(Ordering::SeqCst));
        assert!(host.send_msg(msg(1)).await.is_err());
        drop(peer);
    }

    // Bulk gets a write after at most BULK_EVERY video writes while video is always ready;
    // control goes first; audio that waited longer than 1 s is dropped by the writer.
    #[tokio::test]
    async fn latency_split_bulk_slot_and_stale_audio() {
        let (host, mut peer) = tcp_pair().await;
        let mut host = SessionIo::new(host);
        let (video_tx, video_rx) = mpsc::unbounded_channel();
        for i in 0..12 {
            video_tx.send((Instant::now(), video(i))).unwrap();
        }
        let (h, _) = hooks();
        let writer = host.split(video_rx, h).map_err(|(_, why)| why).unwrap();
        // Queued before the writer runs (current-thread runtime: it runs at the next await).
        host.send_bulk(&msg(100)).await.unwrap();
        host.send_bulk(&msg(101)).await.unwrap();
        let mut audio = Message::new();
        audio.set_audio_frame(AudioFrame::default());
        let stale = Instant::now() - Duration::from_millis(1500);
        host.send_shared(stale, Arc::new(audio)).await.unwrap();
        host.send_msg(msg(50)).await.unwrap();
        let mut order = Vec::new();
        for _ in 0..15 {
            order.push(next_time(&mut peer).await);
        }
        let v = |i: i64| -1000 - i;
        assert_eq!(
            order,
            vec![
                50,
                v(0),
                v(1),
                v(2),
                v(3),
                100,
                v(4),
                v(5),
                v(6),
                v(7),
                101,
                v(8),
                v(9),
                v(10),
                v(11)
            ]
        );
        let window = host.shared().unwrap().take_window();
        assert_eq!(window.audio_dropped, 1);
        assert_eq!(window.bulk_writes, 2);
        host.finish(Some(writer), FINISH_TIMEOUT).await.unwrap();
    }

    // An rx message over DEMOTE_BYTES goes to bulk; the small rx message after it follows it
    // there instead of overtaking it through control (a big clipboard, then a small one).
    #[tokio::test]
    async fn latency_split_rx_order_across_demotion() {
        let (host, mut peer) = tcp_pair().await;
        let mut host = SessionIo::new(host);
        let (_video_tx, video_rx) = mpsc::unbounded_channel();
        let (h, _) = hooks();
        let writer = host.split(video_rx, h).map_err(|(_, why)| why).unwrap();
        let mut misc = Misc::new();
        misc.set_chat_message(ChatMessage {
            text: format!("1:{}", "x".repeat(DEMOTE_BYTES + 1)),
            ..Default::default()
        });
        let mut big = Message::new();
        big.set_misc(misc);
        // Queued before the writer runs (current-thread runtime: it runs at the next await).
        host.send_shared(Instant::now(), Arc::new(big)).await.unwrap();
        host.send_shared(Instant::now(), Arc::new(msg(2))).await.unwrap();
        assert_eq!(next_time(&mut peer).await, 1);
        assert_eq!(next_time(&mut peer).await, 2, "rx order kept");
        let sh = host.shared().unwrap();
        assert_eq!(sh.rx_in_bulk.load(Ordering::Relaxed), 0);
        host.send_shared(Instant::now(), Arc::new(msg(3))).await.unwrap();
        assert_eq!(next_time(&mut peer).await, 3);
        assert_eq!(sh.take_window().bulk_writes, 2, "small rx back in control once drained");
        host.finish(Some(writer), FINISH_TIMEOUT).await.unwrap();
    }

    // KCP: kcp-sys keeps separate channels and wakers for reading and writing, so the halves
    // run concurrently. Over real loopback KCP: both directions at once, in order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latency_split_kcp_concurrent_read_write() {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        a.connect(b.local_addr().unwrap()).await.unwrap();
        b.connect(a.local_addr().unwrap()).await.unwrap();
        let (accepted, connected) = tokio::join!(
            crate::kcp_stream::KcpStream::accept(Arc::new(b), Duration::from_secs(5), None),
            crate::kcp_stream::KcpStream::connect(Arc::new(a), Duration::from_secs(5))
        );
        let (_host_guard, host) = accepted.unwrap();
        let (_peer_guard, mut peer) = connected.unwrap();
        let mut host = SessionIo::new(host);
        let (_video_tx, video_rx) = mpsc::unbounded_channel();
        let (h, _) = hooks();
        let writer = host.split(video_rx, h).map_err(|(_, why)| why).unwrap();
        const N: i64 = 300;
        let filler = "x".repeat(4000);
        for i in 0..N {
            // Several KCP segments each; the number before ':' orders them.
            let mut misc = Misc::new();
            misc.set_chat_message(ChatMessage {
                text: format!("{}:{}", 5000 + i, filler),
                ..Default::default()
            });
            let mut m = Message::new();
            m.set_misc(misc);
            host.send_msg(m).await.unwrap();
        }
        let reader = tokio::spawn(async move {
            let mut host = host;
            for i in 0..N {
                assert_eq!(io_next_time(&mut host).await, i);
            }
            host
        });
        for i in 0..N {
            peer.send(&msg(i)).await.unwrap();
        }
        for i in 0..N {
            assert_eq!(next_time(&mut peer).await, 5000 + i);
        }
        let mut host = hbb_common::timeout(20_000, reader)
            .await
            .expect("reader in time")
            .unwrap();
        host.finish(Some(writer), FINISH_TIMEOUT).await.unwrap();
    }

    #[test]
    fn latency_split_sticky_fallback() {
        let s = Sticky::new();
        let early = Duration::from_secs(10);
        assert!(!s.on_end(early, true, false), "user close does not count");
        assert!(s.on_end(early, false, false));
        assert!(!s.tripped.load(Ordering::Relaxed));
        assert!(!s.on_end(RESET_AFTER, false, false), "a long session clears the count");
        assert!(s.on_end(early, false, false));
        assert!(!s.tripped.load(Ordering::Relaxed));
        assert!(s.on_end(RESET_AFTER * 2, true, true), "a failure always counts");
        assert!(s.tripped.load(Ordering::Relaxed));
    }
}
