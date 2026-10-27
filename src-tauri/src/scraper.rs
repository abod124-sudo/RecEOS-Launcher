use crate::config::Network;
use crate::vanilla;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helper: unescape common HTML entities
// ---------------------------------------------------------------------------
///
/// `&amp;` goes last. Decoded first, it turned the text `&lt;` — which a page
/// spells `&amp;lt;` — into a `<`, decoding what the author wrote as well as
/// the page's own escaping.
pub(crate) fn unescape_html(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&#x2F;", "/")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod unescape_tests {
    use super::unescape_html;

    #[test]
    fn escaped_entities_are_decoded_once() {
        assert_eq!(unescape_html("Tom &amp; Jerry &lt;3"), "Tom & Jerry <3");
        // The text "&lt;3", as a page has to write it.
        assert_eq!(unescape_html("I typed &amp;lt;3"), "I typed &lt;3");
        assert_eq!(unescape_html("&amp;amp;"), "&amp;");
    }
}

// ---------------------------------------------------------------------------
// Helper: resolve a possibly-relative URL against a base
// ---------------------------------------------------------------------------
fn resolve_url(url: &str, base: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    // Protocol-relative (`//img.radie.app/x`): another host, not a path on
    // this one — glued onto the base it became `https://www.radie.app//img...`.
    if let Some(rest) = url.strip_prefix("//") {
        return format!("https://{}", rest);
    }
    let base = base.trim_end_matches('/');
    if url.starts_with('/') {
        format!("{}{}", base, url)
    } else {
        format!("{}/{}", base, url)
    }
}

// ---------------------------------------------------------------------------
// Helper: perform an HTTP GET and return the body as text
// ---------------------------------------------------------------------------
async fn http_get_text(url: &str) -> Result<String, String> {
    // The process-wide client, so these scrapes reuse the existing connection
    // pool instead of paying for a fresh TLS handshake on every card the user
    // opens. reqwest's default redirect policy is the same limit of 10.
    let response = crate::server::http()
        .get(url)
        .timeout(Duration::from_secs(10))
        .header("User-Agent", "RadiumLauncher/1.0")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    // Checked like `server::http_get_json` and `vanilla::api_get_json` do.
    // Without it the body of a 404 or a Cloudflare interstitial went straight
    // into the patterns below, every capture missed, and the caller still
    // reported `success: true` with empty strings — so a room whose page
    // failed to load was indistinguishable from a room with no stats, and
    // nothing said so in the log.
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP error: {}", status));
    }

    // Capped: see `server::MAX_API_BYTES`.
    crate::server::read_text_capped(response).await
}

// ---------------------------------------------------------------------------
// Helper: extract the first capture group from a regex match
// ---------------------------------------------------------------------------

/// Compile `pattern` once and keep it for the life of the process.
///
/// Every pattern in this module is a constant, but `first_capture` used to call
/// `Regex::new` on each invocation — a full parse and DFA build per call. One
/// room detail view runs five of these and a user detail view eight or more,
/// several being the `[\s\S]*?` patterns that are the most expensive to
/// construct, so the compile dominated the parse it was there to do.
///
/// Usage is `re!(PATTERN)`, which declares the `OnceLock` at the call site so
/// each pattern gets its own slot without a separate `static` to keep in sync.
macro_rules! re {
    ($pattern:expr) => {{
        static CELL: std::sync::OnceLock<Option<Regex>> = std::sync::OnceLock::new();
        CELL.get_or_init(|| Regex::new($pattern).ok()).as_ref()
    }};
}

/// First capture group of `re` in `text`.
fn capture_of(re: Option<&Regex>, text: &str) -> Option<String> {
    re?.captures(text)?
        .get(1)
        .map(|m| m.as_str().to_string())
}

/// The three profile/room stat patterns, which differ only by their label.
///
/// Built from a label at runtime before, which meant they could not be
/// `re!`-ed like the rest. There are exactly three labels per page, so each
/// gets its own compiled pattern.
fn stat_capture(label: StatLabel, html: &str) -> String {
    fn pattern(label: &str) -> String {
        format!(
            r#"<p class="font-bold text-\[14px\]!"[^>]*>([\d,]+)</p>\s*<p class="text-\[10px\]">{}</p>"#,
            label
        )
    }
    static CACHE: std::sync::OnceLock<HashMap<&'static str, Option<Regex>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        StatLabel::ALL
            .iter()
            .map(|l| (l.as_str(), Regex::new(&pattern(l.as_str())).ok()))
            .collect()
    });

    cache
        .get(label.as_str())
        .and_then(|re| capture_of(re.as_ref(), html))
        .map(|v| unescape_html(&v))
        .unwrap_or_default()
}

/// Stat labels the two profile pages publish.
#[derive(Clone, Copy)]
enum StatLabel {
    Cheers,
    Favorites,
    Visits,
    Friends,
    Subscribers,
}

impl StatLabel {
    const ALL: [StatLabel; 5] = [
        StatLabel::Cheers,
        StatLabel::Favorites,
        StatLabel::Visits,
        StatLabel::Friends,
        StatLabel::Subscribers,
    ];

    fn as_str(self) -> &'static str {
        match self {
            StatLabel::Cheers => "CHEERS",
            StatLabel::Favorites => "FAVORITES",
            StatLabel::Visits => "VISITS",
            StatLabel::Friends => "FRIENDS",
            StatLabel::Subscribers => "SUBSCRIBERS",
        }
    }
}

// ---------------------------------------------------------------------------
// 1. fetch_room_web_details
// ---------------------------------------------------------------------------
#[tauri::command]
pub async fn fetch_room_web_details(name: String, network: Option<String>) -> Value {
    // Vanilla serves these stats from its API, so there is no page to scrape.
    match Network::parse(network.as_deref()) {
        Network::Vanilla => return vanilla::room_web_details(&name).await,
        Network::Stella => return crate::server::no_public_api(),
        Network::RecEos => return crate::receos_api::room_web_details(&name).await,
        Network::Radium => {}
    }
    let safe_name = vanilla::urlencoding(&name);
    let url = format!("https://www.radie.app/room/{}", safe_name);

    let html = match http_get_text(&url).await {
        Ok(h) => h,
        Err(e) => return json!({ "success": false, "error": e }),
    };

    let base = "https://www.radie.app";

    let description = capture_of(
        re!(r#"</a>\s*<p>([\s\S]*?)</p>\s*<div class="flex border-\[#ccc\] border-t"#),
        &html,
    )
    .map(|v| unescape_html(v.trim()))
    .unwrap_or_default();

    let creator_avatar = capture_of(
        re!(r#"href="/user/[^"]+"[^>]*>[\s\S]*?<img[^>]*src="([^"]+)""#),
        &html,
    )
    .map(|v| resolve_url(&unescape_html(&v), base))
    .unwrap_or_default();

    json!({
        "success": true,
        "cheers": stat_capture(StatLabel::Cheers, &html),
        "favorites": stat_capture(StatLabel::Favorites, &html),
        "visits": stat_capture(StatLabel::Visits, &html),
        "description": description,
        "creatorAvatar": creator_avatar,
    })
}

// ---------------------------------------------------------------------------
// 2. fetch_user_web_details
// ---------------------------------------------------------------------------
#[tauri::command]
pub async fn fetch_user_web_details(
    name: String,
    network: Option<String>,
    account_id: Option<i64>,
) -> Value {
    match Network::parse(network.as_deref()) {
        Network::Vanilla => return vanilla::user_web_details(&name).await,
        Network::RecEos => return crate::receos_api::user_web_details(&name).await,
        // Stella looks a profile up by account id. The id usually comes from
        // the row the profile was opened from; without one, the name is
        // resolved to an account first.
        Network::Stella => {
            let id = match account_id {
                Some(id) => Some(id),
                None => crate::stella_api::account_id_for_name(&name).await,
            };
            return match id {
                Some(id) => crate::stella_api::user_details(id).await,
                None => json!({ "success": false, "error": "No such player." }),
            };
        }
        Network::Radium => {}
    }
    let safe_name = vanilla::urlencoding(&name);
    let url = format!("https://www.radie.app/user/{}", safe_name);

    let html = match http_get_text(&url).await {
        Ok(h) => h,
        Err(e) => return json!({ "success": false, "error": e }),
    };

    let base = "https://www.radie.app";

    let friends = stat_capture(StatLabel::Friends, &html);
    let subscribers = stat_capture(StatLabel::Subscribers, &html);
    let visits = stat_capture(StatLabel::Visits, &html);

    // Status – extract user status from the profile card element
    let status_raw = capture_of(
        re!(r#"<p[^>]*class="[^"]*text-\[#ccc\][^"]*text-\[10px\][^"]*"[^>]*>([\s\S]*?)</p>"#),
        &html,
    )
    .unwrap_or_default();

    let status_inner = if status_raw.contains("<a") {
        capture_of(re!(r#">([^<]+)</a>"#), &status_raw).unwrap_or(status_raw)
    } else {
        status_raw
    };

    let status_clean = status_inner
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_uppercase();

    let status = if status_clean == "^" {
        "ONLINE".to_string()
    } else {
        status_clean
    };

    let bio = capture_of(
        re!(r#"<p class="whitespace-pre-wrap text-\[12px\]">([\s\S]*?)</p>"#),
        &html,
    )
    .map(|v| unescape_html(v.trim()))
    .unwrap_or_default();

    let banner = capture_of(
        re!(r#"background-image:\s*url\(['"]?([^'"\)]+)['"]?\)"#),
        &html,
    )
    .map(|v| resolve_url(&unescape_html(&v), base))
    .unwrap_or_default();

    // Try scraping avatar from og:image meta tag first, then fallback to img tags containing w-18.75 class
    let avatar = capture_of(
        re!(r#"<meta[^>]*property="og:image"[^>]*content="([^"]+)""#),
        &html,
    )
    .or_else(|| {
        capture_of(
            re!(r#"<meta[^>]*content="([^"]+)"[^>]*property="og:image""#),
            &html,
        )
    })
    .or_else(|| {
        capture_of(
            re!(r#"<img[^>]*class="[^"]*w-18\.75[^"]*"[^>]*src="([^"]+)""#),
            &html,
        )
    })
    .or_else(|| {
        capture_of(
            re!(r#"<img[^>]*src="([^"]+)"[^>]*class="[^"]*w-18\.75"#),
            &html,
        )
    })
    .or_else(|| capture_of(re!(r#"w-18\.75[\s\S]*?<img[^>]*src="([^"]+)""#), &html))
    .map(|v| resolve_url(&unescape_html(&v), base))
    .unwrap_or_default();


    json!({
        "success": true,
        "friends": friends,
        "subscribers": subscribers,
        "visits": visits,
        "status": status,
        "bio": bio,
        "banner": banner,
        "avatar": avatar,
    })
}

// ---------------------------------------------------------------------------
// 3. fetch_photo_web_details
// ---------------------------------------------------------------------------
#[tauri::command]
pub async fn fetch_photo_web_details(photo_id: String, network: Option<String>) -> Value {
    match Network::parse(network.as_deref()) {
        Network::Vanilla => {
            return json!({ "success": false, "error": "Vanilla does not publish photo details." })
        }
        Network::Stella | Network::RecEos => return crate::server::no_public_api(),
        Network::Radium => {}
    }
    let safe_photo_id = vanilla::urlencoding(&photo_id);
    let url = format!("https://www.radie.app/photo/{}", safe_photo_id);

    let html = match http_get_text(&url).await {
        Ok(h) => h,
        Err(e) => return json!({ "success": false, "error": e }),
    };

    let creator_username = capture_of(re!(r#"href="/user/([^"\s?]+)""#), &html).unwrap_or_default();
    let room_name = capture_of(re!(r#"href="/room/([^"\s?]+)""#), &html).unwrap_or_default();

    json!({
        "success": true,
        "creatorUsername": creator_username,
        "roomName": room_name,
    })
}

// ---------------------------------------------------------------------------
// 4. fetch_photo_comments
// ---------------------------------------------------------------------------
#[tauri::command]
pub async fn fetch_photo_comments(photo_id: String, network: Option<String>) -> Value {
    match Network::parse(network.as_deref()) {
        Network::Vanilla => {
            return json!({ "success": false, "error": "Vanilla does not publish photo comments." })
        }
        Network::Stella | Network::RecEos => return crate::server::no_public_api(),
        Network::Radium => {}
    }
    let urls = vec![
        format!(
            "https://launcher.radie.app/api/photos/v1/{}/comments?skip=0&take=20",
            vanilla::urlencoding(&photo_id)
        ),
        format!(
            "https://launcher.radie.app/api/comments/v1?photoId={}&skip=0&take=20",
            vanilla::urlencoding(&photo_id)
        ),
        format!(
            "https://api.radie.app/api/photos/v1/{}/comments?skip=0&take=20",
            vanilla::urlencoding(&photo_id)
        ),
    ];

    for url in &urls {
        let body = match http_get_text(url).await {
            Ok(b) => b,
            Err(_) => continue,
        };

        let parsed: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // If the response is directly an array, use it.
        if parsed.is_array() {
            return json!({ "success": true, "comments": parsed });
        }

        // Check for a "Results" field
        if let Some(results) = parsed.get("Results") {
            if results.is_array() {
                return json!({ "success": true, "comments": results });
            }
        }

        // Check for a "comments" field
        if let Some(comments) = parsed.get("comments") {
            if comments.is_array() {
                return json!({ "success": true, "comments": comments });
            }
        }
    }

    json!({
        "success": false,
        "error": "All comment API endpoints failed or returned unexpected data",
        "comments": [],
    })
}
