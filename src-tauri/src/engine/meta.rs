//! Resume metadata for segmented downloads.
//!
//! A `.part` file alone cannot say *which* bytes inside it are real: segments
//! land out of order, so "the file is 300 MiB long" means nothing. The sidecar
//! `<name>.part.yyparts` records the plan and which segments finished, so an
//! interrupted download resumes instead of starting over.
//!
//! The plan is stored, not recomputed. Segment counts adapt to measured
//! throughput, so the same file can legitimately be split differently on the
//! next attempt — replaying a saved "segment 7 of the old plan" against a new
//! plan would write bytes into the wrong offsets.

use std::path::{Path, PathBuf};

use super::plan::Segment;

/// Bumped when the on-disk shape changes; older files are simply discarded.
///
/// 2 added `source`: a plan alone cannot tell "the same file, interrupted" from
/// "a different file that happens to be the same length, downloaded to the same
/// name". See [`Progress::source`].
const FORMAT: u32 = 2;

/// The saved state of one segmented download.
#[derive(Debug, Clone)]
pub struct Progress {
    pub total: u64,
    /// Fingerprint of the file these segments belong to, as built by
    /// [`crate::engine::source::Source::fingerprint`].
    ///
    /// The sidecar lives next to the `.part`, which is named after the *target*
    /// path, and the target path is made unique by appending " (1)" when a name
    /// is taken. Downloading two different files that happen to share a name and
    /// a length into the same folder therefore used to hit the same sidecar and
    /// resume one file's plan onto the other, producing a file of exactly the
    /// right length with the wrong contents. The length check cannot catch that;
    /// only an identity check can.
    pub source: String,
    pub segments: Vec<Segment>,
    done: Vec<bool>,
    /// Segments a worker is fetching right now.
    ///
    /// Never persisted: a claim is only true for this process, and a resumed
    /// download starts with nothing in flight. Without it, every worker in a
    /// round would find the same first unfinished segment and all of them would
    /// fetch it — the pool would serialise onto one segment instead of fanning
    /// out, and the extra copies would write the same bytes concurrently.
    claimed: Vec<bool>,
    /// Set when a segment finished since the last successful write.
    dirty: bool,
}

impl Progress {
    /// A plan whose file identity is unknown. Only tests build one: every real
    /// download knows which file it is resuming, and a plan without that
    /// identity could resume onto the wrong file.
    #[cfg(test)]
    pub fn new(total: u64, segments: Vec<Segment>) -> Self {
        Self::with_source(total, segments, String::new())
    }

    pub fn with_source(total: u64, segments: Vec<Segment>, source: String) -> Self {
        let done = vec![false; segments.len()];
        let claimed = vec![false; segments.len()];
        Self {
            total,
            source,
            segments,
            done,
            claimed,
            dirty: true,
        }
    }

    /// True when this plan belongs to `size` bytes of the file `source` names.
    ///
    /// An empty `source` on either side means "unknown" and is accepted, so a
    /// caller that cannot build a fingerprint still resumes instead of throwing
    /// away a perfectly good plan.
    pub fn belongs_to(&self, size: u64, source: &str) -> bool {
        self.total == size && (self.source.is_empty() || source.is_empty() || self.source == source)
    }

    /// The sidecar path for a partial file.
    pub fn sidecar(part: &Path) -> PathBuf {
        let mut name = part.file_name().unwrap_or_default().to_os_string();
        name.push(".yyparts");
        part.with_file_name(name)
    }

    /// Loads saved state, or `None` when there is nothing usable.
    ///
    /// Every failure path returns `None`: a corrupt sidecar must cost a restart,
    /// never a panic or a wrong file.
    pub fn load(part: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::sidecar(part)).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        if value.get("v").and_then(|v| v.as_u64())? != u64::from(FORMAT) {
            return None;
        }
        let total = value.get("total").and_then(|v| v.as_u64())?;
        let source = value
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let raw = value.get("segments").and_then(|v| v.as_array())?;
        let mut segments = Vec::with_capacity(raw.len());
        for item in raw {
            let pair = item.as_array()?;
            let start = pair.first()?.as_u64()?;
            let end = pair.get(1)?.as_u64()?;
            if end <= start {
                return None;
            }
            segments.push(Segment { start, end });
        }
        if segments.is_empty() {
            return None;
        }
        // The plan must tile the file exactly; anything else is a corrupt file.
        let mut cursor = 0u64;
        for seg in &segments {
            if seg.start != cursor {
                return None;
            }
            cursor = seg.end;
        }
        if cursor != total {
            return None;
        }

        let bits = value.get("done").and_then(|v| v.as_str()).unwrap_or("");
        let done = decode_bits(bits, segments.len());
        let claimed = vec![false; segments.len()];
        Some(Self {
            total,
            source,
            segments,
            done,
            claimed,
            dirty: false,
        })
    }

    /// Bytes confirmed complete so far.
    pub fn completed(&self) -> u64 {
        self.segments
            .iter()
            .zip(self.done.iter())
            .filter(|(_, done)| **done)
            .map(|(seg, _)| seg.len())
            .sum()
    }

    pub fn is_done(&self, index: usize) -> bool {
        self.done.get(index).copied().unwrap_or(false)
    }

    /// Takes the next segment nobody is fetching, and marks it as taken.
    ///
    /// Claim and completion are checked under the same lock as the write, so
    /// two workers can never receive the same segment. A claim is released by
    /// [`Self::release`] when an attempt fails, and by [`Self::mark`] when it
    /// succeeds.
    pub fn claim(&mut self) -> Option<(usize, Segment)> {
        for index in 0..self.segments.len() {
            if self.is_done(index) || self.claimed[index] {
                continue;
            }
            self.claimed[index] = true;
            return Some((index, self.segments[index]));
        }
        None
    }

    /// Gives a claimed segment back, so a later round can retry it.
    pub fn release(&mut self, index: usize) {
        if let Some(flag) = self.claimed.get_mut(index) {
            *flag = false;
        }
    }

    /// Drops every claim. Called between rounds, where no worker is running.
    pub fn release_all(&mut self) {
        self.claimed.iter_mut().for_each(|flag| *flag = false);
    }

    /// Marks a segment complete. Returns the new completed-byte count.
    pub fn mark(&mut self, index: usize) -> u64 {
        if let Some(flag) = self.done.get_mut(index) {
            if !*flag {
                *flag = true;
                self.dirty = true;
            }
        }
        if let Some(flag) = self.claimed.get_mut(index) {
            *flag = false;
        }
        self.completed()
    }

    pub fn remaining(&self) -> usize {
        self.done.iter().filter(|done| !**done).count()
    }

    /// Splits the largest unfinished segment in half.
    ///
    /// Used only in the tail of a download: with three large segments left and
    /// hundreds of workers, 99 % of the connections have nothing to do and the
    /// download finishes at three connections' speed. Cutting those segments
    /// finer fills the pool again.
    ///
    /// `min_bytes` is the floor for either half — below it the extra handshake
    /// costs more than the parallelism wins. Returns `false` when nothing can
    /// be split any further.
    ///
    /// The plan is saved, so the finer cut is what the next run resumes
    /// against; it stays a valid tiling, which is all `load` requires.
    pub fn split_largest(&mut self, min_bytes: u64) -> bool {
        let mut best: Option<(usize, u64)> = None;
        for (index, seg) in self.segments.iter().enumerate() {
            // A claimed segment is being written right now: cutting it would
            // leave the worker writing past its new end, which is the race
            // that produces a file of the right length with holes in it.
            if self.is_done(index) || self.claimed[index] {
                continue;
            }
            let len = seg.len();
            if len < min_bytes.saturating_mul(2) {
                continue;
            }
            if best.is_none_or(|(_, best_len)| len > best_len) {
                best = Some((index, len));
            }
        }
        let Some((index, len)) = best else {
            return false;
        };
        let segment = self.segments[index];
        let mid = segment.start + len / 2;
        self.segments[index] = Segment {
            start: segment.start,
            end: mid,
        };
        self.segments.insert(
            index + 1,
            Segment {
                start: mid,
                end: segment.end,
            },
        );
        self.done.insert(index + 1, false);
        self.claimed.insert(index + 1, false);
        self.dirty = true;
        true
    }

    /// True once every segment is in.
    pub fn finished(&self) -> bool {
        !self.done.is_empty() && self.done.iter().all(|done| *done)
    }

    /// Writes the sidecar if anything changed.
    pub fn save(&mut self, part: &Path) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let segments: Vec<[u64; 2]> = self
            .segments
            .iter()
            .map(|seg| [seg.start, seg.end])
            .collect();
        let value = serde_json::json!({
            "v": FORMAT,
            "total": self.total,
            "source": self.source,
            "segments": segments,
            "done": encode_bits(&self.done),
        });
        let text = serde_json::to_string(&value).unwrap_or_default();
        // Written to a temporary name and renamed into place. A plain write
        // truncates first, so a crash (or a full disk) between the truncate and
        // the last byte leaves a half-written sidecar — and every plan saved so
        // far is gone. The rename is what makes the update atomic; the guard
        // file is unique per process so two attempts cannot collide.
        let path = Self::sidecar(part);
        let mut temp_name = path.file_name().unwrap_or_default().to_os_string();
        temp_name.push(format!(".{}", std::process::id()));
        let temp = path.with_file_name(temp_name);
        std::fs::write(&temp, text)?;
        match std::fs::rename(&temp, &path) {
            Ok(()) => {}
            Err(err) => {
                let _ = std::fs::remove_file(&temp);
                return Err(err);
            }
        }
        self.dirty = false;
        Ok(())
    }

    /// Removes the sidecar, for when the download completes or is discarded.
    pub fn discard(part: &Path) {
        let _ = std::fs::remove_file(Self::sidecar(part));
    }
}

/// Packs the flags into hex, one bit per segment, so a 65 536-segment plan
/// costs 16 KiB instead of a JSON array of booleans.
fn encode_bits(done: &[bool]) -> String {
    let mut out = String::with_capacity(done.len() / 4 + 1);
    for chunk in done.chunks(4) {
        let mut nibble = 0u8;
        for (index, flag) in chunk.iter().enumerate() {
            if *flag {
                nibble |= 1 << index;
            }
        }
        out.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
    }
    out
}

/// Inverse of [`encode_bits`]; short or garbage input reads as "not done".
fn decode_bits(bits: &str, len: usize) -> Vec<bool> {
    let mut out = Vec::with_capacity(len);
    for ch in bits.chars() {
        let nibble = ch.to_digit(16).unwrap_or(0) as u8;
        for bit in 0..4 {
            if out.len() >= len {
                break;
            }
            out.push(nibble & (1 << bit) != 0);
        }
        if out.len() >= len {
            break;
        }
    }
    out.resize(len, false);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs() -> Vec<Segment> {
        vec![
            Segment { start: 0, end: 10 },
            Segment { start: 10, end: 30 },
            Segment { start: 30, end: 35 },
        ]
    }

    #[test]
    fn bits_round_trip() {
        let done = vec![true, false, true, true, false, false, true];
        let bits = encode_bits(&done);
        assert_eq!(decode_bits(&bits, done.len()), done);
    }

    #[test]
    fn short_bits_read_as_pending() {
        assert_eq!(decode_bits("", 3), vec![false, false, false]);
        assert_eq!(decode_bits("1", 3), vec![true, false, false]);
    }

    #[test]
    fn completed_bytes_ignore_pending_segments() {
        let mut progress = Progress::new(35, segs());
        assert_eq!(progress.completed(), 0);
        assert_eq!(progress.remaining(), 3);
        assert_eq!(progress.mark(1), 20);
        assert_eq!(progress.mark(1), 20);
        assert_eq!(progress.remaining(), 2);
        assert!(!progress.finished());
        progress.mark(0);
        progress.mark(2);
        assert!(progress.finished());
        assert_eq!(progress.completed(), 35);
    }

    #[test]
    fn a_claim_is_handed_out_once() {
        let mut progress = Progress::new(35, segs());
        let first = progress.claim().map(|(index, _)| index);
        let second = progress.claim().map(|(index, _)| index);
        let third = progress.claim().map(|(index, _)| index);
        assert_eq!(first, Some(0));
        assert_eq!(second, Some(1));
        assert_eq!(third, Some(2));
        // Every segment is out with a worker, so there is nothing left to give.
        assert!(progress.claim().is_none());
        assert_eq!(progress.remaining(), 3);
    }

    #[test]
    fn a_released_claim_can_be_taken_again() {
        let mut progress = Progress::new(35, segs());
        progress.claim();
        progress.claim();
        progress.release(0);
        assert_eq!(progress.claim().map(|(index, _)| index), Some(0));
    }

    #[test]
    fn finishing_a_claim_frees_it() {
        let mut progress = Progress::new(35, segs());
        progress.claim();
        progress.mark(0);
        progress.release_all();
        assert!(progress.is_done(0));
        assert_eq!(progress.claim().map(|(index, _)| index), Some(1));
    }

    #[test]
    fn a_claimed_segment_is_never_split() {
        let mut progress = Progress::new(200 * 1024 * 1024, vec![Segment { start: 0, end: 200 * 1024 * 1024 }]);
        progress.claim();
        // The only segment is out with a worker: cutting it would leave that
        // worker writing past its new end.
        assert!(!progress.split_largest(1024));
        progress.release(0);
        assert!(progress.split_largest(1024));
    }

    #[test]
    fn save_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("yyparts-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("file.bin.part");

        let mut progress = Progress::new(35, segs());
        progress.mark(0);
        progress.mark(2);
        progress.save(&part).unwrap();

        let loaded = Progress::load(&part).expect("sidecar should load");
        assert_eq!(loaded.total, 35);
        assert_eq!(loaded.segments, segs());
        // Segments 0 and 2 are done: 10 + 5 bytes.
        assert_eq!(loaded.completed(), 15);
        assert!(loaded.is_done(0));
        assert!(!loaded.is_done(1));
        assert!(loaded.is_done(2));

        // The write goes through a temporary file, which must not survive.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "file.bin.part" && name != "file.bin.part.yyparts")
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");

        Progress::discard(&part);
        assert!(Progress::load(&part).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_source_fingerprint_survives_a_reload() {
        let dir = std::env::temp_dir().join(format!("yyparts-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("file.bin.part");

        let mut progress = Progress::with_source(35, segs(), "quark|fid-a".to_string());
        progress.save(&part).unwrap();
        let loaded = Progress::load(&part).expect("sidecar should load");
        assert_eq!(loaded.source, "quark|fid-a");
        assert!(loaded.belongs_to(35, "quark|fid-a"));
        // Same length, different file: the plan must not be reused.
        assert!(!loaded.belongs_to(35, "quark|fid-b"));
        // An unknown identity on either side is accepted, so a caller that
        // cannot build a fingerprint still resumes instead of starting over.
        assert!(loaded.belongs_to(35, ""));

        Progress::discard(&part);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tail_is_split_until_the_pool_is_busy() {
        // One 8 MiB segment left with 4 workers idle: split until there are
        // enough pieces, and the pieces must still tile the file exactly.
        let mut progress = Progress::new(
            8 * 1024 * 1024,
            vec![Segment {
                start: 0,
                end: 8 * 1024 * 1024,
            }],
        );
        let mut splits = 0;
        while progress.remaining() < 4 {
            assert!(progress.split_largest(128 * 1024));
            splits += 1;
            assert!(splits < 10);
        }
        assert_eq!(progress.remaining(), 4);
        let mut cursor = 0;
        for seg in &progress.segments {
            assert_eq!(seg.start, cursor);
            cursor = seg.end;
        }
        assert_eq!(cursor, 8 * 1024 * 1024);
    }

    #[test]
    fn splitting_stops_at_the_floor() {
        // Two 100 KiB segments cannot be halved without dropping below the
        // 128 KiB floor, so nothing changes.
        let mut progress = Progress::new(
            200 * 1024,
            vec![
                Segment {
                    start: 0,
                    end: 100 * 1024,
                },
                Segment {
                    start: 100 * 1024,
                    end: 200 * 1024,
                },
            ],
        );
        assert!(!progress.split_largest(128 * 1024));
        assert_eq!(progress.segments.len(), 2);
    }

    #[test]
    fn splitting_skips_finished_segments() {
        let mut progress = Progress::new(
            4 * 1024 * 1024,
            vec![
                Segment {
                    start: 0,
                    end: 2 * 1024 * 1024,
                },
                Segment {
                    start: 2 * 1024 * 1024,
                    end: 4 * 1024 * 1024,
                },
            ],
        );
        progress.mark(1);
        assert!(progress.split_largest(128 * 1024));
        // The finished second half is untouched; the first half was cut.
        assert_eq!(progress.segments[0].len(), 1024 * 1024);
        assert!(progress.is_done(2));
        assert_eq!(progress.completed(), 2 * 1024 * 1024);
    }

    #[test]
    fn a_plan_with_a_gap_is_rejected() {
        let dir = std::env::temp_dir().join(format!("yyparts-gap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("gap.bin.part");
        std::fs::write(
            Progress::sidecar(&part),
            r#"{"v":1,"total":20,"segments":[[0,10],[15,20]],"done":""}"#,
        )
        .unwrap();
        assert!(Progress::load(&part).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_short_plan_is_rejected() {
        let dir = std::env::temp_dir().join(format!("yyparts-short-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("short.bin.part");
        std::fs::write(
            Progress::sidecar(&part),
            r#"{"v":1,"total":40,"segments":[[0,10],[10,20]],"done":""}"#,
        )
        .unwrap();
        assert!(Progress::load(&part).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_is_rejected() {
        let dir = std::env::temp_dir().join(format!("yyparts-junk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("junk.bin.part");
        for body in ["", "not json", "{\"v\":9}", "[]", "{\"v\":1,\"total\":5}"] {
            std::fs::write(Progress::sidecar(&part), body).unwrap();
            assert!(Progress::load(&part).is_none(), "accepted {body:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
