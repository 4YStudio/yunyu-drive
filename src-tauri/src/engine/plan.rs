//! Segment planning: how many connections, and where the cut points go.
//!
//! The rules come from measuring what actually moves the needle on Quark and
//! Aliyun CDNs (see the notes on `MIN_SEGMENTED` and the size ladders):
//!
//! * **Many small segments beat few large ones.** A CDN caps per *connection*,
//!   and a handful of connections are always much slower than the rest. With
//!   few, large segments each connection gets one or two and a slow one stalls
//!   the whole transfer while the fast ones sit idle ("tail collapse"). Cutting
//!   into ~8 pieces per connection lets a fast connection come back for more.
//! * **But not too small.** Every segment is an HTTP request, and a fresh TLS
//!   handshake costs 0.3–0.6 s. At 128 KiB a 13 KiB/s connection spends 4.7 s
//!   moving bytes, so the handshake would eat 6–13 % of the transfer.
//! * **The head of the file is cut finer still**, so the first megabytes land
//!   quickly instead of waiting for one slow connection to finish a huge first
//!   piece.

/// Below this the handshake overhead outweighs the extra connections, so the
/// caller keeps the single-stream path.
pub const MIN_SEGMENTED: u64 = 8 * 1024 * 1024;

/// Physical ceiling on worker count, whatever a drive's budget says.
pub const MAX_CONNECTIONS: usize = 4096;

/// Guard against a pathological segment count (a 100 GiB file at 128 KiB would
/// otherwise ask for 800 000 workers' worth of bookkeeping).
pub const MAX_SEGMENTS: u64 = 65_536;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// One byte range of the file. `end` is exclusive, so `len() == end - start`
/// and consecutive segments tile `[0, total)` without gaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
}

impl Segment {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

/// How many connections to open for a file of `size` bytes.
///
/// Ladder chosen for a desktop client: small files stay sequential (one
/// connection is already faster than the handshake overhead), and past 10 GiB
/// the win per extra connection has flattened out.
///
/// The rungs are non-decreasing. Measured on a 47 MiB Quark file, this drive
/// hands out speed per connection rather than per account, so the width is what
/// decides the download: 32 connections moved 523 KB/s and 256 moved 1818 KB/s.
/// The same reasoning holds for bigger files, so the ladder only widens with
/// size — a narrower rung in the middle would make a 100 MiB file slower than a
/// 47 MiB one.
pub fn connections_for(size: u64) -> usize {
    if size < 8 * MIB {
        1
    } else if size < GIB {
        256
    } else if size < 10 * GIB {
        384
    } else {
        512
    }
}

/// Smallest segment worth issuing, by file size.
fn min_segment(total: u64) -> u64 {
    if total < 8 * MIB {
        512 * KIB
    } else if total < 64 * MIB {
        256 * KIB
    } else {
        128 * KIB
    }
}

/// Largest segment worth issuing, by file size. Past this a single slow
/// connection holds too much of the file.
fn max_segment(total: u64) -> u64 {
    if total < 32 * MIB {
        MIB
    } else if total < 256 * MIB {
        2 * MIB
    } else if total < 2048 * MIB {
        8 * MIB
    } else {
        16 * MIB
    }
}

/// Cuts `[0, total)` into segments.
///
/// Pure and deterministic: the same `(total, connections)` always yields the
/// same cuts. The resume metadata depends on that — a different plan for the
/// same file would make every saved offset meaningless.
pub fn split(total: u64, connections: usize) -> Vec<Segment> {
    if total == 0 {
        return Vec::new();
    }
    let workers = connections.clamp(1, MAX_CONNECTIONS) as u64;

    // Work-stealing depth: 8 segments per connection.
    let target = total / (workers * 8);
    let body = target.clamp(min_segment(total), max_segment(total));

    // Segment count follows from coverage, not the other way round: rounding
    // `total / body` down would leave the tail uncovered.
    let count = total.div_ceil(body).clamp(1, MAX_SEGMENTS);
    let body = total.div_ceil(count);

    let mut out: Vec<Segment> = Vec::with_capacity(count as usize + 8);
    let mut cursor = 0u64;

    // The first quarter (capped at 64 MiB) is cut into 1 MiB pieces so the head
    // of the file completes early — that is what a preview or a thumbnail
    // reader touches first.
    if total >= 8 * MIB {
        let head_zone = (total / 4).min(64 * MIB);
        // Never coarser than the body: for a mid-size file the body is already
        // under 1 MiB, and a 1 MiB head piece would then *delay* the first
        // megabyte instead of speeding it up.
        let piece = MIB.min(body).max(1);
        while cursor < head_zone {
            let end = (cursor + piece).min(head_zone);
            out.push(Segment { start: cursor, end });
            cursor = end;
        }
    }

    // The rest follows the body size. The cursor walks forward so the segments
    // tile the file exactly; nothing is computed backwards from `total`, which
    // is how a plan ends up 15 MiB short of the end.
    while cursor < total {
        let end = (cursor + body).min(total);
        out.push(Segment { start: cursor, end });
        cursor = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covers(total: u64, segments: &[Segment]) -> bool {
        let mut cursor = 0;
        for seg in segments {
            if seg.start != cursor || seg.end <= seg.start {
                return false;
            }
            cursor = seg.end;
        }
        cursor == total
    }

    #[test]
    fn connections_ladder() {
        assert_eq!(connections_for(0), 1);
        assert_eq!(connections_for(7 * MIB), 1);
        // Measured on a 47 MiB Quark file: 32 connections 523 KB/s, 256
        // connections 1818 KB/s. This drive hands out per-connection speed, so
        // the small-file rung is wide on purpose.
        assert_eq!(connections_for(8 * MIB), 256);
        assert_eq!(connections_for(63 * MIB), 256);
        assert_eq!(connections_for(64 * MIB), 256);
        assert_eq!(connections_for(GIB - 1), 256);
        assert_eq!(connections_for(GIB), 384);
        assert_eq!(connections_for(10 * GIB), 512);
        assert_eq!(connections_for(100 * GIB), 512);
        // Widening with size is the point: a bigger file must never get fewer
        // connections than a smaller one.
        let mut previous = 0;
        for size in [0, 8 * MIB, 64 * MIB, GIB, 10 * GIB, 100 * GIB] {
            let width = connections_for(size);
            assert!(width >= previous, "{size} got narrower: {width} < {previous}");
            previous = width;
        }
    }

    #[test]
    fn segments_tile_the_file_exactly() {
        for total in [
            1u64,
            1024,
            8 * MIB,
            8 * MIB + 1,
            24 * MIB,
            100 * MIB,
            1024 * MIB + 7,
            4 * GIB + 12345,
        ] {
            let segments = split(total, connections_for(total));
            assert!(covers(total, &segments), "gap or overlap for {total}");
            let sum: u64 = segments.iter().map(|s| s.len()).sum();
            assert_eq!(sum, total);
        }
    }

    #[test]
    fn head_is_cut_finer_than_the_body() {
        let total = 200 * MIB;
        let segments = split(total, connections_for(total));
        let head: Vec<_> = segments.iter().filter(|s| s.end <= 50 * MIB).collect();
        assert!(head.len() > 40, "head should be ~1 MiB pieces");
        assert!(head.iter().all(|s| s.len() <= MIB));
        // The head is never coarser than the body.
        let head_max = head.iter().map(|s| s.len()).max().unwrap();
        let body_max = segments.iter().map(|s| s.len()).max().unwrap();
        assert!(head_max <= body_max);
    }

    #[test]
    fn small_files_stay_above_the_handshake_cost() {
        let segments = split(16 * MIB, 32);
        assert!(segments.iter().all(|s| s.len() >= 256 * KIB));
    }

    #[test]
    fn plan_is_deterministic() {
        let a = split(123 * MIB + 7, 128);
        let b = split(123 * MIB + 7, 128);
        assert_eq!(a, b);
    }

    #[test]
    fn segment_count_is_guarded() {
        // A 100 GiB file must not produce more than MAX_SEGMENTS pieces.
        let segments = split(100 * GIB, 512);
        assert!(segments.len() as u64 <= MAX_SEGMENTS + 256);
        assert!(covers(100 * GIB, &segments));
    }

    #[test]
    fn zero_size_yields_nothing() {
        assert!(split(0, 32).is_empty());
    }
}
