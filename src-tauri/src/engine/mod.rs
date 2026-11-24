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
        Ok(meta) => meta.len() == progress.total && progress.completed() > 0,
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

    let sink = worker::Sink::open(part, size)?;
    // Bytes already on disk count towards progress from the first tick.
    let already = progress.completed();
    done.store(base + already, Ordering::Relaxed);
    progress.save(part)?;

    let limiter = Arc::new(worker::Limiter::new());
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
            // Snap progress back to what is durably complete. A worker that
            // died mid-segment already counted the bytes it had written, and
            // the next round fetches that segment again from its start —
            // leaving the counter alone would count those bytes twice and push
            // the bar past 100 %. The dip is honest: those bytes are about to
            // be fetched a second time.
            done.store(base + completed, Ordering::Relaxed);
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
                        &source, segment, &sink, &limiter, &done, &round_token, &control,
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
