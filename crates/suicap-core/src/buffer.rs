//! RAM replay buffer of encoded packets (plan §6.1). Pure logic, no FFI.
//!
//! Invariants: the head is always a video keyframe (eviction removes whole GOPs), and
//! snapshots clone `Bytes` handles only, so export never copies payloads.

use std::{collections::VecDeque, sync::Mutex};

use bytes::{Bytes, BytesMut};

/// Stream index of the video track; audio tracks use 1..
pub const VIDEO_STREAM: u8 = 0;

/// Chunk size for packing compressed packet payloads into shared backing buffers.
const SLAB_CHUNK_BYTES: usize = 64 * 1024;

/// Bump-allocates immutable [`Bytes`] slices from 64 KB chunks so hundreds of packets
/// per second share a few heap blocks instead of each allocating its own buffer.
pub struct PacketSlab {
    buf: BytesMut,
}

impl Default for PacketSlab {
    fn default() -> Self {
        Self::new()
    }
}

impl PacketSlab {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(SLAB_CHUNK_BYTES),
        }
    }

    pub fn copy_from_slice(&mut self, src: &[u8]) -> Bytes {
        if src.is_empty() {
            return Bytes::new();
        }
        if self.buf.capacity() - self.buf.len() < src.len() {
            self.buf = BytesMut::with_capacity(src.len().max(SLAB_CHUNK_BYTES));
        }
        self.buf.extend_from_slice(src);
        self.buf.split().freeze()
    }
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub data: Bytes,
    pub pts_us: i64,
    /// `dts_us - pts_us` fits in signed 32-bit microseconds (±35 min), keeping `Packet` 48 bytes.
    pub dts_offset_us: i32,
    pub stream: u8,
    pub key: bool,
}

impl Packet {
    pub fn new(stream: u8, key: bool, pts_us: i64, dts_us: i64, data: Bytes) -> Self {
        let dts_offset_us = dts_us
            .saturating_sub(pts_us)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        Self {
            data,
            pts_us,
            dts_offset_us,
            stream,
            key,
        }
    }

    pub fn dts_us(&self) -> i64 {
        self.pts_us + i64::from(self.dts_offset_us)
    }

    fn is_video_key(&self) -> bool {
        self.stream == VIDEO_STREAM && self.key
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub packets: usize,
    pub bytes: usize,
    /// Newest video PTS minus head keyframe PTS.
    pub span_us: i64,
}

pub struct ReplayBuffer {
    max_us: i64,
    max_bytes: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    packets: VecDeque<Packet>,
    bytes: usize,
    newest_video_pts: Option<i64>,
}

impl ReplayBuffer {
    pub fn new(max_secs: u32, max_bytes: usize) -> Self {
        Self {
            max_us: i64::from(max_secs) * 1_000_000,
            max_bytes,
            inner: Mutex::default(),
        }
    }

    /// Append a packet. Packets before the first video keyframe are dropped so the head
    /// stays decodable. Lock is held for push + evict only.
    pub fn push(&self, p: Packet) {
        let mut g = self.lock();
        if g.packets.is_empty() && !p.is_video_key() {
            return;
        }
        if p.stream == VIDEO_STREAM {
            g.newest_video_pts = Some(g.newest_video_pts.map_or(p.pts_us, |n| n.max(p.pts_us)));
        }
        g.bytes += p.data.len();
        g.packets.push_back(p);
        self.evict(&mut g);
    }

    /// Evict whole GOPs from the head while the remainder still covers `max_us`
    /// (so the buffer never holds less than requested), or while over `max_bytes`.
    fn evict(&self, g: &mut Inner) {
        let Some(newest) = g.newest_video_pts else { return };
        while let Some(next) = g.packets.iter().skip(1).position(Packet::is_video_key).map(|i| i + 1) {
            let next_pts = g.packets[next].pts_us;
            let over_time = newest - next_pts >= self.max_us;
            let over_bytes = g.bytes > self.max_bytes;
            if !over_time && !over_bytes {
                break;
            }
            let removed: usize = g.packets.drain(..next).map(|p| p.data.len()).sum();
            g.bytes -= removed;
            // Audio that arrived after the new head but is older than it.
            let mut stale = 0;
            g.packets.retain(|p| {
                let keep = p.stream == VIDEO_STREAM || p.pts_us >= next_pts;
                if !keep {
                    stale += p.data.len();
                }
                keep
            });
            g.bytes -= stale;
        }
        if g.packets.capacity() > 1024 && g.packets.capacity() > g.packets.len().saturating_mul(4) {
            g.packets.shrink_to_fit();
        }
    }

    /// Packets from the last video keyframe at or before `newest - secs` to now. If the
    /// buffer is shorter than `secs`, starts at the head. Empty if no video yet.
    pub fn snapshot(&self, secs: u32) -> Vec<Packet> {
        let g = self.lock();
        let Some(newest) = g.newest_video_pts else {
            return Vec::new();
        };
        let cutoff = newest - i64::from(secs) * 1_000_000;
        let start = g
            .packets
            .iter()
            .rposition(|p| p.is_video_key() && p.pts_us <= cutoff)
            .unwrap_or(0);
        let key_pts = g.packets[start].pts_us;
        g.packets
            .iter()
            .skip(start)
            .filter(|p| p.stream == VIDEO_STREAM || p.pts_us >= key_pts)
            .cloned()
            .collect()
    }

    /// Drop everything (device lost, preset/codec change: plan §7).
    pub fn flush(&self) {
        *self.lock() = Inner::default();
    }

    pub fn stats(&self) -> Stats {
        let g = self.lock();
        let span_us = match (g.packets.front(), g.newest_video_pts) {
            (Some(head), Some(newest)) => newest - head.pts_us,
            _ => 0,
        };
        Stats {
            packets: g.packets.len(),
            bytes: g.bytes,
            span_us,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves plain data behind; keep going.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1_000_000;

    fn pkt(stream: u8, key: bool, pts_us: i64, len: usize) -> Packet {
        Packet::new(stream, key, pts_us, pts_us, Bytes::from(vec![0u8; len]))
    }

    #[test]
    fn packet_layout_is_compact_and_preserves_dts() {
        assert!(std::mem::size_of::<Packet>() <= 48);
        let p = Packet::new(VIDEO_STREAM, false, 100_000, 83_333, Bytes::from_static(b"abc"));
        assert_eq!(p.dts_us(), 83_333);
    }

    #[test]
    fn packet_slab_packs_adjacent_slices_into_one_allocation() {
        let mut slab = PacketSlab::new();
        let a = slab.copy_from_slice(b"first");
        let b = slab.copy_from_slice(b"second");
        assert_eq!(&*a, b"first");
        assert_eq!(&*b, b"second");
        // SAFETY: both slices come from one 64 KB allocation chunk.
        let dist = unsafe { b.as_ptr().offset_from(a.as_ptr()) };
        assert_eq!(dist, 5);
    }

    /// `secs` of 10 fps video with a keyframe every second, plus one audio packet per frame.
    fn fill(buf: &ReplayBuffer, secs: i64) {
        for f in 0..secs * 10 {
            let pts = f * SEC / 10;
            buf.push(pkt(VIDEO_STREAM, f % 10 == 0, pts, 100));
            buf.push(pkt(1, true, pts, 10));
        }
    }

    fn head(buf: &ReplayBuffer) -> Packet {
        buf.lock().packets.front().cloned().expect("non-empty")
    }

    #[test]
    fn drops_packets_before_first_keyframe() {
        let buf = ReplayBuffer::new(10, usize::MAX);
        buf.push(pkt(1, true, 0, 10));
        buf.push(pkt(VIDEO_STREAM, false, 0, 10));
        assert_eq!(buf.stats().packets, 0);
        buf.push(pkt(VIDEO_STREAM, true, SEC, 10));
        assert_eq!(buf.stats().packets, 1);
    }

    #[test]
    fn time_eviction_keeps_at_least_max_and_head_is_keyframe() {
        let buf = ReplayBuffer::new(5, usize::MAX);
        fill(&buf, 20);
        let s = buf.stats();
        assert!(s.span_us >= 5 * SEC, "span {} < 5 s", s.span_us);
        assert!(s.span_us < 6 * SEC, "span {} kept more than one extra GOP", s.span_us);
        assert!(head(&buf).is_video_key());
    }

    #[test]
    fn byte_eviction_respects_limit_on_gop_boundaries() {
        // One GOP = 10 * (100 + 10) = 1100 bytes.
        let buf = ReplayBuffer::new(3600, 3_000);
        fill(&buf, 20);
        let s = buf.stats();
        assert!(s.bytes <= 3_000, "bytes {}", s.bytes);
        assert!(s.bytes >= 2 * 1_100, "evicted too much: {}", s.bytes);
        assert!(head(&buf).is_video_key());
    }

    #[test]
    fn byte_accounting_matches_contents() {
        let buf = ReplayBuffer::new(2, 5_000);
        fill(&buf, 9);
        let g = buf.lock();
        assert_eq!(g.bytes, g.packets.iter().map(|p| p.data.len()).sum::<usize>());
    }

    #[test]
    fn eviction_removes_late_audio_older_than_new_head() {
        let buf = ReplayBuffer::new(1, usize::MAX);
        fill(&buf, 2);
        // Late audio stamped at 0.5 s arrives after the 2 s keyframe would evict GOP 0.
        buf.push(pkt(1, true, SEC / 2, 10));
        buf.push(pkt(VIDEO_STREAM, true, 2 * SEC, 100));
        let head_pts = head(&buf).pts_us;
        assert!(buf.lock().packets.iter().all(|p| p.pts_us >= head_pts));
    }

    #[test]
    fn snapshot_starts_on_keyframe_at_or_before_cutoff() {
        let buf = ReplayBuffer::new(60, usize::MAX);
        fill(&buf, 20); // newest video pts = 19.9 s
        let snap = buf.snapshot(5); // cutoff 14.9 s -> keyframe at 14 s
        assert!(snap[0].is_video_key());
        assert_eq!(snap[0].pts_us, 14 * SEC);
        let len = 19 * SEC + 9 * SEC / 10 - snap[0].pts_us;
        assert!(
            (5 * SEC..6 * SEC).contains(&len),
            "clip length must be requested + < 1 GOP"
        );
        assert!(snap.iter().all(|p| p.pts_us >= snap[0].pts_us));
    }

    #[test]
    fn snapshot_longer_than_buffer_starts_at_head() {
        let buf = ReplayBuffer::new(60, usize::MAX);
        fill(&buf, 3);
        let snap = buf.snapshot(30);
        assert_eq!(snap[0].pts_us, 0);
        assert_eq!(snap.len(), buf.stats().packets);
    }

    #[test]
    fn snapshot_shares_payloads() {
        let buf = ReplayBuffer::new(60, usize::MAX);
        fill(&buf, 2);
        let snap = buf.snapshot(60);
        let g = buf.lock();
        assert_eq!(snap[0].data.as_ptr(), g.packets[0].data.as_ptr());
    }

    #[test]
    fn empty_and_flush() {
        let buf = ReplayBuffer::new(60, usize::MAX);
        assert!(buf.snapshot(10).is_empty());
        fill(&buf, 2);
        buf.flush();
        assert_eq!(
            buf.stats(),
            Stats {
                packets: 0,
                bytes: 0,
                span_us: 0
            }
        );
        assert!(buf.snapshot(10).is_empty());
    }
}
