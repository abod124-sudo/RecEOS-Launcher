//! Rec EOS's public community API (`/web/community/*` on [`crate::receos::API_ROOT`]),
//! the same reads rec-eos.com shows, reshaped into the envelopes the page
//! already reads for the other networks (`{Results, TotalResults, TotalKnown}`,
//! PascalCase rooms and photos, camelCase people).
//!
//! Nothing here signs in. Players are named by username: the API filters a
//! player's rooms and photos by username, and its player search has no ids, so
//! a Rec EOS player's `id` in the page is their username.

use std::time::Duration;

use serde_json::{json, Value};

use crate::receos::{API_ROOT, USER_AGENT};

/// Rows the API sends per page: rooms 24, photos 20 (each asks for one more to
/// tell whether another page exists, and says so as `hasMore`).
const ROOMS_PAGE: i64 = 24;
const PHOTOS_PAGE: i64 = 20;
const PLAYERS_PAGE: i64 = 30;

/// Pages fetched at most to serve one request.
const MAX_PAGES: i64 = 6;

/// Largest answer read. A page is a few dozen rows.
const MAX_BYTES: usize = 2 * 1024 * 1024;

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .https_only(true)
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

async fn get_json(path_and_query: &str) -> Result<Value, String> {
    let res = http()
        .get(format!("{API_ROOT}{path_and_query}"))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach Rec EOS: {e}"))?;
    let status = res.status();
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("Rec EOS sent more than expected.".into());
    }
    let body: Value = serde_json::from_slice(&bytes).map_err(|_| format!("Rec EOS answered {status}."))?;
    if !status.is_success() {
        let msg = body["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("Rec EOS answered {status}."));
        return Err(msg);
    }
    Ok(body)
}

fn enc(s: &str) -> String {
    crate::vanilla::urlencoding(s)
}

/// An image the API names (`image_name`, `profile_image`), as an address. The
/// game's stand-in names have no file.
pub(crate) fn image_url(name: &str) -> String {
    let ok = name != "DefaultProfileImage"
        && (16..=68).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    if ok {
        format!("{API_ROOT}/files/{}", enc(name))
    } else {
        String::new()
    }
}

/// A username the API's player routes take.
fn valid_username(name: &str) -> bool {
    (3..=20).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn n(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// The rows for `skip..skip+take` of a list the API pages `page` at a time
/// from `?skip=`, and whether more follow.
async fn paged(path: &str, params: &str, skip: i64, take: i64, page: i64) -> Result<(Vec<Value>, bool), String> {
    let skip = skip.max(0);
    let take = take.max(1);
    let mut rows: Vec<Value> = Vec::new();
    let mut at = skip;
    let mut more = true;
    for _ in 0..MAX_PAGES {
        let sep = if params.is_empty() { "" } else { "&" };
        let body = get_json(&format!("{path}?{params}{sep}skip={at}")).await?;
        let items = body["items"].as_array().cloned().unwrap_or_default();
        more = body["hasMore"].as_bool().unwrap_or(false);
        at += items.len() as i64;
        rows.extend(items);
        if !more || rows.len() as i64 > take || at - skip >= page * MAX_PAGES {
            break;
        }
    }
    let extra = rows.len() as i64 > take;
    rows.truncate(take as usize);
    Ok((rows, more || extra))
}

fn envelope(rows: Vec<Value>, skip: i64, more: bool) -> Value {
    let shown = skip.max(0) + rows.len() as i64;
    json!({
        "success": true,
        "data": {
            "Results": rows,
            "TotalResults": if more { shown + 1 } else { shown },
            "TotalKnown": !more,
        }
    })
}

fn tags_of(room: &Value) -> Vec<String> {
    let raw = s(room, "tags");
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn room_row(r: &Value) -> Value {
    let creator = s(r, "username");
    json!({
        "RoomId": r.get("room_id").cloned().unwrap_or(Value::Null),
        "Name": s(r, "name"),
        "Description": s(r, "description"),
        "ImageName": s(r, "image_name"),
        "ThumbUrl": image_url(s(r, "image_name")),
        "CreatorUsername": if creator.is_empty() { "Unknown" } else { creator },
        "CreatorDisplayName": s(r, "display_name"),
        "CreatorPlayerId": creator,
        "CreatorAvatarUrl": image_url(s(r, "profile_image")),
        "CheerCount": n(r, "cheers"),
        "FavoriteCount": Value::Null,
        "VisitCount": n(r, "visits"),
        "ActivePlayerCount": 0,
        "MaxPlayers": r.get("capacity").cloned().unwrap_or(Value::Null),
        "CreatedAt": s(r, "created_at"),
        "Tags": tags_of(r),
        "Accessibility": "",
    })
}

fn person_row(p: &Value) -> Value {
    let username = s(p, "username");
    let display = s(p, "display_name");
    json!({
        "id": username,
        "userName": username,
        "displayName": if display.is_empty() { username } else { display },
        "bio": "",
        "profileImage": s(p, "profile_image"),
        "AvatarUrl": image_url(s(p, "profile_image")),
        // Only the online list says (its "online"); a search doesn't.
        "isOnline": p.get("online").and_then(Value::as_bool).map_or(Value::Null, Value::from),
        "level": p.get("level").cloned().unwrap_or(Value::Null),
        "isDeveloper": false,
        "isModerator": false,
        "isCommunityTeam": false,
    })
}

fn photo_row(p: &Value) -> Value {
    let username = s(p, "username");
    json!({
        "Id": p.get("id").cloned().unwrap_or(Value::Null),
        "ImageName": s(p, "image_name"),
        "ThumbUrl": image_url(s(p, "image_name")),
        "RoomId": p.get("room_id").cloned().unwrap_or(Value::Null),
        "RoomName": s(p, "room_name"),
        "CreatorPlayerId": username,
        "CreatorUsername": username,
        "CreatorDisplayName": if s(p, "display_name").is_empty() { username } else { s(p, "display_name") },
        "CreatorAvatarUrl": image_url(s(p, "profile_image")),
        "Description": "",
        "CheerCount": n(p, "cheers"),
        "CommentCount": Value::Null,
        "CreatedAt": s(p, "created_at"),
        "TaggedPlayers": [],
    })
}

// ─── Rooms ──────────────────────────────────────────────────────────────────

/// Rooms, newest first for sort 1, otherwise most cheered. A tag is searched
/// for like text (the API's search covers names, descriptions and tags) and
/// then checked against each room's tags.
pub async fn fetch_rooms(skip: i64, take: i64, query: &str, tag: &str, sort_by: i64) -> Value {
    let query = query.trim();
    let tag = tag.trim().to_lowercase();
    let q = if query.is_empty() { tag.as_str() } else { query };
    let mut params = format!("q={}", enc(q));
    if sort_by == 1 {
        params.push_str("&sort=new");
    }
    if tag.is_empty() || query.is_empty() {
        return match paged("/web/community/rooms", &params, skip, take, ROOMS_PAGE).await {
            Ok((rows, more)) => {
                let rows: Vec<Value> = rows
                    .iter()
                    .filter(|r| tag.is_empty() || tags_of(r).iter().any(|t| t.eq_ignore_ascii_case(&tag)))
                    .map(room_row)
                    .collect();
                envelope(rows, skip, more)
            }
            Err(e) => json!({ "success": false, "error": e }),
        };
    }
    // Both a search and a tag: the search goes to the API, and the tag is
    // applied to what comes back.
    match paged("/web/community/rooms", &params, 0, ROOMS_PAGE * MAX_PAGES, ROOMS_PAGE).await {
        Ok((rows, more)) => {
            let matched: Vec<Value> = rows
                .iter()
                .filter(|r| tags_of(r).iter().any(|t| t.eq_ignore_ascii_case(&tag)))
                .map(room_row)
                .collect();
            let page: Vec<Value> = matched.iter().skip(skip.max(0) as usize).take(take.max(1) as usize).cloned().collect();
            let after = (skip.max(0) + page.len() as i64) < matched.len() as i64;
            envelope(page, skip, after || more)
        }
        Err(e) => json!({ "success": false, "error": e }),
    }
}

/// The tag rail: the tags on the most popular rooms.
pub async fn fetch_filters() -> Value {
    let mut tally: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    if let Ok((rows, _)) = paged("/web/community/rooms", "q=", 0, ROOMS_PAGE * 4, ROOMS_PAGE).await {
        for r in &rows {
            for t in tags_of(r) {
                *tally.entry(t.to_lowercase()).or_insert(0) += 1;
            }
        }
    }
    let mut ranked: Vec<(String, usize)> = tally.into_iter().filter(|(_, c)| *c > 1).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let tags: Vec<String> = ranked.into_iter().map(|(t, _)| t).take(16).collect();
    json!({ "success": true, "data": { "PinnedFilters": [], "PopularFilters": tags } })
}

async fn room_by_name(name: &str) -> Result<Option<Value>, String> {
    let name = name.trim().trim_start_matches('^');
    let (rows, _) = paged("/web/community/rooms", &format!("q={}", enc(name)), 0, ROOMS_PAGE * 2, ROOMS_PAGE).await?;
    Ok(rows.into_iter().find(|r| s(r, "name").eq_ignore_ascii_case(name)))
}

pub async fn room_web_details(name: &str) -> Value {
    match room_by_name(name).await {
        Ok(Some(r)) => json!({
            "success": true,
            "cheers": n(&r, "cheers").to_string(),
            "favorites": "",
            "visits": n(&r, "visits").to_string(),
            "description": s(&r, "description"),
            "creatorAvatar": image_url(s(&r, "profile_image")),
        }),
        Ok(None) => json!({ "success": false, "error": "Room not found." }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

pub async fn fetch_user_rooms(username: &str, skip: i64, take: i64) -> Value {
    let username = username.trim();
    if !valid_username(username) {
        return json!({ "success": false, "error": "Invalid player." });
    }
    match paged("/web/community/rooms", &format!("q=&player={}", enc(&username.to_lowercase())), skip, take, ROOMS_PAGE).await {
        Ok((rows, more)) => envelope(rows.iter().map(room_row).collect(), skip, more),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

// ─── People ─────────────────────────────────────────────────────────────────

/// Player search. Without a search (or with one letter, which the API's
/// search doesn't take), players online now, then the most recently seen.
pub async fn fetch_people(skip: i64, take: i64, query: &str) -> Value {
    let query = query.trim();
    if query.chars().count() < 2 {
        return match paged("/web/community/online", "", skip, take, PLAYERS_PAGE).await {
            Ok((rows, more)) => envelope(rows.iter().map(person_row).collect(), skip, more),
            Err(e) => json!({ "success": false, "error": e }),
        };
    }
    match get_json(&format!("/web/community/players?q={}", enc(query))).await {
        Ok(body) => {
            let all: Vec<Value> = body["items"].as_array().cloned().unwrap_or_default();
            let total = all.len() as i64;
            let rows: Vec<Value> = all.iter().skip(skip.max(0) as usize).take(take.max(1) as usize).map(person_row).collect();
            json!({ "success": true, "data": { "Results": rows, "TotalResults": total, "TotalKnown": true } })
        }
        Err(e) => json!({ "success": false, "error": e }),
    }
}

/// A profile, in the shape `fetch_user_web_details` returns for every network.
pub async fn user_web_details(username: &str) -> Value {
    let username = username.trim();
    if !valid_username(username) {
        return json!({ "success": false, "error": "No such player." });
    }
    // The banner is the player's newest public photo, as rec-eos.com shows it.
    let profile_path = format!("/web/community/players/{}", enc(username));
    let photos_path = format!("/web/community/photos?player={}", enc(&username.to_lowercase()));
    let (profile, photos) = tokio::join!(get_json(&profile_path), get_json(&photos_path));
    let banner = photos
        .ok()
        .and_then(|p| p["items"].as_array().and_then(|a| a.first()).map(|x| image_url(s(x, "image_name"))))
        .unwrap_or_default();
    match profile {
        Ok(body) => {
            let a = &body["account"];
            let st = &body["stats"];
            let roles: Vec<String> = body["roles"]
                .as_array()
                .map(|r| r.iter().filter_map(|x| x.as_str().map(str::to_lowercase)).collect())
                .unwrap_or_default();
            json!({
                "success": true,
                "id": s(a, "username"),
                "accountId": a.get("accountId").cloned().unwrap_or(Value::Null),
                "userName": s(a, "username"),
                "displayName": s(a, "displayName"),
                "friends": n(st, "friends").to_string(),
                "subscribers": n(st, "subscribers").to_string(),
                "visits": "",
                "bio": s(a, "bio"),
                "banner": banner,
                "avatar": image_url(s(a, "profileImage")),
                "level": a.get("level").cloned().unwrap_or(Value::Null),
                "isOnline": st.get("online").and_then(Value::as_bool),
                // What the profile's status line reads. Online = public
                // presence in the last 22 minutes (the API's own rule).
                "status": match st.get("online").and_then(Value::as_bool) {
                    Some(true) => "ONLINE",
                    Some(false) => "OFFLINE",
                    None => "",
                },
                "isDeveloper": roles.iter().any(|r| r == "developer"),
                "isModerator": roles.iter().any(|r| r == "moderator"),
            })
        }
        Err(e) => json!({ "success": false, "error": e }),
    }
}

// ─── Photos ─────────────────────────────────────────────────────────────────

pub async fn fetch_user_photos(username: &str, skip: i64, take: i64) -> Value {
    let username = username.trim();
    if !valid_username(username) {
        return json!({ "success": false, "error": "Invalid player." });
    }
    photos(&format!("player={}", enc(&username.to_lowercase())), skip, take).await
}

pub async fn fetch_room_photos(room_id: &str, skip: i64, take: i64) -> Value {
    let Ok(id) = room_id.trim().parse::<i64>() else {
        return json!({ "success": false, "error": "Invalid room id." });
    };
    photos(&format!("room={id}"), skip, take).await
}

pub async fn fetch_recent_photos(skip: i64, take: i64) -> Value {
    photos("", skip, take).await
}

async fn photos(params: &str, skip: i64, take: i64) -> Value {
    match paged("/web/community/photos", params, skip, take, PHOTOS_PAGE).await {
        Ok((rows, more)) => envelope(rows.iter().map(photo_row).collect(), skip, more),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

// ─── Players online ─────────────────────────────────────────────────────────

pub async fn get_player_count() -> Value {
    match crate::receos::web_status().await {
        Ok(v) => match v["online"].as_i64() {
            Some(count) => json!({ "success": true, "count": count }),
            None => json!({ "success": false, "error": "Rec EOS didn't say how many are online." }),
        },
        Err(e) => json!({ "success": false, "error": e }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_built_only_for_real_names() {
        assert_eq!(
            image_url("7c2ee9c5e3dd16632d5741593bd5a7df"),
            "https://api.rec-eos.com/files/7c2ee9c5e3dd16632d5741593bd5a7df"
        );
        assert_eq!(image_url("DefaultProfileImage"), "");
        assert_eq!(image_url(""), "");
        assert_eq!(image_url("../../admin/secret-file-name"), "");
    }

    #[test]
    fn usernames_are_checked_before_they_go_in_a_path() {
        assert!(valid_username("tavindagoat"));
        assert!(!valid_username("a/b"));
        assert!(!valid_username(".."));
        assert!(!valid_username("ab"));
    }

    #[test]
    fn a_room_row_reads_the_api_shape() {
        let r = json!({"room_id": 1000206, "name": "RockBottom", "description": "d", "image_name": "9ec01fd5ee2ffc194eaa53361374e4e5",
            "tags": "[\"pvp\",\"art\"]", "cheers": 1, "visits": 3, "created_at": "2026-10-17T20:28:12.927Z",
            "username": "tavindagoat", "display_name": "Tavin", "profile_image": "DefaultProfileImage", "capacity": 12});
        let row = room_row(&r);
        assert_eq!(row["RoomId"], 1000206);
        assert_eq!(row["CreatorUsername"], "tavindagoat");
        assert_eq!(row["Tags"], json!(["pvp", "art"]));
        assert_eq!(row["CreatorAvatarUrl"], "");
    }

    #[test]
    fn the_envelope_says_when_more_follow() {
        let e = envelope(vec![json!(1), json!(2)], 10, true);
        assert_eq!(e["data"]["TotalResults"], 13);
        assert_eq!(e["data"]["TotalKnown"], false);
        let e = envelope(vec![json!(1)], 0, false);
        assert_eq!(e["data"]["TotalResults"], 1);
        assert_eq!(e["data"]["TotalKnown"], true);
    }
}
