//! Downscaled, disk-cached thumbnails for the remote images the UI shows.
//!
//! ## Why this exists
//!
//! Vanilla serves room and profile images from `/images/<name>` as the file the
//! game uploaded, at whatever size it was captured. Measured 2026-09-09, a
//! typical room image is a **2560x1440 PNG, 3.2 MB on the wire and 14 MB once
//! the webview decodes it** — to be drawn in a card about 200 px wide. A page of
//! twelve room cards is therefore ~35 MB of downloads and well over a hundred
//! megabytes of decoded bitmap, which is most of what made opening Rooms feel
//! like it was loading the entire network.
//!
//! It is also paid *every* time. Those responses carry no `Cache-Control`, no
//! `ETag` and no `Last-Modified`, and Cloudflare reports `cf-cache-status:
//! DYNAMIC`, so the webview has nothing to revalidate against and re-downloads
//! all of it on every visit to the tab.
//!
//! Radium's images come from a resizing CDN and arrive small, but they get
//! nothing off disk either.
//!
//! So: fetch once, downscale to the width the UI actually asked for, keep the
//! result on disk, and serve it over a custom URI scheme so an `<img>` can point
//! straight at it. A room card goes from ~3 MB to ~20 KB, and the second visit
//! costs a file read.
//!
//! ## Reachable from the webview
//!
//! The scheme handler takes a URL out of page content, so it is an outbound
//! request the renderer chooses. [`host_allowed`] restricts it to the image
//! hosts the two networks actually use — without that, any injected markup in
//! the webview could aim this at an arbitrary host, including one on loopback.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::server::{http, read_capped, USER_AGENT};

/// Hosts this may fetch from. Everything else is refused. See the module docs.
///
/// This has to cover every host either network can hand the UI an image URL
/// for, because a host missing from it fails quietly: the request is refused,
/// the `<img>` falls back, and the card shows a placeholder that looks like a
/// broken image rather than a misconfiguration. The sources are:
///
/// * `api.vanillarec.net` — Vanilla's `/images/<name>`, rooms and players both.
/// * `img.radie.app` — Radium's resizing CDN, built by `RADIUM_IMG_BASE`.
/// * `www.radie.app` / `radie.app` — Radium's website. `scraper.rs` reads
///   creator avatars and profile pictures out of its HTML and resolves them
///   against `https://www.radie.app`, so a page-relative `src` arrives on that
///   host rather than on the CDN.
/// * `api.radie.app`, `vanillarec.net`, `cdn.recroomarchive.org` — the
///   remaining hosts either API answers with.
/// * `img.stellaonline.org` — Stella's `/img/<name>?width=`, built by
///   `stella_api::img_url`. Served openly (no User-Agent or token), so the
///   proxy fetches it like any other. `api.stellaonline.org` served them
///   until Stella moved them (by 2026-10-11).
const ALLOWED_HOSTS: &[&str] = &[
    "api.vanillarec.net",
    "vanillarec.net",
    "img.radie.app",
    "api.radie.app",
    "www.radie.app",
    "radie.app",
    "cdn.recroomarchive.org",
    "api.stellaonline.org",
    "img.stellaonline.org",
];

/// Widest thumbnail anyone may ask for.
///
/// The largest thing the UI renders is a feed photo, at roughly 800 CSS px, and
/// the frontend multiplies its request by the display's pixel ratio — so this
/// has to leave room for a 2x screen. A cap is still wanted, to keep a crafted
/// URL from turning the cache into a store of full-resolution originals.
const MAX_WIDTH: u32 = 2048;

/// JPEG quality for re-encoded thumbnails.
///
/// The encoder defaults to 75, which rings visibly around edges on the large
/// flat gradients that fill a screenshot of a 3-D scene. 82 is where that stops
/// being noticeable at the sizes these are drawn.
const JPEG_QUALITY: u8 = 82;

/// Refetch a cached thumbnail after this long. Room images do change — a
/// creator re-photographs their room — and nothing upstream tells us when.
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Total bytes the thumbnail directory may hold before the oldest are dropped.
const CACHE_BUDGET: u64 = 256 * 1024 * 1024;

/// How often to check the directory against [`CACHE_BUDGET`]. Walking it on
/// every miss would cost more than the pruning saves.
const PRUNE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Timeout for pulling one original. Generous: these are multi-megabyte PNGs.
const FETCH_TIMEOUT: Duration = Duration::from_secs(45);

/// Largest original this will pull down.
///
/// The body is buffered whole, six fetches can overlap, and each then becomes a
/// bitmap several times its encoded size, so an unbounded read is the one way a
/// single image can cost the launcher hundreds of megabytes. The biggest thing
/// either network actually serves is a 2560x1440 PNG at around 3 MB, so 24 MB
/// is a wide margin over the real traffic.
const MAX_SOURCE_BYTES: u64 = 24 * 1024 * 1024;

/// Largest source dimensions the decoder will accept.
///
/// `image`'s default `Limits` caps allocation at 512 MB but sets no bound on
/// width or height, which is what lets a tiny crafted file ask for an enormous
/// canvas. Nothing either network serves is anywhere near this.
const MAX_SOURCE_DIMENSION: u32 = 8192;

/// How many originals may be in flight at once.
///
/// Roughly what a browser allows per host. Vanilla's image endpoint is slow and
/// wildly variable — measured between 1.7 s and 30 s for a single file — so
/// some overlap is what stops a page of twelve cards filling in one at a time.
/// Each one in flight holds a multi-megabyte buffer, which is why it is capped
/// at all.
static FETCH_SLOTS: Semaphore = Semaphore::const_new(6);

/// How many images may be decoded and re-encoded at once.
///
/// This is the part that makes the *machine* stutter rather than just the
/// launcher. One 2560x1440 room PNG costs a full inflate, a resize and a JPEG
/// encode over about 14 MB of bitmap; twelve cards arriving together started
/// twelve of those at once, which is every core on a typical laptop plus a few
/// hundred megabytes of allocation churn — enough to be felt across the whole
/// desktop.
///
/// Held to a quarter of the machine so the launcher stays a background citizen
/// while it fills a grid. The work is one-off per image: once a thumbnail is on
/// disk, later views are a file read and none of this runs again.
fn decode_slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Semaphore::new((cores / 4).clamp(1, 4))
    })
}

/// Where thumbnails live. Set once at startup from the Tauri app handle,
/// because the scheme handler is given no access to one.
static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Point the cache at a directory. Called from `run()`'s setup hook.
pub fn init(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = CACHE_DIR.set(dir);
}

fn cache_dir() -> Option<&'static Path> {
    CACHE_DIR.get().map(|p| p.as_path())
}

/// Whether `url` names an image host this is allowed to fetch from.
///
/// Matched against the parsed host rather than by substring: a URL like
/// `https://api.vanillarec.net.evil.test/` contains an allowed name without
/// being one.
fn host_allowed(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" && url.scheme() != "http" {
        return false;
    }
    url.host_str()
        .map(|h| ALLOWED_HOSTS.iter().any(|a| h.eq_ignore_ascii_case(a)))
        .unwrap_or(false)
}

/// The URL to fetch for a thumbnail request: `url` parsed, on an allowed host,
/// and on https.
///
/// A plain `http://` URL is upgraded rather than fetched in the clear. Every
/// allowed host serves https, and an `http://` link still turns up — a scraped
/// Radium page can spell one — which anyone on the network path could then
/// answer with bytes of their choosing for the decoder, or for the webview
/// when a small original is passed through as it is.
fn source_url(url: &str) -> Result<reqwest::Url, String> {
    let mut parsed = reqwest::Url::parse(url).map_err(|_| "Not a URL.".to_string())?;
    if !host_allowed(&parsed) {
        return Err("Image host not allowed.".to_string());
    }
    if parsed.scheme() == "http" {
        parsed
            .set_scheme("https")
            .map_err(|_| "Not a URL.".to_string())?;
    }
    Ok(parsed)
}

/// The client originals are fetched with.
///
/// Its own rather than the shared one, for the redirect policy: [`host_allowed`]
/// only ever sees the URL the page asked for, and the shared client follows
/// any redirect anywhere. A redirect from one of the allowed hosts could then
/// have aimed this at loopback or the local network. Redirects to public hosts
/// are still followed, since an image CDN may legitimately bounce to storage
/// elsewhere — over https only, like the request itself (see [`source_url`]).
fn fetch_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| build_fetch_client(false))
}

/// [`fetch_client`], for an address the user typed: it also refuses to dial a
/// name that resolves to the local network. See [`PublicOnlyResolver`].
fn backdrop_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| build_fetch_client(true))
}

fn build_fetch_client(public_dns_only: bool) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .https_only(true)
        .gzip(true)
        .brotli(true)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !is_public_destination(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }));
    if public_dns_only {
        builder = builder.dns_resolver(Arc::new(PublicOnlyResolver));
    }
    builder.build().unwrap_or_else(|_| http().clone())
}

/// Resolves names the usual way, then keeps only public addresses.
///
/// [`is_public_destination`] can only judge what the URL says. A name is
/// whatever DNS answers for it, and a typed backdrop address — or a redirect
/// from one — can name a host that resolves to the local network: a router's
/// admin page, a NAS, a `.local` device. Checked here, at connect time, every
/// address actually dialled is a public one.
///
/// Only for the backdrop field. Thumbnails come from a fixed list of public
/// hosts, and with a proxy on the local network (a school's, an office's)
/// this is also what the proxy's own address is looked up through — which it
/// would refuse, and with it every picture in the launcher.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public: Vec<std::net::SocketAddr> = found.filter(|a| is_public_ip(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{} is not on the public internet", host).into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Whether `url` points somewhere on the public internet, as far as the URL
/// itself can say: not loopback, a private or link-local range, or a
/// `.localhost` name. What a name resolves to is checked when it is dialled,
/// by [`PublicOnlyResolver`].
fn is_public_destination(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" && url.scheme() != "http" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => is_public_ip(ip),
        Err(_) => {
            let name = host.trim_end_matches('.').to_ascii_lowercase();
            name != "localhost" && !name.ends_with(".localhost")
        }
    }
}

/// Whether an address is on the public internet: not this machine, not the
/// local network, and not a range that is reserved, shared or multicast.
fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || a == 0 // "this network"
                || a >= 240 // reserved
                || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
                || (a == 198 && (b == 18 || b == 19))) // benchmarking
        }
        std::net::IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link-local
                // An IPv4 address carried in IPv6 (mapped, or the old
                // compatible form) reaches whatever that IPv4 address is.
                || ip.to_ipv4().is_some())
        }
    }
}

/// Cache filename for one (url, width) pair, without an extension.
///
/// FNV-1a over the URL rather than a real digest: this names a cache entry, and
/// all that rides on it is that two different URLs are unlikely to collide. The
/// width stays in the clear so the same image at two sizes is plainly two
/// entries, and a directory listing is legible while debugging.
///
/// The extension is added by whichever format [`downscale`] chose, and a cache
/// lookup asks for both — see [`CACHE_FORMATS`].
fn cache_name(url: &str, width: u32) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:016x}_{}", hash, width)
}

/// Extensions a cached thumbnail may carry, with the content type to serve it
/// as. Every one is tried on a lookup, because which a given image got depends
/// on whether it was re-encoded, and if so whether it needed an alpha channel.
const CACHE_FORMATS: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// Originals at or below this size are cached and served exactly as they
/// arrived, with no decode or re-encode at all.
///
/// Re-encoding a small image is pure loss. A Vanilla profile picture is a
/// 256x256 file of about 7 KB; shrinking it to the 144 px an 80 px avatar wants
/// on a 1.5x display saves perhaps three kilobytes, and spends them putting
/// JPEG artifacts into the one image people look at closely. Above this the
/// source is a multi-megabyte room screenshot, where the re-encode is the whole
/// point.
const PASSTHROUGH_MAX_BYTES: usize = 96 * 1024;

/// The format some bytes actually are, as (extension, content type).
///
/// Sniffed rather than read from the response header, because Vanilla's image
/// endpoint answers `Content-Type: image/png` for files that are plainly JPEG
/// (checked 2026-09-10 against three profile pictures, two of which began
/// `FF D8`). Serving those on the header's word would label a JPEG as a PNG.
fn sniff_image(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    // Spelled as byte arrays rather than string escapes so the magic
    // numbers stay readable as numbers.
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];

    if bytes.starts_with(PNG) {
        Some(("png", "image/png"))
    } else if bytes.starts_with(JPEG) {
        Some(("jpg", "image/jpeg"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("gif", "image/gif"))
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(("webp", "image/webp"))
    } else {
        None
    }
}

/// A cached thumbnail for `base`, if one is on disk and still fresh.
async fn cached(dir: &Path, base: &str) -> Option<(Vec<u8>, &'static str)> {
    for (ext, mime) in CACHE_FORMATS {
        let path = dir.join(format!("{}.{}", base, ext));
        if is_fresh(&path) {
            if let Ok(bytes) = tokio::fs::read(&path).await {
                return Some((bytes, mime));
            }
        }
    }
    None
}

/// Per-entry locks, so twelve cards naming the same image download it once.
static IN_FLIGHT: OnceLock<std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    OnceLock::new();

fn in_flight_map() -> &'static std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>> {
    IN_FLIGHT.get_or_init(Default::default)
}

/// A lock for one cache entry, paired with a guard that removes it again.
///
/// Handing back the bare `Arc` left the key in the map forever. Every distinct
/// (url, width) pair added one — and browsing Rooms, People and the feed mints
/// a new pair per image per size — so the map grew for the life of the process.
/// `CREATOR_CACHE` in `vanilla.rs` is bounded for exactly this reason; this one
/// was not.
fn in_flight_lock(name: &str) -> (Arc<AsyncMutex<()>>, InFlightEntry) {
    let mut map = in_flight_map().lock().unwrap_or_else(|e| e.into_inner());
    let lock = map.entry(name.to_string()).or_default().clone();
    (lock, InFlightEntry(name.to_string()))
}

/// Drops the map entry once the last waiter on it is gone.
struct InFlightEntry(String);

impl Drop for InFlightEntry {
    fn drop(&mut self) {
        let mut map = in_flight_map().lock().unwrap_or_else(|e| e.into_inner());
        // Two strong references means the map's own plus this holder's, so
        // nobody else is waiting and the entry can go. A higher count means
        // another request is queued on this exact image and still needs it.
        if map.get(&self.0).map(Arc::strong_count).unwrap_or(0) <= 2 {
            map.remove(&self.0);
        }
    }
}

fn is_fresh(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() < CACHE_TTL)
        .unwrap_or(false)
}

/// The image at `url`, no wider than `width`, with the content type to serve it
/// as.
///
/// Serves the cached file when there is a fresh one, and otherwise downloads,
/// decodes, resizes and stores it.
pub async fn thumbnail(url: &str, width: u32) -> Result<(Vec<u8>, &'static str), String> {
    let width = width.clamp(16, MAX_WIDTH);

    let parsed = source_url(url)?;
    let base = cache_name(parsed.as_str(), width);
    let dir = cache_dir();

    if let Some(d) = dir {
        if let Some(hit) = cached(d, &base).await {
            return Ok(hit);
        }
    }

    // One downloader per entry; everyone else waits here and then re-reads the
    // file the winner wrote. `_entry` removes the map slot when the last waiter
    // on this image is done — see `InFlightEntry`.
    let (gate, _entry) = in_flight_lock(&base);
    let _held = gate.lock().await;

    if let Some(d) = dir {
        if let Some(hit) = cached(d, &base).await {
            return Ok(hit);
        }
    }

    let original = {
        let _slot = FETCH_SLOTS.acquire().await.map_err(|e| e.to_string())?;

        let response = fetch_client()
            .get(parsed.as_str())
            .timeout(FETCH_TIMEOUT)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("HTTP error: {}", response.status()));
        }

        // Refuse an oversized body before reading it. The whole response is
        // buffered in memory, six of them can be in flight at once, and each
        // then becomes a decoded bitmap several times its size — so an image
        // host serving something enormous, by malice or by mistake, is the
        // launcher's memory problem. The declared length is only a hint, so
        // the stream below is capped too.
        if let Some(len) = response.content_length() {
            if len > MAX_SOURCE_BYTES {
                return Err(format!("Image too large: {} bytes.", len));
            }
        }

        read_capped(response, MAX_SOURCE_BYTES).await?
        // The slot is released here, before the CPU work below: waiting on a
        // core is no reason to hold a network slot a neighbouring card wants.
    };

    // Small enough to be worth keeping exactly as it is. Serving the original
    // preserves its format, its alpha and its sharpness, and skips the decode
    // entirely — see [`PASSTHROUGH_MAX_BYTES`].
    if original.len() <= PASSTHROUGH_MAX_BYTES {
        if let Some((ext, mime)) = sniff_image(&original) {
            let bytes = original.to_vec();
            if let Some(d) = dir {
                store(d.join(format!("{}.{}", base, ext)), bytes.clone()).await;
            }
            return Ok((bytes, mime));
        }
        // Unrecognised bytes fall through to the decoder, which is the thing
        // that can say whether they are an image at all.
    }

    // Decoding a 2560x1440 PNG is the expensive part, and on an async worker it
    // would stall every other request sharing that thread. Separate from the
    // fetch limit above because the two bound different resources — see
    // [`decode_slots`].
    let (encoded, ext) = {
        let _slot = decode_slots().acquire().await.map_err(|e| e.to_string())?;
        tokio::task::spawn_blocking(move || downscale(&original, width))
            .await
            .map_err(|e| e.to_string())??
    };

    if let Some(d) = dir {
        store(d.join(format!("{}.{}", base, ext)), encoded.clone()).await;
    }

    let mime = CACHE_FORMATS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mime)| *mime)
        .unwrap_or("application/octet-stream");
    Ok((encoded, mime))
}

/// Largest picture the glass backdrop field will fetch. The same ceiling the
/// page puts on a picked file (`BG_IMAGE_MAX_INPUT_BYTES` in app.js), which it
/// then downscales either way.
const MAX_BACKDROP_BYTES: u64 = 20 * 1024 * 1024;

/// The picture at an address typed into the Liquid Glass backdrop field, as
/// its original bytes.
///
/// The page can't show it straight from that address: the CSP's `img-src`
/// allows no remote host at all — what keeps any markup that gets into the
/// page from loading, or reporting to, anything it likes — so a typed
/// backdrop was refused by the webview and the window painted nothing where
/// it should have been. It is fetched here instead, and the page stores it the
/// way it stores a picked file: downscaled into a `data:` URI.
///
/// Unlike [`thumbnail`], any host will do, since the user typed it — but only
/// over https, only on the public internet (redirects and what the name
/// resolves to included, through [`backdrop_client`]), only up to
/// [`MAX_BACKDROP_BYTES`], and only if what comes back is actually a picture.
pub async fn backdrop_source(url: &str) -> Result<Vec<u8>, String> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|_| "That isn't a web address.".to_string())?;
    if parsed.scheme() != "https" {
        return Err("Use an address that starts with https://".into());
    }
    if !is_public_destination(&parsed) {
        return Err("That address isn't on the internet.".into());
    }

    let response = backdrop_client()
        .get(parsed.as_str())
        .timeout(FETCH_TIMEOUT)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|_| "Couldn't download the picture from that address.".to_string())?;
    if !response.status().is_success() {
        return Err(format!("That address answered HTTP {}.", response.status().as_u16()));
    }
    if response.content_length().is_some_and(|len| len > MAX_BACKDROP_BYTES) {
        return Err("That picture is over 20 MB.".into());
    }
    let bytes = read_capped(response, MAX_BACKDROP_BYTES)
        .await
        .map_err(|_| "Couldn't download the picture from that address (it may be over 20 MB).".to_string())?;
    if sniff_image(&bytes).is_none() {
        return Err("That address isn't a PNG, JPEG, GIF or WebP picture.".into());
    }
    Ok(bytes)
}

/// Decode, shrink to `width`, and re-encode. Returns the bytes and the file
/// extension the chosen format wants.
///
/// JPEG for photographs — these are screenshots of 3-D scenes, the case PNG is
/// worst at and most of why the originals run to megabytes. PNG only where the
/// image actually needs an alpha channel; see [`has_transparency`].
fn downscale(bytes: &[u8], width: u32) -> Result<(Vec<u8>, &'static str), String> {
    // Decoded through a reader with explicit limits rather than
    // `load_from_memory`, whose defaults bound allocation but not dimensions —
    // so a small file declaring an enormous canvas got as far as trying to
    // allocate for it. Refusing by dimension rejects that on the header.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);

    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;

    // An image already at or below the target is re-encoded but not resized —
    // Radium's CDN already serves the size we asked it for, and enlarging it
    // would spend bytes to add nothing.
    let img = if img.width() > width {
        // Lanczos3: these are 4-6x reductions, where a cheaper filter is
        // visibly soft. The resize is a small share of the cost next to
        // inflating the source PNG, so the sharper one is worth its time.
        img.resize(width, u32::MAX, FilterType::Lanczos3)
    } else {
        img
    };

    let mut out = Cursor::new(Vec::new());
    if has_transparency(&img) {
        // JPEG has no alpha channel, and flattening one turns a cut-out avatar
        // into a black square. Images that need this are small, so PNG costs
        // little here.
        img.write_to(&mut out, ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        return Ok((out.into_inner(), "png"));
    }

    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&img.to_rgb8())
        .map_err(|e| e.to_string())?;
    Ok((out.into_inner(), "jpg"))
}

/// Whether an image carries alpha it actually uses.
///
/// The colour type alone is not enough: plenty of sources are RGBA with every
/// pixel opaque, and encoding those as PNG would give up the whole point of
/// re-encoding. Checked after the resize, so this scans a thumbnail rather than
/// the full-size original.
fn has_transparency(img: &DynamicImage) -> bool {
    img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p.0[3] < u8::MAX)
}

/// Write via a temporary file and rename, so a half-written thumbnail is never
/// visible to a concurrent reader or left behind by a crash mid-write.
///
/// The write is awaited, because the caller still holds this image's in-flight
/// lock and everyone queued on it re-reads the cache the moment it is released.
/// Fired off unawaited, the file was usually not there yet when they looked,
/// so each of them downloaded and decoded the same image again — the very
/// duplicate work the lock exists to prevent. A thumbnail is tens of
/// kilobytes, so the wait is a millisecond. Pruning, which walks the whole
/// directory, is still left to run on its own.
async fn store(path: PathBuf, bytes: Vec<u8>) {
    let _ = tokio::task::spawn_blocking(move || write_atomic(&path, &bytes)).await;
    tokio::task::spawn_blocking(maybe_prune);
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    // Distinct per format, so a PNG and a JPEG under the same cache base can
    // never collide on one temp name.
    let tmp = path.with_extension(format!(
        "{}.part",
        path.extension().and_then(|e| e.to_str()).unwrap_or("bin")
    ));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Drop the oldest thumbnails once the directory outgrows [`CACHE_BUDGET`].
///
/// Rate-limited, and deliberately crude: this is a cache, so evicting a little
/// too much costs one refetch.
fn maybe_prune() {
    static LAST: OnceLock<std::sync::Mutex<Option<SystemTime>>> = OnceLock::new();
    {
        let cell = LAST.get_or_init(Default::default);
        let mut last = cell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = *last {
            if SystemTime::now().duration_since(t).unwrap_or_default() < PRUNE_INTERVAL {
                return;
            }
        }
        *last = Some(SystemTime::now());
    }

    let Some(dir) = cache_dir() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
                e.path(),
            ))
        })
        .collect();

    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if total <= CACHE_BUDGET {
        return;
    }

    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, len, path) in files {
        if total <= CACHE_BUDGET {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_networks_own_image_hosts_are_reachable() {
        let allowed = |u: &str| host_allowed(&reqwest::Url::parse(u).unwrap());

        assert!(allowed("https://api.vanillarec.net/images/vnlaroom_13"));
        assert!(allowed("https://img.radie.app/Room_1?width=480"));
        // scraper.rs resolves avatars it reads out of Radium's HTML against
        // `https://www.radie.app`, and those go through this proxy too. Leaving
        // the website off the list breaks every scraped avatar on Radium, and
        // does it silently.
        assert!(allowed("https://www.radie.app/_astro/avatar.png"));
        assert!(allowed("https://radie.app/_astro/avatar.png"));

        // A host that merely contains an allowed name is a different host.
        assert!(!allowed("https://api.vanillarec.net.evil.test/x.png"));
        assert!(!allowed("https://evil.test/api.vanillarec.net/x.png"));
        // And nothing off the public network at all.
        assert!(!allowed("http://127.0.0.1:8080/admin"));
        assert!(!allowed("http://localhost/x.png"));
        assert!(!allowed("file:///C:/Windows/win.ini"));
    }

    /// A real picture comes back as its original bytes.
    #[tokio::test]
    #[ignore = "hits api.vanillarec.net"]
    async fn a_backdrop_address_comes_back_as_the_picture() {
        let bytes = backdrop_source("https://api.vanillarec.net/images/vnlaroom_13")
            .await
            .expect("a room image should download");
        assert!(sniff_image(&bytes).is_some());
        println!("backdrop: {} bytes, {:?}", bytes.len(), sniff_image(&bytes));
    }

    /// Each of these is refused before any request is made.
    #[tokio::test]
    async fn a_backdrop_address_must_be_public_and_https() {
        for bad in [
            "http://example.com/wallpaper.jpg",
            "https://127.0.0.1/wallpaper.jpg",
            "https://localhost/wallpaper.png",
            "https://192.168.1.10/wallpaper.jpg",
            "https://[::1]/wallpaper.jpg",
            "file:///C:/Windows/Web/Wallpaper/img0.jpg",
            "not a url",
        ] {
            assert!(backdrop_source(bad).await.is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_plaintext_image_url_is_fetched_over_https() {
        assert_eq!(
            source_url("http://www.radie.app/_astro/avatar.png").unwrap().as_str(),
            "https://www.radie.app/_astro/avatar.png"
        );
        assert_eq!(
            source_url("http://api.vanillarec.net:80/images/x?1").unwrap().as_str(),
            "https://api.vanillarec.net/images/x?1"
        );
        // Already https: left exactly as it was, cachebuster and all.
        assert_eq!(
            source_url("https://api.vanillarec.net/images/2_webso?1785023462696").unwrap().as_str(),
            "https://api.vanillarec.net/images/2_webso?1785023462696"
        );
        // The host check still comes first.
        assert!(source_url("http://evil.test/x.png").is_err());
        assert!(source_url("not a url").is_err());
    }

    #[test]
    fn redirects_may_not_lead_off_the_public_network() {
        let public = |u: &str| is_public_destination(&reqwest::Url::parse(u).unwrap());

        assert!(public("https://img.radie.app/Room_1"));
        assert!(public("https://some-bucket.s3.amazonaws.com/Room_1.png"));
        assert!(public("https://93.184.216.34/x.png"));

        for local in [
            "http://127.0.0.1:8080/admin",
            "http://localhost/x.png",
            "http://LOCALHOST./x.png",
            "http://radiumimg.localhost/thumb",
            "http://192.168.1.1/",
            "http://10.0.0.5/",
            "http://172.16.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::7f00:1]/",
            "http://100.64.0.1/",
            "http://198.18.0.1/",
            "http://224.0.0.1/",
            "http://255.255.255.255/",
            "http://0.1.2.3/",
            "http://[ff02::1]/",
            "file:///C:/Windows/win.ini",
        ] {
            assert!(!public(local), "{local} should be refused");
        }
    }

    /// A name is judged by what it resolves to, not by how it is spelled: a
    /// typed backdrop address naming a host on the local network is refused
    /// even though nothing in the URL says so.
    #[tokio::test]
    async fn a_name_that_resolves_to_this_machine_is_not_dialled() {
        use reqwest::dns::Resolve;
        let name: reqwest::dns::Name = "localhost".parse().expect("a name");
        assert!(PublicOnlyResolver.resolve(name).await.is_err());

        assert!(is_public_ip("2606:4700::1111".parse().unwrap()), "public IPv6 stays reachable");
        assert!(is_public_ip("104.16.0.1".parse().unwrap()), "public IPv4 stays reachable");
    }

    /// The whole point, measured against a real room image.
    ///
    /// Ignored by default like the other live checks in this crate — run it
    /// with `cargo test --lib -- --ignored --nocapture` when the image
    /// pipeline is in question.
    #[tokio::test]
    #[ignore = "hits api.vanillarec.net"]
    async fn a_room_card_thumbnail_is_a_fraction_of_the_original() {
        let url = "https://api.vanillarec.net/images/vnlaroom_13";

        let original = http()
            .get(url)
            .timeout(FETCH_TIMEOUT)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .expect("the image host should answer")
            .bytes()
            .await
            .expect("the body should arrive");

        let (thumb, ext) = downscale(&original, 480).expect("a room PNG should decode");
        assert_eq!(ext, "jpg", "an opaque screenshot should not come back as PNG");
        let shrunk = image::load_from_memory(&thumb).expect("the output should be a valid image");

        assert_eq!(shrunk.width(), 480, "the card asked for 480px");
        assert!(
            thumb.len() * 10 < original.len(),
            "{} bytes from {} is not worth the round trip",
            thumb.len(),
            original.len()
        );
        println!(
            "original {} bytes -> thumbnail {} bytes ({}x{}), {:.1}x smaller",
            original.len(),
            thumb.len(),
            shrunk.width(),
            shrunk.height(),
            original.len() as f64 / thumb.len() as f64
        );
    }

    /// Encode a tiny image to PNG, as a stand-in source for `downscale`.
    fn png_source(pixels: &[[u8; 4]]) -> Vec<u8> {
        let mut img = image::RgbaImage::new(pixels.len() as u32, 1);
        for (x, px) in pixels.iter().enumerate() {
            img.put_pixel(x as u32, 0, image::Rgba(*px));
        }
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ImageFormat::Png)
            .expect("a 1-row PNG should encode");
        out.into_inner()
    }

    #[test]
    fn an_image_that_uses_alpha_keeps_it() {
        let src = png_source(&[[255, 0, 0, 255], [0, 0, 0, 0]]);

        let (bytes, ext) = downscale(&src, 480).expect("the source should decode");
        assert_eq!(
            ext, "png",
            "a cut-out avatar re-encoded as JPEG becomes a black square"
        );

        let out = image::load_from_memory(&bytes).expect("the output should decode");
        assert!(out.color().has_alpha());
        assert_eq!(
            out.to_rgba8().get_pixel(1, 0).0[3],
            0,
            "the transparent pixel did not survive the round trip"
        );
    }

    #[test]
    fn an_opaque_image_with_an_alpha_channel_still_becomes_a_jpeg() {
        // Plenty of sources are RGBA with every pixel opaque. Treating the
        // colour type alone as "needs alpha" would store every one of those as
        // PNG, which gives up the whole reason for re-encoding.
        let src = png_source(&[[255, 0, 0, 255], [0, 255, 0, 255]]);

        let (_, ext) = downscale(&src, 480).expect("the source should decode");
        assert_eq!(ext, "jpg");
    }

    #[test]
    fn a_format_is_read_from_the_bytes_not_the_header() {
        // Vanilla's image endpoint answers `Content-Type: image/png` for files
        // that are plainly JPEG — checked 2026-09-10 against three profile
        // pictures, two of which began FF D8 FF. Trusting the header would
        // label those PNG.
        assert_eq!(
            sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
            Some(("jpg", "image/jpeg"))
        );
        assert_eq!(
            sniff_image(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]),
            Some(("png", "image/png"))
        );
        assert_eq!(sniff_image(b"GIF89a..."), Some(("gif", "image/gif")));
        assert_eq!(
            sniff_image(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some(("webp", "image/webp"))
        );

        // Anything unrecognised has to fall through to the decoder rather than
        // be passed to the webview as an image on a guess.
        assert_eq!(sniff_image(b"<html>"), None);
        assert_eq!(sniff_image(b""), None);
        assert_eq!(sniff_image(b"RIFF1234WAVE"), None);
    }

    #[test]
    fn every_sniffable_format_can_be_found_in_the_cache() {
        // A passed-through original is stored under the extension sniff_image
        // gave it, and a lookup only checks the extensions in CACHE_FORMATS. A
        // format missing from that list would be written once and then never
        // found again, refetched on every single view.
        for probe in [
            &[0xFF, 0xD8, 0xFF, 0xE0][..],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A][..],
            b"GIF89a...",
            b"RIFF\x00\x00\x00\x00WEBPVP8 ",
        ] {
            let (ext, mime) = sniff_image(probe).expect("the probe should be recognised");
            assert!(
                CACHE_FORMATS.contains(&(ext, mime)),
                "{ext:?} is sniffed but not listed in CACHE_FORMATS"
            );
        }
    }

    #[test]
    fn the_same_image_at_two_widths_is_two_entries() {
        let url = "https://api.vanillarec.net/images/vnlaroom_13";
        assert_ne!(cache_name(url, 480), cache_name(url, 96));
        assert_eq!(cache_name(url, 480), cache_name(url, 480));
        assert_ne!(
            cache_name(url, 480),
            cache_name("https://api.vanillarec.net/images/vnlaroom_8", 480)
        );
        // A cache-buster query is part of the identity, not noise to strip.
        assert_ne!(
            cache_name(url, 480),
            cache_name(&format!("{}?1785023462696", url), 480)
        );
        // And the name has to be usable as a filename on Windows.
        assert!(cache_name(url, 480)
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.'));
    }
}
