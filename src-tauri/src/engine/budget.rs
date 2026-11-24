//! Per-drive connection budget.
//!
//! More connections are not monotonically better: each drive throttles at a
//! different layer, and past the knee extra connections only split the same
//! bandwidth into thinner slices — which is worse, because every extra
//! connection costs a TLS handshake and each one moves slower, so the transfer
//! takes longer than at the knee.
//!
//! The starting points below were measured against each CDN (aggregate
//! throughput at the knee, per account or per IP as noted). On top of that the
//! budget adapts: if throughput collapses while we are well above the floor,
//! the drive is almost certainly throttling us and halving the count recovers
//! more than it loses.

use std::time::{Duration, Instant};

/// Where each drive's throughput stops improving.
#[derive(Debug, Clone, Copy)]
pub struct Profile {
    /// Never go below this — fewer connections than this cannot saturate even
    /// an unthrottled CDN link.
    pub floor: usize,
    /// Never go above this. Past it the drive throttles or the account is
    /// capped, so the connections only add handshakes.
    pub ceiling: usize,
    /// Speed below which a connection counts as half-dead, bytes/s.
    ///
    /// This is a *fallback*: the engine prefers to compare a connection against
    /// its peers' median, because a single number cannot fit drives that
    /// throttle per connection (Quark, 40–80 KiB/s each) and drives that
    /// throttle per IP (Aliyun, ~2.5 MB/s total, so 64 connections see 40 KiB/s
    /// each and a jittery minute drops them all to 10 KiB/s). It is used when
    /// there are no peers to compare against.
    pub slow_floor: f64,
}

impl Profile {
    /// Quark throttles per account; measured 24–26 MB/s at 512 connections and
    /// no gain at 4096 (23 MB/s), so the ceiling is well below the engine max.
    pub const QUARK: Profile = Profile {
        floor: 32,
        ceiling: 512,
        // Quark limits per connection, so anything under 16 KiB/s is genuinely
        // half-dead and a fresh connection is usually much faster.
        slow_floor: 16.0 * 1024.0,
    };

    /// Aliyun throttles per IP to roughly 2.5–2.7 MB/s. Extra connections barely
    /// help the peak, but they do help against tail collapse and mid-transfer
    /// stalls, so a modest count still pays off.
    pub const ALIYUN: Profile = Profile {
        floor: 16,
        ceiling: 64,
        // Aliyun's ~2.5 MB/s per-IP quota spread over 64 connections is ~40
        // KiB/s each, and a jittery minute pushes them all to ~10 KiB/s. A
        // 16 KiB/s floor would mark every one of them slow and start a
        // reconnect storm that costs more than the throttling did.
        slow_floor: 8.0 * 1024.0,
    };

    /// Drives without a measured profile: a conservative middle ground.
    pub const GENERIC: Profile = Profile {
        floor: 8,
        ceiling: 64,
        slow_floor: 8.0 * 1024.0,
    };

    /// 天翼云盘 throttles per connection, not per account, so the knee sits far
    /// above the generic ceiling.
    ///
    /// Measured 2026-11 against a 31 MB file on `shusheng2.mini189.cn` (median of
    /// two 4 s rounds each): 1 conn 1.2 MB/s, 16 conns 4.0, 32 conns 7.5, 64
    /// conns 8.5, 96 conns 10.2, 128 conns 10.7, 192 conns 10.1 — so 128 is the
    /// knee and 192 only adds handshakes. A second file confirmed the shape
    /// (single connection anywhere between 2.0 and 10.2 MB/s depending on which
    /// CDN node answered, 128 conns 8.5). The per-connection rate is jittery
    /// enough that a connection regularly sits at ~100 KiB/s, so the slow floor
    /// stays at the generic 8 KiB/s rather than marking peers half-dead.
    pub const CLOUD189: Profile = Profile {
        floor: 8,
        ceiling: 128,
        slow_floor: 8.0 * 1024.0,
    };

    /// Picks the profile for a drive id (`"quark"`, `"aliyun"`, ...).
    pub fn for_provider(provider: &str) -> Profile {
        match provider {
            "quark" => Profile::QUARK,
            "aliyun" => Profile::ALIYUN,
            "cloud189" => Profile::CLOUD189,
            _ => Profile::GENERIC,
        }
    }

    /// The connection count for a file, honouring both the size ladder and the
    /// drive's own limits.
    pub fn connections_for(&self, size: u64) -> usize {
        super::plan::connections_for(size).clamp(self.floor, self.ceiling)
    }
}

/// Shares one drive's connection budget across the files downloading at once.
///
/// Opening the full count per file is wrong when several run together: Quark
/// limits connections **per account**, so three files at 512 connections ask
/// for 1536 and the drive throttles all of them. The budget is therefore split
/// by file size — a big file gets more connections than a small one, because it
/// has more left to fetch — with a floor so a small file is never starved into
/// a crawl.
///
/// `sizes` holds the size of every file downloading right now (this one
/// included). Returns the count for the file at `index`.
///
/// Sizes are unknown (0) during the brief window before a listing resolves;
/// those fall back to an even split rather than starving everyone to the floor.
pub fn share(profile: Profile, sizes: &[u64], index: usize) -> usize {
    allocate(profile, sizes)
        .get(index)
        .copied()
        .unwrap_or(profile.ceiling)
}

/// The per-file counts for every entry in `sizes`.
///
/// The split is proportional to size, then settled so the total lands on the
/// drive's budget:
///
/// * **A dominant file yields to the floors of the others.** Proportional
///   shares alone give a 30 GiB file 509 of 512 connections and leave a 160 MiB
///   file with 3, below its floor; clamping it up would push the total past what
///   the drive allows. The excess is taken back from the largest file, so the
///   small one keeps its floor and the account stays inside its budget.
/// * **A file that hits the ceiling gives its surplus back**, spread by size
///   over the files that still have room, rather than left unused.
/// * **The floor outranks the budget.** With more files than the budget can give
///   a floor to, every file still gets its floor: a drive that is slightly over
///   budget throttles a little, whereas a file at two connections crawls for its
///   whole transfer.
pub fn allocate(profile: Profile, sizes: &[u64]) -> Vec<usize> {
    if sizes.is_empty() {
        return Vec::new();
    }
    let floor = profile.floor;
    let ceiling = profile.ceiling.max(floor);
    let budget = ceiling.max(floor * sizes.len());

    if sizes.len() == 1 {
        return vec![profile.connections_for(sizes[0])];
    }

    let known: u64 = sizes.iter().copied().filter(|size| *size > 0).sum();
    let even = (budget / sizes.len()).max(floor);
    if known == 0 {
        return vec![even.min(ceiling); sizes.len()];
    }

    // Proportional shares can land above the ceiling (the file is dominant) or
    // below the floor (it is tiny). Clamping both directions breaks the total,
    // so the total is settled afterwards, in one pass each way.
    let mut counts: Vec<usize> = sizes
        .iter()
        .map(|size| {
            if *size == 0 {
                even
            } else {
                (budget as u128 * *size as u128 / known as u128) as usize
            }
        })
        .map(|count| count.clamp(floor, ceiling))
        .collect();

    // Over budget: take the excess back from the files that have the most to
    // give, never below the floor. A file at the floor keeps crawling rather
    // than stopping, which is why the excess comes out of the large files.
    let mut excess = counts.iter().sum::<usize>().saturating_sub(budget);
    while excess > 0 {
        let target = sizes
            .iter()
            .zip(counts.iter())
            .enumerate()
            .filter(|(_, (_, count))| **count > floor)
            .max_by_key(|(_, (_, count))| **count)
            .map(|(index, _)| index);
        let Some(index) = target else {
            // Every file is already at the floor: the floor outranks the budget.
            break;
        };
        let take = excess.min(counts[index] - floor);
        counts[index] -= take;
        excess -= take;
    }

    // Under budget: a file that hit the ceiling gave its share back, and the
    // leftover is worth nothing unless it reaches a file that can use it. Spread
    // it by size over the files that still have room.
    for _ in 0..8 {
        let spent: usize = counts.iter().sum();
        if spent >= budget {
            break;
        }
        let surplus = budget - spent;
        let open: u64 = sizes
            .iter()
            .zip(counts.iter())
            .filter(|(_, count)| **count < ceiling)
            .map(|(size, _)| (*size).max(1))
            .sum();
        if open == 0 {
            break;
        }
        let mut moved = 0usize;
        for (size, count) in sizes.iter().zip(counts.iter_mut()) {
            if *count >= ceiling {
                continue;
            }
            let want = (surplus as u128 * (*size).max(1) as u128 / open as u128) as usize;
            let add = want.min(ceiling - *count);
            *count += add;
            moved += add;
        }
        if moved == 0 {
            // Rounding ate the surplus: hand the remainder to the largest file
            // that still has room, so the budget is actually used.
            let target = sizes
                .iter()
                .zip(counts.iter())
                .enumerate()
                .filter(|(_, (_, count))| **count < ceiling)
                .max_by_key(|(_, (size, _))| **size)
                .map(|(index, _)| index);
            if let Some(index) = target {
                counts[index] += surplus.min(ceiling - counts[index]);
            }
            break;
        }
    }

    counts
}

/// A running throughput sample, used to notice throttling.
///
/// Deliberately simple: the engine reports the aggregate speed once a second,
/// and the budget reacts to sustained collapse rather than to a single noisy
/// sample. Reacting to noise would flap between connection counts and lose more
/// to handshakes than the throttling costs.
#[derive(Debug)]
pub struct Adaptive {
    profile: Profile,
    current: usize,
    /// Best sustained speed seen so far, bytes/s.
    peak: f64,
    /// Speed below which the drive looks throttled, bytes/s.
    floor_speed: f64,
    /// When the current collapse started, if one is in progress.
    collapsing: Option<Instant>,
    /// When the last cut happened, for the recovery cooldown.
    last_cut: Option<Instant>,
}

/// Speed must hold above this share of the peak before the count is raised
/// again. A tighter threshold would re-raise into the same throttle.
const RECOVER_AT: f64 = 0.85;
/// A drop below this share of the peak counts as throttling.
const THROTTLE_AT: f64 = 0.55;
/// How long a collapse must last before the count is halved.
const SUSTAINED: Duration = Duration::from_secs(8);
/// How long after a cut before the count may be raised again.
const COOLDOWN: Duration = Duration::from_secs(45);

impl Adaptive {
    /// The full budget for a lone file. Used by the tests; the engine always
    /// goes through [`Self::with_connections`], which is this case expressed as
    /// "one file, whole share".
    #[cfg(test)]
    pub fn new(profile: Profile, size: u64) -> Self {
        let current = profile.connections_for(size);
        Self {
            profile,
            current,
            peak: 0.0,
            floor_speed: 0.0,
            collapsing: None,
            last_cut: None,
        }
    }

    /// Starts from an explicit count, for a file that shares the drive's budget
    /// with others. The profile's ceiling becomes that share, so adaptation
    /// cannot grow past what the drive can take across all files.
    pub fn with_connections(profile: Profile, connections: usize) -> Self {
        let ceiling = connections.max(profile.floor);
        let profile = Profile {
            floor: profile.floor.min(ceiling),
            ceiling,
            slow_floor: profile.slow_floor,
        };
        Self {
            profile,
            current: connections.clamp(profile.floor, ceiling),
            peak: 0.0,
            floor_speed: 0.0,
            collapsing: None,
            last_cut: None,
        }
    }

    pub fn current(&self) -> usize {
        self.current
    }

    /// Feeds one aggregate speed sample (bytes/s) and returns the count to use
    /// from now on. `now` is passed in so tests do not sleep.
    pub fn observe(&mut self, speed: f64, now: Instant) -> usize {
        if speed > self.peak {
            self.peak = speed;
            self.floor_speed = speed * THROTTLE_AT;
        }
        // Nothing to decide until the peak is meaningful.
        if self.peak < 256.0 * 1024.0 {
            return self.current;
        }

        if speed < self.floor_speed {
            // Time the collapse, not the sample: the count is only cut once the
            // drive has been slow for a while, so a single dip changes nothing.
            let began = *self.collapsing.get_or_insert(now);
            let elapsed = now.saturating_duration_since(began);
            if elapsed >= SUSTAINED && self.current > self.profile.floor {
                self.current = (self.current / 2).max(self.profile.floor);
                self.collapsing = Some(now);
                self.last_cut = Some(now);
            }
            return self.current;
        }
        self.collapsing = None;

        // Recovery needs a cut to recover from. Without one, a steady sample
        // would double the count on every report — the count only ever rises
        // because a previous cut proved the drive was throttled.
        let cooled = self
            .last_cut
            .is_some_and(|at| now.saturating_duration_since(at) >= COOLDOWN);
        if cooled && speed >= self.peak * RECOVER_AT && self.current < self.profile.ceiling {
            self.current = (self.current * 2).min(self.profile.ceiling);
            self.last_cut = Some(now);
        }
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: f64 = 1024.0 * 1024.0;

    #[test]
    fn size_ladder_is_clamped_by_the_profile() {
        // A small file wants 1 connection; the floor is 32 for Quark.
        assert_eq!(Profile::QUARK.connections_for(1024), 32);
        // A 20 GiB file wants 512, which is exactly Quark's ceiling.
        assert_eq!(Profile::QUARK.connections_for(20 * 1024 * 1024 * 1024), 512);
        // Aliyun never exceeds 64 even for a huge file.
        assert_eq!(Profile::ALIYUN.connections_for(20 * 1024 * 1024 * 1024), 64);
        // And never drops below its own floor.
        assert_eq!(Profile::ALIYUN.connections_for(0), 16);
    }

    #[test]
    fn one_file_gets_the_whole_budget() {
        let size = 1024 * 1024 * 1024;
        assert_eq!(
            share(Profile::QUARK, &[size], 0),
            Profile::QUARK.connections_for(size)
        );
    }

    #[test]
    fn concurrent_files_split_by_size() {
        // A 1 GiB file next to a 100 MiB one gets about ten times the share.
        let big = 1024 * 1024 * 1024u64;
        let small = 100 * 1024 * 1024u64;
        let sizes = [big, small];
        let big_share = share(Profile::QUARK, &sizes, 0);
        let small_share = share(Profile::QUARK, &sizes, 1);
        assert!(big_share > small_share);
        // Both stay inside the drive's own limits, and the total does not
        // exceed the account's ceiling — three files must not ask for three
        // times what the drive allows.
        assert!(big_share <= Profile::QUARK.ceiling);
        assert!(small_share >= Profile::QUARK.floor);
        assert!(big_share + small_share <= Profile::QUARK.ceiling * 2);
    }

    #[test]
    fn three_files_do_not_exceed_the_ceiling() {
        let sizes = [512 * 1024 * 1024u64; 3];
        let total: usize = (0..3).map(|i| share(Profile::QUARK, &sizes, i)).sum();
        assert!(
            total <= Profile::QUARK.ceiling,
            "three equal files asked for {total}, above the account ceiling"
        );
    }

    #[test]
    fn an_unknown_size_gets_an_even_split_not_the_floor() {
        // Sizes are 0 until the listing resolves; starving them to the floor
        // would make every download start in slow motion.
        let sizes = [0u64, 0u64];
        assert_eq!(share(Profile::QUARK, &sizes, 0), Profile::QUARK.ceiling / 2);
        assert_eq!(share(Profile::QUARK, &sizes, 1), Profile::QUARK.ceiling / 2);
    }

    #[test]
    fn provider_profiles() {
        assert_eq!(Profile::for_provider("quark").ceiling, 512);
        assert_eq!(Profile::for_provider("aliyun").ceiling, 64);
        assert_eq!(Profile::for_provider("baidu").ceiling, 64);
        assert_eq!(Profile::for_provider("cloud189").ceiling, 128);
        assert_eq!(Profile::for_provider("cloud189").floor, 8);
    }

    #[test]
    fn a_dominant_file_yields_to_the_floor_of_the_others() {
        // Proportional shares alone give the 30 GiB file 509 of 512 connections
        // and leave the 160 MiB one at 2 — below its floor. Clamping the small
        // one up would push the total past the account's budget, so the excess
        // comes back out of the large file instead.
        let big = 30u64 * 1024 * 1024 * 1024;
        let small = 160u64 * 1024 * 1024;
        let sizes = [big, small];
        let counts = allocate(Profile::QUARK, &sizes);
        assert_eq!(
            counts[1],
            Profile::QUARK.floor,
            "the small file must keep its floor"
        );
        assert!(
            counts[0] < Profile::QUARK.ceiling,
            "the big file should give back the excess, got {}",
            counts[0]
        );
        assert!(
            counts.iter().sum::<usize>() <= Profile::QUARK.ceiling,
            "the account budget must hold"
        );
    }

    #[test]
    fn a_capped_file_hands_its_surplus_to_the_others() {
        // Two large files and one that can only use a few connections: whatever
        // the capped file cannot take has to reach the others, or the drive runs
        // at a fraction of what it allows.
        let huge = 200u64 * 1024 * 1024 * 1024;
        let tiny = 9u64 * 1024 * 1024;
        let sizes = [huge, huge, tiny];
        let counts = allocate(Profile::QUARK, &sizes);
        assert_eq!(counts[2], Profile::QUARK.floor);
        assert!(
            counts.iter().sum::<usize>() > Profile::QUARK.floor * 3,
            "the two large files should take the rest of the budget, got {counts:?}"
        );
    }

    #[test]
    fn the_floor_outranks_the_budget() {
        // Twenty files cannot share 512 connections and still give each a useful
        // number, so the budget is exceeded rather than every file crawling.
        let sizes = [8u64 * 1024 * 1024; 20];
        let counts = allocate(Profile::QUARK, &sizes);
        assert!(counts.iter().all(|count| *count >= Profile::QUARK.floor));
        assert!(counts.iter().sum::<usize>() > Profile::QUARK.ceiling);
    }

    #[test]
    fn throttling_halves_after_it_is_sustained() {
        let size = 1024 * 1024 * 1024;
        let mut budget = Adaptive::new(Profile::QUARK, size);
        let start = budget.current();
        let t0 = Instant::now();

        // Full speed for a while: nothing changes.
        for step in 0..20 {
            let at = t0 + Duration::from_secs(step);
            assert_eq!(budget.observe(24.0 * MIB, at), start);
        }

        // Speed collapses: no reaction yet, it must be sustained.
        let collapse_at = t0 + Duration::from_secs(20);
        assert_eq!(budget.observe(4.0 * MIB, collapse_at), start);
        // Eight seconds later the count halves.
        let cut = budget.observe(4.0 * MIB, collapse_at + SUSTAINED);
        assert_eq!(cut, start / 2);
    }

    #[test]
    fn a_short_dip_does_not_cut() {
        let mut budget = Adaptive::new(Profile::QUARK, 1024 * 1024 * 1024);
        let start = budget.current();
        let t0 = Instant::now();
        budget.observe(24.0 * MIB, t0);
        // One second of collapse, then back to normal.
        assert_eq!(
            budget.observe(1.0 * MIB, t0 + Duration::from_secs(1)),
            start
        );
        assert_eq!(
            budget.observe(24.0 * MIB, t0 + Duration::from_secs(2)),
            start
        );
    }

    #[test]
    fn never_cuts_below_the_floor() {
        let mut budget = Adaptive::new(Profile::ALIYUN, 1024 * 1024 * 1024);
        let t0 = Instant::now();
        let mut at = t0;
        budget.observe(2.6 * MIB, at);
        for _ in 0..12 {
            at += SUSTAINED;
            budget.observe(0.1 * MIB, at);
        }
        assert_eq!(budget.current(), Profile::ALIYUN.floor);
    }

    #[test]
    fn recovers_only_after_the_cooldown() {
        let mut budget = Adaptive::new(Profile::QUARK, 1024 * 1024 * 1024);
        let start = budget.current();
        let t0 = Instant::now();
        budget.observe(24.0 * MIB, t0);
        let collapse_at = t0 + Duration::from_secs(10);
        assert_eq!(budget.observe(1.0 * MIB, collapse_at), start);
        let cut_at = collapse_at + SUSTAINED;
        let cut = budget.observe(1.0 * MIB, cut_at);
        assert_eq!(cut, start / 2);

        // Still inside the cooldown: no recovery even at full speed.
        assert_eq!(
            budget.observe(24.0 * MIB, cut_at + Duration::from_secs(5)),
            cut
        );
        // After the cooldown it doubles back up.
        let recovered = budget.observe(24.0 * MIB, cut_at + COOLDOWN);
        assert!(recovered > cut);
    }
}
