pub mod applog;
pub mod background;
pub mod bugreport;
pub mod config;
pub mod defender;
pub mod desktop_notify;
pub mod download;
pub mod frost;
pub mod game;
pub mod receos;
pub mod receos_api;
pub mod receos_social;
pub mod scraper;
pub mod server;
pub mod stella;
pub mod stella_api;
pub mod stella_hub;
pub mod thumbs;
pub mod updater;
pub mod verify;
pub mod vanilla;
pub mod vanilla_auth;
pub mod vanilla_game;
pub mod vanilla_launcher;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Enforce a single running instance. Must be registered before any other
        // plugin. When the user launches the launcher again while one is already
        // running, this fires in the existing process instead of opening a second
        // window — we restore and focus the current window so it comes to front.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // Also how a launcher hidden in the tray comes back when it is
            // started again from the Start menu or a shortcut. Not when the
            // second start is the startup entry itself: that one asked to
            // stay in the tray.
            if !args.iter().any(|a| a == background::BACKGROUND_ARG) {
                background::show_main(app);
            }
        }))
        .plugin(tauri_plugin_shell::init())
        // Every remote image in the UI is loaded through here rather than
        // straight from its origin, so it arrives downscaled to the size the
        // card draws it at and is kept on disk. See the `thumbs` module for
        // what that is worth: Vanilla's room images are 2560x1440 PNGs served
        // with no cache headers at all.
        //
        // Asynchronous, so a slow origin blocks only its own `<img>` rather
        // than the webview's main thread.
        .register_asynchronous_uri_scheme_protocol("radiumimg", |_ctx, request, responder| {
            let target = thumb_request(request.uri());
            tauri::async_runtime::spawn(async move {
                responder.respond(match target {
                    Some((url, width)) => match thumbs::thumbnail(&url, width).await {
                        // The content type comes back with the bytes: a
                        // thumbnail is JPEG unless the source needed an alpha
                        // channel, in which case it stays PNG.
                        Ok((bytes, mime)) => tauri::http::Response::builder()
                            .status(200)
                            .header("Content-Type", mime)
                            // A custom scheme is a separate origin from the
                            // page, so this matches what Tauri's own asset
                            // protocol sends. `<img>` does not need it, but
                            // anything that ever reads one of these with
                            // fetch() would.
                            .header("Access-Control-Allow-Origin", "*")
                            // The bytes are already keyed by URL and width and
                            // are re-derived on a miss, so letting the webview
                            // hold them saves even the file read.
                            .header("Cache-Control", "public, max-age=86400")
                            .body(bytes)
                            .unwrap_or_else(|_| empty_response(500)),
                        // The `<img>` fires `error` and the shared handler in
                        // app.js swaps in the bundled placeholder.
                        Err(_) => empty_response(502),
                    },
                    None => empty_response(400),
                });
            });
        })
        .invoke_handler(tauri::generate_handler![
            // Config
            cmd_get_config,
            cmd_save_config,
            cmd_set_glass_backdrop,
            cmd_fetch_glass_backdrop,
            cmd_set_home_banner,
            // Server / Data
            server::ping_server,
            server::get_player_count,
            server::fetch_rooms,
            server::fetch_people,
            server::fetch_filters,
            server::fetch_user_photos,
            server::fetch_room_photos,
            server::fetch_user_rooms,
            server::fetch_user_inventions,
            server::fetch_user_feed,
            server::fetch_recent_photos,
            server::prefetch_network_data,
            // Scraper
            scraper::fetch_room_web_details,
            scraper::fetch_user_web_details,
            scraper::fetch_photo_web_details,
            scraper::fetch_photo_comments,
            // Vanilla account
            vanilla_auth::vanilla_login,
            vanilla_auth::vanilla_logout,
            vanilla_auth::vanilla_auth_status,
            vanilla_auth::vanilla_account,
            vanilla_auth::vanilla_notifications,
            vanilla_auth::vanilla_room_cheered,
            vanilla_auth::vanilla_set_room_cheer,
            vanilla_auth::vanilla_cheered_photos,
            vanilla_auth::vanilla_toggle_photo_cheer,
            vanilla_auth::vanilla_subscribed,
            vanilla_auth::vanilla_set_subscribed,
            vanilla_auth::vanilla_join_room,
            vanilla_auth::vanilla_current_room,
            // Background / startup
            background::get_autostart,
            background::set_autostart,
            background::set_tray_state,
            background::show_launcher,
            background::tray_menu_state,
            background::tray_menu_show,
            background::tray_menu_hide,
            background::tray_menu_pick,
            background::tray_menu_warm,
            // Desktop notification pop-up
            desktop_notify::desktop_notify,
            desktop_notify::desktop_notif_take,
            desktop_notify::desktop_notif_layout,
            desktop_notify::desktop_notif_open,
            // Download / Install
            download::download_client,
            download::cancel_download,
            download::pause_download,
            download::resumable_download_info,
            download::uninstall_client,
            download::check_install,
            download::check_client_update,
            download::install_vanilla_launcher,
            download::open_client_folder,
            download::select_folder,
            download::get_default_client_dir,
            download::resolve_client_dir,
            verify::verify_client,
            verify::repair_client,
            // Game
            game::launch_game,
            game::autoplay_vanilla,
            vanilla_launcher::vanilla_launcher_settings,
            vanilla_launcher::set_vanilla_launcher_setting,
            vanilla_launcher::open_vanilla_install_folder,
            vanilla_launcher::vanilla_launcher_update_status,
            vanilla_launcher::update_vanilla_launcher,
            game::kill_game,
            game::check_game_running,
            game::check_steam,
            game::check_required_steam_app,
            game::check_smart_app_control,
            // Stella's patch
            stella::stella_patch_status,
            stella::stella_update_patch,
            receos::receos_patch_status,
            receos::receos_update_patch,
            receos::receos_server_status,
            receos_social::receos_auth_status,
            receos_social::receos_login,
            receos_social::receos_logout,
            receos_social::receos_friends,
            receos_social::receos_set_friend_favorite,
            receos_social::receos_accept_friend_request,
            receos_social::receos_send_friend_request,
            receos_social::receos_remove_friend,
            receos_social::receos_notifications,
            receos_social::receos_updates,
            receos_social::receos_chat_threads,
            receos_social::receos_chat_messages,
            receos_social::receos_chat_send,
            receos_social::receos_chat_open,
            receos_social::receos_chat_read,
            receos_social::receos_room_interaction,
            receos_social::receos_set_room_interaction,
            // Stella account
            stella_api::stella_auth_status,
            stella_api::stella_login,
            stella_api::stella_logout,
            stella_api::stella_accounts,
            stella_api::stella_use_account,
            stella_api::stella_set_in_use,
            stella_api::stella_room_players,
            stella_api::stella_join_check,
            stella_api::stella_request_join,
            stella_api::stella_ask_to_join,
            stella_api::stella_notifications,
            stella_api::stella_tokens,
            stella_api::stella_cheered_photos,
            stella_api::stella_set_photo_cheer,
            stella_api::stella_room_interaction,
            stella_api::stella_set_room_interaction,
            stella_hub::stella_friends,
            stella_hub::stella_friends_stop,
            stella_hub::stella_presence,
            stella_hub::stella_set_friend_favorite,
            stella_hub::stella_accept_friend_request,
            // Defender
            defender::add_defender_exclusion,
            defender::remove_defender_exclusion,
            defender::detect_antivirus,
            // Updater
            updater::check_for_update,
            updater::download_update,
            updater::get_version,
            // Logs and bug reports
            applog::log_append,
            applog::log_backend_ready,
            applog::log_previous,
            applog::log_open_folder,
            applog::log_save,
            bugreport::bug_report_preview,
            bugreport::bug_report_cooldown,
            bugreport::submit_bug_report,
        ])
        .setup(|app| {
            let app_handle = app.handle().clone();

            // This session's log file, the last one moved aside for a bug
            // report to attach. First, so a panic anywhere below is written.
            if let Ok(dir) = app.path().app_local_data_dir() {
                applog::init(dir.join("logs"));
            }
            applog::set_app(app_handle.clone());

            // The scheme handler above runs without an app handle, so the
            // thumbnail directory has to be resolved here and handed over.
            // Under the cache dir rather than app data: losing it costs a
            // refetch, and nothing in it is worth backing up.
            thumbs::init(
                app.path()
                    .app_cache_dir()
                    .unwrap_or_else(|_| std::env::temp_dir().join("radium-launcher"))
                    .join("thumbs"),
            );

            // Whether the user logged out of Stella, which outlives a restart,
            // and the window Stella checks is on screen before using Steam.
            if let Ok(dir) = app.path().app_local_data_dir() {
                stella_api::init(app_handle.clone(), dir);
            }

            // Start game monitoring background task
            game::start_game_monitor(app_handle.clone());

            // Give back the Vanilla bulk sets once nothing has read them for a
            // while. See `vanilla::BULK_IDLE_EVICT`.
            vanilla::start_idle_eviction();

            // Tray icon. Not fatal: without it the launcher still works, it
            // just can't be reopened from the tray.
            let _ = background::setup_tray(&app_handle);
            background::apply_startup_default(&app_handle);
            if let Some(main) = app.get_webview_window("main") {
                background::sharpen_window_icon(&main);
                background::round_window_corners(&main);
            }

            // The window is created hidden (tauri.conf.json). Started with
            // Windows, it stays in the tray; otherwise it is shown now.
            if !background::started_in_background() {
                background::show_main(&app_handle);
            }

            // Listen for window maximize/unmaximize events
            if let Some(window) = app.get_webview_window("main") {
                let window_clone = window.clone();
                let popup_app = app_handle.clone();
                window.on_window_event(move |event| {
                    use tauri::Emitter;
                    match event {
                        tauri::WindowEvent::Resized(_) => {
                            if let Ok(maximized) = window_clone.is_maximized() {
                                let _ = window_clone.emit("window-maximized-state", maximized);
                            }
                        }
                        // The notification pop-up is a separate, usually hidden
                        // window; left open it would keep the app running.
                        tauri::WindowEvent::CloseRequested { api, .. } => {
                            if background::hide_instead_of_close(&popup_app) {
                                api.prevent_close();
                            }
                        }
                        tauri::WindowEvent::Destroyed => {
                            desktop_notify::close_popup(&popup_app);
                            background::close_tray_menu(&popup_app);
                        }
                        _ => {}
                    }
                });
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|_app, event| {
            // Every way out — the window, the tray's Quit, an update's
            // restart — passes through here, so a session file without the
            // end marker is one that was cut off.
            if let tauri::RunEvent::Exit = event {
                applog::end_session();
            }
        });
}

/// The image URL and width a `radiumimg://` request is asking for.
///
/// Tauri hands the request as an absolute URI, which on Windows is
/// `http://radiumimg.localhost/thumb?...` and elsewhere `radiumimg://thumb?...`
/// — so only the query is read, and the host and path are ignored rather than
/// matched against a platform-specific shape.
///
/// Returns `None` for anything malformed; the URL itself is checked against the
/// allowed image hosts in `thumbs`, not here.
fn thumb_request(uri: &tauri::http::Uri) -> Option<(String, u32)> {
    let query = uri.query()?;
    let mut url = None;
    let mut width = None;

    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "url" => url = Some(percent_decode(value)),
            "w" => width = value.parse::<u32>().ok(),
            _ => {}
        }
    }
    Some((url?, width?))
}

/// Undo the `encodeURIComponent` the frontend applied to the image URL.
///
/// Decoding works over the bytes throughout. Re-slicing the `&str` by byte
/// index instead — `&s[i + 1..i + 3]` — panics whenever those indices land
/// inside a multi-byte character, which a `%` followed by one ASCII byte and
/// then any non-ASCII character produces. That panic happens on a spawned task
/// in the `radiumimg:` scheme handler, and the release profile sets
/// `panic = "abort"`, so it would take the whole launcher down rather than fail
/// one image. See `a_stray_percent_before_a_multibyte_char_does_not_panic`.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = hex_pair(bytes[i + 1], bytes[i + 2]) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The byte two ASCII hex digits spell, or `None` if either isn't one.
fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    Some(digit(hi)? * 16 + digit(lo)?)
}

#[cfg(test)]
mod thumb_request_tests {
    use super::*;

    #[test]
    fn a_stray_percent_before_a_multibyte_char_does_not_panic() {
        // The exact shape that used to abort the process: a '%' followed by one
        // ASCII byte and then a character whose bytes straddle the index the
        // old `&s[i + 1..i + 3]` slice asked for.
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%é"), "%é");
        assert_eq!(percent_decode("%%é"), "%%é");
        assert_eq!(percent_decode("https://x/ü?%zz"), "https://x/ü?%zz");
    }

    #[test]
    fn ordinary_escapes_still_decode() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fapi.vanillarec.net%2Fimages%2Fa_b"),
            "https://api.vanillarec.net/images/a_b"
        );
        // Lower and upper case hex, and a multi-byte character that was encoded
        // properly, both round-trip.
        assert_eq!(percent_decode("%c3%a9%C3%A9"), "éé");
        // A truncated escape at the very end is passed through, not consumed.
        assert_eq!(percent_decode("abc%4"), "abc%4");
        assert_eq!(percent_decode("%"), "%");
    }

    #[test]
    fn a_malformed_query_is_refused_rather_than_guessed_at() {
        let parse = |q: &str| {
            thumb_request(&format!("http://radiumimg.localhost/thumb?{}", q).parse().unwrap())
        };
        assert_eq!(
            parse("url=https%3A%2F%2Fimg.radie.app%2FRoom_1&w=480"),
            Some(("https://img.radie.app/Room_1".to_string(), 480))
        );
        assert_eq!(parse("url=https%3A%2F%2Fimg.radie.app%2FRoom_1"), None);
        assert_eq!(parse("w=480"), None);
        assert_eq!(parse("url=x&w=notanumber"), None);
    }
}

fn empty_response(status: u16) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(status)
        .body(Vec::new())
        .expect("a status-only response is always well-formed")
}

// Config commands - thin wrappers that pass the app handle
#[tauri::command(async)]
fn cmd_get_config(app: tauri::AppHandle) -> serde_json::Value {
    let cfg = config::current(&app);
    serde_json::to_value(&*cfg).unwrap_or(serde_json::json!({}))
}

#[tauri::command(async)]
fn cmd_save_config(app: tauri::AppHandle, config: serde_json::Value) -> bool {
    match serde_json::from_value::<config::Config>(config) {
        Ok(mut cfg) => {
            // Only reject characters that are illegal in Windows paths anyway
            // (plus control chars). Legal folder names like "Games & Mods" or
            // "100%" must be saveable; the launch path is explicitly quoted at
            // spawn time, so shell metacharacters in the path are inert.
            let bad_dir = |dir: &str| {
                dir.chars().any(|c| c.is_control())
                    || dir.contains('"')
                    || dir.contains('<')
                    || dir.contains('>')
                    || dir.contains('|')
            };
            // Every network's install dir is user-settable, so all get checked.
            if config::Network::ALL.into_iter().any(|n| bad_dir(cfg.install_dir_for(n))) {
                return false;
            }
            config::drop_relative_install_dirs(&mut cfg);

            // The glass tint and backdrop become CSS in a generated
            // stylesheet, so anything that isn't one is repaired on the way in
            // rather than stored and handed back to the renderer next load.
            cfg.glass.sanitize();
            // Home's section order and hidden list: known names only.
            cfg.home.sanitize();

            // Only skins that actually ship. Without this a save could pin the
            // window to a class with no rules behind it.
            if !config::AVAILABLE_THEMES.contains(&cfg.theme.as_str()) {
                cfg.theme = config::DEFAULT_THEME.to_string();
            }
            if !config::AVAILABLE_THEMES.contains(&cfg.baseline_theme.as_str()) {
                cfg.baseline_theme = cfg.theme.clone();
            }

            // Preserve backend-managed fields from the on-disk config. The
            // settings UI keeps a full in-memory copy of the config and writes
            // the whole thing back on every autosave, but it loads that copy
            // once at startup and never learns about fields the backend writes
            // afterwards (e.g. the client build id / version / ETag stamped in
            // by a download, or the one-time version-sync flag). Without this,
            // a stale settings save silently reverts those to their defaults —
            // which reported a freshly-downloaded client as "outdated" on the
            // very next check, causing an endless re-download loop.
            let _lock = config::write_lock();
            // Refused while config.json can't be read: `current` would be the
            // stand-in defaults, and preserving *their* backend fields would
            // blank the client build and version the file really holds.
            let Ok(current) = config::current_checked(&app) else {
                return false;
            };
            cfg.preserve_backend_managed_fields(&current);

            // Last line of defence for the per-network install dirs: whatever
            // the settings form sends, the two networks must never end up
            // resolving to the same client folder — that is what let a Radium
            // download land in the Vanilla folder and made Vanilla report an
            // install it never had.
            config::dedupe_install_dirs(&app, &mut cfg);

            config::save_config(&app, &cfg).is_ok()
        }
        Err(_) => false,
    }
}

/// Set the Liquid Glass backdrop, and nothing else.
///
/// Its own command because the value can be a 1.4 MB data URI: whole-config
/// saves leave it out and keep the stored one (see
/// `preserve_backend_managed_fields`), so an autosave no longer ships and
/// re-parses it. Returns the value as stored, which is blank if it was not a
/// backdrop that is safe to put in the stylesheet.
#[tauri::command(async)]
fn cmd_set_glass_backdrop(app: tauri::AppHandle, value: String) -> Result<String, String> {
    config::update(&app, |cfg| {
        cfg.glass.bg_image = value;
        cfg.glass.sanitize();
        cfg.glass.bg_image.clone()
    })
}

/// Set `network`'s Home banner picture, and nothing else.
///
/// Its own command for the same reason as the glass backdrop: a banner is a
/// data URI of up to a megabyte that whole-config saves leave out. Returns
/// the value as stored, blank if it was not a picture that is safe to paint
/// (see `config::HomeSettings::sanitize`); blank also means "the network's
/// own art".
#[tauri::command(async)]
fn cmd_set_home_banner(app: tauri::AppHandle, network: String, value: String) -> Result<String, String> {
    let network = config::Network::parse(Some(&network));
    config::update(&app, |cfg| {
        *cfg.home.banners.for_network_mut(network) = value;
        cfg.home.sanitize();
        cfg.home.banners.for_network_mut(network).clone()
    })
}

/// Download a picture for the glass backdrop from an address the user typed.
///
/// Handed back as raw bytes (an `ArrayBuffer` on the page), which the page
/// downscales and saves through [`cmd_set_glass_backdrop`] like a picked file.
/// See `thumbs::backdrop_source` for why the page can't load the address
/// itself.
#[tauri::command]
async fn cmd_fetch_glass_backdrop(url: String) -> Result<tauri::ipc::Response, String> {
    thumbs::backdrop_source(&url).await.map(tauri::ipc::Response::new)
}

#[cfg(test)]
mod csp_tests {
    /// The CSP is written twice — `app.security.csp` in `tauri.conf.json`, and a
    /// `<meta http-equiv>` in `index.html` — and a browser enforces the
    /// *intersection* of the two. A source added to only one is therefore still
    /// blocked, silently, and only at runtime. That is exactly how the
    /// `radiumimg:` thumbnail scheme first shipped: allowed in the config,
    /// missing from the meta tag, so every room image failed to load.
    #[test]
    fn the_two_copies_of_the_csp_agree() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("the config is JSON");
        let configured = conf["app"]["security"]["csp"]
            .as_str()
            .expect("tauri.conf.json declares a csp");

        let html = include_str!("../../src/index.html");
        let meta = html
            .split_once(r#"http-equiv="Content-Security-Policy" content=""#)
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(csp, _)| csp)
            .expect("index.html declares a CSP meta tag");

        // Compared directive by directive: the two differ in whitespace and in
        // whether they end with a `;`, neither of which changes the policy.
        let directives = |csp: &str| {
            let mut parts: Vec<String> = csp
                .split(';')
                .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|d| !d.is_empty())
                .collect();
            parts.sort();
            parts
        };

        assert_eq!(
            directives(configured),
            directives(meta),
            "the CSP in tauri.conf.json and the one in src/index.html have drifted; \
             whichever source is missing from either is blocked at runtime"
        );
    }
}
