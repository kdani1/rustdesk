//! Frame-level forward error correction (M21): a systematic Reed-Solomon erasure code over
//! GF(2^8), in-tree (no crate).
//!
//! - Field: GF(256) with the polynomial x^8 + x^4 + x^3 + x^2 + 1 (0x11D), generator 2.
//! - Code: data shards go out unchanged; parity shard `r` of a block is
//!   `sum_j C[r][j] * data[j]` with the Cauchy matrix `C[r][j] = 1 / (x_r + y_j)`,
//!   `x_r = 255 - r`, `y_j = j`. Any square submatrix of a Cauchy matrix is invertible, so any
//!   `data` of the `data + parity` shards of a block rebuild the block (an MDS code), as long as
//!   `data + parity <= 256` (the x and y sets stay disjoint); blocks are kept at 255 or fewer.
//! - Data shards may be shorter than the block's shard length (only the frame's last fragment
//!   is): they count as zero-padded, nothing pads them on the wire.
//! - Speed: `mul_add` (dst ^= c * src) runs AVX2 or SSSE3 nibble-table shuffles on x86-64
//!   (runtime detection), two nibble-table lookups per byte elsewhere. The nibble tables of all
//!   256 coefficients are built at compile time (8 KB), so a call has no setup. 20% parity on a
//!   40 KB frame costs 6 x 30 shard passes of 1352 bytes.
//! - Send order: [`Protect`] yields a block's parity only after its data shards, and encodes it
//!   then, so the frame's first datagram never waits for FEC.
//!
//! Layout (Sunshine style): a frame's `n` data fragments are split into at most
//! [`MAX_BLOCKS`] blocks of at most [`MAX_DATA_PER_BLOCK`] data shards, sizes as equal as
//! possible (the first `n % blocks` blocks have one more); each block gets
//! `max(MIN_PARITY, ceil(data * percent / 100))` parity shards, capped so a block stays within
//! [`MAX_SHARDS`]. A frame whose 4 blocks are too large for that (at 20%: above 4 x 212
//! fragments) gets fewer parity shards than the percentage (still at least 1); one of more
//! than 4 x 254 fragments (about 1.37 MB) goes without FEC.
//! See docs/UDP-VIDEO.md, "FEC".

use super::packet::{Inner, VideoHeader};
use std::ops::Range;
use std::time::{Duration, Instant};

/// Data plus parity shards of one block, at most.
pub const MAX_SHARDS: usize = 255;
/// Data shards per block, at most (roadmap: 200).
pub const MAX_DATA_PER_BLOCK: usize = 200;
/// Blocks per frame, at most (Sunshine: 4).
pub const MAX_BLOCKS: usize = 4;
/// Parity shards per block, at least (Sunshine: 2), unless the block is too large for them.
pub const MIN_PARITY: usize = 2;
/// Default of `udp-video-fec-percent` (Sunshine: 20).
pub const DEFAULT_PERCENT: u32 = 20;
/// Largest `udp-video-fec-percent`.
pub const MAX_PERCENT: u32 = 100;

// ---------------------------------------------------------------------------------------------
// GF(256)

const POLY: u16 = 0x11D;

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
}

const fn build_tables() -> Tables {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
        i += 1;
    }
    while i < 512 {
        exp[i] = exp[i - 255];
        i += 1;
    }
    Tables { exp, log }
}

static GF: Tables = build_tables();

const fn const_mul(t: &Tables, a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
    }
}

/// `[c][0][i] = c * i` and `[c][1][i] = c * (i << 4)`: the products of `c` with every low and
/// every high nibble.
const fn build_nibbles() -> [[[u8; 16]; 2]; 256] {
    let t = build_tables();
    let mut out = [[[0u8; 16]; 2]; 256];
    let mut c = 1;
    while c < 256 {
        let mut i = 0;
        while i < 16 {
            out[c][0][i] = const_mul(&t, c as u8, i as u8);
            out[c][1][i] = const_mul(&t, c as u8, (i << 4) as u8);
            i += 1;
        }
        c += 1;
    }
    out
}

static NIBBLES: [[[u8; 16]; 2]; 256] = build_nibbles();

#[inline]
pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        GF.exp[GF.log[a as usize] as usize + GF.log[b as usize] as usize]
    }
}

/// Multiplicative inverse; `a` must not be 0.
#[inline]
pub fn inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    GF.exp[255 - GF.log[a as usize] as usize]
}

/// Cauchy coefficient of parity row `r` for data column `j` (`r + j < 255`).
#[inline]
fn coef(r: usize, j: usize) -> u8 {
    inv((255 - r) as u8 ^ j as u8)
}

/// `dst[i] ^= c * src[i]` for every byte of `src` (`dst` at least as long).
pub fn mul_add(dst: &mut [u8], src: &[u8], c: u8) {
    let dst = &mut dst[..src.len()];
    match c {
        0 => {}
        1 => {
            for (d, s) in dst.iter_mut().zip(src) {
                *d ^= *s;
            }
        }
        _ => {
            #[cfg(target_arch = "x86_64")]
            {
                if src.len() >= 32 && std::is_x86_feature_detected!("avx2") {
                    // SAFETY: the CPU has AVX2 (checked above).
                    unsafe { simd::mul_add_avx2(dst, src, c) };
                    return;
                }
                if src.len() >= 16 && std::is_x86_feature_detected!("ssse3") {
                    // SAFETY: the CPU has SSSE3 (checked above).
                    unsafe { simd::mul_add_ssse3(dst, src, c) };
                    return;
                }
            }
            mul_add_scalar(dst, src, c);
        }
    }
}

fn mul_add_scalar(dst: &mut [u8], src: &[u8], c: u8) {
    let (lo, hi) = nibble_tables(c);
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= lo[(*s & 0x0f) as usize] ^ hi[(*s >> 4) as usize];
    }
}

/// Products of `c` with every low nibble and every high nibble (precomputed).
#[inline]
fn nibble_tables(c: u8) -> (&'static [u8; 16], &'static [u8; 16]) {
    let t = &NIBBLES[c as usize];
    (&t[0], &t[1])
}

#[cfg(target_arch = "x86_64")]
mod simd {
    use super::{mul_add_scalar, nibble_tables};
    use std::arch::x86_64::*;

    /// `dst.len() == src.len()`.
    #[target_feature(enable = "avx2")]
    pub unsafe fn mul_add_avx2(dst: &mut [u8], src: &[u8], c: u8) {
        let (lo, hi) = nibble_tables(c);
        let lo = _mm256_broadcastsi128_si256(_mm_loadu_si128(lo.as_ptr() as *const __m128i));
        let hi = _mm256_broadcastsi128_si256(_mm_loadu_si128(hi.as_ptr() as *const __m128i));
        let mask = _mm256_set1_epi8(0x0f);
        let n = src.len() / 32 * 32;
        let mut i = 0;
        while i < n {
            let s = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            let d = _mm256_loadu_si256(dst.as_ptr().add(i) as *const __m256i);
            let l = _mm256_shuffle_epi8(lo, _mm256_and_si256(s, mask));
            let h = _mm256_shuffle_epi8(hi, _mm256_and_si256(_mm256_srli_epi64(s, 4), mask));
            let r = _mm256_xor_si256(d, _mm256_xor_si256(l, h));
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r);
            i += 32;
        }
        mul_add_scalar(&mut dst[n..], &src[n..], c);
    }

    /// `dst.len() == src.len()`.
    #[target_feature(enable = "ssse3")]
    pub unsafe fn mul_add_ssse3(dst: &mut [u8], src: &[u8], c: u8) {
        let (lo, hi) = nibble_tables(c);
        let lo = _mm_loadu_si128(lo.as_ptr() as *const __m128i);
        let hi = _mm_loadu_si128(hi.as_ptr() as *const __m128i);
        let mask = _mm_set1_epi8(0x0f);
        let n = src.len() / 16 * 16;
        let mut i = 0;
        while i < n {
            let s = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
            let d = _mm_loadu_si128(dst.as_ptr().add(i) as *const __m128i);
            let l = _mm_shuffle_epi8(lo, _mm_and_si128(s, mask));
            let h = _mm_shuffle_epi8(hi, _mm_and_si128(_mm_srli_epi64(s, 4), mask));
            let r = _mm_xor_si128(d, _mm_xor_si128(l, h));
            _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, r);
            i += 16;
        }
        mul_add_scalar(&mut dst[n..], &src[n..], c);
    }
}

// ---------------------------------------------------------------------------------------------
// The code

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FecError {
    /// Fewer parity shards than missing data shards.
    NotEnough,
    /// A data shard longer than the shard length, parity shards of another length, or a block
    /// larger than `MAX_SHARDS`.
    BadShape,
}

/// The parity shards of one block: `parity` shards of `shard_len` bytes. Data shards shorter
/// than `shard_len` count as zero-padded.
pub fn encode(data: &[&[u8]], shard_len: usize, parity: usize) -> Vec<Vec<u8>> {
    debug_assert!(data.len() + parity <= MAX_SHARDS);
    let mut out = vec![vec![0u8; shard_len]; parity];
    for (j, d) in data.iter().enumerate() {
        let d = &d[..d.len().min(shard_len)];
        for (r, p) in out.iter_mut().enumerate() {
            mul_add(p, d, coef(r, j));
        }
    }
    out
}

/// Rebuilds the missing data shards of one block in place from the parity shards that arrived.
/// Present data shards may be shorter than `shard_len`; rebuilt ones are `shard_len` long (the
/// caller trims the frame's last fragment). Returns the indices rebuilt.
pub fn reconstruct(
    data: &mut [Option<Vec<u8>>],
    parity: &[Option<Vec<u8>>],
    shard_len: usize,
) -> Result<Vec<usize>, FecError> {
    if data.len() + parity.len() > MAX_SHARDS {
        return Err(FecError::BadShape);
    }
    let missing: Vec<usize> = (0..data.len()).filter(|i| data[*i].is_none()).collect();
    if missing.is_empty() {
        return Ok(missing);
    }
    let rows: Vec<usize> = (0..parity.len())
        .filter(|r| parity[*r].is_some())
        .take(missing.len())
        .collect();
    if rows.len() < missing.len() {
        return Err(FecError::NotEnough);
    }
    if data.iter().flatten().any(|d| d.len() > shard_len)
        || parity.iter().flatten().any(|p| p.len() != shard_len)
    {
        return Err(FecError::BadShape);
    }
    // Syndromes: each chosen parity shard minus the present data's share of it.
    let mut syn: Vec<Vec<u8>> = rows
        .iter()
        .map(|r| parity[*r].clone().unwrap_or_default())
        .collect();
    for (a, &r) in rows.iter().enumerate() {
        for (j, d) in data.iter().enumerate() {
            if let Some(d) = d {
                mul_add(&mut syn[a], d, coef(r, j));
            }
        }
    }
    // The m x m Cauchy submatrix of the chosen rows and the missing columns, inverted.
    let mut a: Vec<Vec<u8>> = rows
        .iter()
        .map(|&r| missing.iter().map(|&j| coef(r, j)).collect())
        .collect();
    let a_inv = invert(&mut a).ok_or(FecError::BadShape)?;
    for (b, &j) in missing.iter().enumerate() {
        let mut out = vec![0u8; shard_len];
        for (k, s) in syn.iter().enumerate() {
            mul_add(&mut out, s, a_inv[b][k]);
        }
        data[j] = Some(out);
    }
    Ok(missing)
}

/// Gauss-Jordan inverse of a square matrix over GF(256); None if singular (never for a Cauchy
/// submatrix).
fn invert(a: &mut [Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let n = a.len();
    let mut inv_m: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut row = vec![0u8; n];
            row[i] = 1;
            row
        })
        .collect();
    for col in 0..n {
        let pivot = (col..n).find(|&r| a[r][col] != 0)?;
        a.swap(col, pivot);
        inv_m.swap(col, pivot);
        let f = inv(a[col][col]);
        for x in a[col].iter_mut() {
            *x = mul(*x, f);
        }
        for x in inv_m[col].iter_mut() {
            *x = mul(*x, f);
        }
        for r in 0..n {
            if r == col || a[r][col] == 0 {
                continue;
            }
            let f = a[r][col];
            let (pr, pi) = (a[col].clone(), inv_m[col].clone());
            mul_add(&mut a[r], &pr, f);
            mul_add(&mut inv_m[r], &pi, f);
        }
    }
    Some(inv_m)
}

// ---------------------------------------------------------------------------------------------
// Frame layout

/// `(block count << 4) | block index` in `VideoHeader::fec_block`; 0 = no FEC.
pub fn block_byte(blocks: usize, index: usize) -> u8 {
    ((blocks as u8) << 4) | (index as u8 & 0x0f)
}

/// (block count, block index) of a `fec_block` byte; None for 0 (no FEC) or an index outside.
pub fn split_block_byte(b: u8) -> Option<(usize, usize)> {
    let (count, index) = ((b >> 4) as usize, (b & 0x0f) as usize);
    (count > 0 && index < count).then_some((count, index))
}

/// The data fragments of block `b` when `n` fragments are split into `blocks` blocks.
pub fn block_range(n: usize, blocks: usize, b: usize) -> Range<usize> {
    let base = n / blocks;
    let rem = n % blocks;
    let start = b * base + b.min(rem);
    start..start + base + (b < rem) as usize
}

/// Parity shards for a block of `data` shards.
pub fn parity_for(data: usize, percent: u32) -> usize {
    let want = (data * percent as usize).div_ceil(100).max(MIN_PARITY);
    want.min(MAX_SHARDS.saturating_sub(data))
}

/// Largest block whose parity at `percent` still fits `MAX_SHARDS`, at most
/// `MAX_DATA_PER_BLOCK`.
fn max_data(percent: u32) -> usize {
    (1..=MAX_DATA_PER_BLOCK)
        .rev()
        .find(|d| *d + (*d * percent as usize).div_ceil(100).max(MIN_PARITY) <= MAX_SHARDS)
        .unwrap_or(1)
}

/// How a frame of `n` data fragments is protected: the block count, or None for no FEC
/// (`percent` 0, an empty frame, or more than 4 x 254 fragments).
pub fn plan_blocks(n: usize, percent: u32) -> Option<usize> {
    if percent == 0 || n == 0 {
        return None;
    }
    let blocks = n.div_ceil(max_data(percent)).min(MAX_BLOCKS).min(n);
    let largest = n.div_ceil(blocks);
    (largest < MAX_SHARDS).then_some(blocks)
}

/// One datagram of a protected (or unprotected) frame, in send order.
pub enum Shard<'a> {
    Data(VideoHeader, &'a [u8]),
    Parity(VideoHeader, Vec<u8>),
}

impl Shard<'_> {
    pub fn inner(&self) -> Inner<'_> {
        match self {
            Shard::Data(h, p) => Inner::Video(*h, p),
            Shard::Parity(h, p) => Inner::Fec(*h, p),
        }
    }

    pub fn payload_len(&self) -> usize {
        match self {
            Shard::Data(_, p) => p.len(),
            Shard::Parity(_, p) => p.len(),
        }
    }

    pub fn is_parity(&self) -> bool {
        matches!(self, Shard::Parity(..))
    }
}

/// Adds parity to a fragmented frame (`fragment::fragment`'s output) at `percent`: each block's
/// data shards, then its parity shards, block after block. Returns the shards and the block
/// count (0: sent without FEC). Eager form of [`Protect`].
pub fn protect<'a>(parts: Vec<(VideoHeader, &'a [u8])>, percent: u32) -> (Vec<Shard<'a>>, usize) {
    let p = Protect::new(parts, percent);
    let blocks = p.blocks();
    (p.collect(), blocks)
}

/// The shards of a protected frame in send order (each block's data shards, then its parity
/// shards), encoding a block's parity only when the iterator reaches it: the data shards before
/// it can be sent while nothing has been encoded yet.
pub struct Protect<'a> {
    /// Data fragments with their FEC header fields set.
    parts: Vec<(VideoHeader, &'a [u8])>,
    blocks: usize,
    percent: u32,
    /// Next data fragment to yield.
    next: usize,
    /// Block whose parity is due once `next` reaches its end.
    block: usize,
    parity: std::vec::IntoIter<Shard<'a>>,
    encode_time: Duration,
}

impl<'a> Protect<'a> {
    pub fn new(mut parts: Vec<(VideoHeader, &'a [u8])>, percent: u32) -> Self {
        let n = parts.len();
        let blocks = match plan_blocks(n, percent) {
            Some(b) if parts.first().map_or(false, |(h, _)| h.frame_len > 0) => b,
            _ => 0,
        };
        for b in 0..blocks {
            let range = block_range(n, blocks, b);
            let parity = parity_for(range.len(), percent) as u8;
            for (h, _) in &mut parts[range] {
                h.fec_block = block_byte(blocks, b);
                h.fec_parity = parity;
            }
        }
        Self {
            parts,
            blocks,
            percent,
            next: 0,
            block: 0,
            parity: Vec::new().into_iter(),
            encode_time: Duration::ZERO,
        }
    }

    /// Block count (0: no FEC).
    pub fn blocks(&self) -> usize {
        self.blocks
    }

    /// Payload bytes of all shards, parity included, without encoding anything.
    pub fn wire_len(&self) -> usize {
        let data: usize = self.parts.iter().map(|(_, p)| p.len()).sum();
        let n = self.parts.len();
        let parity: usize = (0..self.blocks)
            .map(|b| {
                let range = block_range(n, self.blocks, b);
                // Only the frame's last fragment is short, so a block's first shard is its
                // longest: the parity length.
                parity_for(range.len(), self.percent) * self.parts[range.start].1.len()
            })
            .sum();
        data + parity
    }

    /// Time spent encoding parity so far.
    pub fn encode_time(&self) -> Duration {
        self.encode_time
    }

    /// Encodes the parity of block `self.block` and moves on to the next block.
    fn encode_block(&mut self) {
        let begin = Instant::now();
        let range = block_range(self.parts.len(), self.blocks, self.block);
        let data: Vec<&[u8]> = self.parts[range.clone()].iter().map(|(_, p)| *p).collect();
        let shard_len = data.first().map_or(0, |d| d.len());
        let template = self.parts[range.start].0;
        let parity = parity_for(data.len(), self.percent);
        let shards: Vec<Shard<'a>> = encode(&data, shard_len, parity)
            .into_iter()
            .enumerate()
            .map(|(r, p)| {
                let h = VideoHeader {
                    frag_index: r as u16,
                    ..template
                };
                Shard::Parity(h, p)
            })
            .collect();
        self.parity = shards.into_iter();
        self.block += 1;
        self.encode_time += begin.elapsed();
    }
}

impl<'a> Iterator for Protect<'a> {
    type Item = Shard<'a>;

    fn next(&mut self) -> Option<Shard<'a>> {
        loop {
            if let Some(s) = self.parity.next() {
                return Some(s);
            }
            if self.block < self.blocks
                && self.next == block_range(self.parts.len(), self.blocks, self.block).end
            {
                self.encode_block();
                continue;
            }
            let (h, p) = *self.parts.get(self.next)?;
            self.next += 1;
            return Some(Shard::Data(h, p));
        }
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    /// Small deterministic generator (xorshift), so the tests need no crate.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    #[test]
    fn latency_fec_field_arithmetic() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1, "{a}");
            assert_eq!(mul(a, 1), a);
            assert_eq!(mul(a, 0), 0);
        }
        // Distributive and commutative on a sample.
        let mut rng = Rng(7);
        for _ in 0..2000 {
            let (a, b, c) = (rng.next() as u8, rng.next() as u8, rng.next() as u8);
            assert_eq!(mul(a, b), mul(b, a));
            assert_eq!(mul(a, b ^ c), mul(a, b) ^ mul(a, c));
            assert_eq!(mul(mul(a, b), c), mul(a, mul(b, c)));
        }
    }

    #[test]
    fn latency_fec_mul_add_paths_agree() {
        let mut rng = Rng(11);
        for len in [0usize, 1, 15, 16, 17, 31, 32, 33, 100, 1352] {
            let src = rng.bytes(len);
            let base = rng.bytes(len + 3);
            for c in 0..=255u8 {
                let mut fast = base.clone();
                mul_add(&mut fast, &src, c);
                let mut slow = base.clone();
                for i in 0..len {
                    slow[i] ^= mul(c, src[i]);
                }
                assert_eq!(fast, slow, "len {len} c {c}");
                #[cfg(target_arch = "x86_64")]
                {
                    if len >= 16 && std::is_x86_feature_detected!("ssse3") {
                        let mut s = base.clone();
                        unsafe { simd::mul_add_ssse3(&mut s[..len], &src, c) };
                        assert_eq!(s, slow, "ssse3 len {len} c {c}");
                    }
                }
            }
        }
    }

    fn shards(rng: &mut Rng, d: usize, len: usize, last_len: usize) -> Vec<Vec<u8>> {
        (0..d)
            .map(|i| rng.bytes(if i + 1 == d { last_len } else { len }))
            .collect()
    }

    #[test]
    fn latency_fec_every_erasure_pattern_small() {
        // d = 5, p = 3: every pattern of up to 3 erasures over the 8 shards recovers.
        let mut rng = Rng(3);
        let (d, p, len) = (5usize, 3usize, 40usize);
        let data = shards(&mut rng, d, len, 17);
        let refs: Vec<&[u8]> = data.iter().map(|v| v.as_slice()).collect();
        let parity = encode(&refs, len, p);
        for mask in 0u32..(1 << (d + p)) {
            if mask.count_ones() as usize > p {
                continue;
            }
            let mut dd: Vec<Option<Vec<u8>>> = (0..d)
                .map(|i| (mask & (1 << i) == 0).then(|| data[i].clone()))
                .collect();
            let pp: Vec<Option<Vec<u8>>> = (0..p)
                .map(|i| (mask & (1 << (d + i)) == 0).then(|| parity[i].clone()))
                .collect();
            let rebuilt = reconstruct(&mut dd, &pp, len).unwrap();
            for i in 0..d {
                let got = dd[i].as_ref().unwrap();
                if rebuilt.contains(&i) {
                    assert_eq!(got.len(), len);
                    assert_eq!(&got[..data[i].len()], &data[i][..], "mask {mask:b}");
                    assert!(got[data[i].len()..].iter().all(|b| *b == 0));
                } else {
                    assert_eq!(got, &data[i]);
                }
            }
        }
        // One erasure too many.
        let mut dd: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        dd[0] = None;
        dd[1] = None;
        let pp = vec![Some(parity[0].clone()), None, None];
        assert_eq!(reconstruct(&mut dd, &pp, len), Err(FecError::NotEnough));
        // Parity of another length.
        let pp = vec![Some(vec![0u8; len + 1]), Some(parity[1].clone()), None];
        assert_eq!(reconstruct(&mut dd, &pp, len), Err(FecError::BadShape));
    }

    #[test]
    fn latency_fec_large_block_random_erasures() {
        let mut rng = Rng(99);
        for (d, p) in [(200usize, 40usize), (212, 43), (253, 2), (1, 2), (2, 2)] {
            let data = shards(&mut rng, d, 1352, 999.min(1352));
            let refs: Vec<&[u8]> = data.iter().map(|v| v.as_slice()).collect();
            let parity = encode(&refs, 1352, p);
            for round in 0..3 {
                let mut dd: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
                let mut pp: Vec<Option<Vec<u8>>> = parity.iter().cloned().map(Some).collect();
                // Erase exactly p shards, spread over data and parity.
                let mut erased = 0;
                while erased < p {
                    let i = (rng.next() as usize) % (d + p);
                    let slot = if i < d { &mut dd[i] } else { &mut pp[i - d] };
                    if slot.take().is_some() {
                        erased += 1;
                    }
                }
                reconstruct(&mut dd, &pp, 1352).unwrap();
                for i in 0..d {
                    let got = dd[i].as_ref().unwrap();
                    assert_eq!(&got[..data[i].len()], &data[i][..], "d {d} p {p} round {round}");
                }
            }
        }
    }

    #[test]
    fn latency_fec_layout() {
        assert_eq!(block_byte(1, 0), 0x10);
        assert_eq!(split_block_byte(0x10), Some((1, 0)));
        assert_eq!(split_block_byte(0x43), Some((4, 3)));
        assert_eq!(split_block_byte(0), None);
        assert_eq!(split_block_byte(0x24), None);
        // Even split, the first blocks one larger.
        assert_eq!(block_range(10, 3, 0), 0..4);
        assert_eq!(block_range(10, 3, 1), 4..7);
        assert_eq!(block_range(10, 3, 2), 7..10);
        assert_eq!(parity_for(1, 20), 2);
        assert_eq!(parity_for(10, 20), 2);
        assert_eq!(parity_for(11, 20), 3);
        assert_eq!(parity_for(200, 20), 40);
        assert_eq!(parity_for(250, 20), 5);
        assert_eq!(parity_for(254, 20), 1);
        assert_eq!(max_data(20), 200);
        assert_eq!(max_data(100), 127);
        assert_eq!(plan_blocks(0, 20), None);
        assert_eq!(plan_blocks(5, 0), None);
        assert_eq!(plan_blocks(1, 20), Some(1));
        assert_eq!(plan_blocks(200, 20), Some(1));
        assert_eq!(plan_blocks(201, 20), Some(2));
        assert_eq!(plan_blocks(800, 20), Some(4));
        assert_eq!(plan_blocks(1000, 20), Some(4));
        assert_eq!(plan_blocks(1016, 20), Some(4));
        assert_eq!(plan_blocks(1017, 20), None);
        assert_eq!(plan_blocks(3, 100), Some(1));
        for n in 1..=1016 {
            let b = plan_blocks(n, 20).unwrap();
            assert!(b <= MAX_BLOCKS);
            for i in 0..b {
                let d = block_range(n, b, i).len();
                assert!(d >= 1 && d + parity_for(d, 20) <= MAX_SHARDS, "n {n}");
                assert!(parity_for(d, 20) >= 1);
            }
            assert_eq!(block_range(n, b, b - 1).end, n);
        }
    }

    #[test]
    fn latency_fec_protect_headers_and_order() {
        let frame: Vec<u8> = (0..30_000u32).map(|i| (i * 7) as u8).collect();
        let meta = super::super::fragment::FrameMeta {
            frame_id: 5,
            ..Default::default()
        };
        let parts = super::super::fragment::fragment(&frame, meta, 1352).unwrap();
        let n = parts.len();
        let (shards, blocks) = protect(parts, 20);
        assert_eq!(blocks, 1);
        let parity = parity_for(n, 20);
        assert_eq!(shards.len(), n + parity);
        for (i, s) in shards.iter().enumerate() {
            let h = match s {
                Shard::Data(h, _) | Shard::Parity(h, _) => *h,
            };
            assert_eq!((h.fec_block, h.fec_parity as usize), (0x10, parity));
            assert_eq!(h.frag_count as usize, n);
            assert_eq!(s.is_parity(), i >= n);
            if s.is_parity() {
                assert_eq!(h.frag_index as usize, i - n);
                assert_eq!(s.payload_len(), 1352);
            }
        }
        // Lazy: nothing is encoded before the block's data shards were taken, and the bytes
        // announced are the bytes yielded.
        let parts = super::super::fragment::fragment(&frame, meta, 1352).unwrap();
        let mut lazy = Protect::new(parts, 20);
        let wire = lazy.wire_len();
        let firsts: Vec<_> = lazy.by_ref().take(n).collect();
        assert!(firsts.iter().all(|s| !s.is_parity()));
        assert_eq!(lazy.encode_time(), Duration::ZERO);
        let rest: Vec<_> = lazy.collect();
        assert_eq!(rest.len(), parity);
        let yielded: usize = firsts.iter().chain(&rest).map(|s| s.payload_len()).sum();
        assert_eq!(yielded, wire);
        // Off: the fragments unchanged.
        let parts = super::super::fragment::fragment(&frame, meta, 1352).unwrap();
        let (shards, blocks) = protect(parts, 0);
        assert_eq!((shards.len(), blocks), (n, 0));
        assert!(shards.iter().all(|s| !s.is_parity()));
    }
}
