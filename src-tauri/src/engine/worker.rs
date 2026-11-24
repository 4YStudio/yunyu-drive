//! Fetching one segment, and the plumbing segments share.
//!
//! Two things live here that the single-stream path never needed:
//!
//! * [`Sink`] — positioned writes. Segments land out of order, so the file is
//!   written by absolute offset instead of appended to. One shared file handle
//!   keeps the descriptor count flat no matter how many workers run.
//! * [`Limiter`] — a shared token bucket. The single-stream limiter in
//!   `transfer.rs` is cumulative (`written / cap` against elapsed time), which
//!   quietly stops working the moment several workers call it: each one is
//!   measured against the whole budget and each one runs at full speed. A
//!   shared virtual clock is the simplest thing that stays correct under
//!   concurrency.
//!
//! Errors are classified here rather than at the call site because only the
//! provider layer sees the HTTP status, and `AppError` is a bare string that
//! drops it. A segment that breaks mid-stream is *not* treated as an expired
//! link: the engine simply reopens it from where it stopped, which recovers
//! from plain network hiccups without minting a new URL.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::plan::Segment;
use super::source::Source;
use crate::error::{AppError, AppResult};
use crate::providers::RangeResult;

/// Why one segment attempt failed.
#[derive(Debug)]
pub enum FetchError {
    /// The drive rejected the request as expired or forbidden; a fresh link
    /// from the provider will very likely work.
    Expired,
    /// The drive answered a ranged request with the whole file, so segmented
    /// downloading cannot work against it.
    RangeIgnored,
    /// Fewer bytes arrived than the segment asked for. This is how a CDN that
    /// answers `200 OK` with a short error page is caught: the framework would
    /// otherwise call it a finished download.
    Short {
        expected: u64,
        got: u64,
    },
    Cancelled,
    /// The connection accepted the request and then delivered nothing for
    /// `READ_TIMEOUT`. Distinct from `Short`: nothing was promised and broken,
    /// the node simply went quiet, so reopening the same range is right.
    Stalled {
        got: u64,
    },
    /// The connection was dropped on purpose because it was far slower than its
    /// peers. Not a failure: the segment reopens immediately, with no backoff,
    /// because a new connection may well land on a faster CDN node — and the
    /// bytes already written are kept either way.
    Slow {
        bps: f64,
    },
    /// Anything else, including a stream that broke part-way. The caller
    /// retries the segment before treating this as fatal.
    Failed(AppError),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Expired => write!(f, "下载链接已失效"),
            FetchError::RangeIgnored => write!(f, "服务器不支持分段下载"),
            FetchError::Short { expected, got } => {
                write!(f, "分段数据不完整：期望 {expected} 字节，收到 {got} 字节")
            }
            FetchError::Cancelled => write!(f, "已取消"),
            FetchError::Stalled { got } => {
                write!(f, "连接无响应：{got} 字节后 {} 秒没有数据", READ_TIMEOUT.as_secs())
            }
            FetchError::Slow { bps } => {
                write!(f, "连接过慢（{:.0} B/s），已换新连接", bps)
            }
            FetchError::Failed(err) => write!(f, "{err}"),
        }
    }
}

impl FetchError {
    /// True when reopening the same segment (no new link) is worth a try.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            FetchError::Failed(_)
                | FetchError::Short { .. }
                | FetchError::Slow { .. }
                | FetchError::Stalled { .. }
        )
    }

    /// True when the retry should not wait: nothing went wrong, the connection
    /// was simply too slow and a fresh one is the fix.
    pub fn immediate(&self) -> bool {
        matches!(self, FetchError::Slow { .. })
    }

    /// True when the connection went quiet. These reopen at once: the range is
    /// still valid, the node just stopped talking, and waiting before asking
    /// again only extends the stall.
    pub fn silent(&self) -> bool {
        matches!(self, FetchError::Stalled { .. })
    }
}

/// Positioned writes into the shared partial file.
///
/// The handle is a blocking `std::fs::File` on purpose: `tokio::fs` offers no
/// positioned write, and a per-worker `tokio::fs::File` would cost one
/// descriptor per connection — hundreds of them. Writes are batched by the
/// caller, so the `spawn_blocking` hop happens a few times per megabyte, not
/// once per network chunk.
#[derive(Clone)]
pub struct Sink {
    file: Arc<std::fs::File>,
}

impl Sink {
    /// Opens `path` for writing and pins its length to `total`.
    ///
    /// The length is set up front so a partially written file is still a
    /// playable prefix of the final media instead of a truncated container.
    pub fn open(path: &std::path::Path, total: u64) -> AppResult<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        if file.metadata()?.len() != total {
            file.set_len(total)?;
        }
        Ok(Self {
            file: Arc::new(file),
        })
    }

    /// Writes `data` at `offset`.
    pub async fn write_at(&self, offset: u64, data: Bytes) -> AppResult<()> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || write_all_at(&file, &data, offset)).await??;
        Ok(())
    }

    /// Flushes to disk. Called once when every segment is in, before the
    /// caller renames the file into place.
    pub async fn sync(&self) -> AppResult<()> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.sync_all()).await??;
        Ok(())
    }
}

#[cfg(unix)]
fn write_all_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn write_all_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0usize;
    while written < buf.len() {
        match file.seek_write(&buf[written..], offset + written as u64)? {
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "seek_write wrote 0 bytes",
                ))
            }
            n => written += n,
        }
    }
    Ok(())
}

/// Shared bandwidth budget across all workers.
///
/// Hands out send slots from a single virtual clock: a worker that asks for
/// `n` bytes is told when its bytes are "paid for", and the clock advances by
/// `n / rate`. Because every worker draws from the same clock the aggregate
/// rate stays at the cap no matter how many of them run — which the cumulative
/// limiter cannot do.
pub struct Limiter {
    next_free: Mutex<Instant>,
}

impl Limiter {
    pub fn new() -> Self {
        Self {
            next_free: Mutex::new(Instant::now()),
        }
    }

    /// Waits until `bytes` of budget are available. A cap of `0` means
    /// unlimited and returns immediately.
    ///
    /// The clock starts at "now", so the first chunk is free — the limiter
    /// behaves like a token bucket with one chunk of burst. That keeps a single
    /// worker from being delayed before its first byte, while the aggregate
    /// rate still settles at the cap.
    pub async fn acquire(&self, bytes: u64) {
        // Read the cap every time so a settings change applies to downloads
        // already in flight.
        self.acquire_with(bytes, crate::transfer::speed_limit_bytes())
            .await;
    }

    /// `acquire` with the cap passed in. Tests use this: the setting is a
    /// process-wide static, so a test that changed it would race every other
    /// test in the binary.
    pub async fn acquire_with(&self, bytes: u64, cap: u64) {
        if cap == 0 || bytes == 0 {
            return;
        }
        let now = Instant::now();
        let wait = {
            let mut next_free = self.next_free.lock().await;
            // `max(now)` means idle time is not banked into a burst.
            let start = (*next_free).max(now);
            let cost = Duration::from_secs_f64(bytes as f64 / cap as f64);
            *next_free = start + cost;
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

/// How much is buffered before a segment is written to disk. Big enough that
/// the blocking hop is rare, small enough that progress is visible promptly.
///
/// This is an upper bound on a flush, not the only reason to flush: a segment
/// here is typically 128-256 KiB, so a segment is *smaller* than this buffer and
/// waiting for it to fill meant the progress counter only moved when a whole
/// segment finished. On a slow node that took twenty-five seconds, the UI
/// showed 0 KB/s the entire time and then jumped — reported as "the download
/// hangs at zero before it starts". See the window close in [`drain`], which
/// flushes whatever has arrived at least once per speed window.
const FLUSH_BYTES: usize = 512 * 1024;

/// How many times one segment is reopened before the attempt is called failed.
const SEGMENT_ATTEMPTS: u32 = 4;

/// How many times one segment may drop a slow connection and reopen.
///
/// Generous on purpose. Measured on one drive, a third of all connections land
/// on a node that delivers 10 KB/s while the rest do 580 KB/s, so replacing the
/// bad ones *is* the mechanism that makes the download fast: thirty-two
/// connections replaced their slow third about 160 times in ten seconds and
/// moved 2.5-6 MB/s instead of 300 KB/s. Three attempts per segment ran out
/// almost immediately and left every slow node in place for the whole download.
/// The list is still bounded, and once it is spent the threshold drops to zero
/// so the segment simply crawls to its end instead of reopening forever.
const SPEED_RECONNECTS: u32 = 80;

/// How long a connection is watched before it may be judged slow.
///
/// Short enough to catch a bad node while it is still worth replacing. The
/// spread between nodes is enormous (10 KB/s against 580 KB/s), so a healthy
/// connection finishes its whole segment inside one window and is never judged;
/// only one that is visibly crawling survives long enough to be measured.
const SPEED_WINDOW: Duration = Duration::from_millis(1500);

/// The first window closes early, for reporting only.
///
/// Progress is flushed when a window closes, so the first window decides how
/// long the transfer list sits at 0 KB/s after the user clicks download. A full
/// window there meant a second and a half of nothing even when bytes were
/// already arriving; 400 ms is enough to have a real reading. The window is not
/// used to judge the connection — 400 ms of a 20 KB/s connection looks like a
/// burst, and reconnecting on that would churn every good connection at start.
const FIRST_WINDOW: Duration = Duration::from_millis(400);
/// Age at which a connection is replaced even though it is still moving.
const SPEED_ROTATE: Duration = Duration::from_millis(2500);
/// A connection delivering this much is worth keeping past `SPEED_ROTATE`.
const ROTATE_KEEP_BPS: f64 = 1024.0 * 1024.0;

/// How much of an over-long body is read after the wanted range is complete.
///
/// Breaking out of a response early kills the connection: the pool cannot reuse
/// a stream whose body was abandoned, so the next request pays a fresh TCP and
/// TLS handshake. Measured on one of these CDNs, 32 connections looping over
/// 256 KiB ranges moved 256 KB/s when each one broke out at the range end and
/// 1130 KB/s when each read its body to the end — a 4.4x difference that has
/// nothing to do with the bytes actually wanted. Quark in particular ignores
/// the range end and keeps sending (a 256 KiB request was answered with 263 KiB
/// of body), so the tail is usually waiting in the socket and costs nothing.
///
/// What is worth reading is the tail already in flight, not however much the
/// server feels like sending. This drive answers a 256 KiB range by streaming
/// the rest of the file, so a byte budget alone is not a bound: draining 256 KiB
/// at the 10 KB/s a bad node gives costs twenty-five seconds per segment, and
/// the watchdog cannot see it because drained bytes are not progress. Measured
/// that way, 47 MB took 107 s and every segment took its full 24 s in the tail.
/// So the drain is bounded by time as well, and both bounds are small.
const DRAIN_MAX_BYTES: u64 = 64 * 1024;
/// Wall-clock bound on draining, see [`DRAIN_MAX_BYTES`].
const DRAIN_MAX_TIME: Duration = Duration::from_millis(1500);

/// How long a connection may deliver nothing before it is dropped.
///
/// A CDN that accepts a request and then goes silent leaves `stream.next()`
/// pending forever. Without a deadline that one worker never returns, its round
/// never finishes, and the entire pool waits on it — the symptom is a download
/// stuck at a few KB/s while every other connection sits idle. Fifteen seconds
/// is long enough to ride out a slow node handing over and short enough that a
/// dead one costs little; the bytes already written are kept either way.
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// First retry delay. Deliberately small: most failures are one bad node, and
/// waiting seconds before trying a different one throws away throughput.
const RETRY_BASE: Duration = Duration::from_millis(150);

/// Ceiling for the exponential retry backoff.
///
/// Never wait the full read timeout between attempts. A drive that answers
/// `503` under load turns a fixed multi-second wait into "higher concurrency is
/// slower": measured on one of these CDNs, 16 connections moved 5.0 MB/s while
/// 32 connections managed 1.74 MB/s, purely because every failed chunk slept.
const RETRY_CAP: Duration = Duration::from_secs(5);

/// `RETRY_BASE` doubled once per failed attempt, capped at `RETRY_CAP`.
///
/// `attempt` is 0-based: the first retry of a segment passes 0 and waits
/// `RETRY_BASE`.
pub fn retry_delay(attempt: u32) -> Duration {
    let steps = attempt.min(10);
    let mut delay = RETRY_BASE;
    for _ in 0..steps {
        delay *= 2;
        if delay >= RETRY_CAP {
            return RETRY_CAP;
        }
    }
    delay.min(RETRY_CAP)
}

/// What a segment worker is allowed to do beyond fetching bytes.
#[derive(Debug, Clone)]
pub struct Control {
    /// Below this many bytes per second the connection is dropped and reopened.
    /// `0.0` disables preemption entirely.
    pub slow_threshold: f64,
    /// Few segments remain, so a reconnect costs a larger share of the work.
    pub endgame: bool,
    /// Where each finished window's throughput is reported, so the round can
    /// compare connections against each other instead of against a constant.
    pub speeds: Option<std::sync::Arc<tokio::sync::Mutex<Vec<f64>>>>,
    /// Shared allowance for replacing slow connections, so a throttled drive
    /// does not lose every connection in the same second.
    pub reconnects: Option<std::sync::Arc<tokio::sync::Mutex<super::gate::SpeedReconnects>>>,
}

/// Pulls one segment into the file.
///
/// `reported` receives the bytes this call moved, so the caller can attribute
/// progress. `done` is bumped in place as bytes land.
#[allow(clippy::too_many_arguments)]
pub async fn run_segment(
    source: &Arc<dyn Source>,
    segment: Segment,
    sink: &Sink,
    limiter: &Limiter,
    done: &std::sync::atomic::AtomicU64,
    token: &CancellationToken,
    control: &Control,
) -> Result<(), FetchError> {
    let mut cursor = segment.start;
    let mut threshold = control.slow_threshold;
    let mut preemptions = 0u32;
    // Only real failures are counted here. A preempted connection has not
    // failed — it was replaced on purpose — so it must not spend a retry:
    // otherwise a segment that keeps landing on bad nodes gives up after three
    // draws while the pool still has plenty of attempts left.
    let mut failures = 0u32;

    loop {
        if token.is_cancelled() {
            return Err(FetchError::Cancelled);
        }

        let control = Control {
            slow_threshold: threshold,
            endgame: control.endgame,
            speeds: control.speeds.clone(),
            reconnects: control.reconnects.clone(),
        };
        match source.open(cursor, Some(segment.end)).await {
            RangeResult::Body(handle) => {
                match drain(
                    handle,
                    segment,
                    &mut cursor,
                    sink,
                    limiter,
                    done,
                    token,
                    &control,
                )
                .await
                {
                    Ok(()) => return Ok(()),
                    Err(FetchError::Cancelled) => return Err(FetchError::Cancelled),
                    Err(err) if err.immediate() => {
                        // A slow connection is a lottery ticket, not a failure:
                        // reopen at once rather than sleeping. Once the
                        // allowance is spent the threshold drops to zero, so
                        // the next connection is not judged at all and the
                        // segment finishes however slowly it must.
                        preemptions += 1;
                        if preemptions >= SPEED_RECONNECTS {
                            threshold = 0.0;
                        }
                        continue;
                    }
                    Err(err) if err.silent() => {
                        // A silent node is not a reason to back off either: the
                        // range is untouched and asking again is cheap. It does
                        // count as an attempt, so a range that is silent every
                        // time is handed back to the round rather than retried
                        // forever.
                        failures += 1;
                        if failures >= SEGMENT_ATTEMPTS {
                            return Err(err);
                        }
                        continue;
                    }
                    Err(err) => {
                        if !err.retryable() {
                            return Err(err);
                        }
                        failures += 1;
                        if failures >= SEGMENT_ATTEMPTS {
                            return Err(err);
                        }
                        // Exponential, not linear: a drive refusing under load
                        // must be given room to recover, but the first retry
                        // should be almost immediate. `failures - 1` so the
                        // first retry waits `RETRY_BASE`, not twice it.
                        tokio::time::sleep(retry_delay(failures - 1)).await;
                    }
                }
            }
            RangeResult::Expired => return Err(FetchError::Expired),
            RangeResult::RangeIgnored => return Err(FetchError::RangeIgnored),
            // The CDN served a different offset than asked for. Writing those
            // bytes would corrupt the file, so this attempt is abandoned; the
            // next one asks again, and a fresh url usually lands on a node that
            // honours the range.
            RangeResult::StartMismatch { asked, served } => {
                return Err(FetchError::Failed(AppError::new(format!(
                    "服务端返回的区间起点不符（请求 {asked}，服务端 {served}），已放弃本次分段"
                ))))
            }
            RangeResult::Failed(err) => {
                failures += 1;
                if failures >= SEGMENT_ATTEMPTS {
                    return Err(FetchError::Failed(err));
                }
                tokio::time::sleep(retry_delay(failures - 1)).await;
            }
        }
    }
}

/// Consumes a ranged body into the file, advancing `cursor` as it goes.
#[allow(clippy::too_many_arguments)]
async fn drain(
    handle: crate::providers::DownloadHandle,
    segment: Segment,
    cursor: &mut u64,
    sink: &Sink,
    limiter: &Limiter,
    done: &std::sync::atomic::AtomicU64,
    token: &CancellationToken,
    control: &Control,
) -> Result<(), FetchError> {
    use std::sync::atomic::Ordering;

    let mut stream = handle.stream;
    let mut buffer: Vec<u8> = Vec::with_capacity(FLUSH_BYTES);
    // `buffer` holds bytes that have been received but not yet written, so the
    // durable position is `*cursor` and the read position is
    // `*cursor + buffer.len()`. Advancing `*cursor` on arrival instead would
    // mean a failed write leaves the cursor past bytes that never reached the
    // disk: the reopen would start after the gap and the file would be exactly
    // the right length with a hole in it.
    let mut buffer_at = *cursor;
    // Throughput is judged over a window, not per chunk: a single slow chunk is
    // jitter, ten seconds of slow chunks is a bad connection.
    //
    // The window is closed on *arrival*, not on flush. A connection delivering
    // 1 KiB/s fills a 512 KiB flush buffer in eight minutes, so measuring only
    // when a flush happens would leave the worst connections unjudged for the
    // whole segment — exactly the ones preemption exists to replace.
    //
    // `tokio::time::Instant` rather than `std`: it is the same clock in
    // production and a controllable one under `start_paused` tests.
    let mut window_at = tokio::time::Instant::now();
    let mut window_bytes = 0u64;
    let mut first_window = true;
    // A connection that goes quiet must not hold the round hostage. The CDN
    // sometimes accepts a request and then stops sending (a node shedding
    // load, a truncated range): without a read deadline the worker waits
    // forever, the round never ends, and the whole pool stalls behind it —
    // measured as a 47 MB quark download crawling at 75 KB/s with thirty
    // healthy connections idle. GeZi uses the same idea (`Chunk.Timeout`
    // starts at 15 s and is only ever raised for retries).
    let mut idle_at = tokio::time::Instant::now();
    let mut received = 0u64;
    // Set once the wanted range is complete. The loop then keeps reading the
    // body to its end (up to `DRAIN_MAX_BYTES`) so the connection can go back to
    // the pool instead of being torn down mid-stream.
    let mut finishing = *cursor >= segment.end;
    let mut overflow = 0u64;
    let mut draining_at: Option<tokio::time::Instant> = None;

    /// Writes everything buffered and advances the durable position.
    ///
    /// Split out because it now happens for two reasons — the buffer is full,
    /// or the speed window closed — and both must move `*cursor` and `done`
    /// together, or the next read starts after bytes that never reached disk.
    async fn flush(
        buffer: &mut Vec<u8>,
        buffer_at: u64,
        cursor: &mut u64,
        sink: &Sink,
        limiter: &Limiter,
        done: &std::sync::atomic::AtomicU64,
    ) -> Result<(), FetchError> {
        if buffer.is_empty() {
            return Ok(());
        }
        let flushed = buffer.len() as u64;
        let payload = Bytes::from(std::mem::take(buffer));
        sink.write_at(buffer_at, payload)
            .await
            .map_err(FetchError::Failed)?;
        *cursor += flushed;
        done.fetch_add(flushed, Ordering::Relaxed);
        limiter.acquire(flushed).await;
        Ok(())
    }

    loop {
        let item = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(FetchError::Cancelled),
            _ = tokio::time::sleep_until(idle_at + READ_TIMEOUT) => {
                return Err(FetchError::Stalled { got: received });
            }
            item = stream.next() => item,
        };

        let Some(item) = item else { break };
        let chunk = item.map_err(FetchError::Failed)?;
        if chunk.is_empty() {
            continue;
        }
        idle_at = tokio::time::Instant::now();
        received += chunk.len() as u64;

        if finishing {
            let since = *draining_at.get_or_insert_with(tokio::time::Instant::now);
            overflow += chunk.len() as u64;
            if overflow >= DRAIN_MAX_BYTES || since.elapsed() >= DRAIN_MAX_TIME {
                break;
            }
            continue;
        }

        // Never write past the segment: a server that ignores the range end
        // would otherwise overwrite the neighbouring segment.
        let next = *cursor + buffer.len() as u64;
        let room = segment.end.saturating_sub(next);
        let take = (chunk.len() as u64).min(room) as usize;
        if take == 0 {
            // The range is full; the rest of this body is somebody else's bytes.
            finishing = true;
            draining_at = Some(tokio::time::Instant::now());
            overflow += chunk.len() as u64;
            continue;
        }
        if buffer.is_empty() {
            buffer_at = *cursor;
        }
        buffer.extend_from_slice(&chunk[..take]);
        // Throughput is counted on arrival — that is what the connection is
        // delivering. Progress is counted on the write below, because only
        // then are the bytes actually part of the file.
        window_bytes += take as u64;

        if buffer.len() >= FLUSH_BYTES {
            flush(&mut buffer, buffer_at, cursor, sink, limiter, done).await?;
        }

        // Judged on arrival, so a connection that is too slow to fill a flush
        // buffer is still caught inside one window instead of at the end.
        let elapsed = window_at.elapsed();
        let window = if first_window { FIRST_WINDOW } else { SPEED_WINDOW };
        if elapsed >= window {
            let bps = window_bytes as f64 / elapsed.as_secs_f64();
            window_at = tokio::time::Instant::now();
            window_bytes = 0;
            // Report what has arrived even when it is far short of a full
            // flush buffer: `done` is what the UI turns into a speed, and a
            // segment smaller than `FLUSH_BYTES` would otherwise hold its
            // bytes back until the whole segment landed.
            flush(&mut buffer, buffer_at, cursor, sink, limiter, done).await?;
            // The short first window is a report, not a verdict: it is too
            // brief to tell a burst from a healthy connection, and a
            // reconnect here would churn the whole pool at start.
            if first_window {
                first_window = false;
                continue;
            }
            if let Some(speeds) = control.speeds.as_ref() {
                speeds.lock().await.push(bps);
            }
            // A connection on this CDN delivers most of its bytes in the first
            // couple of seconds and then decays to a fraction of that. Rotating
            // on age — not only on being slow — keeps re-buying the fast part.
            let stale = elapsed >= SPEED_ROTATE && bps < ROTATE_KEEP_BPS;
            if !finishing && (stale || (control.slow_threshold > 0.0
                && bps < control.slow_threshold))
                && super::gate::worth_reconnecting(
                    bps,
                    segment.end.saturating_sub(*cursor),
                    control.endgame,
                )
            {
                // The allowance is shared: when a drive throttles, every
                // connection notices at once, and replacing them all drops
                // throughput to zero and pays a handshake for each. A refusal
                // here means "keep crawling", not "fail" — the bytes are
                // still moving and the next window may replace it.
                let allowed = match control.reconnects.as_ref() {
                    Some(governor) => governor.lock().await.acquire(Instant::now()),
                    None => true,
                };
                if allowed {
                    // Whatever is buffered belongs to this segment and is about
                    // to be fetched again by the next connection: write it so
                    // the reopen starts from the true offset.
                    flush(&mut buffer, buffer_at, cursor, sink, limiter, done).await?;
                    return Err(FetchError::Slow { bps });
                }
            }
        }

        if *cursor + buffer.len() as u64 >= segment.end {
            // Everything wanted has arrived. Drain the tail rather than break:
            // see `DRAIN_MAX_BYTES` for why the extra reads are cheaper than a
            // new handshake, and why they are capped in both bytes and time.
            finishing = true;
            draining_at = Some(tokio::time::Instant::now());
        }
    }

    flush(&mut buffer, buffer_at, cursor, sink, limiter, done).await?;

    if *cursor < segment.end {
        return Err(FetchError::Short {
            expected: segment.len(),
            got: *cursor - segment.start,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("yyworker-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_land_at_their_offsets() {
        let dir = temp_dir("offset");
        let path = dir.join("f.bin");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let sink = Sink::open(&path, 12).unwrap();
            // Deliberately out of order, the way segments actually finish.
            sink.write_at(8, Bytes::from_static(b"ijkl")).await.unwrap();
            sink.write_at(0, Bytes::from_static(b"abcdefgh"))
                .await
                .unwrap();
            sink.sync().await.unwrap();
        });
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdefghijkl");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_pins_the_length() {
        let dir = temp_dir("len");
        let path = dir.join("f.bin");
        std::fs::write(&path, b"stale content from a previous attempt").unwrap();
        let _sink = Sink::open(&path, 4).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 1 MiB/s in the bytes-per-second unit the limiter takes.
    const MIB_PER_SEC: u64 = 1024 * 1024;

    #[tokio::test]
    async fn limiter_is_unlimited_at_zero() {
        let limiter = Limiter::new();
        let started = Instant::now();
        limiter.acquire_with(50 * 1024 * 1024, 0).await;
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn limiter_paces_shared_budget() {
        // 1 MiB/s cap, 256 KiB per request. The first request is free (one
        // chunk of burst), so two concurrent requests take ~0.25s: the second
        // waits for the first to be paid for. Without a shared clock both
        // would return immediately and the cap would not exist.
        let limiter = Arc::new(Limiter::new());
        let started = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..2 {
            let limiter = Arc::clone(&limiter);
            handles.push(tokio::spawn(async move {
                limiter.acquire_with(256 * 1024, MIB_PER_SEC).await;
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200),
            "shared budget not enforced, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(900),
            "pacing overshot, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn limiter_holds_the_rate_over_time() {
        // Four sequential 256 KiB requests at 1 MiB/s: one burst, then three
        // paid for — 0.75s in total. This is the property that matters: the
        // rate, not a single hand-out.
        let limiter = Limiter::new();
        let started = Instant::now();
        for _ in 0..4 {
            limiter.acquire_with(256 * 1024, MIB_PER_SEC).await;
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(650),
            "rate not held, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1400),
            "pacing overshot, took {elapsed:?}"
        );
    }

    #[test]
    fn short_bodies_are_reported() {
        // A segment that got fewer bytes than asked must not pass as done —
        // this is the "200 OK with an error page" case.
        let err = FetchError::Short {
            expected: 1024,
            got: 60,
        };
        assert!(err.to_string().contains("不完整"));
        assert!(err.retryable());
    }

    #[test]
    fn expiry_is_not_retryable_without_a_new_link() {
        assert!(!FetchError::Expired.retryable());
        assert!(!FetchError::RangeIgnored.retryable());
        assert!(!FetchError::Cancelled.retryable());
    }

    #[test]
    fn retry_backoff_is_exponential_and_capped() {
        assert_eq!(retry_delay(0), Duration::from_millis(150));
        assert_eq!(retry_delay(1), Duration::from_millis(300));
        assert_eq!(retry_delay(2), Duration::from_millis(600));
        assert_eq!(retry_delay(3), Duration::from_millis(1200));
        assert_eq!(retry_delay(4), Duration::from_millis(2400));
        assert_eq!(retry_delay(5), Duration::from_millis(4800));
        // Never longer than the cap, however many attempts have failed.
        assert_eq!(retry_delay(6), RETRY_CAP);
        assert_eq!(retry_delay(100), RETRY_CAP);
    }

    #[test]
    fn a_slow_connection_is_retryable_but_not_a_failure() {
        let slow = FetchError::Slow { bps: 2048.0 };
        assert!(slow.retryable());
        assert!(slow.immediate(), "a slow connection reopens with no delay");
        assert!(slow.to_string().contains("过慢"));
        // A refusal is not something a retry fixes.
        assert!(!FetchError::RangeIgnored.retryable());
    }

    #[test]
    fn progress_counter_is_not_shared_between_segments() {
        // Guards the arithmetic the engine relies on when attributing bytes.
        let done = AtomicU64::new(0);
        done.fetch_add(10, Ordering::Relaxed);
        assert_eq!(done.load(Ordering::Relaxed), 10);
    }

    /// Serves one body of `chunk` repeated until the range is covered, slowly.
    struct TrickleSource {
        chunk: usize,
        pace: Duration,
    }

    #[async_trait::async_trait]
    impl super::super::source::Source for TrickleSource {
        async fn open(
            &self,
            _start: u64,
            _end: Option<u64>,
        ) -> crate::providers::RangeResult {
            let chunk = self.chunk;
            let pace = self.pace;
            let stream = futures_util::stream::unfold((), move |()| async move {
                tokio::time::sleep(pace).await;
                Some((Ok(Bytes::from(vec![7u8; chunk])), ()))
            });
            crate::providers::RangeResult::Body(crate::providers::DownloadHandle {
                size: 0,
                md5: None,
                stream: Box::new(Box::pin(stream)),
            })
        }

        fn label(&self) -> &str {
            "trickle"
        }

        fn fingerprint(&self) -> String {
            "trickle".to_string()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_trickle_is_preempted_without_filling_a_flush_buffer() {
        // 4 KiB every 200 ms is ~20 KiB/s, far below the 1 MiB/s threshold. The
        // segment is 8 MiB, so it never fills a 512 KiB flush buffer during the
        // window — the connection has to be judged on arrival, not on flush.
        //
        // `drain` is called directly rather than through `run_segment`: the
        // outer loop eventually gives up on preempting and lets the last
        // attempt crawl to the end, which is the intended behaviour but hides
        // whether the window fired at all.
        let dir = temp_dir("trickle");
        let path = dir.join("f.bin");
        let sink = Sink::open(&path, 8 * 1024 * 1024).unwrap();
        let source: Arc<dyn super::super::source::Source> = Arc::new(TrickleSource {
            chunk: 4096,
            pace: Duration::from_millis(200),
        });
        let segment = Segment {
            start: 0,
            end: 8 * 1024 * 1024,
        };
        let handle = match source.open(segment.start, Some(segment.end)).await {
            crate::providers::RangeResult::Body(handle) => handle,
            _ => panic!("expected a body"),
        };
        let done = AtomicU64::new(0);
        let token = CancellationToken::new();
        let control = Control {
            slow_threshold: 1024.0 * 1024.0,
            endgame: false,
            speeds: None,
            reconnects: None,
        };
        let mut cursor = segment.start;
        let err = drain(
            handle,
            segment,
            &mut cursor,
            &sink,
            &Limiter::new(),
            &done,
            &token,
            &control,
        )
        .await
        .expect_err("a trickling connection must be dropped");
        assert!(
            matches!(err, FetchError::Slow { .. }),
            "expected Slow, got {err:?}"
        );
        // The bytes that did arrive are on disk, so the reopen starts from the
        // true offset rather than re-fetching them.
        assert!(done.load(Ordering::Relaxed) > 0, "nothing landed");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8 * 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A source that never ends: one 64 KiB chunk every `pace`, forever.
    struct EndlessSource {
        chunk: usize,
        pace: Duration,
    }

    #[async_trait::async_trait]
    impl super::super::source::Source for EndlessSource {
        async fn open(
            &self,
            _start: u64,
            _end: Option<u64>,
        ) -> crate::providers::RangeResult {
            let chunk = self.chunk;
            let pace = self.pace;
            let stream = futures_util::stream::unfold((), move |()| async move {
                tokio::time::sleep(pace).await;
                Some((Ok(Bytes::from(vec![9u8; chunk])), ()))
            });
            crate::providers::RangeResult::Body(crate::providers::DownloadHandle {
                size: 0,
                md5: None,
                stream: Box::new(Box::pin(stream)),
            })
        }

        fn label(&self) -> &str {
            "endless"
        }

        fn fingerprint(&self) -> String {
            "endless".to_string()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_is_reported_before_the_segment_finishes() {
        // A segment is typically 128-256 KiB while a flush buffer is 512 KiB, so
        // waiting for a full buffer meant the counter only moved when the whole
        // segment landed. On a slow CDN node that is twenty-five seconds of a
        // readout stuck at 0 KB/s, which users report as "the download hangs at
        // zero before it starts".
        //
        // The source here never ends, so any non-zero reading has to come from
        // the speed-window flush inside `drain` — the tail flush never runs.
        let dir = temp_dir("early");
        let path = dir.join("f.bin");
        let total = 8 * 1024 * 1024u64;
        let sink = Sink::open(&path, total).unwrap();
        let source: Arc<dyn super::super::source::Source> = Arc::new(EndlessSource {
            // 4 KiB every 200 ms: two windows' worth is 64 KiB, an eighth of a
            // flush buffer, so nothing can be reported by a full-buffer flush.
            chunk: 4096,
            pace: Duration::from_millis(200),
        });
        let segment = Segment { start: 0, end: total };
        let handle = match source.open(segment.start, Some(segment.end)).await {
            crate::providers::RangeResult::Body(handle) => handle,
            _ => panic!("expected a body"),
        };
        let done = Arc::new(AtomicU64::new(0));
        let token = CancellationToken::new();
        let control = Control {
            // Never judge the connection: this is about reporting, not preempting.
            slow_threshold: 0.0,
            endgame: false,
            speeds: None,
            reconnects: None,
        };

        let running = {
            let done = Arc::clone(&done);
            let token = token.clone();
            tokio::spawn(async move {
                let mut cursor = segment.start;
                drain(
                    handle,
                    segment,
                    &mut cursor,
                    &sink,
                    &Limiter::new(),
                    &done,
                    &token,
                    &control,
                )
                .await
            })
        };

        // Past the short first window, but before a full speed window closes:
        // the report has to come from the early flush, not from a regular one.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let mid = done.load(Ordering::Relaxed);
        assert!(
            mid > 0,
            "progress stayed at zero while the segment was still downloading"
        );

        token.cancel();
        let outcome = running.await.unwrap();
        assert!(
            matches!(outcome, Err(FetchError::Cancelled)),
            "expected the drain to stop on cancel"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
