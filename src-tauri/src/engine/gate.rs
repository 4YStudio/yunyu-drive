//! Gates: small decision objects that keep the engine from making things worse.
//!
//! Each one exists because of a specific failure that was measured, not
//! theorised. They are pure — no I/O, no timers, time is passed in — so the
//! exact thresholds can be asserted in tests instead of discovered in the field.

use std::time::{Duration, Instant};

/// How many times a drive may answer a ranged request with the whole file
/// before the engine concludes it really cannot do ranges.
///
/// A single `200` where a `206` was expected is usually a hiccup — a proxy, a
/// CDN node that lost the Range header, an auth blip. Treating the first one as
/// "this drive cannot do ranges" throws away a download that was working and
/// silently halves the speed. Three in a minute is a pattern.
const RANGE_TOLERANCE: u32 = 3;

/// Window for counting refusals. Long enough that a genuine downgrade fills it
/// immediately, short enough that occasional hiccups spread over an hour do not
/// accumulate into a false verdict.
const RANGE_WINDOW: Duration = Duration::from_secs(60);

/// Counts ranged requests that came back without a `206`.
///
/// The window resets the *count*, not just the start time: a drive that refuses
/// once a minute would otherwise reach the tolerance after an hour and be
/// downgraded for no reason.
#[derive(Debug, Default)]
pub struct RangeGate {
    count: u32,
    first: Option<Instant>,
}

impl RangeGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one refusal and reports whether the drive should now be treated
    /// as unable to serve ranges.
    pub fn observe(&mut self, now: Instant) -> bool {
        match self.first {
            Some(first) if now.saturating_duration_since(first) <= RANGE_WINDOW => {
                self.count = self.count.saturating_add(1);
            }
            _ => {
                self.count = 1;
                self.first = Some(now);
            }
        }
        self.count > RANGE_TOLERANCE
    }

    pub fn count(&self) -> u32 {
        self.count
    }
}

/// Ramp-up for the worker pool.
///
/// Opening every connection at once means hundreds of DNS lookups, TCP
/// handshakes and TLS negotiations firing simultaneously; any jitter at that
/// moment takes a large batch of connections down together, they all back off
/// together, and throughput collapses in a visible cycle.
///
/// The sequence grows *quadratically* (1, 2, 4, 16, 256, …) rather than
/// doubling: doubling needs eight steps to reach 256, squaring needs four, so
/// the ramp costs about a second instead of several. A linear "add 125 per
/// tick" variant was tried and rejected as too slow.
#[derive(Debug)]
pub struct SlowStart {
    allowed: usize,
    max: usize,
}

impl SlowStart {
    pub fn new(max: usize) -> Self {
        Self {
            // One connection to begin with: the whole point is not to open the
            // pool until a single connection has proven the path works.
            allowed: 1,
            max: max.max(1),
        }
    }

    /// Restores a previously reached rung — used when the round's budget
    /// changed, so the ramp keeps its progress instead of restarting at 1.
    pub fn allowed(&self) -> usize {
        self.allowed
    }

    /// Widens the window by one step. Returns the new limit.
    pub fn advance(&mut self) -> usize {
        self.allowed = next_allowed(self.allowed, self.max);
        self.allowed
    }

    /// Drops the ramp and opens the whole pool — used when resuming, where the
    /// connections were already proven to work.
    pub fn open_all(&mut self) {
        self.allowed = self.max;
    }
}

/// The next rung: quadratic growth, clamped to `max`.
pub fn next_allowed(current: usize, max: usize) -> usize {
    if max < 1 {
        return 1;
    }
    if current < 1 {
        return 1;
    }
    if current >= max {
        return max;
    }
    if current == 1 {
        return 2.min(max);
    }
    // `current > max / current` would overflow on multiply.
    if current > max / current {
        return max;
    }
    (current * current).min(max)
}

/// Detects "every connection is slow", which means the drive is throttling the
/// whole IP rather than a few connections having drawn bad nodes.
///
/// This distinction decides whether reconnecting is worth anything: against a
/// per-connection limit a slow connection is a bad lottery ticket and a fresh
/// one may be fast, but against a per-IP quota every fresh connection gets the
/// same share, so reconnecting is pure loss — and it also drops the bytes that
/// connection had already buffered.
///
/// The verdict is *sticky* for a hold period: samples arrive faster than the
/// quota changes, and without stickiness the gate would flicker and preemption
/// would resume in the middle of the very condition it is meant to avoid.
#[derive(Debug)]
pub struct OverallSlow {
    hold_until: Option<Instant>,
    logged: bool,
}

/// Minimum samples before a verdict: with two connections one slow one is
/// indistinguishable from a systemic limit.
const MIN_SAMPLES: usize = 4;
/// How long a "systemic" verdict survives after the last supporting sample.
const HOLD: Duration = Duration::from_secs(30);

impl OverallSlow {
    pub fn new() -> Self {
        Self {
            hold_until: None,
            logged: false,
        }
    }

    /// Feeds one sample round.
    ///
    /// * `sampled` — connections that reported a speed
    /// * `slow` — how many of them were below the watchdog threshold
    /// * `inflight` — connections currently open
    /// * `endgame_inflight` — at or below this, preemption is always allowed
    ///   (a handful of tail segments cannot starve anything)
    pub fn evaluate(
        &mut self,
        sampled: usize,
        slow: usize,
        inflight: usize,
        endgame_inflight: usize,
        now: Instant,
    ) -> bool {
        // In the endgame there is nothing to starve and every slow connection
        // is worth replacing, so never suppress. The flag is deliberately left
        // alone so a later "recovered" line still makes sense.
        if inflight <= endgame_inflight {
            return false;
        }
        if sampled < MIN_SAMPLES {
            // Not enough peers to tell systemic throttling from one unlucky
            // connection, so do not suppress: suppression here is permanent in
            // practice, because a round that starts with an empty sample list
            // (the very first one, and every round after it while the pool is
            // still climbing) never revisits the decision.
            return false;
        }
        if slow * 2 >= sampled {
            self.hold_until = Some(now + HOLD);
        }
        match self.hold_until {
            Some(until) if now < until => {
                self.logged = true;
                true
            }
            _ => false,
        }
    }

    /// True once per systemic episode, so the caller can log it without
    /// flooding.
    pub fn take_logged(&mut self) -> bool {
        std::mem::replace(&mut self.logged, false)
    }
}

impl Default for OverallSlow {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-connection slow threshold, derived from the *other* connections rather
/// than from a fixed number.
///
/// A fixed floor cannot work across drives. Quark limits per connection
/// (40–80 KiB/s each, so anything under 16 KiB/s is half-dead), while Aliyun
/// limits per IP (~2.5 MB/s total, so 64 connections each see ~40 KiB/s and a
/// jittery minute pushes them all to 10 KiB/s). A single 16 KiB/s floor marks
/// every Aliyun connection slow, they all reconnect, and the reconnect storm —
/// hundreds of TCP+TLS handshakes — costs more than the throttling did.
///
/// So the real threshold is `max(peer_median * 0.5, death_floor)`: the median
/// catches stragglers, and the death floor still catches connections that are
/// truly dead no matter how slow the peers are.
const MEDIAN_SHARE: f64 = 0.5;
/// A connection below this is dead, whatever its peers are doing.
const DEATH_FLOOR_BPS: f64 = 4.0 * 1024.0;

/// Median of a sample set. Small sets are the norm here (a round's worth of
/// connections), so a copy-and-sort is both simpler and faster than a selection
/// algorithm.
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

/// How much slower than its peers a connection must be before it is replaced.
///
/// Deliberately stricter than [`MEDIAN_SHARE`]: the watchdog asks "is this
/// connection lagging", which can be a gentle question, but acting on it costs
/// a handshake and the bytes buffered on the old connection. Replacing at 0.34
/// of the median means a connection has to be genuinely bad — a third of what
/// its siblings manage — before it is thrown away.
const PREEMPT_SHARE: f64 = 0.34;
/// Floor for the replacement decision, bytes/s.
///
/// This one *is* an absolute floor, unlike [`MEDIAN_SHARE`]'s line, because it
/// has to work in the endgame: with one or two connections left there is no
/// peer median worth trusting, and "below 0.34 of yourself" never fires.
///
/// It has to be high enough to catch the bad half of a CDN's node lottery.
/// Measured on one drive, twenty-four connections opened at once delivered
/// 10 KB/s on seven of them and 550-730 KB/s on the rest — a 55x spread between
/// nodes, not a gentle per-IP throttle. The old 12 KB/s floor sat below the bad
/// nodes' own speed, so they were never replaced and the whole download ran at
/// the pace of its slowest third. 100 KB/s is still under a tenth of a healthy
/// node here and comfortably above a dead one.
const PREEMPT_MIN_BPS: f64 = 100.0 * 1024.0;

/// The speed below which a connection is worth dropping and reopening.
pub fn preempt_threshold(peer_speeds: &[f64]) -> f64 {
    let relative = median(peer_speeds) * PREEMPT_SHARE;
    relative.max(PREEMPT_MIN_BPS)
}

/// The speed below which a connection is worth dropping and reopening.
pub fn slow_threshold(peer_speeds: &[f64], absolute_floor: f64) -> f64 {
    let dynamic = median(peer_speeds) * MEDIAN_SHARE;
    // The dynamic line, when it is meaningful, replaces the absolute floor —
    // taking the max of both is exactly the bug described above.
    let base = if dynamic > 0.0 {
        dynamic
    } else {
        absolute_floor
    };
    base.max(DEATH_FLOOR_BPS)
}

/// How large a tail piece should be, given what one connection is achieving.
///
/// The tail is cut into pieces so the idle pool has work while the last few
/// large segments finish. The right size is a few seconds of one connection's
/// throughput: big enough that a piece is not dominated by its handshake, small
/// enough that the pool is not waiting on a single slow connection to close out
/// the download. A fixed size cannot do that — it is far too coarse when the
/// drive is crawling and needlessly fine when it is fast.
///
/// Returns `min` when nothing has been measured yet, so the caller keeps its
/// own default rather than guessing.
pub fn tail_slice_bytes(peer_speeds: &[f64], min: u64) -> u64 {
    let avg = median(peer_speeds);
    if avg <= 0.0 {
        return min;
    }
    // Five seconds of one connection: the same target the planner uses when it
    // decides how deep to cut, so a tail piece costs about as much as a
    // handshake is worth waiting for.
    let want = (avg * TAIL_TARGET_SECONDS) as u64;
    want.clamp(min, TAIL_MAX_BYTES)
}

/// Seconds of one connection a tail piece should hold.
const TAIL_TARGET_SECONDS: f64 = 5.0;
/// Ceiling for a tail piece: past this, cutting finer stops helping because the
/// download is already down to its last few connections.
const TAIL_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Whether dropping a connection to reopen it is worth the handshake.
///
/// A connection that would take longer to finish its remaining bytes than a
/// reconnect costs is worth replacing. In the endgame the bar is raised: there
/// are few segments left, so a reconnect throws away a larger share of the work
/// still to do and the tail is short anyway.
pub fn worth_reconnecting(bps: f64, remaining: u64, endgame: bool) -> bool {
    if remaining == 0 {
        return false;
    }
    // Zero throughput over the observation window means the connection is dead,
    // not merely slow. Without this a hung connection is never replaced.
    if bps <= 0.0 {
        return true;
    }
    let cost_ms = if endgame { 5000.0 } else { 1000.0 };
    remaining as f64 * 1000.0 / bps > cost_ms
}

/// Throttles how often connections may be replaced for being slow.
///
/// When a drive throttles, several connections notice at the same moment and
/// all decide to reconnect. That is strictly worse than doing nothing: the
/// transfers they abandon drop to zero for a second, and every replacement
/// starts over with a fresh handshake. Capping the rate at a few per window
/// spreads the replacements out, so the rest keep making progress.
///
/// Only *slow* reconnects are throttled. A connection that broke, timed out or
/// got a `503` has no choice but to retry, and holding that back would only add
/// delay to a transfer that is already stuck.
#[derive(Debug)]
pub struct SpeedReconnects {
    window_start: Option<Instant>,
    used: u32,
}

/// Window the replacement allowance is counted over.
const RECONNECT_WINDOW: Duration = Duration::from_secs(10);
/// Replacements allowed inside one window.
///
/// Only the endgame needs a cap: with one connection left, replacing it is a
/// coin flip that costs a handshake, and a run of bad luck would spin. While
/// the pool is wide the useful limit is the work itself — measured, thirty-two
/// connections replacing their slow third burned through ~160 reopens in ten
/// seconds and gained 8-20x throughput, so a cap of four was throttling the
/// only mechanism that made the drive fast.
const RECONNECT_MAX: u32 = 200;

impl SpeedReconnects {
    pub fn new() -> Self {
        Self {
            window_start: None,
            used: 0,
        }
    }

    /// Asks permission to replace one slow connection.
    pub fn acquire(&mut self, now: Instant) -> bool {
        let expired = match self.window_start {
            Some(start) => now.saturating_duration_since(start) > RECONNECT_WINDOW,
            None => true,
        };
        if expired {
            self.window_start = Some(now);
            self.used = 0;
        }
        if self.used >= RECONNECT_MAX {
            return false;
        }
        self.used += 1;
        true
    }
}

impl Default for SpeedReconnects {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_refusals_are_tolerated_three_times() {
        let mut gate = RangeGate::new();
        let t0 = Instant::now();
        assert!(!gate.observe(t0));
        assert!(!gate.observe(t0));
        assert!(!gate.observe(t0));
        assert!(
            gate.observe(t0),
            "the fourth refusal in a window is a pattern"
        );
    }

    #[test]
    fn refusals_spread_over_hours_do_not_accumulate() {
        let mut gate = RangeGate::new();
        let t0 = Instant::now();
        for step in 0..10 {
            let at = t0 + RANGE_WINDOW * (step + 1) + Duration::from_secs(1);
            assert!(
                !gate.observe(at),
                "a refusal per window must never reach the tolerance"
            );
        }
    }

    #[test]
    fn slow_start_grows_quadratically() {
        assert_eq!(next_allowed(1, 256), 2);
        assert_eq!(next_allowed(2, 256), 4);
        assert_eq!(next_allowed(4, 256), 16);
        assert_eq!(next_allowed(16, 256), 256);
        assert_eq!(next_allowed(256, 256), 256);
        // Reaching the ceiling must not overflow.
        assert_eq!(next_allowed(4096, 4096), 4096);
        // Squaring a large value must saturate at the ceiling rather than wrap.
        assert_eq!(next_allowed(65_536, usize::MAX), 65_536 * 65_536);
        assert_eq!(next_allowed(usize::MAX, usize::MAX), usize::MAX);
    }

    #[test]
    fn slow_start_reaches_the_pool_in_a_few_steps() {
        let mut ramp = SlowStart::new(512);
        let mut steps = 0;
        while ramp.allowed() < 512 {
            ramp.advance();
            steps += 1;
            assert!(steps < 8, "ramp should not take {steps} steps");
        }
        assert_eq!(ramp.allowed(), 512);
    }

    #[test]
    fn systemic_slow_is_held_for_a_while() {
        let mut gate = OverallSlow::new();
        let t0 = Instant::now();
        assert!(
            !gate.evaluate(8, 1, 64, 3, t0),
            "one straggler is not systemic"
        );
        assert!(gate.evaluate(8, 5, 64, 3, t0), "half slow is systemic");
        // Still systemic a few seconds later, even though this sample is clean.
        assert!(gate.evaluate(8, 0, 64, 3, t0 + Duration::from_secs(5)));
        // After the hold it clears.
        assert!(!gate.evaluate(8, 0, 64, 3, t0 + HOLD + Duration::from_secs(1)));
    }

    #[test]
    fn endgame_always_allows_preemption() {
        let mut gate = OverallSlow::new();
        let t0 = Instant::now();
        assert!(gate.evaluate(8, 8, 64, 3, t0));
        // Two connections left: everything may be replaced.
        assert!(!gate.evaluate(2, 2, 2, 3, t0));
    }

    #[test]
    fn too_few_samples_neither_condemn_nor_preempt() {
        // Suppressing preemption on an empty sample list used to be permanent in
        // practice: the first round has no peers yet, and every later round
        // starts empty while the pool is still climbing, so the "not enough
        // evidence" verdict never got revisited and no slow connection was ever
        // replaced. Two slow peers out of two is exactly the case that must not
        // be read as "the whole drive is throttled".
        let mut gate = OverallSlow::new();
        let t0 = Instant::now();
        assert!(!gate.evaluate(2, 2, 64, 3, t0));
    }

    #[test]
    fn threshold_follows_the_peers_not_a_constant() {
        // Aliyun: every connection around 40 KiB/s. A 16 KiB/s floor would have
        // marked them all slow; half the median does not.
        let peers = [40.0 * 1024.0, 42.0 * 1024.0, 38.0 * 1024.0, 41.0 * 1024.0];
        let limit = slow_threshold(&peers, 16.0 * 1024.0);
        assert!(limit < 40.0 * 1024.0);
        assert!(limit > 16.0 * 1024.0);

        // A jittery Aliyun minute: everyone at 10 KiB/s. The absolute floor
        // must not be maxed in, or every connection would be dropped.
        let jittery = [10.0 * 1024.0, 9.0 * 1024.0, 11.0 * 1024.0, 10.0 * 1024.0];
        assert!(slow_threshold(&jittery, 16.0 * 1024.0) < 10.0 * 1024.0);
    }

    #[test]
    fn dead_connections_are_always_dropped() {
        // Even when every peer is slow, a 1 KiB/s connection is dead.
        let peers = [5.0 * 1024.0, 5.0 * 1024.0];
        assert!(slow_threshold(&peers, 16.0 * 1024.0) >= DEATH_FLOOR_BPS);
    }

    #[test]
    fn median_of_an_even_set_averages_the_middle() {
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[]), 0.0);
    }

    #[test]
    fn the_replacement_line_is_stricter_than_the_watchdog_line() {
        let peers = [580.0 * 1024.0, 590.0 * 1024.0, 570.0 * 1024.0, 580.0 * 1024.0];
        let watchdog = slow_threshold(&peers, 16.0 * 1024.0);
        let replace = preempt_threshold(&peers);
        assert!(replace < watchdog, "replacing must be the stricter test");
        // A connection at a third of its peers is bad enough to drop.
        assert!(190.0 * 1024.0 < replace);
        // One that merely lags is not.
        assert!(300.0 * 1024.0 > replace);
    }

    #[test]
    fn a_bad_node_is_replaced_even_beside_healthy_ones() {
        // The measured node lottery: seven connections at 10 KB/s beside
        // seventeen at ~580 KB/s. The floor must catch the bad ones on its own,
        // because the good ones finish their segment before a window closes and
        // therefore never report a speed to compare against.
        let bad = 10.0 * 1024.0;
        assert!(bad < preempt_threshold(&[]), "caught with no peers at all");
        assert!(bad < preempt_threshold(&[bad]), "caught beside itself");
        assert!(
            bad < preempt_threshold(&[580.0 * 1024.0; 16]),
            "caught beside healthy peers"
        );
    }

    #[test]
    fn the_replacement_line_holds_with_no_peers_to_compare() {
        // Endgame: one connection, no median. The absolute floor is all there
        // is, and it must not collapse to zero.
        assert_eq!(preempt_threshold(&[]), PREEMPT_MIN_BPS);
        assert!(preempt_threshold(&[1.0]) >= PREEMPT_MIN_BPS);
    }

    #[test]
    fn slow_reconnects_are_rate_limited() {
        let mut governor = SpeedReconnects::new();
        let t0 = Instant::now();
        for _ in 0..RECONNECT_MAX {
            assert!(governor.acquire(t0), "the allowance is not spent yet");
        }
        assert!(!governor.acquire(t0), "the allowance is spent");
        // A new window restores it.
        assert!(governor.acquire(t0 + RECONNECT_WINDOW + Duration::from_secs(1)));
    }

    #[test]
    fn a_tail_piece_follows_the_measured_speed() {
        let min = 128 * 1024;
        // Nothing measured yet: keep the caller's default rather than guess.
        assert_eq!(tail_slice_bytes(&[], min), min);
        // A crawling drive gets small pieces, a fast one larger ones — five
        // seconds of one connection either way.
        let slow = tail_slice_bytes(&[40.0 * 1024.0, 40.0 * 1024.0], min);
        let fast = tail_slice_bytes(&[2.0 * 1024.0 * 1024.0, 2.0 * 1024.0 * 1024.0], min);
        assert!(slow < fast, "slow {slow} should be below fast {fast}");
        assert!(slow >= min);
        assert!(fast <= TAIL_MAX_BYTES);
    }

    #[test]
    fn reconnecting_pays_off_only_when_there_is_a_lot_left() {
        // 1 MiB/s connection with 100 KiB left: a reconnect costs more.
        assert!(!worth_reconnecting(1024.0 * 1024.0, 100 * 1024, false));
        // Same connection with 100 MiB left: replace it.
        assert!(worth_reconnecting(
            1024.0 * 1024.0,
            100 * 1024 * 1024,
            false
        ));
        // A hung connection is always worth replacing.
        assert!(worth_reconnecting(0.0, 4096, false));
        // Nothing left: never.
        assert!(!worth_reconnecting(1024.0, 0, false));
    }
}
