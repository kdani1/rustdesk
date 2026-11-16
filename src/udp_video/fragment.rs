//! Splitting a frame into datagram-sized fragments, and putting it back together on the viewer
//! with a latency-first release rule and loss statistics.
//!
//! Release rule (no jitter buffer): frames leave in frame_id order as soon as they are complete.
//! The oldest missing frame is declared lost when a later frame completed more than
//! `gap_grace` ago (5 ms: reordering on a LAN is shorter than that), or when its first
//! fragment (or, if none arrived, the oldest later fragment) is older than `frame_timeout`
//! (50 ms). Both are estimates to tune from shadow data.
//!
//! After a loss the decoder must not get a frame whose reference is missing, so with
//! `gate_after_loss` every frame up to the next KEY or RECOVERY_POINT frame is dropped and
//! counted; the `Lost` event is the caller's cue to request a keyframe.
//!
//! FEC (M21): parity shards (`push_parity`) are kept with their frame; as soon as a block has
//! as many parity shards as it misses data shards, the missing ones are rebuilt
//! (`fec::reconstruct`) and the frame completes at once, with no wait. A frame that completes
//! without loss is delivered before its parity arrives; that parity is then only counted.

use super::{
    fec,
    packet::{VideoHeader, FLAG_KEY, FLAG_RECOVERY_POINT},
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Largest frame the lane carries: a 4K IDR at a high bitrate stays well below this.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragmentError {
    TooLarge,
    /// Fragment payload budget of 0.
    NoRoom,
}

/// What the sender knows about a frame besides its bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameMeta {
    pub frame_id: u32,
    pub display: u8,
    pub epoch: u8,
    pub flags: u8,
    pub send_us: u32,
}

/// Splits `frame` into fragments of at most `max_payload` bytes. All but the last are exactly
/// `max_payload` long. An empty frame is one empty fragment.
pub fn fragment<'a>(
    frame: &'a [u8],
    meta: FrameMeta,
    max_payload: usize,
) -> Result<Vec<(VideoHeader, &'a [u8])>, FragmentError> {
    if max_payload == 0 {
        return Err(FragmentError::NoRoom);
    }
    if frame.len() > MAX_FRAME_LEN {
        return Err(FragmentError::TooLarge);
    }
    let count = frame.len().div_ceil(max_payload).max(1);
    if count > u16::MAX as usize {
        return Err(FragmentError::TooLarge);
    }
    let header = |i: usize| VideoHeader {
        frame_id: meta.frame_id,
        display: meta.display,
        epoch: meta.epoch,
        flags: meta.flags,
        frag_index: i as u16,
        frag_count: count as u16,
        fec_block: 0,
        fec_parity: 0,
        frame_len: frame.len() as u32,
        send_us: meta.send_us,
    };
    if frame.is_empty() {
        return Ok(vec![(header(0), frame)]);
    }
    Ok(frame
        .chunks(max_payload)
        .enumerate()
        .map(|(i, chunk)| (header(i), chunk))
        .collect())
}

/// `a` comes before `b` in wrapping frame_id order.
fn id_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[derive(Clone, Copy, Debug)]
pub struct ReassemblyConfig {
    pub frame_timeout: Duration,
    pub gap_grace: Duration,
    /// Frames ahead of the oldest undelivered one that are tracked at once. A fragment further
    /// ahead means the lane stalled: everything before it is declared lost.
    pub max_ahead: u32,
    pub max_frame_len: usize,
    pub gate_after_loss: bool,
    /// Where a display starts when its first fragment arrives. None: at that fragment's frame
    /// (shadow mode). Live mode starts every display at 0, the id of the first frame the host
    /// gives the lane, so a lost first frame is a loss like any other.
    pub first_frame_id: Option<u32>,
}

impl Default for ReassemblyConfig {
    fn default() -> Self {
        Self {
            frame_timeout: Duration::from_millis(50),
            gap_grace: Duration::from_millis(5),
            max_ahead: 64,
            max_frame_len: MAX_FRAME_LEN,
            gate_after_loss: true,
            first_frame_id: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompleteFrame {
    pub display: u8,
    pub epoch: u8,
    pub frame_id: u32,
    pub flags: u8,
    pub send_us: u32,
    pub data: Vec<u8>,
    pub first_fragment_at: Instant,
    pub completed_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Frame(CompleteFrame),
    /// Frames `from..=to` of `display` will never be delivered.
    Lost {
        display: u8,
        from: u32,
        to: u32,
    },
}

/// Counters since the last `take_stats`. Datagram-level loss comes from the lane seq, which is
/// shared by all displays and control packets, so `datagrams_expected` counts seqs the
/// receiver did not see as lost even if they were pings; on a video lane that is noise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LossStats {
    pub fragments_received: u64,
    pub fragments_duplicate: u64,
    /// For a frame already delivered or declared lost.
    pub fragments_late: u64,
    /// Inconsistent with the other fragments of the frame, or a frame too large.
    pub fragments_invalid: u64,
    pub frames_completed: u64,
    pub frames_lost: u64,
    /// Fragments missing from frames that were declared lost (those whose count is known).
    pub fragments_missing: u64,
    /// Complete frames dropped because they followed a loss and were not KEY/RECOVERY_POINT.
    pub frames_gated: u64,
    /// Datagrams whose seq was below the highest seq seen before them.
    pub reorder_events: u64,
    /// Largest such distance in seqs.
    pub reorder_max: u64,
    /// Highest seq minus lowest seq plus one over the window: datagrams that should have come.
    pub datagrams_expected: u64,
    pub datagrams_received: u64,
    /// FEC (M21): parity shards received (also those of frames that needed none).
    pub parity_received: u64,
    /// FEC: frames that completed only because missing data shards were rebuilt from parity.
    pub fec_recovered: u64,
    /// FEC: data shards rebuilt.
    pub fec_shards: u64,
    /// FEC: frames declared lost although they carried FEC (at least one of their shards
    /// arrived and said so): more shards of a block were missing than parity arrived. Part of
    /// `frames_lost`; a frame of which nothing arrived is lost without being counted here.
    pub fec_unrecoverable: u64,
}

impl LossStats {
    /// Lost datagrams per expected, 0..=1.
    pub fn datagram_loss(&self) -> f64 {
        if self.datagrams_expected == 0 {
            return 0.0;
        }
        self.datagrams_expected
            .saturating_sub(self.datagrams_received) as f64
            / self.datagrams_expected as f64
    }
}

/// FEC state of one block of a pending frame.
#[derive(Clone, Default)]
struct FecBlock {
    /// Parity shards of the block (0: no shard of the block arrived yet).
    parity_count: u8,
    parity: Vec<Option<Vec<u8>>>,
    parity_received: u16,
    /// Length of the parity shards (0: none arrived yet).
    shard_len: usize,
}

struct PendingFrame {
    frame_id: u32,
    epoch: u8,
    flags: u8,
    frag_count: u16,
    frame_len: u32,
    send_us: u32,
    fragments: Vec<Option<Vec<u8>>>,
    received: u16,
    bytes: usize,
    corrupt: bool,
    first_at: Instant,
    completed_at: Option<Instant>,
    /// FEC block count of the frame (`fec_block >> 4`; 0: no FEC).
    fec_blocks: u8,
    fec: Vec<FecBlock>,
    /// Data shards rebuilt from parity.
    recovered: u16,
}

impl PendingFrame {
    fn new(h: &VideoHeader, now: Instant) -> Self {
        let fec_blocks = h.fec_block >> 4;
        Self {
            frame_id: h.frame_id,
            epoch: h.epoch,
            flags: h.flags,
            frag_count: h.frag_count,
            frame_len: h.frame_len,
            send_us: h.send_us,
            fragments: vec![None; h.frag_count as usize],
            received: 0,
            bytes: 0,
            corrupt: false,
            first_at: now,
            completed_at: None,
            fec_blocks,
            fec: vec![FecBlock::default(); fec_blocks as usize],
            recovered: 0,
        }
    }

    fn matches(&self, h: &VideoHeader) -> bool {
        self.epoch == h.epoch
            && self.flags == h.flags
            && self.frag_count == h.frag_count
            && self.frame_len == h.frame_len
            && self.fec_blocks == h.fec_block >> 4
    }

    /// The FEC block a shard with this header belongs to, after checking that its FEC fields
    /// fit the frame: the block exists, holds `data_index` (data shards), and its parity count
    /// agrees with what earlier shards of the block said. None: inconsistent.
    fn fec_block_of(&mut self, h: &VideoHeader, data_index: Option<u16>) -> Option<usize> {
        let (count, b) = fec::split_block_byte(h.fec_block)?;
        let n = self.frag_count as usize;
        if count != self.fec_blocks as usize || count > n {
            return None;
        }
        let range = fec::block_range(n, count, b);
        if data_index.map_or(false, |i| !range.contains(&(i as usize)))
            || h.fec_parity == 0
            || range.len() + h.fec_parity as usize > fec::MAX_SHARDS
        {
            return None;
        }
        let blk = &mut self.fec[b];
        if blk.parity_count == 0 {
            blk.parity_count = h.fec_parity;
            blk.parity = vec![None; h.fec_parity as usize];
        } else if blk.parity_count != h.fec_parity {
            return None;
        }
        Some(b)
    }

    /// Rebuilds block `b`'s missing data shards if enough parity is there. False: the shards do
    /// not fit together (the frame is corrupt).
    fn try_recover(&mut self, b: usize, stats: &mut LossStats) -> bool {
        // Without parity there is nothing to do (and no need to count holes on every fragment).
        if self.fec[b].parity_received == 0 {
            return true;
        }
        let n = self.frag_count as usize;
        let range = fec::block_range(n, self.fec_blocks as usize, b);
        let missing = range
            .clone()
            .filter(|i| self.fragments[*i].is_none())
            .count();
        let blk = &self.fec[b];
        if missing == 0 || (blk.parity_received as usize) < missing {
            return true;
        }
        let shard_len = blk.shard_len;
        let mut data: Vec<Option<Vec<u8>>> = range
            .clone()
            .map(|i| self.fragments[i].take())
            .collect();
        let rebuilt = fec::reconstruct(&mut data, &blk.parity, shard_len);
        let single = range.len() == 1;
        let mut ok = rebuilt.is_ok();
        let rebuilt = rebuilt.unwrap_or_default();
        for k in rebuilt.iter().copied() {
            let i = range.start + k;
            // Every fragment but the frame's last is `shard_len` long (a block's first shard is
            // its longest, and only the last fragment is short); the last one is what is left of
            // the frame, or the whole shard when it is alone in its block.
            let len = if i + 1 == n && !single {
                (self.frame_len as usize).checked_sub((n - 1) * shard_len)
            } else {
                Some(shard_len)
            };
            match (len, data[k].as_mut()) {
                (Some(len), Some(d)) if len >= 1 && len <= shard_len => {
                    d.truncate(len);
                    self.received += 1;
                    self.bytes += len;
                }
                _ => ok = false,
            }
        }
        for (k, d) in data.into_iter().enumerate() {
            self.fragments[range.start + k] = d;
        }
        if ok {
            self.recovered += rebuilt.len() as u16;
            stats.fec_shards += rebuilt.len() as u64;
        }
        ok
    }

    /// Marks the frame complete (or corrupt) once every data fragment is there.
    fn check_complete(&mut self, now: Instant, stats: &mut LossStats) {
        if self.bytes > self.frame_len as usize {
            self.corrupt = true;
            stats.fragments_invalid += 1;
        } else if self.received == self.frag_count {
            if self.bytes == self.frame_len as usize {
                self.completed_at = Some(now);
                if self.recovered > 0 {
                    stats.fec_recovered += 1;
                }
            } else {
                self.corrupt = true;
                stats.fragments_invalid += 1;
            }
        }
    }
}

struct DisplayState {
    display: u8,
    /// Oldest frame not yet delivered or declared lost.
    next_id: u32,
    /// Sorted by frame_id (wrapping), all at or after `next_id`.
    pending: Vec<PendingFrame>,
    awaiting_recovery: bool,
    /// A frame was released (delivered, gated or declared lost), so `next_id - 1` is settled.
    released: bool,
}

pub struct Reassembler {
    config: ReassemblyConfig,
    displays: Vec<DisplayState>,
    events: VecDeque<Event>,
    stats: LossStats,
    lowest_seq: Option<u64>,
    highest_seq: u64,
}

impl Reassembler {
    pub fn new(config: ReassemblyConfig) -> Self {
        Self {
            config,
            displays: Vec::new(),
            events: VecDeque::new(),
            stats: LossStats::default(),
            lowest_seq: None,
            highest_seq: 0,
        }
    }

    /// Starts (or restarts) a display at `first_frame_id`, the id the host announces when the
    /// lane goes active. Pending state of that display is dropped. Without it, the first
    /// fragment seen starts the display.
    pub fn start_display(&mut self, display: u8, first_frame_id: u32) {
        self.displays.retain(|d| d.display != display);
        self.displays.push(DisplayState {
            display,
            next_id: first_frame_id,
            pending: Vec::new(),
            awaiting_recovery: false,
            released: false,
        });
    }

    /// The last frame of `display` that is settled: delivered, dropped behind a loss, or
    /// declared lost. None before the first.
    pub fn settled(&self, display: u8) -> Option<u32> {
        self.displays
            .iter()
            .find(|d| d.display == display && d.released)
            .map(|d| d.next_id.wrapping_sub(1))
    }

    /// Drops frames of `display` until a key or recovery-point frame, as after a loss: the
    /// caller could not use a frame that was delivered (it did not parse).
    pub fn force_recovery(&mut self, display: u8) {
        if let Some(d) = self.displays.iter_mut().find(|d| d.display == display) {
            d.awaiting_recovery = true;
        }
    }

    /// Whether `display` drops frames until a key or recovery-point frame.
    pub fn awaiting_recovery(&self, display: u8) -> bool {
        self.displays
            .iter()
            .any(|d| d.display == display && d.awaiting_recovery)
    }

    /// Records the lane seq of an authenticated datagram, for reorder and datagram loss.
    pub fn note_seq(&mut self, seq: u64) {
        self.stats.datagrams_received += 1;
        if self.lowest_seq.is_none() {
            self.lowest_seq = Some(seq);
        }
        if seq < self.highest_seq {
            let depth = self.highest_seq - seq;
            self.stats.reorder_events += 1;
            self.stats.reorder_max = self.stats.reorder_max.max(depth);
        } else {
            self.highest_seq = seq;
        }
        if let Some(lo) = self.lowest_seq {
            if seq < lo {
                self.lowest_seq = Some(seq);
            }
        }
        let lo = self.lowest_seq.unwrap_or(seq);
        self.stats.datagrams_expected = self.highest_seq - lo + 1;
    }

    /// The pending frame a shard with this header goes to, created if new; None if the frame
    /// was already delivered or declared lost (`late` then counts it).
    fn locate(&mut self, h: &VideoHeader, now: Instant, late: bool) -> Option<(usize, usize)> {
        let config = self.config;
        if !self.displays.iter().any(|d| d.display == h.display) {
            self.start_display(h.display, config.first_frame_id.unwrap_or(h.frame_id));
        }
        let di = self.displays.iter().position(|d| d.display == h.display)?;
        if id_before(h.frame_id, self.displays[di].next_id) {
            self.stats.fragments_late += late as u64;
            return None;
        }
        let ahead = h.frame_id.wrapping_sub(self.displays[di].next_id);
        if ahead >= config.max_ahead {
            // The lane stalled or a burst was lost: give up on everything before this frame.
            let new_next = h.frame_id.wrapping_sub(config.max_ahead - 1);
            self.declare_lost_until(di, new_next);
        }
        let d = &mut self.displays[di];
        let pos = d
            .pending
            .iter()
            .position(|p| !id_before(p.frame_id, h.frame_id));
        let idx = match pos {
            Some(i) if d.pending[i].frame_id == h.frame_id => i,
            Some(i) => {
                d.pending.insert(i, PendingFrame::new(h, now));
                i
            }
            None => {
                d.pending.push(PendingFrame::new(h, now));
                d.pending.len() - 1
            }
        };
        Some((di, idx))
    }

    /// Adds one fragment. Complete frames and losses are queued for `poll`.
    pub fn push(&mut self, h: &VideoHeader, payload: &[u8], now: Instant) {
        if h.frag_count == 0
            || h.frag_index >= h.frag_count
            || h.frame_len as usize > self.config.max_frame_len
        {
            self.stats.fragments_invalid += 1;
            return;
        }
        let Some((di, idx)) = self.locate(h, now, true) else {
            return;
        };
        let stats = &mut self.stats;
        let p = &mut self.displays[di].pending[idx];
        if p.completed_at.is_some() || p.corrupt {
            stats.fragments_duplicate += (p.completed_at.is_some()) as u64;
            stats.fragments_invalid += (p.corrupt) as u64;
            return;
        }
        let block = if p.fec_blocks > 0 && p.matches(h) {
            p.fec_block_of(h, Some(h.frag_index))
        } else {
            None
        };
        if !p.matches(h) || (p.fec_blocks > 0 && block.is_none()) {
            p.corrupt = true;
            stats.fragments_invalid += 1;
            return;
        }
        let slot = &mut p.fragments[h.frag_index as usize];
        if slot.is_some() {
            stats.fragments_duplicate += 1;
            return;
        }
        // Every fragment but the last is the same size, so a frame can only fit if its bytes
        // add up; checked when complete.
        *slot = Some(payload.to_vec());
        p.received += 1;
        p.bytes += payload.len();
        stats.fragments_received += 1;
        if let Some(b) = block {
            if !p.try_recover(b, stats) {
                p.corrupt = true;
                stats.fragments_invalid += 1;
            }
        }
        if !p.corrupt {
            p.check_complete(now, stats);
        }
        self.release(now);
    }

    /// Adds one parity shard (M21). A block that can be rebuilt is rebuilt at once.
    pub fn push_parity(&mut self, h: &VideoHeader, payload: &[u8], now: Instant) {
        self.stats.parity_received += 1;
        if h.frag_count == 0
            || h.fec_block == 0
            || payload.is_empty()
            || h.frame_len as usize > self.config.max_frame_len
        {
            self.stats.fragments_invalid += 1;
            return;
        }
        // Parity of a frame delivered without it is the normal case, not a late fragment.
        let Some((di, idx)) = self.locate(h, now, false) else {
            return;
        };
        let stats = &mut self.stats;
        let p = &mut self.displays[di].pending[idx];
        if p.completed_at.is_some() || p.corrupt {
            return;
        }
        let block = if p.matches(h) {
            p.fec_block_of(h, None)
        } else {
            None
        };
        let Some(b) = block.filter(|b| {
            let blk = &p.fec[*b];
            (h.frag_index as usize) < blk.parity.len()
                && (blk.shard_len == 0 || blk.shard_len == payload.len())
        }) else {
            p.corrupt = true;
            stats.fragments_invalid += 1;
            return;
        };
        let blk = &mut p.fec[b];
        let slot = &mut blk.parity[h.frag_index as usize];
        if slot.is_some() {
            stats.fragments_duplicate += 1;
            return;
        }
        *slot = Some(payload.to_vec());
        blk.parity_received += 1;
        blk.shard_len = payload.len();
        if !p.try_recover(b, stats) {
            p.corrupt = true;
            stats.fragments_invalid += 1;
            return;
        }
        p.check_complete(now, stats);
        self.release(now);
    }

    /// Applies the timeouts and returns everything ready, in order.
    pub fn poll(&mut self, now: Instant) -> Vec<Event> {
        self.release(now);
        self.events.drain(..).collect()
    }

    /// When `poll` next has something to do, if anything is pending. Computed exactly as
    /// `release` decides, so a `poll` at this instant always makes progress: a receive loop that
    /// sleeps until it never spins. An instant in the past means "poll now".
    pub fn next_deadline(&self) -> Option<Instant> {
        let c = self.config;
        self.displays
            .iter()
            .filter_map(|d| {
                let head = d.pending.first().filter(|p| p.frame_id == d.next_id);
                // A complete or corrupt head is released by the next poll, whatever the time.
                if let Some(h) = head.filter(|h| h.completed_at.is_some() || h.corrupt) {
                    return Some(h.first_at);
                }
                // Same start as `release`: the head's own first fragment when it is present,
                // otherwise the oldest later fragment.
                let started = match head {
                    Some(h) => h.first_at,
                    None => d.pending.iter().map(|p| p.first_at).min()?,
                };
                let mut at = started + c.frame_timeout;
                if let Some(done) = d
                    .pending
                    .iter()
                    .filter(|p| p.frame_id != d.next_id)
                    .filter_map(|p| p.completed_at)
                    .min()
                {
                    at = at.min(done + c.gap_grace);
                }
                Some(at)
            })
            .min()
    }

    pub fn stats(&self) -> &LossStats {
        &self.stats
    }

    /// Returns the counters and starts a new window.
    pub fn take_stats(&mut self) -> LossStats {
        self.lowest_seq = None;
        self.highest_seq = 0;
        std::mem::take(&mut self.stats)
    }

    fn release(&mut self, now: Instant) {
        let c = self.config;
        for di in 0..self.displays.len() {
            loop {
                let d = &mut self.displays[di];
                let next_id = d.next_id;
                let head_is_next = d.pending.first().map_or(false, |p| p.frame_id == next_id);
                if head_is_next && d.pending[0].completed_at.is_some() {
                    let p = d.pending.remove(0);
                    d.next_id = next_id.wrapping_add(1);
                    self.deliver(di, p);
                    continue;
                }
                if head_is_next && d.pending[0].corrupt {
                    self.declare_lost_until(di, next_id.wrapping_add(1));
                    continue;
                }
                // The head frame is incomplete or entirely missing: lose it on a timeout.
                let started = if head_is_next {
                    Some(d.pending[0].first_at)
                } else {
                    d.pending.iter().map(|p| p.first_at).min()
                };
                let Some(started) = started else {
                    break;
                };
                let later_done = d
                    .pending
                    .iter()
                    .filter(|p| p.frame_id != next_id)
                    .filter_map(|p| p.completed_at)
                    .min();
                let timed_out = now.saturating_duration_since(started) >= c.frame_timeout;
                let overtaken =
                    later_done.map_or(false, |t| now.saturating_duration_since(t) >= c.gap_grace);
                if timed_out || overtaken {
                    self.declare_lost_until(di, next_id.wrapping_add(1));
                    continue;
                }
                break;
            }
        }
    }

    fn deliver(&mut self, di: usize, p: PendingFrame) {
        let d = &mut self.displays[di];
        d.released = true;
        if d.awaiting_recovery && self.config.gate_after_loss {
            if p.flags & (FLAG_KEY | FLAG_RECOVERY_POINT) == 0 {
                self.stats.frames_gated += 1;
                return;
            }
            d.awaiting_recovery = false;
        }
        self.stats.frames_completed += 1;
        let mut data = Vec::with_capacity(p.frame_len as usize);
        for f in p.fragments.into_iter().flatten() {
            data.extend_from_slice(&f);
        }
        self.events.push_back(Event::Frame(CompleteFrame {
            display: d.display,
            epoch: p.epoch,
            frame_id: p.frame_id,
            flags: p.flags,
            send_us: p.send_us,
            data,
            first_fragment_at: p.first_at,
            completed_at: p.completed_at.unwrap_or(p.first_at),
        }));
    }

    /// Declares every frame from `next_id` up to (not including) `until` lost, merging with a
    /// `Lost` event for the frames just before, if that is the last event queued.
    fn declare_lost_until(&mut self, di: usize, until: u32) {
        let d = &mut self.displays[di];
        let from = d.next_id;
        if !id_before(from, until) {
            return;
        }
        let to = until.wrapping_sub(1);
        let lost = until.wrapping_sub(from) as u64;
        // Frames that completed in the range still count as lost: they were never delivered,
        // because an earlier frame held them back.
        while d
            .pending
            .first()
            .map_or(false, |p| id_before(p.frame_id, until))
        {
            let p = d.pending.remove(0);
            if p.completed_at.is_none() && !p.corrupt {
                self.stats.fragments_missing += (p.frag_count - p.received) as u64;
                if p.fec_blocks > 0 {
                    self.stats.fec_unrecoverable += 1;
                }
            }
        }
        d.next_id = until;
        d.awaiting_recovery = true;
        d.released = true;
        self.stats.frames_lost += lost;
        let display = d.display;
        if let Some(Event::Lost {
            display: last_display,
            to: last_to,
            ..
        }) = self.events.back_mut()
        {
            if *last_display == display && last_to.wrapping_add(1) == from {
                *last_to = to;
                return;
            }
        }
        self.events.push_back(Event::Lost { display, from, to });
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    fn frame_bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn meta(frame_id: u32, flags: u8) -> FrameMeta {
        FrameMeta {
            frame_id,
            display: 0,
            epoch: 0,
            flags,
            send_us: frame_id.wrapping_mul(16_667),
        }
    }

    fn delivered_ids(events: &[Event]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Frame(f) => Some(f.frame_id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fragment_sizes() {
        let f = frame_bytes(1000, 1);
        let parts = fragment(&f, meta(1, 0), 300).unwrap();
        assert_eq!(parts.len(), 4);
        assert!(parts[..3].iter().all(|(_, p)| p.len() == 300));
        assert_eq!(parts[3].1.len(), 100);
        assert!(parts
            .iter()
            .all(|(h, _)| h.frag_count == 4 && h.frame_len == 1000));
        assert_eq!(fragment(&[], meta(1, 0), 300).unwrap().len(), 1);
        assert_eq!(fragment(&f, meta(1, 0), 1000).unwrap().len(), 1);
        assert_eq!(fragment(&f, meta(1, 0), 0), Err(FragmentError::NoRoom));
        let big = vec![0u8; MAX_FRAME_LEN + 1];
        assert_eq!(
            fragment(&big, meta(1, 0), 1352),
            Err(FragmentError::TooLarge)
        );
        let many = vec![0u8; 70_000];
        assert_eq!(fragment(&many, meta(1, 0), 1), Err(FragmentError::TooLarge));
    }

    #[test]
    fn in_order_round_trip() {
        let mut r = Reassembler::new(Default::default());
        let now = Instant::now();
        let mut sent = Vec::new();
        for id in 0..5u32 {
            let f = frame_bytes(5_000 + id as usize * 333, id as u8);
            for (h, p) in fragment(&f, meta(id, FLAG_KEY * (id == 0) as u8), 1352).unwrap() {
                r.push(&h, p, now);
            }
            sent.push(f);
        }
        let ev = r.poll(now);
        assert_eq!(delivered_ids(&ev), vec![0, 1, 2, 3, 4]);
        for (e, f) in ev.iter().zip(&sent) {
            match e {
                Event::Frame(c) => assert_eq!(&c.data, f),
                _ => panic!(),
            }
        }
        assert_eq!(r.stats().frames_completed, 5);
        assert_eq!(r.stats().frames_lost, 0);
    }

    #[test]
    fn reorder_within_and_across_frames() {
        let mut r = Reassembler::new(Default::default());
        r.start_display(0, 10);
        let now = Instant::now();
        let a = frame_bytes(4_000, 1);
        let b = frame_bytes(3_000, 2);
        let mut parts: Vec<_> = fragment(&a, meta(10, FLAG_KEY), 1000).unwrap();
        parts.extend(fragment(&b, meta(11, 0), 1000).unwrap());
        // Frame 11 first, then frame 10 backwards.
        let order = [4usize, 5, 6, 3, 2, 1, 0];
        for (n, &i) in order.iter().enumerate() {
            let (h, p) = parts[i];
            r.note_seq(i as u64 + 1);
            r.push(&h, p, now + Duration::from_micros(n as u64 * 100));
        }
        let ev = r.poll(now + Duration::from_millis(1));
        assert_eq!(delivered_ids(&ev), vec![10, 11]);
        let s = r.stats();
        assert_eq!(s.frames_lost, 0);
        assert_eq!(s.reorder_events, 4);
        assert_eq!(s.reorder_max, 6);
        assert_eq!(s.datagrams_expected, 7);
        assert_eq!(s.datagram_loss(), 0.0);
    }

    /// Live mode: every display starts at frame 0, so a first frame that never arrives is lost
    /// and the P-frame after it is gated; `settled` follows every release.
    #[test]
    fn live_start_at_zero_and_settled() {
        let mut r = Reassembler::new(ReassemblyConfig {
            first_frame_id: Some(0),
            ..Default::default()
        });
        let t0 = Instant::now();
        assert_eq!(r.settled(0), None);
        let f = frame_bytes(1_500, 1);
        for (h, p) in fragment(&f, meta(1, 0), 1000).unwrap() {
            r.push(&h, p, t0);
        }
        // Frame 0 never came: frame 1 completed, so after the grace 0 is lost and 1 is gated.
        let ev = r.poll(t0 + Duration::from_millis(6));
        assert_eq!(
            ev,
            vec![Event::Lost {
                display: 0,
                from: 0,
                to: 0
            }]
        );
        assert_eq!(r.settled(0), Some(1));
        assert!(r.awaiting_recovery(0));
        assert_eq!(r.stats().frames_gated, 1);
        for (h, p) in fragment(&f, meta(2, FLAG_KEY), 1000).unwrap() {
            r.push(&h, p, t0 + Duration::from_millis(7));
        }
        assert_eq!(
            delivered_ids(&r.poll(t0 + Duration::from_millis(7))),
            vec![2]
        );
        assert_eq!(r.settled(0), Some(2));
        assert!(!r.awaiting_recovery(0));
        assert_eq!(r.settled(1), None);
    }

    #[test]
    fn duplicate_fragments_are_counted_not_delivered_twice() {
        let mut r = Reassembler::new(Default::default());
        let now = Instant::now();
        let f = frame_bytes(2_500, 3);
        let parts = fragment(&f, meta(0, FLAG_KEY), 1000).unwrap();
        r.push(&parts[0].0, parts[0].1, now);
        r.push(&parts[0].0, parts[0].1, now);
        for (h, p) in &parts[1..] {
            r.push(h, p, now);
        }
        r.push(&parts[2].0, parts[2].1, now);
        let ev = r.poll(now);
        assert_eq!(delivered_ids(&ev), vec![0]);
        // One duplicate before completion; the one after delivery is late.
        assert_eq!(r.stats().fragments_duplicate, 1);
        assert_eq!(r.stats().fragments_late, 1);
    }

    #[test]
    fn loss_by_timeout_then_gate_until_keyframe() {
        let mut r = Reassembler::new(Default::default());
        r.start_display(0, 0);
        let t0 = Instant::now();
        let push_all =
            |r: &mut Reassembler, id: u32, flags: u8, skip: Option<usize>, t: Instant| {
                let f = frame_bytes(3_000, id as u8);
                for (i, (h, p)) in fragment(&f, meta(id, flags), 1000)
                    .unwrap()
                    .into_iter()
                    .enumerate()
                {
                    if Some(i) != skip {
                        r.push(&h, p, t);
                    }
                }
            };
        push_all(&mut r, 0, FLAG_KEY, None, t0);
        // Frame 1 loses its middle fragment; it is the only thing pending.
        push_all(&mut r, 1, 0, Some(1), t0 + Duration::from_millis(16));
        let ev = r.poll(t0 + Duration::from_millis(20));
        assert_eq!(delivered_ids(&ev), vec![0]);
        assert_eq!(r.next_deadline(), Some(t0 + Duration::from_millis(66)));
        let ev = r.poll(t0 + Duration::from_millis(66));
        assert_eq!(
            ev,
            vec![Event::Lost {
                display: 0,
                from: 1,
                to: 1
            }]
        );
        // A complete P-frame after the loss is gated; the next keyframe goes through.
        push_all(&mut r, 2, 0, None, t0 + Duration::from_millis(70));
        push_all(&mut r, 3, FLAG_KEY, None, t0 + Duration::from_millis(80));
        push_all(&mut r, 4, 0, None, t0 + Duration::from_millis(90));
        let ev = r.poll(t0 + Duration::from_millis(91));
        assert_eq!(delivered_ids(&ev), vec![3, 4]);
        let s = r.take_stats();
        assert_eq!(s.frames_lost, 1);
        assert_eq!(s.fragments_missing, 1);
        assert_eq!(s.frames_gated, 1);
        assert_eq!(s.frames_completed, 3);
        assert_eq!(r.stats(), &LossStats::default());
    }

    // Reordered first fragments: frame 11 starts before frame 10, the head. The deadline must
    // follow the head's own start, as `release` does, or a loop sleeping until it would spin.
    #[test]
    fn deadline_follows_head_start_when_reordered() {
        let mut r = Reassembler::new(Default::default());
        r.start_display(0, 10);
        let t0 = Instant::now();
        let first_only = |r: &mut Reassembler, id: u32, t: Instant| {
            let f = frame_bytes(3_000, id as u8);
            let (h, p) = fragment(&f, meta(id, 0), 1000)
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            r.push(&h, p, t);
        };
        first_only(&mut r, 11, t0);
        first_only(&mut r, 10, t0 + Duration::from_millis(3));
        assert_eq!(r.next_deadline(), Some(t0 + Duration::from_millis(53)));
        assert!(r.poll(t0 + Duration::from_millis(50)).is_empty());
        // Every poll at the deadline makes progress or moves the deadline forward.
        let mut now = t0 + Duration::from_millis(50);
        let mut rounds = 0;
        while let Some(at) = r.next_deadline() {
            rounds += 1;
            assert!(rounds < 10, "poll at next_deadline made no progress");
            now = now.max(at);
            let ev = r.poll(now);
            assert!(!ev.is_empty() || r.next_deadline().map_or(true, |n| n > at));
        }
        assert_eq!(r.stats().frames_lost, 2);
    }

    #[test]
    fn overtaken_by_a_later_frame_after_grace() {
        let mut r = Reassembler::new(Default::default());
        r.start_display(0, 0);
        let t0 = Instant::now();
        // Frame 0 never arrives at all; frame 1 completes.
        let f = frame_bytes(800, 9);
        for (h, p) in fragment(&f, meta(1, FLAG_RECOVERY_POINT), 1000).unwrap() {
            r.push(&h, p, t0);
        }
        assert!(delivered_ids(&r.poll(t0 + Duration::from_millis(4))).is_empty());
        let ev = r.poll(t0 + Duration::from_millis(5));
        assert_eq!(
            ev[0],
            Event::Lost {
                display: 0,
                from: 0,
                to: 0
            }
        );
        // Recovery point lifts the gate.
        assert_eq!(delivered_ids(&ev), vec![1]);
        assert_eq!(r.stats().fragments_missing, 0);
    }

    #[test]
    fn consecutive_losses_merge_and_far_jump_resets() {
        let mut r = Reassembler::new(Default::default());
        r.start_display(0, 0);
        let t0 = Instant::now();
        let f = frame_bytes(500, 1);
        for (h, p) in fragment(&f, meta(100, FLAG_KEY), 1000).unwrap() {
            r.push(&h, p, t0);
        }
        // 100 is >= max_ahead (64) past 0: frames 0..=36 are given up at once, 37..=99 later.
        let ev = r.poll(t0 + Duration::from_millis(10));
        assert_eq!(
            ev[0],
            Event::Lost {
                display: 0,
                from: 0,
                to: 99
            }
        );
        assert_eq!(delivered_ids(&ev), vec![100]);
        assert_eq!(r.stats().frames_lost, 100);
    }

    #[test]
    fn inconsistent_fragments_lose_the_frame() {
        let mut r = Reassembler::new(Default::default());
        r.start_display(0, 0);
        let t0 = Instant::now();
        let f = frame_bytes(2_000, 1);
        let parts = fragment(&f, meta(0, FLAG_KEY), 1000).unwrap();
        r.push(&parts[0].0, parts[0].1, t0);
        let mut bad = parts[1].0;
        bad.frame_len = 2_001;
        r.push(&bad, parts[1].1, t0);
        let ev = r.poll(t0);
        assert_eq!(
            ev,
            vec![Event::Lost {
                display: 0,
                from: 0,
                to: 0
            }]
        );
        assert!(r.stats().fragments_invalid >= 1);
        // Oversized declared length is rejected outright.
        let mut huge = parts[0].0;
        huge.frame_id = 1;
        huge.frame_len = (MAX_FRAME_LEN + 1) as u32;
        let before = r.stats().fragments_invalid;
        r.push(&huge, parts[0].1, t0);
        assert_eq!(r.stats().fragments_invalid, before + 1);
    }

    #[test]
    fn displays_are_independent_and_ids_wrap() {
        let mut r = Reassembler::new(Default::default());
        let t0 = Instant::now();
        r.start_display(0, u32::MAX);
        r.start_display(1, 5);
        for (display, id) in [(0u8, u32::MAX), (1, 5), (0, 0), (1, 6)] {
            let f = frame_bytes(1_500, id as u8);
            let m = FrameMeta {
                display,
                ..meta(id, FLAG_KEY)
            };
            for (h, p) in fragment(&f, m, 1000).unwrap() {
                r.push(&h, p, t0);
            }
        }
        let ev = r.poll(t0);
        let got: Vec<(u8, u32)> = ev
            .iter()
            .filter_map(|e| match e {
                Event::Frame(f) => Some((f.display, f.frame_id)),
                _ => None,
            })
            .collect();
        assert_eq!(got, vec![(0, u32::MAX), (1, 5), (0, 0), (1, 6)]);
        assert_eq!(r.stats().frames_lost, 0);
    }

    #[test]
    fn datagram_loss_from_seq_gaps() {
        let mut r = Reassembler::new(Default::default());
        for seq in [1u64, 2, 4, 5, 8] {
            r.note_seq(seq);
        }
        let s = r.stats();
        assert_eq!(s.datagrams_expected, 8);
        assert_eq!(s.datagrams_received, 5);
        assert!((s.datagram_loss() - 3.0 / 8.0).abs() < 1e-9);
    }
}
