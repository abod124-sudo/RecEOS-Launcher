//! "Verify game files": check the installed client against what was
//! installed, and repair the files that fail, the way Steam's "Verify
//! integrity of game files" does.
//!
//! **What the files are checked against.** A client zip already carries the
//! size and CRC-32 of every file in its central directory. Extraction records
//! that list in the client folder ([`FILES_MANIFEST`]), and a check reads every
//! file back and compares. A CRC-32 catches corruption (a bad sector, a copy
//! cut short, a file some other program rewrote); it is not a defence against
//! someone swapping files on purpose, and isn't meant as one. An install made
//! before the list existed has nothing trustworthy to be compared with — the
//! zip is deleted once it is extracted — so it has to be reinstalled once.
//!
//! **Repair** fetches only what is broken, when the server allows it: with
//! byte ranges, the zip's central directory and then each damaged file's own
//! bytes are read straight off the server ([`RangeReader`]). A server that
//! sends only the whole file (Stella's) can't be read from the middle, but it
//! can be read from the start and hung up on: the list also records where
//! each file sits in the zip, so a repair streams the zip only as far as the
//! last damaged file, inflating those as they go by ([`repair_from_stream`]),
//! and never stores the rest. A list from before positions were recorded is
//! repaired the same way, walking the zip's headers to find each file
//! ([`repair_by_scan`]). Only a zip that walk can't find its way through
//! falls back to downloading all of it (`download::download_and_repair`).
//! Either way a file is only ever replaced by the same file: its size and
//! CRC-32 in the zip being read must be the ones recorded at install. A zip
//! that has since been rebuilt is a different client, and the answer to that
//! is an update or a reinstall, not a mix of two builds.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::Emitter;

use crate::config::{self, Network};
use crate::download;
use crate::game;

/// The launcher's list of the installed client's files, in the client folder.
/// Reserved: an archive entry of this name is never extracted over it.
pub const FILES_MANIFEST: &str = ".radium-files";

const MANIFEST_VERSION: u32 = 1;

/// One file of the client as its zip described it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Relative to the client folder, `/`-separated, as [`archive_path`] spells it.
    pub path: String,
    pub size: u64,
    pub crc32: u32,
    /// Where the file sits in the zip it was installed from, for a repair
    /// that reads that zip as a stream. Absent from a list written before
    /// positions were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<ZipSpot>,
}

/// A file's place in its zip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZipSpot {
    /// Offset of its local header.
    pub header: u64,
    /// Offset of its data, past that header.
    pub data: u64,
    /// Length of its data as stored.
    pub csize: u64,
    /// How the data is stored: 0 as is, 8 deflated, anything else
    /// (`u16::MAX`) something a stream repair doesn't inflate itself.
    pub method: u16,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    files: Vec<FileRecord>,
}

/// Why a file failed its check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Problem {
    Missing,
    /// Not the size it was installed at: cut short, or overwritten.
    Size,
    /// The right size, the wrong bytes.
    Content,
    /// There, but couldn't be read — locked, or its permissions changed.
    Unreadable,
}

// ── The file list ───────────────────────────────────────────────────────────

/// An archive path in the one spelling the list uses: its components joined
/// with `/`. `None` for anything that isn't a plain relative path.
fn archive_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// A listed path as a path inside the client folder, or `None` if it could
/// reach outside it. The list sits in a folder other programs can write to,
/// and a repair writes where it says, so it is not taken on trust.
fn safe_rel_path(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for (i, part) in path.split('/').enumerate() {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains(['\\', ':'])
            || part.chars().any(|c| c.is_control())
            || (i == 0 && download::is_reserved_name(part))
        {
            return None;
        }
        out.push(part);
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Files the game, or a mod loaded into it, rewrites as it runs. Their size
/// and CRC from the zip say nothing about them afterwards, so they are left
/// out of the list — checked, they would fail every time, and a repair would
/// put a mod's settings back to how they shipped.
fn is_user_state(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".log")
        || lower.contains("bepinex/config/")
        || lower.contains("bepinex/cache/")
        // Rec EOS's settings, caches and room pictures, which the mod and its
        // in-game updater write.
        || lower.starts_with("userdata/")
}

/// Every file in `archive`, with the size and CRC-32 its central directory
/// gives. Read from that directory alone, so it costs nothing next to the
/// extraction it is recorded for. Where a name appears twice the later entry
/// wins, as it does when the archive is extracted.
pub fn record_files<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Vec<FileRecord> {
    let mut out: Vec<FileRecord> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(i) else { continue };
        if entry.is_dir() {
            continue;
        }
        let Some(path) = entry.enclosed_name().as_deref().and_then(archive_path) else { continue };
        if safe_rel_path(&path).is_none() || is_user_state(&path) {
            continue;
        }
        // `by_index_raw` has read the local header, so the data offset is known.
        let method = match entry.compression() {
            zip::CompressionMethod::Stored => 0,
            zip::CompressionMethod::Deflated => 8,
            _ => u16::MAX,
        };
        let spot = Some(ZipSpot {
            header: entry.header_start(),
            data: entry.data_start(),
            csize: entry.compressed_size(),
            method,
        });
        let record = FileRecord { path, size: entry.size(), crc32: entry.crc32(), at: spot };
        match at.get(&record.path.to_lowercase()) {
            Some(&j) => out[j] = record,
            None => {
                at.insert(record.path.to_lowercase(), out.len());
                out.push(record);
            }
        }
    }
    out
}

pub fn write_files_manifest(dir: &Path, files: &[FileRecord]) -> std::io::Result<()> {
    let manifest = Manifest { version: MANIFEST_VERSION, files: files.to_vec() };
    let text = serde_json::to_string(&manifest).map_err(std::io::Error::other)?;
    fs::write(dir.join(FILES_MANIFEST), text)
}

/// The recorded list, or `None` for an install made before there was one (or
/// a list that can't be read). Entries that could reach outside the folder
/// are dropped.
pub fn read_files_manifest(dir: &Path) -> Option<Vec<FileRecord>> {
    let text = fs::read_to_string(dir.join(FILES_MANIFEST)).ok()?;
    let manifest: Manifest = serde_json::from_str(&text).ok()?;
    if manifest.version != MANIFEST_VERSION {
        return None;
    }
    Some(manifest.files.into_iter().filter(|f| safe_rel_path(&f.path).is_some()).collect())
}

// ── Checking ────────────────────────────────────────────────────────────────

/// How far a check or repair has got, for its progress events.
struct Meter {
    files: usize,
    bytes: u64,
    started: Instant,
    last_emit: Instant,
}

const EMIT_INTERVAL: Duration = Duration::from_millis(100);

impl Meter {
    fn new(files: usize, bytes: u64) -> Self {
        Meter { files, bytes, started: Instant::now(), last_emit: download::emit_now_baseline(EMIT_INTERVAL) }
    }

    /// A `download-progress` payload for `phase`, or `None` while throttled.
    /// Progress and the ETA go by bytes: a client is a few huge files among
    /// thousands of small ones, and a count of files would crawl, then leap.
    fn event(&mut self, phase: &str, done: usize, bytes_done: u64, path: &str, force: bool) -> Option<Value> {
        if !force && self.last_emit.elapsed() < EMIT_INTERVAL {
            return None;
        }
        self.last_emit = Instant::now();
        let pct = if self.bytes > 0 { (bytes_done as f64 / self.bytes as f64 * 100.0).min(100.0) as i64 } else { 100 };
        let elapsed = self.started.elapsed().as_secs_f64();
        let eta = if bytes_done > 0 && elapsed >= 1.0 {
            (self.bytes.saturating_sub(bytes_done) as f64 / (bytes_done as f64 / elapsed)) as i64
        } else {
            -1
        };
        Some(json!({
            "phase": phase,
            "pct": pct,
            "done": done,
            "totalEntries": self.files,
            "entry": path.rsplit('/').next().unwrap_or(path),
            "eta": eta,
        }))
    }
}

/// Read one file back and judge it against its record. `on_bytes` is told of
/// each chunk read, for progress. Fails only with [`download::CANCELLED`].
fn check_file(
    path: &Path,
    record: &FileRecord,
    buf: &mut [u8],
    on_bytes: &mut dyn FnMut(u64),
) -> Result<Option<Problem>, String> {
    let meta = match fs::metadata(path) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Ok(Some(Problem::Missing)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some(Problem::Missing)),
        Err(_) => return Ok(Some(Problem::Unreadable)),
    };
    if meta.len() != record.size {
        return Ok(Some(Problem::Size));
    }
    let Ok(mut file) = fs::File::open(path) else { return Ok(Some(Problem::Unreadable)) };
    let mut hasher = crc32fast::Hasher::new();
    loop {
        if download::cancel_requested() {
            return Err(download::CANCELLED.into());
        }
        let n = match file.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(Some(Problem::Unreadable)),
        };
        hasher.update(&buf[..n]);
        on_bytes(n as u64);
    }
    Ok((hasher.finalize() != record.crc32).then_some(Problem::Content))
}

/// Check every listed file under `dir`, and return the ones that failed.
/// Fails only with [`download::CANCELLED`].
pub fn check_files(
    dir: &Path,
    files: &[FileRecord],
    progress: &mut dyn FnMut(Value),
) -> Result<Vec<(FileRecord, Problem)>, String> {
    let total: u64 = files.iter().map(|f| f.size).sum();
    let mut meter = Meter::new(files.len(), total);
    let mut buf = vec![0u8; 1024 * 1024];
    let mut damaged = Vec::new();
    let mut bytes_done = 0u64;
    for (i, record) in files.iter().enumerate() {
        let Some(rel) = safe_rel_path(&record.path) else { continue };
        let mut read = 0u64;
        let problem = check_file(&dir.join(rel), record, &mut buf, &mut |n| {
            read += n;
            if let Some(event) = meter.event("verify", i, bytes_done + read, &record.path, false) {
                progress(event);
            }
        })?;
        // A file judged without reading all of it still counts as checked.
        bytes_done += record.size;
        if let Some(problem) = problem {
            damaged.push((record.clone(), problem));
        }
        if let Some(event) = meter.event("verify", i + 1, bytes_done, &record.path, i + 1 == files.len()) {
            progress(event);
        }
    }
    Ok(damaged)
}

// ── Repairing ───────────────────────────────────────────────────────────────

/// Replace each of `damaged` under `dir` with its copy from `archive`, and
/// return how many were replaced.
///
/// Every one of them is looked up before anything is written: if the archive
/// is not the build the install came from — a file missing from it, or there
/// at another size or CRC — nothing is touched, since repairing from it would
/// leave a client made of two builds. Each file is written beside its target
/// and only moved over it once its bytes have been checked, so a repair that
/// stops partway leaves every file either as it was or whole.
pub fn repair_from_archive<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    dir: &Path,
    damaged: &[FileRecord],
    progress: &mut dyn FnMut(Value),
) -> Result<usize, String> {
    let mut index: HashMap<String, usize> = HashMap::new();
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(i) else { continue };
        if let Some(path) = entry.enclosed_name().as_deref().and_then(archive_path) {
            index.insert(path.to_lowercase(), i);
        }
    }

    let mut plan = Vec::new();
    let mut different = Vec::new();
    for record in damaged {
        let same = index.get(&record.path.to_lowercase()).copied().filter(|&i| {
            archive
                .by_index_raw(i)
                .map(|e| !e.is_dir() && e.size() == record.size && e.crc32() == record.crc32)
                .unwrap_or(false)
        });
        match (same, safe_rel_path(&record.path)) {
            (Some(i), Some(rel)) => plan.push((record, i, dir.join(rel))),
            _ => different.push(record.path.as_str()),
        }
    }
    if let Some(first) = different.first() {
        return Err(format!(
            "The client on the server is not the build you have installed: {} of the damaged \
             files are different in it (first: {}), so it can't repair them. Update or reinstall \
             the client instead.",
            different.len(),
            first
        ));
    }

    let total: u64 = plan.iter().map(|(r, _, _)| r.size).sum();
    let mut meter = Meter::new(plan.len(), total);
    let mut buf = vec![0u8; 1024 * 1024];
    let mut bytes_done = 0u64;
    for (n, (record, i, target)) in plan.iter().enumerate() {
        if download::cancel_requested() {
            return Err(download::CANCELLED.into());
        }
        if let Some(event) = meter.event("repair", n, bytes_done, &record.path, true) {
            progress(event);
        }
        let mut entry = archive
            .by_index(*i)
            .map_err(|e| format!("Couldn't read {} from the client zip: {}", record.path, e))?;
        // The zip reader checks the CRC itself at the end of the entry, which
        // surfaces as a read error; write_entry checks it again regardless.
        write_entry(
            &mut entry,
            target,
            record,
            &mut buf,
            &mut |written| {
                if let Some(event) = meter.event("repair", n, bytes_done + written, &record.path, false) {
                    progress(event);
                }
            },
            &|| format!("{} came through damaged. Try the repair again.", record.path),
        )?;
        bytes_done += record.size;
    }
    if let Some(event) = meter.event("repair", plan.len(), total, "", true) {
        progress(event);
    }
    Ok(plan.len())
}

/// Write `record`'s file from `src` over `target`, staged beside it and only
/// moved into place once its size and CRC-32 are the recorded ones.
/// `on_bytes` hears how much has been written; `mismatch` words the error
/// for bytes that turn out not to be the recorded file.
fn write_entry(
    src: &mut dyn Read,
    target: &Path,
    record: &FileRecord,
    buf: &mut [u8],
    on_bytes: &mut dyn FnMut(u64),
    mismatch: &dyn Fn() -> String,
) -> Result<(), String> {
    write_staged(target, &mut |out| {
        let mut hasher = crc32fast::Hasher::new();
        let mut written = 0u64;
        loop {
            if download::cancel_requested() {
                return Err(download::CANCELLED.into());
            }
            let got = src.read(buf).map_err(|e| read_error(&record.path, e))?;
            if got == 0 {
                break;
            }
            written += got as u64;
            if written > record.size {
                return Err(mismatch());
            }
            hasher.update(&buf[..got]);
            out.write_all(&buf[..got]).map_err(|e| format!("Couldn't write {}: {}", record.path, e))?;
            on_bytes(written);
        }
        if written != record.size || hasher.finalize() != record.crc32 {
            return Err(mismatch());
        }
        Ok(())
    })
}

/// A read that failed partway through `path`: the user's cancel, passed on
/// as itself, or the source giving out.
fn read_error(path: &str, e: std::io::Error) -> String {
    if download::cancel_requested() {
        download::CANCELLED.into()
    } else {
        format!("{} came through damaged: {}", path, e)
    }
}

/// Write a file's new bytes beside `target`, then move them over it. `fill`
/// writes the bytes and has the last word on whether they are right; on any
/// failure the staged copy is removed and `target` is left as it was.
fn write_staged(
    target: &Path,
    fill: &mut dyn FnMut(&mut std::io::BufWriter<fs::File>) -> Result<(), String>,
) -> Result<(), String> {
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let staged = target.with_file_name(format!("{}.radium-repair", name));
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Couldn't create {}: {}", parent.display(), e))?;
    }
    let result = (|| {
        let file = fs::File::create(&staged).map_err(|e| format!("Couldn't write {}: {}", staged.display(), e))?;
        let mut out = std::io::BufWriter::with_capacity(1024 * 1024, file);
        fill(&mut out)?;
        out.flush().map_err(|e| format!("Couldn't write {}: {}", staged.display(), e))?;
        drop(out);
        // A read-only file can't be replaced on Windows. The flag is the
        // user's or an old copy's, not the client's, so it goes.
        if let Ok(meta) = fs::metadata(target) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = fs::set_permissions(target, perms);
            }
        }
        fs::rename(&staged, target).map_err(|e| {
            format!("Couldn't replace {}: {}. Is something else using it?", target.display(), e)
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

// ── Reading a zip as a stream, as far as the last damaged file ─────────────

/// Whether every one of `damaged` can be repaired from a stream: its place in
/// the zip is recorded, and stored in a way this module inflates itself.
fn streamable(damaged: &[FileRecord]) -> bool {
    !damaged.is_empty() && damaged.iter().all(|r| r.at.is_some_and(|s| s.method == 0 || s.method == 8))
}

/// How far into the zip a stream repair of `damaged` reads before hanging up.
fn stream_end(damaged: &[FileRecord]) -> u64 {
    damaged.iter().filter_map(|r| r.at).map(|s| s.data + s.csize).max().unwrap_or(0)
}

/// A stream repair's answer to a zip that isn't laid out as the list says.
const DIFFERENT: &str = "The client on the server is not the build you have installed, so it can't \
                         repair from it. Update or reinstall the client instead.";

/// [`repair_by_scan`]'s answer to a zip it can't walk: the caller downloads
/// all of it instead.
pub const CANT_SCAN: &str = "The client zip can't be read one file at a time.";

/// A reader that keeps count of how far into its source it has read.
struct Counted<R> {
    inner: R,
    pos: std::rc::Rc<std::cell::Cell<u64>>,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(out)?;
        self.pos.set(self.pos.get() + n as u64);
        Ok(n)
    }
}

/// `download-progress` for a stream repair: it is a download, of a length
/// known up front — where the last damaged file ends — so even a server that
/// sends no length gets a percentage and an ETA.
struct StreamMeter {
    end: u64,
    started: Instant,
    last_emit: Instant,
}

impl StreamMeter {
    fn event(&mut self, pos: u64, force: bool) -> Option<Value> {
        if !force && self.last_emit.elapsed() < EMIT_INTERVAL {
            return None;
        }
        self.last_emit = Instant::now();
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let speed = pos as f64 / elapsed;
        let pct = if self.end > 0 { (pos as f64 / self.end as f64 * 100.0).min(99.0) as i64 } else { -1 };
        let eta = if speed > 0.0 && elapsed >= 1.0 { (self.end.saturating_sub(pos) as f64 / speed) as i64 } else { -1 };
        Some(json!({
            "phase": "download",
            "pct": pct,
            "downloaded": pos,
            "total": self.end,
            "speed": speed as u64,
            "eta": eta,
        }))
    }
}

/// Repair `damaged` from `body`, the client zip read from its first byte.
///
/// The bytes up to each damaged file are read and dropped; each file's local
/// header is checked against the list (its name, its storage and where its
/// data starts — a rebuilt zip moves things, and is caught here), and its data
/// is inflated straight into place. Reading stops after the last of them, so
/// nothing past it is downloaded. Every file written is checked against its
/// recorded size and CRC-32, so whatever this repairs before running into a
/// changed zip is still the right file.
pub fn repair_from_stream<R: Read>(
    body: R,
    dir: &Path,
    damaged: &[FileRecord],
    progress: &mut dyn FnMut(Value),
) -> Result<usize, String> {
    let mut plan = Vec::new();
    for record in damaged {
        match (record.at, safe_rel_path(&record.path)) {
            (Some(spot), Some(rel)) if spot.method == 0 || spot.method == 8 => plan.push((record, spot, dir.join(rel))),
            _ => return Err("These files can't be repaired from a stream.".into()),
        }
    }
    plan.sort_by_key(|(_, spot, _)| spot.header);

    let pos = std::rc::Rc::new(std::cell::Cell::new(0u64));
    let mut body = Counted { inner: body, pos: pos.clone() };
    let mut meter = StreamMeter {
        end: stream_end(damaged),
        started: Instant::now(),
        last_emit: download::emit_now_baseline(EMIT_INTERVAL),
    };
    let mut buf = vec![0u8; 1024 * 1024];
    let read_exact = |body: &mut Counted<R>, out: &mut [u8], path: &str| {
        body.read_exact(out).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof { DIFFERENT.to_string() } else { read_error(path, e) }
        })
    };

    for (record, spot, target) in &plan {
        // Up to this file's header, read and dropped.
        if spot.header < pos.get() {
            return Err(DIFFERENT.into());
        }
        while pos.get() < spot.header {
            if download::cancel_requested() {
                return Err(download::CANCELLED.into());
            }
            let want = ((spot.header - pos.get()) as usize).min(buf.len());
            let got = body.read(&mut buf[..want]).map_err(|e| read_error(&record.path, e))?;
            if got == 0 {
                return Err(DIFFERENT.into());
            }
            if let Some(event) = meter.event(pos.get(), false) {
                progress(event);
            }
        }

        // Its local header: is this the file the list says is here?
        let mut fixed = [0u8; 30];
        read_exact(&mut body, &mut fixed, &record.path)?;
        let le16 = |at: usize| u16::from_le_bytes([fixed[at], fixed[at + 1]]);
        if u32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]) != 0x0403_4b50 || le16(8) != spot.method {
            return Err(DIFFERENT.into());
        }
        let mut name = vec![0u8; le16(26) as usize];
        read_exact(&mut body, &mut name, &record.path)?;
        let mut extra = vec![0u8; le16(28) as usize];
        read_exact(&mut body, &mut extra, &record.path)?;
        let name = String::from_utf8_lossy(&name).replace('\\', "/");
        let same_name = archive_path(Path::new(&name)).is_some_and(|p| p.eq_ignore_ascii_case(&record.path));
        if !same_name || pos.get() != spot.data {
            return Err(DIFFERENT.into());
        }

        // Its data, inflated into place as it arrives.
        let data = (&mut body).take(spot.csize);
        let mut on_bytes = |_: u64| {
            if let Some(event) = meter.event(pos.get(), false) {
                progress(event);
            }
        };
        let mismatch = || DIFFERENT.to_string();
        let mut rest = if spot.method == 8 {
            let mut inflate = flate2::read::DeflateDecoder::new(data);
            write_entry(&mut inflate, target, record, &mut buf, &mut on_bytes, &mismatch)?;
            inflate.into_inner()
        } else {
            let mut stored = data;
            write_entry(&mut stored, target, record, &mut buf, &mut on_bytes, &mismatch)?;
            stored
        };
        // Whatever the inflater didn't need of the stored length, so the
        // count lands where the next header is expected.
        std::io::copy(&mut rest, &mut std::io::sink()).map_err(|e| read_error(&record.path, e))?;
    }
    if let Some(event) = meter.event(pos.get(), true) {
        progress(event);
    }
    Ok(plan.len())
}

/// Repair `damaged` from `body`, the client zip read from its first byte, for
/// a list that doesn't record where in the zip they are (one written before
/// positions were).
///
/// Walks the zip's local headers in order: each damaged file is written as it
/// goes by, every other is read past and dropped, and reading stops once the
/// last of them is written — as far as [`repair_from_stream`] would read, only
/// without knowing how far that is until it gets there. `size` is the whole
/// zip's length, if the server said, for the progress bar. Every file written
/// is checked against its recorded size and CRC-32. Fails with [`CANT_SCAN`]
/// at an entry whose end it can't find without the central directory, which
/// is at the far end of the zip.
pub fn repair_by_scan<R: Read>(
    body: R,
    dir: &Path,
    damaged: &[FileRecord],
    size: Option<u64>,
    progress: &mut dyn FnMut(Value),
) -> Result<usize, String> {
    use flate2::bufread::DeflateDecoder;
    let mut wanted: HashMap<String, (&FileRecord, PathBuf)> = damaged
        .iter()
        .filter_map(|r| Some((r.path.to_lowercase(), (r, dir.join(safe_rel_path(&r.path)?)))))
        .collect();
    let count = wanted.len();

    let pos = std::rc::Rc::new(std::cell::Cell::new(0u64));
    // Buffered, so an entry whose length only its deflate stream knows can be
    // inflated without reading past where it ends.
    let mut body = std::io::BufReader::with_capacity(1024 * 1024, Counted { inner: body, pos: pos.clone() });
    let mut meter = StreamMeter {
        end: size.unwrap_or(0),
        started: Instant::now(),
        last_emit: download::emit_now_baseline(EMIT_INTERVAL),
    };
    let mut buf = vec![0u8; 1024 * 1024];
    let read_full = |body: &mut std::io::BufReader<Counted<R>>, out: &mut [u8]| {
        body.read_exact(out).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof { DIFFERENT.to_string() } else { read_error("the client zip", e) }
        })
    };

    while !wanted.is_empty() {
        let mut fixed = [0u8; 30];
        read_full(&mut body, &mut fixed)?;
        let le16 = |at: usize| u16::from_le_bytes([fixed[at], fixed[at + 1]]);
        let le32 = |at: usize| u32::from_le_bytes([fixed[at], fixed[at + 1], fixed[at + 2], fixed[at + 3]]);
        if le32(0) != 0x0403_4b50 {
            // The central directory, past the last file, and some never came.
            return Err(DIFFERENT.into());
        }
        let (flags, method) = (le16(6), le16(8));
        let mut name = vec![0u8; le16(26) as usize];
        read_full(&mut body, &mut name)?;
        let mut extra = vec![0u8; le16(28) as usize];
        read_full(&mut body, &mut extra)?;
        let (csize, zip64) = local_csize(le32(18), le32(22), &extra);
        // Sizes written after the data rather than in the header.
        let descriptor = flags & 0x08 != 0;
        let name = String::from_utf8_lossy(&name).replace('\\', "/");
        let hit = archive_path(Path::new(&name)).and_then(|p| wanted.remove(&p.to_lowercase()));

        // How long its data is: as its header says, or for a stored file
        // whose header doesn't, as long as the file. A deflated one without
        // either ends where its deflate stream does.
        let len = match hit {
            _ if !descriptor => Some(csize),
            Some((record, _)) if method == 0 => Some(record.size),
            _ => None,
        };
        let readable = method == 0 || method == 8;
        if (len.is_none() && method != 8) || (hit.is_some() && (!readable || flags & 0x01 != 0)) {
            return Err(CANT_SCAN.into());
        }

        let mut on_bytes = |_: u64| {
            if let Some(event) = meter.event(pos.get(), false) {
                progress(event);
            }
        };
        match (hit, len) {
            (Some((record, target)), Some(len)) if method == 8 => {
                let mut inflate = DeflateDecoder::new((&mut body).take(len));
                write_entry(&mut inflate, &target, record, &mut buf, &mut on_bytes, &|| DIFFERENT.to_string())?;
                drain(&mut inflate.into_inner(), &mut buf, &mut on_bytes, &record.path)?;
            }
            (Some((record, target)), None) => {
                let mut inflate = DeflateDecoder::new(&mut body);
                write_entry(&mut inflate, &target, record, &mut buf, &mut on_bytes, &|| DIFFERENT.to_string())?;
            }
            (Some((record, target)), Some(len)) => {
                write_entry(&mut (&mut body).take(len), &target, record, &mut buf, &mut on_bytes, &|| DIFFERENT.to_string())?;
            }
            (None, Some(len)) => drain(&mut (&mut body).take(len), &mut buf, &mut on_bytes, &name)?,
            (None, None) => drain(&mut DeflateDecoder::new(&mut body), &mut buf, &mut on_bytes, &name)?,
        }

        if descriptor {
            // An optional signature, the CRC-32, then both sizes: 8 bytes
            // each for an entry with a zip64 field, 4 otherwise.
            let mut word = [0u8; 4];
            read_full(&mut body, &mut word)?;
            let sizes = if zip64 { 16 } else { 8 };
            let rest = if u32::from_le_bytes(word) == 0x0807_4b50 { 4 + sizes } else { sizes };
            read_full(&mut body, &mut vec![0u8; rest])?;
        }
    }
    if let Some(event) = meter.event(pos.get(), true) {
        progress(event);
    }
    Ok(count)
}

/// Read `from` to its end and drop it. `on_bytes` hears of each chunk, for
/// progress.
fn drain(from: &mut dyn Read, buf: &mut [u8], on_bytes: &mut dyn FnMut(u64), path: &str) -> Result<(), String> {
    loop {
        if download::cancel_requested() {
            return Err(download::CANCELLED.into());
        }
        match from.read(buf).map_err(|e| read_error(path, e))? {
            0 => return Ok(()),
            n => on_bytes(n as u64),
        }
    }
}

/// An entry's compressed size from its local header, from its zip64 field
/// where the header defers to one, and whether it has a zip64 field at all
/// (which widens the sizes in its data descriptor to 8 bytes).
fn local_csize(csize: u32, size: u32, extra: &[u8]) -> (u64, bool) {
    let mut at = 0;
    while at + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[at], extra[at + 1]]);
        let len = u16::from_le_bytes([extra[at + 2], extra[at + 3]]) as usize;
        let field = &extra[at + 4..(at + 4 + len).min(extra.len())];
        if id == 0x0001 {
            // The original size first, then the compressed, each there only
            // when the header's own is 0xFFFFFFFF.
            let mut values = field.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c));
            if size == u32::MAX {
                values.next();
            }
            let compressed = if csize == u32::MAX { values.next() } else { None };
            return (compressed.unwrap_or(csize as u64), true);
        }
        at += 4 + len;
    }
    (csize as u64, false)
}

/// An HTTP body as a blocking `Read`, for the repair above, which runs on
/// the blocking pool. Waits for each chunk in short slices, so a cancel is
/// seen within a quarter second even while the server is quiet, and gives up
/// on one silent for [`download::STALL_TIMEOUT`].
struct BodyReader<S, T> {
    runtime: tokio::runtime::Handle,
    stream: S,
    chunk: Option<T>,
    at: usize,
}

impl<S, T> Read for BodyReader<S, T>
where
    S: futures_util::Stream<Item = reqwest::Result<T>> + Unpin,
    T: AsRef<[u8]>,
{
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use futures_util::StreamExt;
        const POLL: Duration = Duration::from_millis(250);
        loop {
            if let Some(chunk) = &self.chunk {
                let bytes = chunk.as_ref();
                if self.at < bytes.len() {
                    let n = out.len().min(bytes.len() - self.at);
                    out[..n].copy_from_slice(&bytes[self.at..self.at + n]);
                    self.at += n;
                    return Ok(n);
                }
            }
            let quiet_since = Instant::now();
            let next = loop {
                if download::cancel_requested() {
                    return Err(std::io::Error::other(download::CANCELLED));
                }
                match self.runtime.block_on(tokio::time::timeout(POLL, self.stream.next())) {
                    Ok(next) => break next,
                    Err(_) if quiet_since.elapsed() >= download::STALL_TIMEOUT => {
                        return Err(std::io::Error::other("the download stalled"));
                    }
                    Err(_) => {}
                }
            };
            match next {
                None => return Ok(0),
                Some(Ok(chunk)) => {
                    self.chunk = Some(chunk);
                    self.at = 0;
                }
                Some(Err(e)) => return Err(std::io::Error::other(e)),
            }
        }
    }
}

/// Repair `damaged` from a server that sends only the whole zip: read it from
/// the start, as far as the last damaged file, and hang up. Where the list
/// doesn't say where that is, the zip's headers are walked to find out; a zip
/// they can't be walked through fails with [`CANT_SCAN`], before or after
/// some of the files were repaired.
async fn repair_by_stream(
    app: &tauri::AppHandle,
    network: Network,
    client_dir: &str,
    damaged: Vec<FileRecord>,
) -> Result<Value, String> {
    let _guard = download::claim_client_task()?;
    download::reset_cancel();
    if game::game_running(app) {
        return Err("Close the game before repairing its files.".into());
    }
    let url = download::resolve_client_url(app, network).await?;
    let http = range_client()?;
    let response = tokio::time::timeout(
        download::RESPONSE_TIMEOUT,
        http.get(&url).header("User-Agent", download::download_user_agent(network)).send(),
    )
    .await
    .map_err(|_| "The download server did not respond. Try again later.".to_string())?
    .map_err(|e| format!("Couldn't reach the download server: {}", e))?;
    if !response.status().is_success() {
        return Err(format!("The download server answered {}.", response.status()));
    }
    let known = streamable(&damaged);
    let size = response.content_length().filter(|n| *n > 0);
    let _ = app.emit("download-progress", json!({
        "phase": "download", "pct": 0, "downloaded": 0,
        "total": if known { stream_end(&damaged) } else { size.unwrap_or(0) }, "speed": 0, "eta": -1
    }));

    let runtime = tokio::runtime::Handle::current();
    let stream = Box::pin(response.bytes_stream());
    let repaired = {
        let app = app.clone();
        let dir = client_dir.to_string();
        tokio::task::spawn_blocking(move || {
            let body = BodyReader { runtime, stream, chunk: None, at: 0 };
            let mut progress = |progress| {
                let _ = app.emit("download-progress", progress);
            };
            if known {
                repair_from_stream(body, Path::new(&dir), &damaged, &mut progress)
            } else {
                repair_by_scan(body, Path::new(&dir), &damaged, size, &mut progress)
            }
            // Dropping the body here is what hangs up on the rest of the zip.
        })
        .await
        .map_err(|e| format!("Repair task failed: {}", e))??
    };
    let _ = app.emit("download-progress", json!({ "phase": "done", "pct": 100 }));
    Ok(json!({ "success": true, "repaired": repaired }))
}

// ── Reading a zip off the server, a range at a time ─────────────────────────

/// A remote file as `Read + Seek`, fetched a range at a time, so the zip
/// reader can open a zip on a server and read only the parts it needs: the
/// central directory at the end, then the damaged files' own bytes.
///
/// A few recent blocks are kept, since the zip reader goes back and forth
/// between the central directory at the end and each file's header. Reading
/// straight on from the end of the block just read doubles the next fetch, up
/// to [`RangeReader::MAX_FETCH`], so a large file streams in a few dozen
/// requests rather than thousands. A jump elsewhere fetches the aligned block
/// around it: the zip reader finds the end of the archive by stepping
/// backwards from the last byte, and a fetch starting at each step would miss
/// on every one of them.
pub struct RangeReader<F: FnMut(u64, u64) -> std::io::Result<Vec<u8>>> {
    fetch: F,
    len: u64,
    pos: u64,
    /// `(start, bytes)`, most recently used first.
    blocks: std::collections::VecDeque<(u64, Vec<u8>)>,
    next: u64,
}

impl<F: FnMut(u64, u64) -> std::io::Result<Vec<u8>>> RangeReader<F> {
    const MIN_FETCH: u64 = 256 * 1024;
    const MAX_FETCH: u64 = 8 * 1024 * 1024;
    const BLOCKS: usize = 3;

    /// `fetch(start, len)` returns exactly `len` bytes from `start`.
    pub fn new(len: u64, fetch: F) -> Self {
        RangeReader { fetch, len, pos: 0, blocks: Default::default(), next: Self::MIN_FETCH }
    }

    /// Bring the block holding `pos` to the front, fetching it if needed.
    fn load(&mut self) -> std::io::Result<()> {
        let pos = self.pos;
        let held = |(start, bytes): &(u64, Vec<u8>)| pos >= *start && pos < start + bytes.len() as u64;
        if let Some(i) = self.blocks.iter().position(held) {
            let block = self.blocks.remove(i).expect("found above");
            self.blocks.push_front(block);
            return Ok(());
        }
        let sequential = self.blocks.front().is_some_and(|(start, bytes)| pos == start + bytes.len() as u64);
        self.next = if sequential { (self.next * 2).min(Self::MAX_FETCH) } else { Self::MIN_FETCH };
        let start = if sequential { pos } else { pos - pos % Self::MIN_FETCH };
        let want = self.next.min(self.len - start);
        let got = (self.fetch)(start, want)?;
        if got.len() as u64 != want {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the server sent a different amount than asked for",
            ));
        }
        self.blocks.push_front((start, got));
        self.blocks.truncate(Self::BLOCKS);
        Ok(())
    }
}

impl<F: FnMut(u64, u64) -> std::io::Result<Vec<u8>>> Read for RangeReader<F> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        self.load()?;
        let (start, bytes) = self.blocks.front().expect("just loaded");
        let at = (self.pos - start) as usize;
        let n = out.len().min(bytes.len() - at);
        out[..n].copy_from_slice(&bytes[at..at + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<F: FnMut(u64, u64) -> std::io::Result<Vec<u8>>> Seek for RangeReader<F> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = target
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

/// A zip on a server that answers byte ranges.
struct RemoteZip {
    url: String,
    user_agent: &'static str,
    total: u64,
    etag: Option<String>,
}

fn range_client() -> Result<reqwest::Client, String> {
    // As the download's: https only, redirects included, and bytes as they
    // are on the wire, since that is what ranges count.
    reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .no_gzip()
        .no_brotli()
        .build()
        .map_err(|e| e.to_string())
}

/// Ask for the first byte. A 206 with the file's length means the server can
/// send single files; anything else means it can't, and the response is
/// dropped unread — for Stella's, that is the start of a 4.7 GB body.
async fn probe_ranges(http: &reqwest::Client, url: &str, user_agent: &'static str) -> Result<Option<RemoteZip>, String> {
    let response = tokio::time::timeout(
        download::RESPONSE_TIMEOUT,
        http.get(url).header("User-Agent", user_agent).header(reqwest::header::RANGE, "bytes=0-0").send(),
    )
    .await
    .map_err(|_| "The download server did not respond. Try again later.".to_string())?
    .map_err(|e| format!("Couldn't reach the download server: {}", e))?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        || download::parse_content_range_start(response.headers()) != Some(0)
    {
        if !response.status().is_success() {
            return Err(format!("The download server answered {}.", response.status()));
        }
        return Ok(None);
    }
    let Some(total) = download::parse_content_range_total(response.headers()) else { return Ok(None) };
    Ok(Some(RemoteZip { url: url.to_string(), user_agent, total, etag: download::extract_etag(response.headers()) }))
}

/// `len` bytes of `zip` from `start`. Refused unless it is that exact range of
/// the same file the probe saw: a server that stops honouring ranges would
/// otherwise start sending the whole file, and a rebuilt zip would hand over
/// bytes from a different build.
async fn fetch_range(http: &reqwest::Client, zip: &RemoteZip, start: u64, len: u64) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    let end = start + len - 1;
    let response = tokio::time::timeout(
        download::RESPONSE_TIMEOUT,
        http.get(&zip.url)
            .header("User-Agent", zip.user_agent)
            .header(reqwest::header::RANGE, format!("bytes={}-{}", start, end))
            .send(),
    )
    .await
    .map_err(|_| "The download server stopped responding.".to_string())?
    .map_err(|e| format!("Download request failed: {}", e))?;
    let headers = response.headers();
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        || download::parse_content_range_start(headers) != Some(start)
        || download::parse_content_range_total(headers) != Some(zip.total)
    {
        return Err("The download server stopped sending parts of the file.".into());
    }
    if let (Some(then), Some(now)) = (&zip.etag, download::extract_etag(headers)) {
        if *then != now {
            return Err("The client on the server changed while it was being read. Try again.".into());
        }
    }
    let mut body = Vec::with_capacity(len as usize);
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::time::timeout(download::STALL_TIMEOUT, stream.next())
            .await
            .map_err(|_| "The download stalled.".to_string())?;
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(|e| format!("Download stream error: {}", e))?;
        if body.len() + chunk.len() > len as usize {
            return Err("The download server sent more than was asked for.".into());
        }
        body.extend_from_slice(&chunk);
        if download::cancel_requested() {
            return Err(download::CANCELLED.into());
        }
    }
    Ok(body)
}

// ── Commands ────────────────────────────────────────────────────────────────

/// What the last check found, for the repair that follows it.
struct LastCheck {
    network: Network,
    client_dir: String,
    damaged: Vec<FileRecord>,
}

static LAST_CHECK: Mutex<Option<LastCheck>> = Mutex::new(None);

/// Whether `client_dir` holds an install this launcher made and listed: one
/// that can be checked, and repaired, even with its executable gone.
pub fn has_files_manifest(client_dir: &str) -> bool {
    !client_dir.is_empty() && Path::new(client_dir).join(FILES_MANIFEST).is_file()
}

/// The folder of `network`'s installed client, or why there is none to check.
///
/// An install whose executable has gone still counts, as long as its file
/// list is there: a missing exe is exactly what a check and repair are for.
fn installed_client_dir(app: &tauri::AppHandle, network: Network) -> Result<String, String> {
    let cfg = config::current(app);
    let client_dir = config::get_client_dir_for(app, &cfg, network);
    let exe = cfg.game_exe_for(network);
    let exe_there = !exe.is_empty() && Path::new(exe).exists();
    if download::install_incomplete(&client_dir) || !(exe_there || has_files_manifest(&client_dir)) {
        return Err("There's no installed client to check.".into());
    }
    Ok(client_dir)
}

/// Check every file of `network`'s installed client. Emits `download-progress`
/// events with phase `verify`. Answers `{status}`: `ok`, `damaged` (with
/// `damaged: [{path, problem}]`), `no-manifest` for an install made before
/// the list was kept, `cancelled`, or `error` with `error`.
#[tauri::command]
pub async fn verify_client(app: tauri::AppHandle, network: Option<String>) -> Value {
    let network = Network::parse(network.as_deref());
    match verify_impl(&app, network).await {
        Ok(value) => value,
        Err(e) if e == download::CANCELLED => json!({ "status": "cancelled" }),
        Err(e) => json!({ "status": "error", "error": e }),
    }
}

async fn verify_impl(app: &tauri::AppHandle, network: Network) -> Result<Value, String> {
    let _guard = download::claim_client_task()?;
    download::reset_cancel();
    if game::game_running(app) {
        return Err("Close the game before checking its files.".into());
    }
    let client_dir = installed_client_dir(app, network)?;
    let Some(files) = read_files_manifest(Path::new(&client_dir)) else {
        return Ok(json!({ "status": "no-manifest" }));
    };

    let damaged = {
        let app = app.clone();
        let dir = client_dir.clone();
        let files = files.clone();
        tokio::task::spawn_blocking(move || {
            check_files(Path::new(&dir), &files, &mut |progress| {
                let _ = app.emit("download-progress", progress);
            })
        })
        .await
        .map_err(|e| format!("File check failed: {}", e))??
    };

    let list: Vec<Value> = damaged
        .iter()
        .map(|(record, problem)| json!({ "path": record.path, "problem": problem }))
        .collect();
    let damaged_bytes: u64 = damaged.iter().map(|(record, _)| record.size).sum();
    *LAST_CHECK.lock().unwrap_or_else(|e| e.into_inner()) = Some(LastCheck {
        network,
        client_dir,
        damaged: damaged.into_iter().map(|(record, _)| record).collect(),
    });
    Ok(json!({
        "status": if list.is_empty() { "ok" } else { "damaged" },
        "checked": files.len(),
        "damaged": list,
        "damagedBytes": damaged_bytes,
    }))
}

/// Repair the files the last check of `network` found damaged.
///
/// Without `full`, only those files are fetched, when the server answers byte
/// ranges; when it doesn't, nothing is downloaded and the answer is
/// `{needsFullDownload: true, total}` (`total` in bytes, or null when the
/// server doesn't say) for the page to ask first. With `full`, the zip is read
/// from the start as far as the last damaged file (see [`repair_by_stream`]),
/// or downloaded whole where that can't be done. Answers `{success,
/// repaired}` or `{success: false, error}`.
#[tauri::command]
pub async fn repair_client(app: tauri::AppHandle, network: Option<String>, full: Option<bool>) -> Value {
    let network = Network::parse(network.as_deref());
    match repair_impl(&app, network, full.unwrap_or(false)).await {
        Ok(value) => value,
        Err(e) => json!({ "success": false, "error": e }),
    }
}

async fn repair_impl(app: &tauri::AppHandle, network: Network, full: bool) -> Result<Value, String> {
    let client_dir = installed_client_dir(app, network)?;
    let damaged = {
        let last = LAST_CHECK.lock().unwrap_or_else(|e| e.into_inner());
        match last.as_ref() {
            Some(check) if check.network == network && check.client_dir == client_dir => check.damaged.clone(),
            _ => return Err("Check the files first.".into()),
        }
    };
    if damaged.is_empty() {
        return Ok(json!({ "success": true, "repaired": 0 }));
    }

    let result = if full {
        match repair_by_stream(app, network, &client_dir, damaged.clone()).await {
            // Files it repaired before giving up are taken from the full
            // download again, as the same files.
            Err(e) if e == CANT_SCAN => download::download_and_repair(app.clone(), network, damaged).await?,
            result => result?,
        }
    } else {
        match repair_by_ranges(app, network, &client_dir, damaged.clone()).await? {
            Some(value) => value,
            None => return Ok(full_download_needed(app, network, &damaged).await),
        }
    };
    *LAST_CHECK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(result)
}

/// Fetch just the damaged files from a server that answers byte ranges.
/// `None` when it doesn't.
async fn repair_by_ranges(
    app: &tauri::AppHandle,
    network: Network,
    client_dir: &str,
    damaged: Vec<FileRecord>,
) -> Result<Option<Value>, String> {
    let _guard = download::claim_client_task()?;
    download::reset_cancel();
    if game::game_running(app) {
        return Err("Close the game before repairing its files.".into());
    }
    let url = download::resolve_client_url(app, network).await?;
    let http = range_client()?;
    let Some(remote) = probe_ranges(&http, &url, download::download_user_agent(network)).await? else {
        return Ok(None);
    };

    let runtime = tokio::runtime::Handle::current();
    let repaired = {
        let app = app.clone();
        let dir = client_dir.to_string();
        tokio::task::spawn_blocking(move || {
            let total = remote.total;
            let reader = RangeReader::new(total, |start, len| {
                runtime
                    .block_on(fetch_range(&http, &remote, start, len))
                    .map_err(std::io::Error::other)
            });
            let mut archive = zip::ZipArchive::new(reader)
                .map_err(|e| format!("Couldn't read the client zip on the server: {}", e))?;
            repair_from_archive(&mut archive, Path::new(&dir), &damaged, &mut |progress| {
                let _ = app.emit("download-progress", progress);
            })
        })
        .await
        .map_err(|e| format!("Repair task failed: {}", e))?
    };
    // A cancel inside a fetch surfaces as an I/O error wrapping it.
    let repaired = repaired.map_err(|e| if download::cancel_requested() { download::CANCELLED.to_string() } else { e })?;
    let _ = app.emit("download-progress", json!({ "phase": "done", "pct": 100 }));
    Ok(Some(json!({ "success": true, "repaired": repaired })))
}

/// The answer for a server that sends only the whole zip: how big the client
/// is, if the server says, and — when the list records where the damaged
/// files are — how much of it the repair reads before it can hang up.
async fn full_download_needed(app: &tauri::AppHandle, network: Network, damaged: &[FileRecord]) -> Value {
    let mut total = Value::Null;
    if let (Ok(url), Ok(http)) = (download::resolve_client_url(app, network).await, range_client()) {
        let head = http.head(&url).header("User-Agent", download::download_user_agent(network)).send();
        if let Ok(Ok(response)) = tokio::time::timeout(download::RESPONSE_TIMEOUT, head).await {
            if let Some(len) = response.content_length().filter(|n| *n > 0) {
                total = json!(len);
            }
        }
    }
    let needed = if streamable(damaged) { json!(stream_end(damaged)) } else { Value::Null };
    json!({ "success": false, "needsFullDownload": true, "total": total, "needed": needed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A zip of `files`, stored or deflated alternately so both paths are read.
    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            for (i, (name, data)) in files.iter().enumerate() {
                let method = if i % 2 == 0 { zip::CompressionMethod::Deflated } else { zip::CompressionMethod::Stored };
                let options = zip::write::SimpleFileOptions::default().compression_method(method);
                zip.start_file(*name, options).unwrap();
                zip.write_all(data).unwrap();
            }
            zip.add_directory("empty/", zip::write::SimpleFileOptions::default()).unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("radium-verify-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn install(dir: &Path, zip: &[u8]) -> Vec<FileRecord> {
        let mut archive = zip::ZipArchive::new(Cursor::new(zip.to_vec())).unwrap();
        archive.extract(dir).unwrap();
        let files = record_files(&mut archive);
        write_files_manifest(dir, &files).unwrap();
        read_files_manifest(dir).unwrap()
    }

    /// `n` bytes that don't compress (xorshift), so a zip of them is as big as
    /// they are and a test of how little gets fetched means something.
    fn big(n: usize) -> Vec<u8> {
        let mut x: u32 = 0x9e37_79b9;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    #[test]
    fn records_files_but_not_folders_logs_or_mod_settings() {
        let zip = zip_of(&[
            ("RecRoom.exe", b"exe"),
            ("RecRoom_Data/level0", b"level"),
            ("Player.log", b"log"),
            ("BepInEx/config/BepInEx.cfg", b"cfg"),
            ("BepInEx/plugins/mod.dll", b"mod"),
        ]);
        let mut archive = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
        let paths: Vec<String> = record_files(&mut archive).into_iter().map(|f| f.path).collect();
        assert_eq!(paths, ["RecRoom.exe", "RecRoom_Data/level0", "BepInEx/plugins/mod.dll"]);
    }

    #[test]
    fn a_listed_path_never_reaches_outside_the_folder() {
        for bad in ["../x", "a/../../x", "/x", "C:/x", "a\\..\\x", "", "a//b", ".radium-install", "./x"] {
            assert!(safe_rel_path(bad).is_none(), "{bad:?} should be refused");
        }
        assert_eq!(safe_rel_path("RecRoom_Data/level0"), Some(PathBuf::from("RecRoom_Data").join("level0")));
    }

    #[test]
    fn a_manifest_entry_that_escapes_is_dropped_on_read() {
        let dir = temp_dir("escape");
        let files = vec![
            FileRecord { path: "ok.dll".into(), size: 1, crc32: 1, at: None },
            FileRecord { path: "../evil.dll".into(), size: 1, crc32: 1, at: None },
        ];
        write_files_manifest(&dir, &files).unwrap();
        assert_eq!(read_files_manifest(&dir).unwrap(), files[..1]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_missing_resized_and_changed_files() {
        let dir = temp_dir("check");
        let zip = zip_of(&[("a.dll", b"alpha"), ("sub/b.dat", &big(300_000)), ("c.txt", b"gamma")]);
        let files = install(&dir, &zip);

        assert!(check_files(&dir, &files, &mut |_| {}).unwrap().is_empty());

        fs::remove_file(dir.join("a.dll")).unwrap();
        let mut b = fs::read(dir.join("sub/b.dat")).unwrap();
        b[123_456] ^= 0xff;
        fs::write(dir.join("sub/b.dat"), &b).unwrap();
        fs::write(dir.join("c.txt"), b"gamma and more").unwrap();

        let found: Vec<(String, Problem)> = check_files(&dir, &files, &mut |_| {})
            .unwrap()
            .into_iter()
            .map(|(f, p)| (f.path, p))
            .collect();
        assert_eq!(
            found,
            [
                ("a.dll".to_string(), Problem::Missing),
                ("sub/b.dat".to_string(), Problem::Content),
                ("c.txt".to_string(), Problem::Size),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn repair_replaces_only_the_damaged_files() {
        let dir = temp_dir("repair");
        let zip = zip_of(&[("a.dll", b"alpha"), ("sub/b.dat", &big(300_000)), ("c.txt", b"gamma")]);
        let files = install(&dir, &zip);
        fs::remove_file(dir.join("a.dll")).unwrap();
        fs::write(dir.join("sub/b.dat"), b"broken").unwrap();
        // A file nobody damaged, with a timestamp a rewrite would change.
        let untouched = fs::metadata(dir.join("c.txt")).unwrap().modified().unwrap();

        let damaged: Vec<FileRecord> = check_files(&dir, &files, &mut |_| {}).unwrap().into_iter().map(|(f, _)| f).collect();
        let mut archive = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
        assert_eq!(repair_from_archive(&mut archive, &dir, &damaged, &mut |_| {}).unwrap(), 2);

        assert!(check_files(&dir, &files, &mut |_| {}).unwrap().is_empty());
        assert_eq!(fs::metadata(dir.join("c.txt")).unwrap().modified().unwrap(), untouched);
        assert!(!dir.join("sub/b.dat.radium-repair").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_different_build_repairs_nothing() {
        let dir = temp_dir("rebuilt");
        let old = zip_of(&[("a.dll", b"alpha"), ("b.dll", b"beta")]);
        let files = install(&dir, &old);
        fs::write(dir.join("a.dll"), b"xxxxx").unwrap();
        fs::write(dir.join("b.dll"), b"yyyy").unwrap();
        let damaged: Vec<FileRecord> = check_files(&dir, &files, &mut |_| {}).unwrap().into_iter().map(|(f, _)| f).collect();

        // b.dll is the same in the new build; a.dll is not.
        let new = zip_of(&[("a.dll", b"ALPHA"), ("b.dll", b"beta")]);
        let mut archive = zip::ZipArchive::new(Cursor::new(new)).unwrap();
        let err = repair_from_archive(&mut archive, &dir, &damaged, &mut |_| {}).unwrap_err();
        assert!(err.contains("not the build you have installed"), "{err}");
        // Neither was touched, not even the one that could have been.
        assert_eq!(fs::read(dir.join("b.dll")).unwrap(), b"yyyy");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn repairs_through_ranges_fetching_little_of_the_zip() {
        let dir = temp_dir("ranges");
        let padding = big(3_000_000);
        let zip = zip_of(&[("big.bin", &padding), ("small.dll", b"small file"), ("other.bin", &big(2_000_000))]);
        let files = install(&dir, &zip);
        fs::write(dir.join("small.dll"), b"SMALL FILE").unwrap();
        let damaged: Vec<FileRecord> = check_files(&dir, &files, &mut |_| {}).unwrap().into_iter().map(|(f, _)| f).collect();
        assert_eq!(damaged.len(), 1);

        let mut fetched = 0u64;
        let reader = RangeReader::new(zip.len() as u64, |start, len| {
            fetched += len;
            Ok(zip[start as usize..(start + len) as usize].to_vec())
        });
        let mut archive = zip::ZipArchive::new(reader).unwrap();
        assert_eq!(repair_from_archive(&mut archive, &dir, &damaged, &mut |_| {}).unwrap(), 1);
        drop(archive);
        assert!(check_files(&dir, &files, &mut |_| {}).unwrap().is_empty());
        // The central directory and the one small file — not the megabytes
        // around them.
        assert!(fetched < 1_000_000, "fetched {fetched} of {}", zip.len());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The whole range path against a real CDN: probe, read the central
    /// directory off the server, and repair one file from it. A Python wheel
    /// is a plain zip, on a CDN that answers byte ranges — which no revival
    /// server this launcher knows does today.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "hits pypi.org and files.pythonhosted.org"]
    async fn repairs_a_file_from_a_real_zip_over_https_ranges() {
        let meta: Value = reqwest::get("https://pypi.org/pypi/six/1.16.0/json").await.unwrap().json().await.unwrap();
        let url = meta["urls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["packagetype"] == "bdist_wheel")
            .and_then(|u| u["url"].as_str())
            .unwrap()
            .to_string();
        let http = range_client().unwrap();
        let remote = probe_ranges(&http, &url, "Radium Launcher test").await.unwrap().expect("answers ranges");
        let dir = temp_dir("live");
        let runtime = tokio::runtime::Handle::current();
        let fetched = tokio::task::spawn_blocking({
            let dir = dir.clone();
            move || {
                let mut fetched = 0u64;
                let reader = RangeReader::new(remote.total, |start, len| {
                    fetched += len;
                    runtime.block_on(fetch_range(&http, &remote, start, len)).map_err(std::io::Error::other)
                });
                let mut archive = zip::ZipArchive::new(reader).unwrap();
                let target = record_files(&mut archive).into_iter().find(|f| f.path == "six.py").unwrap();
                assert_eq!(repair_from_archive(&mut archive, &dir, std::slice::from_ref(&target), &mut |_| {}).unwrap(), 1);
                assert!(check_files(&dir, &[target], &mut |_| {}).unwrap().is_empty());
                drop(archive);
                fetched
            }
        })
        .await
        .unwrap();
        eprintln!("fetched {fetched} bytes to repair six.py");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A reader over `data` that remembers how far it was read.
    struct Tally<'a> {
        data: &'a [u8],
        at: usize,
    }
    impl Read for Tally<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = out.len().min(self.data.len() - self.at);
            out[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    #[test]
    fn a_stream_repair_reads_only_as_far_as_the_last_damaged_file() {
        let dir = temp_dir("stream");
        // Deflated, stored, deflated (see zip_of), with megabytes after them.
        let zip = zip_of(&[
            ("RecRoom.exe", b"the game itself"),
            ("Recroom_Release.exe", &big(40_000)),
            ("GameAssembly.dll", b"assembly"),
            ("RecRoom_Data/sharedassets0.assets", &big(3_000_000)),
        ]);
        let files = install(&dir, &zip);
        assert!(streamable(&files));
        fs::remove_file(dir.join("RecRoom.exe")).unwrap();
        fs::remove_file(dir.join("Recroom_Release.exe")).unwrap();
        let untouched = fs::metadata(dir.join("GameAssembly.dll")).unwrap().modified().unwrap();
        let damaged: Vec<FileRecord> = check_files(&dir, &files, &mut |_| {}).unwrap().into_iter().map(|(f, _)| f).collect();
        assert_eq!(damaged.len(), 2);

        let mut body = Tally { data: &zip, at: 0 };
        assert_eq!(repair_from_stream(&mut body, &dir, &damaged, &mut |_| {}).unwrap(), 2);
        assert!(check_files(&dir, &files, &mut |_| {}).unwrap().is_empty());
        assert_eq!(fs::metadata(dir.join("GameAssembly.dll")).unwrap().modified().unwrap(), untouched);
        // It stopped where the second exe ends, not 3 MB later.
        assert_eq!(body.at as u64, stream_end(&damaged));
        assert!(body.at < 100_000, "read {} of {}", body.at, zip.len());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The stream path against a real server that sends only whole files, as
    /// Stella's does: GitHub's generated source zips answer a range with
    /// the whole file. Reads it from the start, repairs a file near the front,
    /// and hangs up long before the end.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "hits github.com"]
    async fn repairs_from_a_real_stream_and_hangs_up_early() {
        const URL: &str = "https://github.com/benjaminp/six/archive/refs/tags/1.16.0.zip";
        let http = range_client().unwrap();
        let whole = http.get(URL).send().await.unwrap().bytes().await.unwrap().to_vec();
        let dir = temp_dir("live-stream");
        let files = install(&dir, &whole);
        assert!(streamable(&files));
        // The file whose data ends first, so the stream can stop early.
        let first = files.iter().min_by_key(|f| f.at.unwrap().data + f.at.unwrap().csize).unwrap().clone();
        fs::remove_file(dir.join(safe_rel_path(&first.path).unwrap())).unwrap();

        let response = http.get(URL).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let runtime = tokio::runtime::Handle::current();
        let stream = Box::pin(response.bytes_stream());
        let damaged = vec![first.clone()];
        let target = dir.clone();
        let read = tokio::task::spawn_blocking(move || {
            let mut body = Counted { inner: BodyReader { runtime, stream, chunk: None, at: 0 }, pos: Default::default() };
            let pos = body.pos.clone();
            assert_eq!(repair_from_stream(&mut body, &target, &damaged, &mut |_| {}).unwrap(), 1);
            pos.get()
        })
        .await
        .unwrap();
        assert!(check_files(&dir, std::slice::from_ref(&first), &mut |_| {}).unwrap().is_empty());
        eprintln!("read {read} of {} bytes to repair {}", whole.len(), first.path);
        assert!(read < whole.len() as u64 / 2);

        // And again with the list as the first version wrote it, walking the
        // zip's headers to find the file.
        fs::remove_file(dir.join(safe_rel_path(&first.path).unwrap())).unwrap();
        let response = http.get(URL).send().await.unwrap();
        let runtime = tokio::runtime::Handle::current();
        let stream = Box::pin(response.bytes_stream());
        let damaged = without_positions(vec![first.clone()]);
        let target = dir.clone();
        tokio::task::spawn_blocking(move || {
            let body = BodyReader { runtime, stream, chunk: None, at: 0 };
            assert_eq!(repair_by_scan(body, &target, &damaged, None, &mut |_| {}).unwrap(), 1);
        })
        .await
        .unwrap();
        assert!(check_files(&dir, std::slice::from_ref(&first), &mut |_| {}).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stream_from_a_rebuilt_zip_repairs_nothing_wrongly() {
        let dir = temp_dir("stream-rebuilt");
        let old = zip_of(&[("a.dll", b"alpha"), ("b.dll", b"beta")]);
        let files = install(&dir, &old);
        fs::write(dir.join("b.dll"), b"BROKEN").unwrap();
        let damaged: Vec<FileRecord> = check_files(&dir, &files, &mut |_| {}).unwrap().into_iter().map(|(f, _)| f).collect();

        // The same names, other contents: b.dll is no longer where the list
        // says, or no longer the file it recorded.
        let new = zip_of(&[("a.dll", b"ALPHA, longer now"), ("b.dll", b"beta 2")]);
        let err = repair_from_stream(&mut Tally { data: &new, at: 0 }, &dir, &damaged, &mut |_| {}).unwrap_err();
        assert!(err.contains("not the build you have installed"), "{err}");
        assert_eq!(fs::read(dir.join("b.dll")).unwrap(), b"BROKEN");
        assert!(!dir.join("b.dll.radium-repair").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A list as the first version of Verify Files wrote it: no positions.
    fn without_positions(files: Vec<FileRecord>) -> Vec<FileRecord> {
        files.into_iter().map(|f| FileRecord { at: None, ..f }).collect()
    }

    #[test]
    fn a_scan_repairs_a_list_without_positions_reading_only_as_far_as_needed() {
        let dir = temp_dir("scan");
        let zip = zip_of(&[
            ("RecRoom.exe", b"the game itself"),
            ("Recroom_Release.exe", &big(40_000)),
            ("GameAssembly.dll", b"assembly"),
            ("RecRoom_Data/sharedassets0.assets", &big(3_000_000)),
        ]);
        let files = without_positions(install(&dir, &zip));
        assert!(!streamable(&files));
        fs::remove_file(dir.join("RecRoom.exe")).unwrap();
        fs::write(dir.join("Recroom_Release.exe"), b"broken").unwrap();
        let untouched = fs::metadata(dir.join("GameAssembly.dll")).unwrap().modified().unwrap();
        let damaged: Vec<FileRecord> = check_files(&dir, &files, &mut |_| {}).unwrap().into_iter().map(|(f, _)| f).collect();
        assert_eq!(damaged.len(), 2);

        let mut body = Tally { data: &zip, at: 0 };
        assert_eq!(repair_by_scan(&mut body, &dir, &damaged, None, &mut |_| {}).unwrap(), 2);
        assert!(check_files(&dir, &files, &mut |_| {}).unwrap().is_empty());
        assert_eq!(fs::metadata(dir.join("GameAssembly.dll")).unwrap().modified().unwrap(), untouched);
        // It read past GameAssembly.dll's header at most (plus the buffer's
        // one read ahead), not the 3 MB after.
        assert!(body.at < 1_200_000, "read {} of {}", body.at, zip.len());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A zip as a streaming writer makes one: every entry deflated, its sizes
    /// and CRC in a data descriptor after the data and zero in its header.
    fn zip_with_descriptors(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in files {
            let mut deflate = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            deflate.write_all(data).unwrap();
            let packed = deflate.finish().unwrap();
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0x08u16.to_le_bytes());
            out.extend_from_slice(&8u16.to_le_bytes());
            out.extend_from_slice(&[0; 16]);
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&packed);
            out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
            out.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
            out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        }
        // Where the central directory would start.
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out
    }

    #[test]
    fn a_scan_finds_its_way_past_entries_sized_only_after_their_data() {
        let dir = temp_dir("scan-descriptor");
        let skipped = big(500_000);
        let zip = zip_with_descriptors(&[("first.bin", &skipped), ("wanted.dll", b"the one"), ("last.bin", b"tail")]);
        let record = |path: &str, data: &[u8]| FileRecord {
            path: path.into(),
            size: data.len() as u64,
            crc32: crc32fast::hash(data),
            at: None,
        };
        let damaged = [record("last.bin", b"tail"), record("wanted.dll", b"the one")];
        assert_eq!(repair_by_scan(&mut Tally { data: &zip, at: 0 }, &dir, &damaged, None, &mut |_| {}).unwrap(), 2);
        assert_eq!(fs::read(dir.join("wanted.dll")).unwrap(), b"the one");
        assert_eq!(fs::read(dir.join("last.bin")).unwrap(), b"tail");
        assert!(!dir.join("first.bin").exists());

        // A file the zip doesn't have runs into the central directory.
        let err = repair_by_scan(&mut Tally { data: &zip, at: 0 }, &dir, &[record("gone.dll", b"x")], None, &mut |_| {})
            .unwrap_err();
        assert!(err.contains("not the build you have installed"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scan_of_a_rebuilt_zip_repairs_nothing_wrongly() {
        let dir = temp_dir("scan-rebuilt");
        let files = without_positions(install(&dir, &zip_of(&[("a.dll", b"alpha"), ("b.dll", b"beta")])));
        fs::write(dir.join("b.dll"), b"BROKEN").unwrap();
        let damaged: Vec<FileRecord> = check_files(&dir, &files, &mut |_| {}).unwrap().into_iter().map(|(f, _)| f).collect();
        let new = zip_of(&[("a.dll", b"alpha"), ("b.dll", b"beta 2")]);
        let err = repair_by_scan(&mut Tally { data: &new, at: 0 }, &dir, &damaged, None, &mut |_| {}).unwrap_err();
        assert!(err.contains("not the build you have installed"), "{err}");
        assert_eq!(fs::read(dir.join("b.dll")).unwrap(), b"BROKEN");
        assert!(!dir.join("b.dll.radium-repair").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_list_from_before_positions_were_kept_is_not_streamed() {
        let old = FileRecord { path: "a.dll".into(), size: 5, crc32: 1, at: None };
        assert!(!streamable(&[old]));
        // And one stored some way it can't inflate itself.
        let spot = ZipSpot { header: 0, data: 40, csize: 5, method: u16::MAX };
        assert!(!streamable(&[FileRecord { path: "a.dll".into(), size: 5, crc32: 1, at: Some(spot) }]));
    }

    #[test]
    fn range_reads_grow_while_sequential_and_match_the_file() {
        let data = big(40 * 1024 * 1024);
        let mut sizes = Vec::new();
        let mut reader = RangeReader::new(data.len() as u64, |start, len| {
            sizes.push(len);
            Ok(data[start as usize..(start + len) as usize].to_vec())
        });
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
        drop(reader);
        assert_eq!(sizes[0], 256 * 1024);
        assert!(sizes.contains(&(8 * 1024 * 1024)));
        assert!(sizes.len() < 12, "{} fetches", sizes.len());
    }
}
