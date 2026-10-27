use crate::config::Network;
use crate::vanilla;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub(crate) const USER_AGENT: &str = "Radium-Launcher";

/// Process-wide reqwest client (connection pool reuse). Timeouts are applied
/// per-request via `.timeout()` since callers want different limits.
///
/// Compression is negotiated on every request. It costs nothing on the small
/// JSON calls and is what makes Vanilla's bulk `/ws` endpoints usable at all:
/// its room dump is 11.4 MB of JSON that arrives as 1.0 MB of brotli, and the
/// player dump 51.6 MB as 5.3 MB.
pub(crate) fn http() -> &'static Client {
    static HTTP: OnceLock<Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        Client::builder()
            .gzip(true)
            .brotli(true)
            .build()
            .unwrap_or_else(|_| Client::new())
    })
}

/// Process-wide client whose User-Agent is `BestHTTP`, the one string Stella's
/// Cloudflare lets through to its game API (see `stella_api`). Everything on
/// `api.stellaonline.org` except `/download/*` is a 403 block page without it,
/// so this is kept separate from [`http`] (whose `Radium-Launcher` UA is used
/// for Radium's own API and Stella's open download paths) rather than switched
/// per request.
///
/// Idle connections are dropped after 20 seconds rather than reqwest's 90:
/// Stella's edge closes them sooner than that, and a request sent down one it
/// had already closed failed at once with "error sending request" — the
/// intermittent Rooms error. Keep-alive probes catch the rest; `stella_api`
/// retries what still slips through.
pub(crate) fn http_client_besthttp() -> &'static Client {
    static HTTP: OnceLock<Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        Client::builder()
            .user_agent(crate::stella_api::USER_AGENT)
            .gzip(true)
            .brotli(true)
            .pool_idle_timeout(Duration::from_secs(20))
            .tcp_keepalive(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new())
    })
}

/// Read a response body, refusing to buffer more than `max` bytes.
///
/// `Response::bytes()` reads to the end however long that is; a server can
/// under-declare `Content-Length` or omit it entirely — and a compressed body
/// can inflate to many times what it declared — so the cap has to apply to
/// bytes as they arrive rather than to the header.
pub(crate) async fn read_capped(response: reqwest::Response, max: u64) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;

    let mut out: Vec<u8> = Vec::with_capacity(
        response.content_length().unwrap_or(0).min(max) as usize,
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if out.len() as u64 + chunk.len() as u64 > max {
            return Err("The response was too large.".to_string());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Largest API reply (JSON, or a scraped HTML page) buffered in one go.
///
/// Every page, profile and list the launcher asks for is kilobytes; the
/// biggest is a prolific Vanilla photographer's whole photo list, a few
/// megabytes. The shared client decompresses gzip and brotli, where a few
/// kilobytes on the wire can inflate to gigabytes, so `text()` and `json()`
/// were one hostile or broken reply away from holding all of it.
pub(crate) const MAX_API_BYTES: u64 = 32 * 1024 * 1024;

/// A response body as text, capped at [`MAX_API_BYTES`].
pub(crate) async fn read_text_capped(response: reqwest::Response) -> Result<String, String> {
    let bytes = read_capped(response, MAX_API_BYTES).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Shared GET helper with User-Agent header and 10s timeout.
pub(crate) async fn http_get_json(url: &str) -> Result<Value, String> {
    let response = http()
        .get(url)
        .timeout(Duration::from_secs(10))
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP error: {}", status));
    }

    let body = read_capped(response, MAX_API_BYTES).await?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

/// Most rows one page may ask for.
///
/// The frontend asks for 12 to 60, but nothing enforced it, and on Vanilla
/// every row on a page is resolved through the site proxy in batches of 20 —
/// so one oversized `take` fanned out into hundreds of concurrent requests to
/// someone else's server.
const MAX_TAKE: i64 = 100;

/// The page size an `args` object asks for, held to 1..=[`MAX_TAKE`].
fn page_size(args: &Value, default: i64) -> i64 {
    args.get("take").and_then(|v| v.as_i64()).unwrap_or(default).clamp(1, MAX_TAKE)
}

/// Which network an incoming `args` object is asking about.
///
/// Absent or unrecognised means Radium, so an older frontend (or a command
/// invoked without the field) keeps its existing behaviour.
fn network_of(args: &Value) -> Network {
    Network::parse(args.get("network").and_then(|v| v.as_str()))
}

/// The reply for anything asked of Stella's API.
///
/// Stella publishes none that the launcher knows of — no rooms, people, photos
/// or player count — and the UI hides everything that would ask. Without this
/// guard those calls fell through to Radium's branch, as every network but
/// Vanilla did, and would have shown Radium's rooms under Stella's name.
pub(crate) fn no_public_api() -> Value {
    json!({
        "success": false,
        "unsupported": true,
        "error": "This network does not publish this."
    })
}

/// Ping a server and return its online status, latency, and HTTP status code.
#[tauri::command]
pub async fn ping_server(url: String) -> Value {
    let parsed = match reqwest::Url::parse(&url) {
        Ok(u) => u,
        Err(_) => return json!({"error": "Invalid URL format"}),
    };
    let host = parsed.host_str().unwrap_or("");
    let is_cdn = host == "cdn.recroomarchive.org";
    let is_vanilla = host == "api.vanillarec.net" || host == "vanillarec.net";
    let is_stella = host == "api.stellaonline.org";
    let is_receos = host == "api.rec-eos.com";
    if is_receos {
        let start = Instant::now();
        return match crate::receos::web_status().await {
            Ok(v) => json!({
                "online": v["ok"].as_bool().unwrap_or(false) && v["maintenance"].is_null(),
                "maintenance": !v["maintenance"].is_null(),
                "closedBeta": v["closedBeta"].as_bool().unwrap_or(false),
                "latency": start.elapsed().as_millis() as i64,
            }),
            Err(_) => json!({ "online": false, "latency": -1 }),
        };
    }
    if host != "api.radie.app"
        && host != "www.radie.app"
        && host != "launcher.radie.app"
        && !is_cdn
        && !is_vanilla
        && !is_stella
    {
        return json!({ "online": false, "latency": -1, "error": "Untrusted URL." });
    }

    // Stella has no health route, and its downloads are all it serves. A HEAD
    // on the patch answers 405 from Stella's own server (it allows GET only)
    // without sending the file, which is proof enough that it is up. Only that
    // (or a plain success) counts: a Cloudflare 5xx means Stella is down, and a
    // Cloudflare 403 block page says nothing about Stella at all. This is the
    // host the game's NameServer lives on (the patch points ns.rec.net here).
    if is_stella {
        let start = Instant::now();
        return match http()
            .head(crate::stella::PATCH_URL)
            .header("User-Agent", crate::stella::USER_AGENT)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) => json!({
                "online": resp.status().is_success()
                    || resp.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED,
                "latency": start.elapsed().as_millis() as i64,
                "status": resp.status().as_u16(),
            }),
            Err(_) => json!({ "online": false, "latency": -1 }),
        };
    }

    // The radie.app API exposes a `/health` endpoint; the recroomarchive CDN does
    // not, so ping its root instead and treat any HTTP response as reachable.
    // Vanilla has no health route either — its player-count endpoint is the
    // cheapest thing that proves the API is serving, not merely resolving.
    let ping_url = if is_vanilla {
        vanilla::ping_url()
    } else if is_cdn {
        url.trim_end_matches('/').to_string()
    } else {
        format!("{}/health", url.trim_end_matches('/'))
    };

    let start = Instant::now();

    match http().get(&ping_url).timeout(Duration::from_secs(5)).send().await {
        Ok(resp) => {
            let latency = start.elapsed().as_millis() as i64;
            let status = resp.status();
            // For the API a successful /health response means online; for the CDN
            // any response at all means the host is reachable.
            let online = if is_cdn { true } else { status.is_success() };
            json!({
                "online": online,
                "latency": latency,
                "status": status.as_u16(),
            })
        }
        Err(_) => {
            json!({
                "online": false,
                "latency": -1,
            })
        }
    }
}

/// Get the current online player count from the Radium API.
#[tauri::command]
pub async fn get_player_count(app: tauri::AppHandle, network: Option<String>) -> Value {
    match Network::parse(network.as_deref()) {
        Network::Vanilla => return vanilla::get_player_count().await,
        // Counted from Stella's live presence; see `stella_hub::player_count`.
        Network::Stella => return crate::stella_hub::player_count(app),
        Network::RecEos => return crate::receos_api::get_player_count().await,
        Network::Radium => {}
    }

    let url = "https://api.radie.app/api/players/v1/online";

    match http()
        .get(url)
        .timeout(Duration::from_secs(60))
        .header("User-Agent", USER_AGENT)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                return json!({ "success": false, "error": format!("HTTP error: {}", status) });
            }
            let body = read_capped(resp, MAX_API_BYTES).await;
            match body.and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string())) {
                Ok(data) => {
                    let count = data.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
                    json!({ "success": true, "count": count })
                }
                Err(e) => json!({ "success": false, "error": e.to_string() }),
            }
        }
        Err(e) => json!({ "success": false, "error": e.to_string() }),
    }
}

/// Fetch a paginated list of rooms with optional search query and tag filter.
#[tauri::command]
pub async fn fetch_rooms(app: tauri::AppHandle, args: Value) -> Value {
    let skip = args.get("skip").and_then(|v| v.as_i64()).unwrap_or(0);
    let take = page_size(&args, 20);
    let sort_by = args.get("sortBy").and_then(|v| v.as_i64()).unwrap_or(0);
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tag = args
        .get("tag")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    match network_of(&args) {
        // Vanilla has no server-side search, tag, sort or skip parameter. Its
        // whole public room set is cached in the module instead, so all four
        // are applied there over every room rather than over one API page.
        Network::Vanilla => return vanilla::fetch_rooms(skip, take, &query, &tag, sort_by).await,
        Network::Stella => return crate::stella_api::fetch_rooms(&app, skip, take, &query, &tag, sort_by).await,
        Network::RecEos => return crate::receos_api::fetch_rooms(skip, take, &query, &tag, sort_by).await,
        Network::Radium => {}
    }

    let mut url = match reqwest::Url::parse("https://launcher.radie.app/api/rooms/v1/") {
        Ok(u) => u,
        Err(e) => return json!({ "success": false, "error": e.to_string() }),
    };

    {
        let mut query_pairs = url.query_pairs_mut();
        query_pairs.append_pair("skip", &skip.to_string());
        query_pairs.append_pair("take", &take.to_string());
        query_pairs.append_pair("sortBy", &sort_by.to_string());
        if !query.is_empty() {
            query_pairs.append_pair("query", &query);
        }
        if !tag.is_empty() {
            query_pairs.append_pair("tag", &tag);
        }
    }

    match http_get_json(url.as_str()).await {
        Ok(data) => json!({ "success": true, "data": data }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

/// Fetch a paginated list of people with optional search query.
#[tauri::command]
pub async fn fetch_people(app: tauri::AppHandle, args: Value) -> Value {
    let skip = args.get("skip").and_then(|v| v.as_i64()).unwrap_or(0);
    let take = page_size(&args, 15);
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    match network_of(&args) {
        Network::Vanilla => return vanilla::fetch_people(skip, take, &query).await,
        // With no query, Stella's list is the players online now, which the
        // live hub (started through the app) knows.
        Network::Stella => return crate::stella_api::fetch_people(&app, skip, take, &query).await,
        Network::RecEos => return crate::receos_api::fetch_people(skip, take, &query).await,
        Network::Radium => {}
    }

    let mut url = match reqwest::Url::parse("https://launcher.radie.app/api/user/v1") {
        Ok(u) => u,
        Err(e) => return json!({ "success": false, "error": e.to_string() }),
    };

    {
        let mut query_pairs = url.query_pairs_mut();
        query_pairs.append_pair("skip", &skip.to_string());
        query_pairs.append_pair("take", &take.to_string());
        if !query.is_empty() {
            query_pairs.append_pair("query", &query);
        }
    }

    match http_get_json(url.as_str()).await {
        Ok(data) => json!({ "success": true, "data": data }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

/// Fetch available room filters (tags / categories).
#[tauri::command]
pub async fn fetch_filters(network: Option<String>) -> Value {
    match Network::parse(network.as_deref()) {
        // Vanilla publishes no filter endpoint, so the tag list is tallied from
        // the cached room set. See vanilla::fetch_filters.
        Network::Vanilla => return vanilla::fetch_filters().await,
        Network::Stella => return crate::stella_api::fetch_filters().await,
        Network::RecEos => return crate::receos_api::fetch_filters().await,
        Network::Radium => {}
    }

    let url = "https://api.radie.app/api/rooms/v1/filters";

    match http_get_json(url).await {
        Ok(data) => json!({ "success": true, "data": data }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

/// Fetch photos taken in a specific room.
///
/// Stella has a direct per-room photo endpoint. Radium and Vanilla do not, so
/// they answer `unsupported` and the frontend falls back to scanning the
/// network-wide feed and filtering by room id.
#[tauri::command]
pub async fn fetch_room_photos(args: Value) -> Value {
    let room_id = match args.get("roomId") {
        Some(v) if v.is_number() => v.to_string(),
        Some(v) if v.is_string() => v.as_str().unwrap().to_string(),
        _ => return json!({ "success": false, "error": "roomId is required" }),
    };
    let skip = args.get("skip").and_then(|v| v.as_i64()).unwrap_or(0);
    let take = page_size(&args, 60);

    match network_of(&args) {
        Network::Stella => crate::stella_api::fetch_room_photos(&room_id, skip, take).await,
        Network::RecEos => crate::receos_api::fetch_room_photos(&room_id, skip, take).await,
        _ => no_public_api(),
    }
}

/// Fetch photos for a specific user.
#[tauri::command]
pub async fn fetch_user_photos(args: Value) -> Value {
    let user_id = match args.get("userId") {
        // v.to_string() renders any JSON number verbatim — as_i64().unwrap()
        // would panic on a non-integer (e.g. float) userId.
        Some(v) if v.is_number() => v.to_string(),
        Some(v) if v.is_string() => v.as_str().unwrap().to_string(),
        _ => return json!({ "success": false, "error": "userId is required" }),
    };
    let skip = args.get("skip").and_then(|v| v.as_i64()).unwrap_or(0);
    let take = page_size(&args, 40);

    match network_of(&args) {
        Network::Vanilla => return vanilla::fetch_user_photos(&user_id, skip, take).await,
        Network::Stella => return crate::stella_api::fetch_user_photos(&user_id, skip, take).await,
        Network::RecEos => return crate::receos_api::fetch_user_photos(&user_id, skip, take).await,
        Network::Radium => {}
    }

    let url = format!(
        "https://launcher.radie.app/api/user/v1/{}/photos?skip={}&take={}",
        vanilla::urlencoding(&user_id),
        skip,
        take
    );

    match http_get_json(&url).await {
        Ok(data) => json!({ "success": true, "data": data }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

/// Fetch the inventions a specific user has published. Only Stella publishes
/// them; elsewhere the answer is an empty, complete list, and the page doesn't
/// offer the tab there anyway.
#[tauri::command]
pub async fn fetch_user_inventions(args: Value) -> Value {
    let user_id = match args.get("userId") {
        Some(v) if v.is_number() => v.to_string(),
        Some(v) if v.is_string() => v.as_str().unwrap().to_string(),
        _ => return json!({ "success": false, "error": "userId is required" }),
    };
    let skip = args.get("skip").and_then(|v| v.as_i64()).unwrap_or(0);
    let take = page_size(&args, 24);

    match network_of(&args) {
        Network::Stella => crate::stella_api::fetch_user_inventions(&user_id, skip, take).await,
        Network::Radium | Network::Vanilla | Network::RecEos => json!({
            "success": true,
            "data": { "Results": [], "TotalResults": 0, "TotalKnown": true }
        }),
    }
}

/// Fetch rooms for a specific user.
#[tauri::command]
pub async fn fetch_user_rooms(args: Value) -> Value {
    let user_id = match args.get("userId") {
        // v.to_string() renders any JSON number verbatim — as_i64().unwrap()
        // would panic on a non-integer (e.g. float) userId.
        Some(v) if v.is_number() => v.to_string(),
        Some(v) if v.is_string() => v.as_str().unwrap().to_string(),
        _ => return json!({ "success": false, "error": "userId is required" }),
    };
    let skip = args.get("skip").and_then(|v| v.as_i64()).unwrap_or(0);
    let take = page_size(&args, 20);

    match network_of(&args) {
        Network::Vanilla => return vanilla::fetch_user_rooms(&user_id, skip, take).await,
        Network::Stella => return crate::stella_api::fetch_user_rooms(&user_id, skip, take).await,
        Network::RecEos => return crate::receos_api::fetch_user_rooms(&user_id, skip, take).await,
        Network::Radium => {}
    }

    let url = format!(
        "https://launcher.radie.app/api/user/v1/{}/rooms?skip={}&take={}",
        vanilla::urlencoding(&user_id),
        skip,
        take
    );

    match http_get_json(&url).await {
        Ok(data) => json!({ "success": true, "data": data }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

/// Fetch the feed for a specific user.
#[tauri::command]
pub async fn fetch_user_feed(args: Value) -> Value {
    let user_id = match args.get("userId") {
        // v.to_string() renders any JSON number verbatim — as_i64().unwrap()
        // would panic on a non-integer (e.g. float) userId.
        Some(v) if v.is_number() => v.to_string(),
        Some(v) if v.is_string() => v.as_str().unwrap().to_string(),
        _ => return json!({ "success": false, "error": "userId is required" }),
    };
    let skip = args.get("skip").and_then(|v| v.as_i64()).unwrap_or(0);
    let take = page_size(&args, 40);

    match network_of(&args) {
        // Vanilla publishes no activity feed. The UI hides the FEEDS tab, so
        // this is only reachable defensively.
        // Neither Vanilla nor Stella publishes a per-user activity feed, so the
        // FEEDS tab shows an empty state rather than erroring.
        Network::Vanilla | Network::Stella | Network::RecEos => {
            return json!({
                "success": true,
                "data": { "Results": [], "TotalResults": 0 }
            })
        }
        Network::Radium => {}
    }

    let url = format!(
        "https://launcher.radie.app/api/user/v1/{}/feed?skip={}&take={}",
        vanilla::urlencoding(&user_id),
        skip,
        take
    );

    match http_get_json(&url).await {
        Ok(data) => json!({ "success": true, "data": data }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

/// Warm a network's caches before the user opens a tab that needs them.
///
/// Fire-and-forget: nothing waits on it and it reports nothing, so the
/// frontend can call it once the launcher is idle after boot. Only Vanilla has
/// anything to warm — Radium's lists are server-paged and each page is small.
#[tauri::command]
pub async fn prefetch_network_data(network: Option<String>) {
    match Network::parse(network.as_deref()) {
        Network::Vanilla => vanilla::prefetch(),
        Network::Stella => crate::stella_api::prefetch_rooms(),
        Network::Radium | Network::RecEos => {}
    }
}

/// Fetch the recent photo feed.
#[tauri::command]
pub async fn fetch_recent_photos(args: Value) -> Value {
    let skip = args.get("skip").and_then(|v| v.as_i64()).unwrap_or(0);
    let take = page_size(&args, 100);

    match network_of(&args) {
        Network::Vanilla => {
            // An explicit Refresh must go back to the network rather than be
            // served the list the tab is already showing.
            if args.get("refresh").and_then(|v| v.as_bool()).unwrap_or(false) {
                vanilla::invalidate_feed().await;
            }
            return vanilla::fetch_recent_photos(skip, take).await;
        }
        Network::Stella => return no_public_api(),
        Network::RecEos => return crate::receos_api::fetch_recent_photos(skip, take).await,
        Network::Radium => {}
    }

    let url = format!(
        "https://launcher.radie.app/api/photos/v1/feed?skip={}&take={}",
        skip, take
    );

    match http_get_json(&url).await {
        Ok(data) => json!({ "success": true, "data": data }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}
