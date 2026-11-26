//! Segmented, multi-connection download.
//!
//! One stream against a CDN runs at whatever that one connection happens to
//! give — often a few hundred KiB/s, because these drives rate-limit per
//! connection. Splitting the file into many small segments and fetching them
//! over many connections in parallel turns that into the sum of hundreds of
//! connections, which is where the speed comes from. No membership, no private
//! API: the same download endpoint the official client uses, called with a
//! byte range.
//!
//! The parts that make it work, in the order they matter:
//!
//! 1. **Many small segments, not a few large ones.** Segments are sized from
//!    the connection count (`connections * 8` deep), so a slow connection that
//!    grabs a segment finishes quickly and the others pick up the rest. With
//!    few large segments the whole download ends when the *slowest* connection
//!    finishes its single chunk — the classic tail collapse.
//! 2. **The first stretch in small pieces.** Media players read the header
//!    first, so the head of the file is fetched in finer slices; otherwise the
//!    file stays at zero bytes until one slow connection has pulled tens of
//!    megabytes.
//! 3. **A connection budget per drive.** Quark throttles per account, Aliyun
//!    per IP; past the knee, extra connections only add handshakes.
//! 4. **Links are minted fresh.** These CDN links expire in minutes, so a long
//!    download has to re-mint mid-flight rather than die at 40%.
//! 5. **Length is verified.** Some CDNs answer an expired link with `200 OK`
//!    and a tiny error page; only the byte count gives it away.
//!
//! Everything here is an optimisation: if any of it cannot be done, the caller
//! falls back to the single-stream path and the download still works.

mod budget;
mod gate;
mod meta;
mod plan;
pub mod source;
mod worker;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::providers::DriveProvider;

pub use plan::Segment;

/// Live connection counters, shared between the engine and the transfer list.
///
/// The engine knows how wide the pool actually is (the budget is per account,
/// and a round only opens as many workers as there are segments left); the UI
/// otherwise has no way to tell a 256-connection download from a single stream,
/// and "why is it slow" is usually answered by that number.
#[derive(Debug, Default)]
pub struct Stats {
    connections: AtomicU64,
    segments: AtomicU64,
    finished: AtomicU64,
}

impl Stats {
    /// Records the shape of the round about to run.
    pub fn report(&self, connections: usize, segments: usize, finished: usize) {
        self.connections.store(connections as u64, Ordering::Relaxed);
        self.segments.store(segments as u64, Ordering::Relaxed);
        self.finished.store(finished as u64, Ordering::Relaxed);
    }

    pub fn connections(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }

    pub fn segments(&self) -> u64 {
        self.segments.load(Ordering::Relaxed)
    }

    pub fn finished(&self) -> u64 {
        self.finished.load(Ordering::Relaxed)
    }

    /// Drops the connection count without forgetting the plan, for the moment
    /// the last round ends.
    pub fn idle(&self) {
        self.connections.store(0, Ordering::Relaxed);
    }
}

/// Floor for a tail-refill split. Matches the planner's 128 KiB minimum: below
/// that the handshake is a larger cost than the parallelism it buys.
const TAIL_MIN_BYTES: u64 = 128 * 1024;

/// Which files are downloading segmented right now, per drive.
///
/// Quark limits connections **per account**, not per file: three files each
/// opening 512 connections ask for 1536 and the drive throttles all of them.
/// Knowing the sibling sizes lets each file take a share proportional to what
/// it still has to fetch.
static ACTIVE: std::sync::Mutex<Vec<(&'static str, u64, u64)>> = std::sync::Mutex::new(Vec::new());

/// The budget bucket a provider draws from.
///
/// Drives with a measured profile get their own bucket, because their ceilings
/// differ (Quark 512, 天翼 128, the rest 64) and pooling two drives under one key
/// would hand one of them a budget it cannot use while starving the other.
fn bucket(provider: &str) -> &'static str {
    match provider {
        "quark" => "quark",
        "aliyun" => "aliyun",
        "cloud189" => "cloud189",
        _ => "other",
    }
}

/// Registers a file and returns its share of the drive's connection budget.
///
/// The returned id must be handed to [`leave`] when the download ends, on every
/// path — a stale entry would shrink every later download's share.
fn enter(provider: &str, size: u64, profile: budget::Profile) -> (u64, usize) {
    let mut active = match ACTIVE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let id = active.iter().map(|(_, _, id)| *id).max().unwrap_or(0) + 1;
    // The id doubles as the ordering key, so keep it on the entry.
    let key = bucket(provider);
    active.push((key, size, id));
    let sizes: Vec<u64> = active
        .iter()
        .filter(|(entry_key, _, _)| *entry_key == key)
        .map(|(_, size, _)| *size)
        .collect();
    let index = sizes.len() - 1;
    (id, budget::share(profile, &sizes, index))
}

/// Removes a file from the registry.
fn leave(provider: &str, id: u64) {
    let key = bucket(provider);
    let mut active = match ACTIVE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    active.retain(|(entry_key, _, entry_id)| !(*entry_key == key && *entry_id == id));
}

/// Removes the resume sidecar next to `part`, if any.
///
/// Called when a partial is discarded (task removed, download abandoned): the
/// plan without the file it describes would only mislead the next attempt.
pub fn discard_progress(part: &Path) {
    meta::Progress::discard(part);
}

/// What a leftover partial file means for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeState {
    /// No sidecar: the file length is the only truth, as it always was.
    None,
    /// A sidecar says every segment was fetched; only the rename is missing.
    Finished,
    /// A sidecar says work remains. The length is *not* evidence of
    /// completeness: the engine preallocates the file, so a failed run leaves
    /// it at full size with holes in it.
    Partial,
}

/// Reads the sidecar to decide whether a partial file can be trusted.
///
/// Without this, a download interrupted between the preallocation and the last
/// segment would look finished on the next attempt and the sparse file would be
/// renamed into place.
/// How much of a partial is really on disk, according to its plan.
///
/// The file itself is pre-allocated, so its length is the size of the *whole*
/// file from the first moment. Only the plan knows what has been written.
pub fn plan_bytes(part: &Path) -> u64 {
    meta::Progress::load(part).map(|plan| plan.bytes()).unwrap_or(0)
}

pub fn resume_state(part: &Path, size: u64, source: &str) -> ResumeState {
    match meta::Progress::load(part).filter(|saved| saved.belongs_to(size, source)) {
        Some(saved) if saved.finished() => ResumeState::Finished,
        Some(_) => ResumeState::Partial,
        None => ResumeState::None,
    }
}

/// Files at or above this size are worth segmenting. Below it, the handshake
/// cost of dozens of connections outweighs the extra throughput.
pub const MIN_SEGMENTED: u64 = plan::MIN_SEGMENTED;

/// How many times one download may mint a fresh link before giving up.
///
/// A minted url lives a few minutes, so the number of refreshes a download
/// legitimately needs is "how many link lifetimes does this file take at the
/// speed it is actually going". A fixed allowance cannot answer that: at
/// 200 KB/s a 9.5 GB file needs about 150 refreshes, and a fixed cap of a few
/// dozen would abandon a download that was progressing perfectly.
///
/// `speed` is bytes per second, measured; `0.0` means nothing has moved yet and
/// the floor is used. The floor keeps a small file from looping, the cap keeps a
/// broken drive from retrying forever.
fn refresh_budget(size: u64, speed: f64) -> u32 {
    // How much data one link is good for. `LINK_TTL` is what these drives
    // publish, and the ratio leaves room for the tail of the window during
    // which the url is already dead.
    const LINK_TTL: f64 = 300.0;
    const USABLE: f64 = 0.75;
    const FLOOR: u32 = 5;
    const CAP: u32 = 400;
    if speed <= 0.0 {
        // Nothing measured: fall back to a size-derived guess, roughly a
        // megabyte per second, so a big file still gets more attempts.
        let guess = (size / (8 * 1024 * 1024)).clamp(1, 120) as u32;
        return guess.clamp(FLOOR, CAP);
    }
    let per_refresh = speed * LINK_TTL * USABLE;
    if per_refresh <= 0.0 {
        return FLOOR;
    }
    let needed = (size as f64 / per_refresh).ceil() as u64 + 1;
    (needed.clamp(u64::from(FLOOR), u64::from(CAP))) as u32
}

/// Runs a segmented download into `part`.
///
/// Returns `Ok(false)` when the drive or the file does not suit segmentation,
/// so the caller can fall back to one stream. Returns `Ok(true)` only when the
/// whole file is on disk.
///
/// `done` counts from `base`, so a batch of files can share one counter; the
/// base stays baked into the total because every update is additive.
///
/// `stats` receives the pool's shape (how many connections are open, how many
/// pieces the file was cut into) for the transfer list; pass `None` when
/// nobody is watching.
#[allow(clippy::too_many_arguments)]
pub async fn download_with_stats(
    drive: Arc<dyn DriveProvider>,
    source: Arc<dyn source::Source>,
    provider: &str,
    part: &Path,
    size: u64,
    done: Arc<AtomicU64>,
    base: u64,
    token: CancellationToken,
    stats: Option<Arc<Stats>>,
    cap: Option<u64>,
) -> AppResult<bool> {
    if size < MIN_SEGMENTED || !drive.supports_ranges() {
        return Ok(false);
    }

    let profile = budget::Profile::for_provider(provider);
    // Share the drive's budget with the files already downloading: the limit is
    // per account, so three files at full count throttle each other.
    let (slot, connections) = enter(provider, size, profile);
    let _guard = LeaveGuard {
        provider: provider.to_string(),
        slot,
    };

    // A resumed download must replay the *saved* plan, not a freshly computed
    // one: the plan depends on the measured speed, so recomputing it would
    // write re-fetched bytes at the wrong offsets.
    let identity = source.fingerprint();
    let saved = meta::Progress::load(part).filter(|saved| saved.belongs_to(size, &identity));
    let resumable = saved
        .as_ref()
        .is_some_and(|saved| reused_is_partial(part, saved));
    let progress = match saved {
        Some(saved) if resumable => saved,
        _ => {
            // No usable state: throw away whatever was there and plan afresh.
            meta::Progress::discard(part);
            meta::Progress::with_source(size, plan::split(size, connections), identity)
        }
    };

    if progress.finished() {
        // Everything was already fetched last time; the caller just needs the
        // file. Leave the rename to it so the completion path stays in one place.
        done.store(base + size, Ordering::Relaxed);
        meta::Progress::discard(part);
        // The bytes are already on disk, so no url is needed — but a share
        // source may still be holding a copy it made in the user's drive.
        source.cleanup().await;
        return Ok(true);
    }

    if progress.completed() > 0 {
        crate::ylog!(
            "download: resuming {} with {}/{} segments left",
            part.display(),
            progress.remaining(),
            progress.segments.len()
        );
    }
    let outcome = run_shared(
        source,
        part,
        size,
        done,
        base,
        token,
        profile,
        connections,
        progress,
        stats.clone(),
        cap,
    )
    .await;
    // The pool is gone once this returns; a readout still claiming 256 open
    // connections would be the last thing the list ever showed.
    if let Some(stats) = stats.as_deref() {
        stats.idle();
    }
    outcome
}

/// Releases a registry slot on every exit path, including errors and early
/// returns. A leaked entry would permanently shrink the drive's budget.
struct LeaveGuard {
    provider: String,
    slot: u64,
}

impl Drop for LeaveGuard {
    fn drop(&mut self) {
        leave(&self.provider, self.slot);
    }
}

/// True when the partial file and its sidecar can be trusted to continue.
fn reused_is_partial(part: &Path, progress: &meta::Progress) -> bool {
    match std::fs::metadata(part) {
        // `bytes()` and not `completed()`: a single-connection run may not have
        // finished a single segment yet, and its partially written segment is
        // exactly what a resume must keep.
        Ok(meta) => meta.len() == progress.total && progress.bytes() > 0,
        Err(_) => false,
    }
}

/// The shared body of both entry paths: open the file, fan out, wait.
///
/// Workers are disposable. A worker that loses its link, hits a broken
/// connection or gets refused simply stops; the loop below notices the segments
/// still pending and starts another round. Because every attempt asks the
/// provider for a fresh link, a new round *is* the link refresh — there is no
/// separate refresh path that could disagree with the fetch path.
#[allow(clippy::too_many_arguments)]
async fn run_shared(
    source: Arc<dyn source::Source>,
    part: &Path,
    size: u64,
    done: Arc<AtomicU64>,
    base: u64,
    token: CancellationToken,
    profile: budget::Profile,
    connections: usize,
    mut progress: meta::Progress,
    stats: Option<Arc<Stats>>,
    cap: Option<u64>,
) -> AppResult<bool> {
    // Pre-flight before fanning out: one DNS resolve and one request. Hundreds
    // of connections starting at once would otherwise all fail together on a
    // momentary hiccup.
    if !preflight(source.as_ref(), size, &token).await? {
        return Ok(false);
    }

    crate::ylog!(
        "download: segmenting {} into {} pieces over {} connections",
        source.label(),
        progress.segments.len(),
        connections
    );

    if let Some(stats) = stats.as_deref() {
        // The plan, not the width: no connection is open yet, and claiming the
        // full budget before preflight has even run would be a lie the readout
        // then has to walk back.
        stats.report(0, progress.segments.len(), progress.completed() as usize);
    }

    // The plan goes to disk *before* the file is pinned to its full length:
    // a preallocated part without a sidecar is indistinguishable from a
    // finished download, and `ResumeState::None` would have to re-fetch it.
    progress.save(part)?;
    let sink = worker::Sink::open(part, size)?;
    // Bytes already on disk count towards progress from the first tick.
    let already = progress.bytes();
    done.store(base + already, Ordering::Relaxed);

    // One limiter per segmented download: with a per-task cap the workers of a
    // single file share it, while other files keep their own budget.
    let limiter = Arc::new(match cap {
        Some(own) => worker::Limiter::with_cap(own),
        None => worker::Limiter::new(),
    });
    let progress = Arc::new(Mutex::new(progress));
    // Start from this file's share of the drive's budget rather than the whole
    // of it, and let adaptation work inside that share.
    let adaptive = Arc::new(Mutex::new(budget::Adaptive::with_connections(
        profile,
        connections,
    )));
    let refusals = Arc::new(AtomicU64::new(0));
    // Errors seen since the last round boundary, so the round can tell "this
    // round worked" from "this round hit a dead link" without inspecting
    // per-worker state after they are gone.
    let errors_this_round = Arc::new(AtomicU64::new(0));
    let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Recomputed every round from the measured speed: the allowance has to
    // follow the transfer, not a guess made before it started.
    let started_at = Instant::now();
    let mut rounds = refresh_budget(size, 0.0);

    // A refusal this task saw counts only for this task: sharing the gate would
    // let one file's hiccup downgrade another file that was working fine.
    let mut range_gate = gate::RangeGate::new();
    // Opens a handful of connections first and widens quadratically: hundreds
    // of simultaneous handshakes fail together when DNS or the CDN hiccups.
    //
    // The gate outlives a round. It used to be rebuilt at every round boundary,
    // and because a round only ends when every worker is done, the first round
    // ran on a single connection until it had drained the whole queue — a 47 MB
    // quark file measured 75 KB/s while thirty connections' worth of budget sat
    // unused. The gate now widens while the pool is running.
    let ramp = Arc::new(Mutex::new(gate::SlowStart::new(
        adaptive.lock().await.current(),
    )));
    if already > 0 {
        // Resuming: these connections were already proven to work, so there is
        // nothing to learn from ramping up again.
        ramp.lock().await.open_all();
    }
    // Suppresses slow-connection preemption when the whole drive is throttled.
    let mut overall = gate::OverallSlow::new();
    // Peer throughput samples, cleared every round: they answer "is this
    // connection slower than its siblings", which a constant cannot.
    let speeds: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    // Shared allowance for dropping slow connections, so a throttled drive does
    // not lose all of them at the same moment.
    let reconnects = Arc::new(Mutex::new(gate::SpeedReconnects::new()));
    // Consecutive rounds where not one byte moved, and how many are tolerated.
    let mut stalled_rounds = 0u32;
    const MAX_STALLED_ROUNDS: u32 = 6;    // The sidecar is rewritten at most this often while segments complete. A
    // segment finishing is the only event worth persisting, but with hundreds
    // of them the plan would be rewritten hundreds of times a second — each
    // write is a temp file plus a rename, done under the same lock the workers
    // claim under, so it competes with the download itself. A crash loses at
    // most this much work, and those segments are fetched again anyway.
    const SAVE_INTERVAL: Duration = Duration::from_secs(2);
    let last_save = Arc::new(Mutex::new(Instant::now()));
    // At or below this many pending segments the download is in its tail: every
    // connection is worth replacing, and there is nothing left to starve.
    const ENDGAME_SEGMENTS: usize = 4;
    // Connections beyond this many are started at the same instant as the
    // eighth: spreading the whole pool would add seconds of delay to a wide
    // round for no extra benefit.
    const STAGGER_SLOTS: usize = 8;
    const STAGGER_STEP: Duration = Duration::from_millis(25);
    // How often the gate is allowed to widen, how long a worker waits for its
    // slot to open, and how long the gate may keep widening before it is opened
    // outright. Ten seconds is a floor, not a target: a healthy pool reaches
    // its full width in four steps.
    const GATE_TICK: Duration = Duration::from_millis(150);
    const GATE_POLL: Duration = Duration::from_millis(50);
    const GATE_MAX_WAIT: Duration = Duration::from_secs(10);

    // Samples aggregate throughput once a second so the budget can react to a
    // drive that throttles harder as the connection count climbs.
    let sampling = tokio::spawn({
        let adaptive = Arc::clone(&adaptive);
        let done = Arc::clone(&done);
        let token = token.clone();
        async move {
            let mut last = done.load(Ordering::Relaxed);
            let mut at = Instant::now();
            loop {
                tokio::select! {
                    _ = token.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                let now = Instant::now();
                let current = done.load(Ordering::Relaxed);
                let elapsed = now.saturating_duration_since(at).as_secs_f64();
                if elapsed > 0.0 {
                    let speed = current.saturating_sub(last) as f64 / elapsed;
                    adaptive.lock().await.observe(speed, now);
                }
                last = current;
                at = now;
            }
        }
    });

    // Widens the gate while the pool is running, independently of the round
    // boundaries. A round only ends when every worker is done, so tying the
    // ramp to rounds meant the first round ran on one connection until the
    // queue was empty.
    let gate_ticker = tokio::spawn({
        let ramp = Arc::clone(&ramp);
        let token = token.clone();
        async move {
            let opened = Instant::now();
            loop {
                tokio::select! {
                    _ = token.cancelled() => return,
                    _ = tokio::time::sleep(GATE_TICK) => {}
                }
                let mut gate = ramp.lock().await;
                if opened.elapsed() >= GATE_MAX_WAIT {
                    // Something is holding the pool at the first rung (a drive
                    // that never finishes a segment, say). Waiting the full ten
                    // seconds would waste the whole transfer, so open it.
                    gate.open_all();
                    return;
                }
                gate.advance();
            }
        }
    });

    let mut round = 0u32;
    loop {
        if token.is_cancelled() {
            sampling.abort();
            gate_ticker.abort();
            return Err(AppError::new("已取消"));
        }

        let completed_at_start = {
            let state = progress.lock().await;
            if state.finished() {
                break;
            }
            let completed = state.completed();
            // Snap progress back to what the plan says is on disk. A worker
            // that died mid-segment counted bytes the plan also remembers
            // (`written`), and the next round resumes that segment *from*
            // them rather than refetching it — so the count must keep them.
            // Snapping to `completed()` would drop a partially written
            // segment from the total, and nothing later would add it back:
            // the bar then sits below the truth for the rest of the run.
            done.store(base + state.bytes(), Ordering::Relaxed);
            completed
        };

        // Fan out one round. The count follows the adaptive budget, so a drive
        // that throttled gets a narrower round next time, and the slow-start
        // ramp so the first round is narrow.
        // How many connections this file is allowed right now. The slow-start
        // gate decides how many of them may start work, and widens while the
        // pool runs.
        let budget_now = adaptive.lock().await.current();

        // No worker survives a round, so nothing is still claimed: clearing the
        // flags here is what lets a failed segment be picked up again.
        {
            let mut state = progress.lock().await;
            state.release_all();
        }

        // Take this round's throughput samples before anything else: both the
        // tail piece size and the slow-connection line are derived from them.
        let peers = {
            let mut samples = speeds.lock().await;
            std::mem::take(&mut *samples)
        };

        // Tail refill: with only a few large segments left the pool is idle, so
        // cut the biggest ones finer until there is roughly one piece per
        // connection the budget allows — not one per segment that happens to be
        // left. Cutting to the segment count instead is what leaves the tail
        // running on three connections while four hundred sit idle.
        let tail_bytes = gate::tail_slice_bytes(&peers, TAIL_MIN_BYTES);
        {
            let mut state = progress.lock().await;
            let mut refilled = false;
            while state.remaining() < budget_now {
                if !state.split_largest(tail_bytes) {
                    break;
                }
                refilled = true;
            }
            if refilled {
                let _ = state.save(part);
            }
        }
        let pending = {
            let state = progress.lock().await;
            state.remaining()
        };
        let workers = budget_now.min(pending).max(1);
        let taps: Arc<Vec<std::sync::atomic::AtomicU64>> = {
            let state = progress.lock().await;
            Arc::new(
                (0..state.segments.len())
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect(),
            )
        };
        crate::ylog!(
            "download: round {round} over {workers} connections ({pending} segments left)"
        );
        if let Some(stats) = stats.as_deref() {
            let state = progress.lock().await;
            let finished = state.segments.len() - pending;
            stats.report(workers, state.segments.len(), finished);
        }

        // Decide whether dropping slow connections is worth anything right now.
        let slow_floor = profile.slow_floor;
        let threshold = gate::slow_threshold(&peers, slow_floor);
        // How many connections were below the line this round. Comparing each
        // connection with its peers is the only way to tell "this one drew a bad
        // CDN node" from "the drive is throttling the whole IP" — and only the
        // first is worth reconnecting for.
        let slow_peers = peers.iter().filter(|bps| **bps < threshold).count();
        let systemic = overall.evaluate(
            peers.len(),
            slow_peers,
            workers,
            ENDGAME_SEGMENTS,
            Instant::now(),
        );
        let preempt = if systemic {
            0.0
        } else {
            // Replacing is a stricter test than noticing: the watchdog line
            // answers "is this connection lagging", but acting on it costs a
            // handshake and whatever the old connection had buffered.
            gate::preempt_threshold(&peers)
        };
        if overall.take_logged() {
            crate::ylog!(
                "download: drive looks throttled as a whole ({} peers), holding {} connections",
                peers.len(),
                workers
            );
        }
        let control = worker::Control {
            slow_threshold: preempt,
            // With a handful of segments left, a reconnect costs a larger share
            // of the remaining work than the faster connection can win back.
            endgame: pending <= ENDGAME_SEGMENTS,
            speeds: Some(Arc::clone(&speeds)),
            reconnects: Some(Arc::clone(&reconnects)),
        };
        let mut handles = Vec::with_capacity(workers);
        // A round-scoped token, cancelled by the first worker that is told its
        // link has expired. Every worker in this round is using a url from the
        // same mint, so the other few hundred would only collect the same
        // rejection; stopping them lets the next round mint a fresh link at
        // once instead of after a few hundred failed requests.
        let round_token = token.child_token();
        for index in 0..workers {
            let source = Arc::clone(&source);
            let sink = sink.clone();
            let limiter = Arc::clone(&limiter);
            let progress = Arc::clone(&progress);
            let done = Arc::clone(&done);
            let round_token = round_token.clone();
            let refusals = Arc::clone(&refusals);
            let errors_this_round = Arc::clone(&errors_this_round);
            let last_error = Arc::clone(&last_error);
            let last_save = Arc::clone(&last_save);
            let ramp = Arc::clone(&ramp);
            let part = part.to_path_buf();
            let control = control.clone();
            let stats = stats.clone();
            let tap_bytes = Arc::clone(&taps);

            handles.push(tokio::spawn(async move {
                // Stagger the first connections. A whole round starting at once
                // means hundreds of handshakes in the same instant, and TLS
                // negotiation is CPU-bound: the machine, the home NAT table and
                // the CDN all see a burst rather than a stream. 25 ms apart is
                // enough to spread it out; the total delay is capped at 175 ms
                // so a wide pool still starts promptly.
                let slots = index.min(STAGGER_SLOTS);
                if slots > 0 {
                    tokio::select! {
                        biased;
                        _ = round_token.cancelled() => return,
                        _ = tokio::time::sleep(STAGGER_STEP * slots as u32) => {}
                    }
                }
                loop {
                    if round_token.is_cancelled() {
                        return;
                    }
                    // Wait for this connection's slot on the slow-start gate.
                    // The gate widens while the pool is running, so a worker
                    // that has not been released yet simply waits a little
                    // instead of the whole round running at the first rung.
                    let mut waited = Duration::from_millis(0);
                    loop {
                        if round_token.is_cancelled() {
                            return;
                        }
                        let open = {
                            let gate = ramp.lock().await;
                            index < gate.allowed()
                        };
                        if open || waited >= GATE_MAX_WAIT {
                            break;
                        }
                        tokio::select! {
                            biased;
                            _ = round_token.cancelled() => return,
                            _ = tokio::time::sleep(GATE_POLL) => {}
                        }
                        waited += GATE_POLL;
                    }

                    // Take the next segment nobody is fetching. Claiming under
                    // the same lock as the completion flags is what stops the
                    // whole pool from piling onto the first unfinished segment.
                    let next = {
                        let mut state = progress.lock().await;
                        state.claim()
                    };
                    let Some((index, segment)) = next else {
                        // Every remaining segment is already claimed by another
                        // worker, and nothing new appears until the next round:
                        // stop rather than spin. The round ends when the last
                        // worker returns, and the loop below decides whether
                        // another one is needed.
                        return;
                    };

                    match worker::run_segment(
                        &source,
                        segment,
                        &sink,
                        &limiter,
                        &done,
                        &round_token,
                        &control,
                        Some(&worker::Tap {
                            index,
                            base: segment.start,
                            bytes: tap_bytes.clone(),
                        }),
                    )
                    .await
                    {
                        Ok(()) => {
                            let mut state = progress.lock().await;
                            state.mark(index);
                            if let Some(stats) = stats.as_deref() {
                                let finished = state.segments.len() - state.remaining();
                                stats.finished.store(finished as u64, Ordering::Relaxed);
                            }
                            let mut at = last_save.lock().await;
                            if at.elapsed() >= SAVE_INTERVAL {
                                let _ = state.save(&part);
                                *at = Instant::now();
                            }
                        }
                        Err(worker::FetchError::Cancelled) => {
                            progress.lock().await.release(index);
                            return;
                        }
                        Err(worker::FetchError::RangeIgnored) => {
                            progress.lock().await.release(index);
                            refusals.fetch_add(1, Ordering::Relaxed);
                            errors_this_round.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        Err(err) => {
                            // Expired, short bodies and broken streams all end
                            // the worker: the next round re-mints the link and
                            // picks the segment up where it stopped. The claim
                            // is dropped so that round can take it.
                            crate::ylog!("download: segment {index} stopped: {err}");
                            progress.lock().await.release(index);
                            errors_this_round.fetch_add(1, Ordering::Relaxed);
                            *last_error.lock().await = Some(err.to_string());
                            if matches!(err, worker::FetchError::Expired) {
                                // Stop the rest of the round: they hold urls
                                // from the same mint and would fail the same
                                // way. The loop below notices the pending
                                // segments and starts a round with a fresh one.
                                round_token.cancel();
                            }
                            return;
                        }
                    }
                }
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }

        // Persist whatever finished in this round, however short it was. The
        // per-segment saves above are throttled, so this is what guarantees a
        // round's work survives.
        {
            let mut state = progress.lock().await;
            // Fold the workers' live counters into the plan *before* saving:
            // this save is what a pause leaves behind, and it has to include
            // the segment that was still in flight.
            for (index, seen) in taps.iter().enumerate() {
                state.extend(index, seen.load(Ordering::Relaxed));
            }
            let _ = state.save(part);
        }

        if token.is_cancelled() {
            sampling.abort();
            gate_ticker.abort();
            return Err(AppError::new("已取消"));
        }

        {
            let state = progress.lock().await;
            if state.finished() {
                break;
            }
            // Read and reset: a refusal is an event of one round, not a state
            // the file carries. Leaving the counter set means a single hiccup in
            // round one keeps re-arming the gate every round after it, and four
            // rounds later the drive is declared range-incapable even though it
            // has been serving ranges fine the whole time.
            let refused = refusals.swap(0, Ordering::Relaxed);
            if refused > 0 {
                // One `200` where a `206` was expected is usually a hiccup; a
                // few inside a minute means the drive really cannot do ranges.
                crate::ylog!(
                    "download: drive answered a ranged request with the whole file ({} this minute)",
                    range_gate.count()
                );
                if range_gate.observe(Instant::now()) {
                    drop(state);
                    sampling.abort();
                    gate_ticker.abort();
                    meta::Progress::discard(part);
                    return Ok(false);
                }
                // Before the verdict, only give up if nothing at all landed —
                // otherwise this is a working download with a flaky node.
                if state.completed() == 0 && refused >= 2 {
                    drop(state);
                    sampling.abort();
                    gate_ticker.abort();
                    meta::Progress::discard(part);
                    return Ok(false);
                }
            }
        }


        // Every worker in the round saw an error: a dead link, a refusal, a
        // broken stream. Counted per round (not cumulatively) because the
        // inter-round pause exists to stop hammering a drive that is pushing
        // back — a round where every segment finished cleanly has nothing to
        // be polite about, and pausing there is what makes the slow-start ramp
        // cost seconds instead of milliseconds.
        let round_errors = errors_this_round.swap(0, Ordering::Relaxed);

        // Nothing moved this round: the segments were all reopened and all
        // failed before writing a byte. A few rounds of that is a dead drive,
        // not a slow one — keep the resume state and tell the user, rather than
        // looping until the refresh budget runs out.
        let moved = {
            let state = progress.lock().await;
            state.completed() > completed_at_start
        };
        if moved {
            stalled_rounds = 0;
        } else {
            stalled_rounds += 1;
            if stalled_rounds >= MAX_STALLED_ROUNDS {
                sampling.abort();
                gate_ticker.abort();
                let mut state = progress.lock().await;
                let missing = state.remaining();
                let completed = state.completed();
                let _ = state.save(part);
                drop(state);
                let detail = last_error
                    .lock()
                    .await
                    .as_deref()
                    .map(|err| format!("，最后一次错误：{err}"))
                    .unwrap_or_default();
                return Err(AppError::new(format!(
                    "分段下载无进展：连续 {MAX_STALLED_ROUNDS} 轮没有写入任何数据（还差 {missing} 个分段，已完成 {completed} / {size} 字节）{detail}"
                )));
            }
        }

        round += 1;
        // Re-derive the refresh allowance from what this download has actually
        // managed so far. A link expires on a clock, not on a round count, so
        // the number of refreshes needed is a function of throughput.
        {
            let elapsed = started_at.elapsed().as_secs_f64();
            if elapsed >= 5.0 {
                let moved = done.load(Ordering::Relaxed).saturating_sub(base);
                if moved > 0 {
                    rounds = rounds.max(refresh_budget(size, moved as f64 / elapsed));
                }
            }
        }
        if round > rounds {
            sampling.abort();
            gate_ticker.abort();
            let mut state = progress.lock().await;
            let missing = state.remaining();
            let completed = state.completed();
            let _ = state.save(part);
            drop(state);
            let detail = last_error
                .lock()
                .await
                .as_deref()
                .map(|err| format!("，最后一次错误：{err}"))
                .unwrap_or_default();
            return Err(AppError::new(format!(
                "分段下载失败：{missing} 个分段缺失（已下载 {completed} / {size} 字节）{detail}"
            )));
        }

        // Back off between rounds so a drive that is refusing everything is not
        // hammered. Only after a round that actually failed: most healthy rounds
        // end because every segment in them completed, and sleeping then would
        // turn the ramp-up into a series of stalls.
        if round_errors > 0 {
            let wait = Duration::from_millis(200 * u64::from(round.min(10)));
            tokio::select! {
                biased;
                _ = token.cancelled() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    sampling.abort();
    gate_ticker.abort();
    sink.sync().await?;
    meta::Progress::discard(part);
    done.store(base + size, Ordering::Relaxed);
    // A share download may have copied the file into the user's drive to get a
    // url at all; that copy is no longer needed.
    source.cleanup().await;
    Ok(true)
}

/// One cheap reachability check before fanning out.
///
/// Returns `Ok(false)` when the drive turns out not to serve ranges, so the
/// caller can pick the single-stream path without losing the download.
async fn preflight(
    source: &dyn source::Source,
    size: u64,
    token: &CancellationToken,
) -> AppResult<bool> {
    // Probe from the middle of the file, not from zero. A request for `bytes=0-`
    // is indistinguishable from a request for the whole file: a CDN that
    // ignores `Range` entirely still answers it with `200` and a body, so a
    // zero-offset probe passes against a drive that cannot be segmented at all.
    // Asking for a non-zero offset is what makes the answer meaningful — the
    // drive has to return `206` (or the provider has to say the range was
    // ignored) for the probe to prove anything.
    const PROBE_BYTES: u64 = 64 * 1024;
    let start = if size > PROBE_BYTES * 2 {
        size / 2
    } else {
        0
    };
    let probe = Segment {
        start,
        end: (start + PROBE_BYTES).min(size.max(PROBE_BYTES)),
    };
    let mut delay = Duration::from_millis(400);
    for attempt in 0..3 {
        if token.is_cancelled() {
            return Err(AppError::new("已取消"));
        }
        match source.open(probe.start, Some(probe.end)).await {
            crate::providers::RangeResult::Body(_) => return Ok(true),
            crate::providers::RangeResult::Expired => {
                // A share url that died between the listing and the first
                // request is not a dead download: the source has already
                // dropped its cache, so the next attempt mints a fresh one.
                if attempt == 2 {
                    return Err(AppError::new("下载链接已失效"));
                }
                crate::ylog!("download: preflight link expired, minting a fresh one");
                tokio::time::sleep(delay).await;
                delay *= 3;
            }
            crate::providers::RangeResult::RangeIgnored => return Ok(false),
            crate::providers::RangeResult::StartMismatch { .. } => {
                // A node that answers from the wrong offset cannot be trusted
                // with segments; one stream is the safe answer.
                return Ok(false);
            }
            crate::providers::RangeResult::Failed(err) => {
                if attempt == 2 {
                    return Err(err);
                }
                crate::ylog!("download: preflight failed ({err}), retrying");
                tokio::time::sleep(delay).await;
                delay *= 3;
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_budget_scales_with_the_file() {
        // Nothing measured: a size-derived guess, floor for small files.
        assert_eq!(refresh_budget(8 * 1024 * 1024, 0.0), 5);
        assert_eq!(refresh_budget(64 * 1024 * 1024 * 1024, 0.0), 120);

        // A slow download needs many more refreshes than a fast one of the same
        // file: the url expires on a clock, not on a byte count.
        let size = 9 * 1024 * 1024 * 1024u64;
        let slow = refresh_budget(size, 200.0 * 1024.0);
        let fast = refresh_budget(size, 20.0 * 1024.0 * 1024.0);
        assert!(slow > fast, "slow {slow} should exceed fast {fast}");
        assert!(slow > 100, "9.5 GB at 200 KB/s needs well over 100: {slow}");
        // Never below the floor, never above the cap.
        assert_eq!(refresh_budget(1024 * 1024, 1e12), 5);
        assert_eq!(refresh_budget(u64::MAX / 2, 1.0), 400);
    }

    #[test]
    fn a_preallocated_partial_is_not_mistaken_for_a_finished_one() {
        let dir = std::env::temp_dir().join(format!("yy-resume-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let part = dir.join("movie.part");
        let size = 8 * 1024 * 1024u64;
        let identity = "quark:quark|fid-1";

        // No sidecar at all: the old behaviour, length is the truth.
        std::fs::write(&part, vec![0u8; size as usize]).unwrap();
        assert_eq!(resume_state(&part, size, identity), ResumeState::None);

        // A plan with work left: the file is full length but sparse, so the
        // caller must not treat it as done.
        let mut progress = meta::Progress::with_source(
            size,
            plan::split(size, 32),
            identity.to_string(),
        );
        progress.save(&part).unwrap();
        assert_eq!(resume_state(&part, size, identity), ResumeState::Partial);

        // Every segment marked: only the rename is missing.
        let count = progress.segments.len();
        for index in 0..count {
            progress.mark(index);
        }
        progress.save(&part).unwrap();
        assert_eq!(resume_state(&part, size, identity), ResumeState::Finished);

        // The same length belonging to a different file says nothing about this
        // one. Two drives can hand out a file with the same name and the same
        // size, and the sidecar is named after the target, not the source.
        assert_eq!(
            resume_state(&part, size, "yun139:yun139|fid-2"),
            ResumeState::None
        );

        // A plan for a different file says nothing about this one.
        let mut other = meta::Progress::new(size * 2, plan::split(size * 2, 32));
        other.save(&part).unwrap();
        assert_eq!(resume_state(&part, size, identity), ResumeState::None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_connection_registry_balances_and_releases() {
        let profile = budget::Profile::QUARK;
        let big = 1024 * 1024 * 1024u64;
        let small = 64 * 1024 * 1024u64;

        // Alone, a file takes the whole budget.
        let (a, alone) = enter("quark", big, profile);
        assert_eq!(alone, profile.connections_for(big));
        leave("quark", a);

        // Two files share it, and the larger one gets the larger share.
        let (a, big_share) = enter("quark", big, profile);
        let (b, small_share) = enter("quark", small, profile);
        assert!(big_share > small_share);
        assert!(big_share + small_share <= profile.ceiling * 2);
        leave("quark", a);
        leave("quark", b);

        // After both leave, a new file gets the whole budget again — a leaked
        // entry would permanently shrink it.
        let (c, again) = enter("quark", big, profile);
        assert_eq!(again, profile.connections_for(big));
        leave("quark", c);
    }

    #[test]
    fn another_drives_files_do_not_share_the_budget() {
        let profile = budget::Profile::QUARK;
        let size = 1024 * 1024 * 1024u64;
        let (a, _) = enter("quark", size, profile);
        // Aliyun has its own per-IP budget, so a Quark download must not shrink it.
        let (b, aliyun_share) = enter("aliyun", size, profile);
        assert_eq!(aliyun_share, profile.connections_for(size));
        leave("quark", a);
        leave("aliyun", b);
    }

    #[test]
    fn small_files_are_not_segmented() {
        // The guard lives in `download`; this pins the constant it uses.
        assert_eq!(MIN_SEGMENTED, 8 * 1024 * 1024);
    }

    #[test]
    fn a_missing_part_file_is_not_resumed() {
        let progress = meta::Progress::new(20, vec![Segment { start: 0, end: 20 }]);
        let missing = std::env::temp_dir().join("yyengine-not-there.part");
        assert!(!reused_is_partial(&missing, &progress));
    }

    #[test]
    fn a_truncated_part_file_is_not_resumed() {
        let dir = std::env::temp_dir().join(format!("yyengine-short-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("short.part");
        std::fs::write(&part, b"short").unwrap();
        let mut progress = meta::Progress::new(20, vec![Segment { start: 0, end: 20 }]);
        progress.mark(0);
        // The sidecar says the file is 20 bytes but it is 5: discard, restart.
        assert!(!reused_is_partial(&part, &progress));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_complete_part_file_is_resumed() {
        let dir = std::env::temp_dir().join(format!("yyengine-full-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("full.part");
        std::fs::write(&part, b"0123456789").unwrap();
        let mut progress = meta::Progress::new(10, vec![Segment { start: 0, end: 10 }]);
        progress.mark(0);
        assert!(reused_is_partial(&part, &progress));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod resume_bytes_tests {
    use super::source::Source as _;
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    struct Fake {
        size: u64,
        delay_ms: u64,
    }

    struct Chan(tokio::sync::mpsc::Receiver<Result<bytes::Bytes, crate::error::AppError>>);

    impl futures_util::Stream for Chan {
        type Item = Result<bytes::Bytes, crate::error::AppError>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }

    fn byte_at(i: u64) -> u8 {
        (i % 251) as u8
    }

    #[async_trait::async_trait]
    impl source::Source for Fake {
        async fn open(&self, start: u64, end: Option<u64>) -> crate::providers::RangeResult {
            let last = end.unwrap_or(self.size);
            let size = self.size;
            let delay = self.delay_ms;
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, crate::error::AppError>>(4);
            tokio::spawn(async move {
                let mut cursor = start;
                while cursor < last {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    let take = (last - cursor).min(32 * 1024);
                    let chunk: Vec<u8> = (cursor..cursor + take).map(byte_at).collect();
                    cursor += take;
                    if tx.send(Ok(bytes::Bytes::from(chunk))).await.is_err() {
                        return;
                    }
                }
            });
            crate::providers::RangeResult::body(
                crate::providers::DownloadHandle {
                    size,
                    md5: None,
                    stream: Box::new(Chan(rx)),
                },
                start,
                Some(&format!("bytes {start}-{}/{size}", last.max(start + 1) - 1)),
            )
        }

        fn label(&self) -> &str {
            "fake"
        }

        fn fingerprint(&self) -> String {
            format!("fake|{}", self.size)
        }
    }

    async fn one_round(
        src: Arc<Fake>,
        part: &std::path::Path,
        size: u64,
        done: Arc<std::sync::atomic::AtomicU64>,
        stop_at: Option<u64>,
        connections: usize,
    ) -> crate::error::AppResult<bool> {
        let token = tokio_util::sync::CancellationToken::new();
        let state = meta::Progress::load(part)
            .filter(|plan| plan.belongs_to(size, &src.fingerprint()));
        let plan = match state {
            Some(saved) if super::reused_is_partial(part, &saved) => saved,
            _ => {
                meta::Progress::discard(part);
                meta::Progress::with_source(
                    size,
                    plan::split(size, connections),
                    src.fingerprint(),
                )
            }
        };
        if let Some(limit) = stop_at {
            let token = token.clone();
            let done = Arc::clone(&done);
            tokio::spawn(async move {
                loop {
                    if done.load(Ordering::Relaxed) >= limit {
                        token.cancel();
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
            });
        }
        super::run_shared(
            src,
            part,
            size,
            done,
            0,
            token,
            budget::Profile::QUARK,
            connections,
            plan,
            None,
            None,
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_paused_download_resumes_without_counting_a_byte_twice() {
        let dir = std::env::temp_dir().join(format!("yy-resume-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("big.part");
        let size = 24 * 1024 * 1024u64;

        for connections in [1usize, 4] {
            let _ = std::fs::remove_file(&part);
            meta::Progress::discard(&part);
            let src = Arc::new(Fake { size, delay_ms: 3 });
            let done = Arc::new(std::sync::atomic::AtomicU64::new(0));

            // Pause at roughly a third of the file.
            let first = one_round(Arc::clone(&src), &part, size, Arc::clone(&done), Some(size / 3), connections).await;
            let after_pause = super::plan_bytes(&part);
            assert!(first.is_err() || first.as_ref().is_ok_and(|v| !v), "round 1 should stop early: {first:?}");
            assert!(after_pause > 0, "connections={connections}: nothing recorded");
            assert!(after_pause <= size / 2 + 1024 * 1024, "connections={connections}: plan claims {after_pause} after pausing near 1/3");

            // Continue: the counter must never exceed the file.
            let resumed = one_round(Arc::clone(&src), &part, size, Arc::clone(&done), None, connections).await;
            assert!(resumed.as_ref().is_ok_and(|v| *v), "connections={connections}: resume failed {resumed:?}");
            let total = done.load(Ordering::Relaxed);
            assert!(total <= size, "connections={connections}: counted {total} for a {size} byte file");

            // Every byte on disk must be the right one.
            let bytes = std::fs::read(&part).unwrap();
            assert_eq!(bytes.len() as u64, size);
            let bad = bytes.iter().enumerate().position(|(i, b)| *b != byte_at(i as u64));
            assert!(bad.is_none(), "connections={connections}: corrupt at {bad:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Live resume probe against the account this machine is signed into.
///
/// `cargo test --lib live_resume_probe -- --ignored --nocapture`
///
/// Uploads one ~84 MiB file into a throwaway folder, pauses a segmented
/// download at ~20 MiB, prints the three counters, resumes it while sampling
/// `done` / `plan_bytes`, checks every byte, then deletes the folder and the
/// local files. Nothing outside `yunyu-resume-probe-<pid>` and /tmp is touched.
#[cfg(test)]
mod live_resume_probe {
    use super::source::ProviderSource;
    use super::*;
    use crate::error::AppError;
    use crate::providers::{Cloud189Provider, CounterSink, DriveProvider, ProviderId, UploadSink};
    use std::io::{Read, Write};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    const SIZE: u64 = 84 * 1024 * 1024;
    const PAUSE_AT: u64 = 20 * 1024 * 1024;
    const SECOND_PAUSE: u64 = 45 * 1024 * 1024;
    const BUF: usize = 1024 * 1024;

    fn byte_at(i: u64) -> u8 {
        (i % 251) as u8
    }

    fn write_payload(path: &Path, size: u64) -> AppResult<()> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut buf = vec![0u8; BUF];
        let mut at = 0u64;
        while at < size {
            let take = ((size - at) as usize).min(BUF);
            for (k, slot) in buf[..take].iter_mut().enumerate() {
                *slot = byte_at(at + k as u64);
            }
            out.write_all(&buf[..take])?;
            at += take as u64;
        }
        out.flush()?;
        Ok(())
    }

    /// Bytes that are actually non-zero: the engine preallocates, so the file
    /// length alone cannot tell written data from holes.
    fn count_nonzero(path: &Path) -> AppResult<u64> {
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; BUF];
        let mut count = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                return Ok(count);
            }
            count += buf[..n].iter().filter(|b| **b != 0).count() as u64;
        }
    }

    /// Full sequential compare against `byte_at`; returns the number of
    /// mismatched bytes (0 = identical).
    fn count_mismatches(path: &Path, size: u64) -> AppResult<u64> {
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; BUF];
        let mut at = 0u64;
        let mut bad = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            for (k, got) in buf[..n].iter().enumerate() {
                if *got != byte_at(at + k as u64) {
                    if bad == 0 {
                        println!("[verify] first mismatch at byte {}", at + k as u64);
                    }
                    bad += 1;
                }
            }
            at += n as u64;
        }
        if at != size {
            println!("[verify] length {} != expected {size}", at);
            return Ok(bad.max(1));
        }
        Ok(bad)
    }

    fn spawn_cancel_at(done: Arc<AtomicU64>, token: CancellationToken, at: u64) {
        tokio::spawn(async move {
            loop {
                if done.load(Ordering::Relaxed) >= at {
                    token.cancel();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn engine_run(
        drive: Arc<dyn DriveProvider>,
        source: Arc<dyn source::Source>,
        part: &Path,
        size: u64,
        provider: &str,
        done: Arc<AtomicU64>,
        pause_at: Option<u64>,
        secs: u64,
    ) -> Result<AppResult<bool>, tokio::time::error::Elapsed> {
        let token = CancellationToken::new();
        if let Some(at) = pause_at {
            spawn_cancel_at(Arc::clone(&done), token.clone(), at);
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(secs),
            super::download_with_stats(drive, source, provider, part, size, done, 0, token, None, None),
        )
        .await
    }

    /// The three numbers that decide which layer miscounts.
    fn report(
        tag: &str,
        part: &Path,
        size: u64,
        done: u64,
        source: &Arc<dyn source::Source>,
    ) -> AppResult<()> {
        let plan = super::plan_bytes(part);
        let len = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        let nonzero = if part.exists() { count_nonzero(part)? } else { 0 };
        let fingerprint = source.fingerprint();
        let state = super::resume_state(part, size, &fingerprint);
        println!(
            "[{tag}] plan_bytes={plan} file_len={len} done={done} nonzero_bytes={nonzero} resume_state={state:?} fingerprint={fingerprint}"
        );
        let side = meta::Progress::sidecar(part);
        match std::fs::read_to_string(&side) {
            Ok(raw) => println!(
                "[{tag}] sidecar={} len={} head={}",
                side.display(),
                raw.len(),
                &raw[..raw.len().min(200)]
            ),
            Err(err) => println!("[{tag}] sidecar={} unreadable: {err}", side.display()),
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn start_sampler(
        done: Arc<AtomicU64>,
        part: &Path,
        initial: (u64, u64),
    ) -> (
        Arc<Mutex<Vec<(u64, u64)>>>,
        Arc<AtomicBool>,
        tokio::task::JoinHandle<()>,
    ) {
        let samples: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(vec![initial]));
        let stop = Arc::new(AtomicBool::new(false));
        let part = part.to_path_buf();
        let handle = {
            let done = Arc::clone(&done);
            let samples = Arc::clone(&samples);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let point = (done.load(Ordering::Relaxed), super::plan_bytes(&part));
                    samples.lock().unwrap().push(point);
                }
            })
        };
        (samples, stop, handle)
    }

    async fn stop_sampler(stop: Arc<AtomicBool>, handle: tokio::task::JoinHandle<()>) {
        stop.store(true, Ordering::Relaxed);
        let _ = handle.await;
    }

    fn print_samples(tag: &str, snaps: &[(u64, u64)]) {
        let mut points: Vec<(u64, u64)> = Vec::new();
        for (done, plan) in snaps {
            if points.last().is_none_or(|(d, p)| d != done || p != plan) {
                points.push((*done, *plan));
            }
        }
        println!("[{tag} samples] {} distinct (done, plan_bytes) points", points.len());
        let head = 20.min(points.len());
        let tail = if points.len() > 40 { 20 } else { 0 };
        for (done, plan) in &points[..head] {
            println!("[{tag} samples] done={done} plan={plan}");
        }
        if tail > 0 {
            println!("[{tag} samples] ... {} points omitted ...", points.len() - head - tail);
            for (done, plan) in &points[points.len() - tail..] {
                println!("[{tag} samples] done={done} plan={plan}");
            }
        }
    }

    async fn probe(
        drive: Arc<dyn DriveProvider>,
        folder: &str,
        local: &Path,
        part: &Path,
    ) -> AppResult<()> {
        let provider = ProviderId::Cloud189.as_str();
        let name = "probe-84m.bin";

        let quota = drive.quota().await?;
        println!("[probe] provider={provider} quota={quota:?}");

        write_payload(local, SIZE)?;
        println!(
            "[probe] local payload {} -> {} bytes",
            local.display(),
            std::fs::metadata(local)?.len()
        );

        let uploaded = Arc::new(AtomicU64::new(0));
        let sink: Arc<dyn UploadSink> = Arc::new(CounterSink::new(Arc::clone(&uploaded)));
        let up = tokio::time::timeout(
            std::time::Duration::from_secs(1800),
            drive.upload(local, folder, name, sink, CancellationToken::new()),
        )
        .await
        .map_err(|_| AppError::new("上传超时（1800s）"))??;
        println!("[probe] upload reported bytes={}", uploaded.load(Ordering::Relaxed));

        let fid = match up {
            Some(fid) => fid,
            None => {
                let listing = drive.list(folder).await?;
                listing
                    .entries
                    .iter()
                    .find(|e| e.name == name)
                    .map(|e| e.fid.clone())
                    .ok_or_else(|| AppError::new("上传后在目录里找不到测试文件"))?
            }
        };
        let listing = drive.list(folder).await?;
        let entry = listing
            .entries
            .iter()
            .find(|e| e.fid == fid)
            .ok_or_else(|| AppError::new("目录里没有测试文件"))?;
        let size = entry.size;
        println!("[probe] uploaded fid={fid} size={size} (local {SIZE})");

        let source: Arc<dyn source::Source> = Arc::new(ProviderSource {
            drive: Arc::clone(&drive),
            fid: fid.clone(),
            label: part.display().to_string(),
        });

        // --- run A: pause near 20 MiB -------------------------------------
        let done = Arc::new(AtomicU64::new(0));
        let first = engine_run(
            Arc::clone(&drive),
            Arc::clone(&source),
            part,
            size,
            provider,
            Arc::clone(&done),
            Some(PAUSE_AT),
            900,
        )
        .await;
        println!("[pause1] run -> {first:?}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        report("pause1", part, size, done.load(Ordering::Relaxed), &source)?;

        // --- run B: resume, pause again near 45 MiB -----------------------
        let done_b = Arc::new(AtomicU64::new(0));
        let second = engine_run(
            Arc::clone(&drive),
            Arc::clone(&source),
            part,
            size,
            provider,
            Arc::clone(&done_b),
            Some(SECOND_PAUSE),
            900,
        )
        .await;
        println!("[pause2] run -> {second:?}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        report("pause2", part, size, done_b.load(Ordering::Relaxed), &source)?;

        // --- run C: resume to the end, sampling done/plan every 500 ms ----
        let done_c = Arc::new(AtomicU64::new(0));
        let (samples, stop, sampler) = start_sampler(Arc::clone(&done_c), part, (0, super::plan_bytes(part)));
        let third = engine_run(
            Arc::clone(&drive),
            Arc::clone(&source),
            part,
            size,
            provider,
            Arc::clone(&done_c),
            None,
            1800,
        )
        .await;
        stop_sampler(stop, sampler).await;
        println!("[engine done] run -> {third:?}");
        let final_done = done_c.load(Ordering::Relaxed);
        let final_len = std::fs::metadata(part)?.len();
        let snaps = samples.lock().unwrap().clone();
        let peak = snaps.iter().map(|(d, _)| *d).max().unwrap_or(0).max(final_done);
        println!(
            "[engine done] final done={final_done} file_len={final_len} declared_size={size} peak_done={peak} overshoot={}",
            peak.saturating_sub(size)
        );
        print_samples("engine", &snaps);
        let bad = count_mismatches(part, size)?;
        println!("[engine verify] mismatched_bytes={bad} (sequential compare against b[i]=(i%251))");
        if bad != 0 {
            return Err(AppError::new(format!("内容校验失败：{bad} 字节不一致")));
        }

        // --- aggregator phase: same pause/resume through transfer.rs ------
        // `run_provider_download_for_test` is `run_provider_download` with base=0,
        // i.e. exactly what `spawn_download` calls for a provider download.
        let part2 = part.with_extension("part2");
        let final2 = part.with_extension("agg.done");
        let _ = std::fs::remove_file(&part2);
        let _ = std::fs::remove_file(&final2);
        meta::Progress::discard(&part2);

        let done_e = Arc::new(AtomicU64::new(0));
        let token_e = CancellationToken::new();
        spawn_cancel_at(Arc::clone(&done_e), token_e.clone(), PAUSE_AT);
        let agg_pause = tokio::time::timeout(
            std::time::Duration::from_secs(900),
            crate::transfer::run_provider_download_for_test(
                &drive,
                &fid,
                &part2,
                &final2,
                size,
                Arc::clone(&done_e),
                token_e,
            ),
        )
        .await;
        println!("[agg pause] run -> {agg_pause:?}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let seed = std::fs::metadata(&part2).map(|m| m.len()).unwrap_or(0);
        report("agg pause", &part2, size, done_e.load(Ordering::Relaxed), &source)?;
        println!("[agg pause] retry_download would seed transferred={seed} (transfer.rs:1391 uses metadata.len())");

        let done_f = Arc::new(AtomicU64::new(0));
        let (samples2, stop2, sampler2) =
            start_sampler(Arc::clone(&done_f), &part2, (0, super::plan_bytes(&part2)));
        let agg_resume = tokio::time::timeout(
            std::time::Duration::from_secs(1800),
            crate::transfer::run_provider_download_for_test(
                &drive,
                &fid,
                &part2,
                &final2,
                size,
                Arc::clone(&done_f),
                CancellationToken::new(),
            ),
        )
        .await;
        stop_sampler(stop2, sampler2).await;
        println!("[agg done] run -> {agg_resume:?}");
        let agg_done = done_f.load(Ordering::Relaxed);
        let agg_len = std::fs::metadata(&final2).map(|m| m.len()).unwrap_or(0);
        let snaps2 = samples2.lock().unwrap().clone();
        let agg_peak = snaps2.iter().map(|(d, _)| *d).max().unwrap_or(0).max(agg_done);
        println!(
            "[agg done] final done={agg_done} final_len={agg_len} declared_size={size} peak_done={agg_peak} overshoot={}",
            agg_peak.saturating_sub(size)
        );
        print_samples("agg", &snaps2);
        let bad2 = count_mismatches(&final2, size)?;
        println!("[agg verify] mismatched_bytes={bad2} (sequential compare against b[i]=(i%251))");
        if bad2 != 0 {
            return Err(AppError::new(format!("聚合层内容校验失败：{bad2} 字节不一致")));
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "live: uploads 84 MiB to the 189 account and resumes a paused download"]
    async fn live_resume_probe() {
        let pid = std::process::id();
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive: Arc<dyn DriveProvider> =
            Arc::new(Cloud189Provider::load(&dir).expect("cloud189.json"));
        let folder_name = format!("yunyu-resume-probe-{pid}");
        let local = std::env::temp_dir().join(format!("yunyu-resume-probe-{pid}.bin"));
        let part = std::env::temp_dir().join(format!("yunyu-resume-probe-{pid}.part"));
        let part2 = part.with_extension("part2");
        let final2 = part.with_extension("agg.done");
        let _ = std::fs::remove_file(&local);
        let _ = std::fs::remove_file(&part);
        meta::Progress::discard(&part);

        let folder = drive.make_dir("0", &folder_name).await.expect("make_dir");
        println!("[probe] scratch folder {folder_name} -> {folder}");

        let outcome = probe(Arc::clone(&drive), &folder, &local, &part).await;
        println!("[probe] outcome -> {outcome:?}");

        let removed = drive.delete(std::slice::from_ref(&folder)).await;
        println!("[cleanup] delete {folder_name} -> {removed:?}");
        let still = drive
            .list("0")
            .await
            .map(|l| l.entries.iter().any(|e| e.name == folder_name));
        println!("[cleanup] folder still listed -> {still:?}");
        for leftover in [&part, &part2, &final2] {
            meta::Progress::discard(leftover);
            let _ = std::fs::remove_file(leftover);
        }
        let _ = std::fs::remove_file(&local);
        println!(
            "[cleanup] local part exists={} part2 exists={} final2 exists={} payload exists={}",
            part.exists(),
            part2.exists(),
            final2.exists(),
            local.exists()
        );

        outcome.expect("probe failed");
    }
}
