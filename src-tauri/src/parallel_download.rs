//! Several connections at once for one big download.
//!
//! The Rec EOS client is a 3 GB zip served from R2 through Cloudflare, which
//! doesn't cache a file that size at the edge: every byte comes from the
//! bucket, and one connection to it runs at whatever that single stream
//! manages over the player's distance to it. Players far from the bucket saw
//! a few MB/s. Asking for the file in pieces over several connections adds
//! those streams together.
//!
//! The file is cut into [`PIECE_SIZE`] pieces that [`WORKERS`] connections
//! take in order, each writing its bytes straight to their place in the
//! `.part` file. Every piece's progress is what the resume metadata records,
//! and only bytes already written count, so a pause, a dropped connection or a
//! closed launcher continues from exactly what is on disk.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::download::{
    cancel_requested, parse_content_range_start, parse_content_range_total, pause_requested,
    RESPONSE_TIMEOUT, STALL_TIMEOUT,
};

/// Connections used at once. More than this mostly adds requests: a handful
/// already fill a home connection, and the server sees one player.
pub(crate) const WORKERS: usize = 6;

/// Size of one piece. Small enough that a connection that finishes its piece
/// early picks up another rather than the last few leaving one slow stream to
/// finish alone; large enough that a request per piece costs nothing.
pub(crate) const PIECE_SIZE: u64 = 32 * 1024 * 1024;

/// Files smaller than this are fetched over one connection as before.
pub(crate) const MIN_PARALLEL_SIZE: u64 = 64 * 1024 * 1024;

/// Bytes a connection collects before writing them out.
const WRITE_BUFFER: usize = 1024 * 1024;

/// Tries for one piece in a row without it getting any further. A single
/// flaky connection is retried rather than failing the whole download.
const MAX_ATTEMPTS: u32 = 5;

/// How often the pieces look at the pause and cancel flags while waiting.
const POLL: Duration = Duration::from_millis(250);

/// One piece of the file: the next byte to fetch, and the byte after its last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub pos: u64,
    pub end: u64,
}

impl Piece {
    pub fn done(&self) -> bool {
        self.pos >= self.end
    }
}

/// A fresh download of `total` bytes, in pieces.
pub(crate) fn split(total: u64) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + PIECE_SIZE).min(total);
        pieces.push(Piece { pos: start, end });
        start = end;
    }
    pieces
}

/// Bytes still to fetch.
pub(crate) fn remaining(pieces: &[Piece]) -> u64 {
    pieces.iter().map(|p| p.end.saturating_sub(p.pos)).sum()
}

/// Whether recorded pieces describe a file of `total` bytes, each piece
/// starting where the one before it ended. Anything else is a metadata file
/// that can't be trusted to say what is on disk.
pub(crate) fn pieces_cover(pieces: &[Piece], total: u64) -> bool {
    if pieces.is_empty() || total == 0 {
        return false;
    }
    // A piece's start isn't recorded, only how far it got; the starts follow
    // from the piece size.
    let mut start = 0;
    for p in pieces {
        let end = (start + PIECE_SIZE).min(total);
        if p.end != end || p.pos < start || p.pos > p.end {
            return false;
        }
        start = end;
    }
    start == total
}

pub(crate) enum Outcome {
    Done,
    Paused,
    Cancelled,
}

pub(crate) enum FetchError {
    /// The file at the URL is no longer the one being downloaded.
    Changed,
    /// Pieces kept failing. What is on disk is good to continue from.
    Failed(String),
}

/// Live numbers for the progress readout.
pub(crate) struct Progress {
    pub downloaded: u64,
    pub session_bytes: u64,
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

struct Shared {
    http: reqwest::Client,
    url: String,
    user_agent: &'static str,
    etag: String,
    total: u64,
    file: File,
    pieces: Mutex<Vec<Piece>>,
    next: AtomicUsize,
    downloaded: AtomicU64,
    /// Set to stop every connection: a pause, a cancel, or one that failed.
    stop: AtomicBool,
}

impl Shared {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || cancel_requested() || pause_requested()
    }

    fn claim(&self) -> Option<usize> {
        let pieces = self.pieces.lock().unwrap();
        loop {
            let i = self.next.fetch_add(1, Ordering::SeqCst);
            if i >= pieces.len() {
                return None;
            }
            if !pieces[i].done() {
                return Some(i);
            }
        }
    }

    fn snapshot(&self) -> Vec<Piece> {
        self.pieces.lock().unwrap().clone()
    }
}

enum PieceEnd {
    Done,
    Stopped,
}

/// Why one try at a piece ended early: worth another try, or not.
enum TryError {
    Retry(String),
    Changed,
    Fatal(String),
}

/// Fetch the pieces still missing into `part`, which is already `total`
/// bytes long. `save` gets the pieces' progress about once a second and once
/// more at the end, for the resume metadata; `progress` gets the counts
/// several times a second.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fetch(
    http: reqwest::Client,
    url: String,
    user_agent: &'static str,
    etag: String,
    total: u64,
    part: &Path,
    pieces: Vec<Piece>,
    mut progress: impl FnMut(Progress),
    mut save: impl FnMut(&[Piece]),
) -> Result<Outcome, FetchError> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(part)
        .map_err(|e| FetchError::Failed(format!("Failed to open partial file: {e}")))?;
    let already = total - remaining(&pieces);
    let shared = Arc::new(Shared {
        http,
        url,
        user_agent,
        etag,
        total,
        file,
        pieces: Mutex::new(pieces),
        next: AtomicUsize::new(0),
        downloaded: AtomicU64::new(already),
        stop: AtomicBool::new(false),
    });

    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..WORKERS {
        let shared = shared.clone();
        workers.spawn(async move { worker(&shared).await });
    }

    let mut failure: Option<FetchError> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut last_save = Instant::now();
    loop {
        tokio::select! {
            joined = workers.join_next() => {
                let Some(joined) = joined else { break };
                let result = joined.unwrap_or_else(|e| Err(TryError::Fatal(format!("A download task failed: {e}"))));
                if let Err(e) = result {
                    shared.stop.store(true, Ordering::SeqCst);
                    // A changed file wins over any other failure: what's on
                    // disk is then not worth continuing from.
                    let e = match e {
                        TryError::Changed => FetchError::Changed,
                        TryError::Retry(m) | TryError::Fatal(m) => FetchError::Failed(m),
                    };
                    if !matches!(failure, Some(FetchError::Changed)) {
                        failure = Some(e);
                    }
                }
            }
            _ = tick.tick() => {
                let downloaded = shared.downloaded.load(Ordering::SeqCst);
                progress(Progress { downloaded, session_bytes: downloaded - already });
                if last_save.elapsed() >= Duration::from_secs(1) {
                    last_save = Instant::now();
                    save(&shared.snapshot());
                }
            }
        }
    }

    let pieces = shared.snapshot();
    save(&pieces);
    if cancel_requested() {
        return Ok(Outcome::Cancelled);
    }
    if let Some(e) = failure {
        return Err(e);
    }
    if pause_requested() {
        return Ok(Outcome::Paused);
    }
    if remaining(&pieces) > 0 {
        return Err(FetchError::Failed("The download ended early.".into()));
    }
    Ok(Outcome::Done)
}

async fn worker(shared: &Shared) -> Result<(), TryError> {
    while let Some(i) = shared.claim() {
        if shared.stopping() {
            return Ok(());
        }
        match fetch_piece(shared, i).await? {
            PieceEnd::Done => {}
            PieceEnd::Stopped => return Ok(()),
        }
    }
    Ok(())
}

async fn fetch_piece(shared: &Shared, i: usize) -> Result<PieceEnd, TryError> {
    let mut attempts = 0;
    loop {
        let before = shared.pieces.lock().unwrap()[i].pos;
        match try_piece(shared, i).await {
            Ok(end) => return Ok(end),
            Err(TryError::Retry(msg)) => {
                let after = shared.pieces.lock().unwrap()[i].pos;
                if after > before {
                    attempts = 0;
                }
                attempts += 1;
                if attempts >= MAX_ATTEMPTS {
                    return Err(TryError::Retry(msg));
                }
                // Waited in slices, so Pause and Cancel still answer at once.
                let until = Instant::now() + Duration::from_secs(attempts as u64);
                while Instant::now() < until {
                    if shared.stopping() {
                        return Ok(PieceEnd::Stopped);
                    }
                    tokio::time::sleep(POLL).await;
                }
            }
            Err(e) => return Err(e),
        }
    }
}

async fn try_piece(shared: &Shared, i: usize) -> Result<PieceEnd, TryError> {
    let Piece { pos: start, end } = shared.pieces.lock().unwrap()[i];
    if start >= end {
        return Ok(PieceEnd::Done);
    }
    let req = shared
        .http
        .get(&shared.url)
        .header("User-Agent", shared.user_agent)
        .header(reqwest::header::RANGE, format!("bytes={}-{}", start, end - 1))
        // The server sends the piece only while the file still has this ETag,
        // and the whole file otherwise, so two builds are never spliced.
        .header(reqwest::header::IF_RANGE, shared.etag.clone());
    let response = match tokio::time::timeout(RESPONSE_TIMEOUT, req.send()).await {
        Err(_) => return Err(TryError::Retry("The download server did not respond.".into())),
        Ok(Err(e)) => return Err(TryError::Retry(format!("Download request failed: {e}"))),
        Ok(Ok(r)) => r,
    };
    let status = response.status();
    if status == reqwest::StatusCode::OK || status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        return Err(TryError::Changed);
    }
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(TryError::Retry(format!("HTTP error: {status}")));
    }
    if parse_content_range_start(response.headers()) != Some(start)
        || parse_content_range_total(response.headers()) != Some(shared.total)
    {
        return Err(TryError::Changed);
    }

    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(WRITE_BUFFER);
    // Where `buf` goes in the file: everything before it is written.
    let mut written = start;
    let mut last_data = Instant::now();

    // Write out what's collected, then record it as fetched.
    let flush = |buf: &mut Vec<u8>, written: &mut u64| -> Result<(), TryError> {
        if buf.is_empty() {
            return Ok(());
        }
        write_all_at(&shared.file, buf, *written)
            .map_err(|e| TryError::Fatal(format!("Failed to write chunk: {e}")))?;
        *written += buf.len() as u64;
        buf.clear();
        shared.pieces.lock().unwrap()[i].pos = *written;
        Ok(())
    };

    loop {
        if shared.stopping() {
            flush(&mut buf, &mut written)?;
            return Ok(PieceEnd::Stopped);
        }
        let next = match tokio::time::timeout(POLL, stream.next()).await {
            Err(_) => {
                if last_data.elapsed() >= STALL_TIMEOUT {
                    flush(&mut buf, &mut written)?;
                    return Err(TryError::Retry(format!(
                        "The download stalled (no data for {} seconds).",
                        STALL_TIMEOUT.as_secs()
                    )));
                }
                continue;
            }
            Ok(next) => next,
        };
        last_data = Instant::now();
        let chunk = match next {
            None => {
                flush(&mut buf, &mut written)?;
                if written >= end {
                    return Ok(PieceEnd::Done);
                }
                return Err(TryError::Retry("The connection closed early.".into()));
            }
            Some(Err(e)) => {
                flush(&mut buf, &mut written)?;
                return Err(TryError::Retry(format!("Download stream error: {e}")));
            }
            Some(Ok(c)) => c,
        };
        let room = (end - written - buf.len() as u64) as usize;
        if chunk.len() > room {
            // More than the range asked for: not a server to trust bytes from.
            return Err(TryError::Changed);
        }
        buf.extend_from_slice(&chunk);
        shared.downloaded.fetch_add(chunk.len() as u64, Ordering::SeqCst);
        if buf.len() >= WRITE_BUFFER {
            flush(&mut buf, &mut written)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_split_into_pieces_that_cover_it() {
        let total = PIECE_SIZE * 3 + 5;
        let pieces = split(total);
        assert_eq!(pieces.len(), 4);
        assert_eq!(pieces[3], Piece { pos: PIECE_SIZE * 3, end: total });
        assert!(pieces_cover(&pieces, total));
        assert_eq!(remaining(&pieces), total);
    }

    #[test]
    fn recorded_pieces_must_match_the_file() {
        let total = PIECE_SIZE * 2;
        let mut pieces = split(total);
        pieces[0].pos = PIECE_SIZE; // finished
        pieces[1].pos = PIECE_SIZE + 10; // partway
        assert!(pieces_cover(&pieces, total));
        assert_eq!(remaining(&pieces), PIECE_SIZE - 10);
        assert!(!pieces_cover(&pieces, total + 1));
        assert!(!pieces_cover(&pieces[..1], total));
        let mut bad = pieces.clone();
        bad[1].pos = 3; // before its own start
        assert!(!pieces_cover(&bad, total));
        assert!(!pieces_cover(&[], 0));
    }

    fn body(len: usize) -> Arc<Vec<u8>> {
        Arc::new((0..len).map(|i| (i.wrapping_mul(2654435761) >> 7) as u8).collect())
    }

    /// A local server for `body`. With `ranges`, it answers byte ranges, and
    /// cuts every third answer off partway so the retries are exercised;
    /// without, it ignores them and sends the whole file, like a server whose
    /// file changed (If-Range failing).
    async fn serve(body: Arc<Vec<u8>>, ranges: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let body = body.clone();
                let n = count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        let got = sock.read(&mut buf).await.unwrap_or(0);
                        if got == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..got]);
                    }
                    let req = String::from_utf8_lossy(&req).to_lowercase();
                    let range = req.lines().find_map(|l| l.strip_prefix("range: bytes=").map(str::to_string));
                    let (head, slice) = match (ranges, range) {
                        (true, Some(r)) => {
                            let (a, b) = r.trim().split_once('-').unwrap();
                            let a: usize = a.parse().unwrap();
                            let b: usize = b.parse().unwrap();
                            (
                                format!(
                                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                                    b + 1 - a, a, b, body.len()
                                ),
                                &body[a..=b],
                            )
                        }
                        _ => (
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()),
                            &body[..],
                        ),
                    };
                    let _ = sock.write_all(head.as_bytes()).await;
                    let cut = ranges && n % 3 == 1;
                    let send = if cut { &slice[..slice.len() / 2] } else { slice };
                    let _ = sock.write_all(send).await;
                });
            }
        });
        format!("http://{addr}/client")
    }

    fn temp_part(tag: &str, len: u64) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("radium-pieces-{tag}-{}", std::process::id()));
        File::create(&path).unwrap().set_len(len).unwrap();
        path
    }

    #[tokio::test]
    async fn pieces_assemble_the_file_through_dropped_connections() {
        let body = body((PIECE_SIZE * 2 + 12345) as usize);
        let url = serve(body.clone(), true).await;
        let total = body.len() as u64;
        let part = temp_part("whole", total);
        let mut saved = Vec::new();
        let outcome = fetch(
            reqwest::Client::new(),
            url,
            "test",
            "\"e\"".into(),
            total,
            &part,
            split(total),
            |_| {},
            |p| saved = p.to_vec(),
        )
        .await;
        assert!(matches!(outcome, Ok(Outcome::Done)));
        assert_eq!(remaining(&saved), 0);
        assert!(std::fs::read(&part).unwrap() == *body);
        let _ = std::fs::remove_file(&part);
    }

    #[tokio::test]
    async fn a_server_that_sends_the_whole_file_is_a_changed_file() {
        let body = body(1000);
        let url = serve(body.clone(), false).await;
        let part = temp_part("changed", 1000);
        let outcome = fetch(
            reqwest::Client::new(),
            url,
            "test",
            "\"e\"".into(),
            1000,
            &part,
            split(1000),
            |_| {},
            |_| {},
        )
        .await;
        assert!(matches!(outcome, Err(FetchError::Changed)));
        let _ = std::fs::remove_file(&part);
    }

    /// The real Rec EOS client from R2, paused partway and resumed from the
    /// saved pieces, must hash to the pin. Prints the speed.
    /// `cargo test --lib -- --ignored the_real_client_downloads_in_pieces --nocapture`
    #[tokio::test]
    #[ignore = "network, 3 GB"]
    async fn the_real_client_downloads_in_pieces() {
        let url = crate::receos::CLIENT_URL.to_string();
        let http = reqwest::Client::builder().https_only(true).no_gzip().no_brotli().build().unwrap();
        let head = http
            .get(&url)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
            .unwrap();
        let total = parse_content_range_total(head.headers()).unwrap();
        let etag = crate::download::extract_etag(head.headers()).unwrap();
        let part = temp_part("real", total);
        let ua = crate::download::download_user_agent(crate::config::Network::RecEos);

        // Pause after a few seconds.
        let started = Instant::now();
        let pauser = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(8)).await;
            crate::download::pause_download();
        });
        let mut saved = split(total);
        let first = fetch(http.clone(), url.clone(), ua, etag.clone(), total, &part, saved.clone(), |_| {}, |p| saved = p.to_vec()).await;
        pauser.await.unwrap();
        assert!(matches!(first, Ok(Outcome::Paused)));
        let after_pause = total - remaining(&saved);
        println!("paused at {} MB", after_pause / 1_000_000);
        assert!(after_pause > 0 && remaining(&saved) > 0);
        crate::download::reset_pause();

        let mut done = saved.clone();
        let second = fetch(http, url, ua, etag, total, &part, saved, |_| {}, |p| done = p.to_vec()).await;
        assert!(matches!(second, Ok(Outcome::Done)));
        let secs = started.elapsed().as_secs_f64();
        println!("{} MB in {:.1} s = {:.1} MB/s", total / 1_000_000, secs, total as f64 / 1e6 / secs);
        let hash = crate::download::sha256_file(&part).unwrap();
        let _ = std::fs::remove_file(&part);
        assert!(crate::download::digest_matches(&hash, crate::receos::CLIENT_SHA256), "{hash}");
    }
}
