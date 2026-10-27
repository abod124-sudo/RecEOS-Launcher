// ─── Log ─────────────────────────────────────────────────────────────────────
// Every line the launcher logs, kept three ways: here for the Logs page
// (logs.js draws it) and for bug reports; and on disk, a file per session,
// through applog.rs — so a report can still attach the session that froze or
// crashed after the launcher has been restarted.
//
// First in this file, and `var` throughout: `var` has no temporal dead zone,
// so anything from here on can log. Startup is when the launcher is most
// likely to break (missing runtime, bad config, denied permissions), and a
// failure there used to leave no trace at all.

var LOG_LEVELS = { info: 'INFO', ok: 'OK', warn: 'WARN', error: 'ERROR' };
/// Where a line comes from. The Logs page filters on it, and it is written
/// into the line so a log read outside the launcher says the same.
var LOG_SOURCES = {
  launcher: 'LAUNCHER',  // the launcher itself: startup, settings, themes
  update:   'UPDATE',    // launcher updates
  server:   'SERVER',    // server status, player counts, lookups
  account:  'ACCOUNT',   // signing in to Vanilla and Stella
  install:  'INSTALL',   // download, install, verify, uninstall
  game:     'GAME',      // launching, antivirus checks, the running game
};
/// How many lines the page keeps. The file on disk keeps the whole session.
var LOG_MAX = 2000;
var logEntries = [];     // { seq, time, level, source, msg }, oldest first
var logSeq = 0;
var logListeners = [];   // called with each new entry (logs.js)
var logDiskQueue = [];
var logDiskTimer = null;
var logSessionStart = new Date();

function logPad2(n) { return String(n).padStart(2, '0'); }
function logClock(d = new Date()) {
  return `${logPad2(d.getHours())}:${logPad2(d.getMinutes())}:${logPad2(d.getSeconds())}`;
}
function logDate(d = new Date()) {
  return `${d.getFullYear()}-${logPad2(d.getMonth() + 1)}-${logPad2(d.getDate())}`;
}

/// An entry as one line of text, the way it is saved, copied and sent:
/// `[17:27:18] ERROR  ACCOUNT   Sign-in failed`. Padded with spaces, which
/// is the only alignment a text file has; a message's own line breaks are
/// indented under its start.
function logLineText(e) {
  const head = `[${e.time}] ${LOG_LEVELS[e.level].padEnd(5)}  ${LOG_SOURCES[e.source].padEnd(8)}  `;
  return head + e.msg.split('\n').join('\n' + ' '.repeat(head.length));
}

/// Log a line. `type` is a key of LOG_LEVELS, `source` a key of LOG_SOURCES.
function addLog(msg, type = 'info', source = 'launcher') {
  const entry = {
    seq: ++logSeq,
    time: logClock(),
    level: LOG_LEVELS[type] ? type : 'info',
    source: LOG_SOURCES[source] ? source : 'launcher',
    msg: String(msg).replace(/\r\n?/g, '\n'),
  };
  logEntries.push(entry);
  if (logEntries.length > LOG_MAX) logEntries.splice(0, logEntries.length - LOG_MAX);

  logDiskQueue.push(logLineText(entry));
  if (!logDiskTimer) logDiskTimer = setTimeout(flushLogToDisk, 700);

  for (const fn of logListeners) {
    try { fn(entry); } catch (e) { /* a broken view must not stop logging */ }
  }
}

/// Hand the queued lines to applog.rs. Straight through Tauri's global rather
/// than the window.radium shim below, which doesn't exist yet when the first
/// lines are logged.
function flushLogToDisk() {
  logDiskTimer = null;
  if (!logDiskQueue.length) return;
  const invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
  if (!invoke) { logDiskTimer = setTimeout(flushLogToDisk, 1000); return; }
  const lines = logDiskQueue.splice(0, logDiskQueue.length);
  invoke('log_append', { lines }).catch(() => {});
}
// A reload (Ctrl+R) restarts the page but not the session file; mark where.
logDiskQueue.push(`===== Session started ${logDate(logSessionStart)} ${logClock(logSessionStart)} =====`);
window.addEventListener('pagehide', flushLogToDisk);

/// Lines the backend logs (applog::backend): the address and server answer
/// behind a failed Stella request, a moved service, a dropped connection.
/// The ones from before this page was listening come from log_backend_ready.
(async function listenForBackendLog() {
  const tauri = window.__TAURI__;
  if (!tauri || !tauri.event || !tauri.core) return;
  const take = (line) => { if (line && line.msg) addLog(line.msg, line.level, line.source); };
  try {
    await tauri.event.listen('backend-log', (e) => take(e.payload));
    for (const line of (await tauri.core.invoke('log_backend_ready')) || []) take(line);
  } catch (e) { /* the page's own lines are still logged */ }
})();

// ─── Uncaught fault capture ──────────────────────────────────────────────────
var seenFaults = new Set();

function recordFault(label, message, where) {
  // Deduped: a fault inside a render or polling loop would otherwise flood the
  // log and push out the history that explains it.
  const key = `${label}|${message}|${where || ''}`;
  if (seenFaults.has(key)) return;
  if (seenFaults.size > 50) seenFaults.clear();
  seenFaults.add(key);
  addLog(`${label}: ${message}${where ? ` (${where})` : ''}`, 'error');
}

window.addEventListener('error', (e) => {
  // Failed <img>/<script> loads fire this too, with the element as the target.
  // The UI has its own onerror fallbacks for those, so they are noise here.
  if (e && e.target && e.target !== window && e.target.tagName) return;
  const file = e && e.filename ? e.filename.split('/').pop() : '';
  const where = file ? `${file}:${e.lineno}:${e.colno}` : '';
  recordFault('Uncaught error', (e && e.message) || String((e && e.error) || 'unknown'), where);
});

window.addEventListener('unhandledrejection', (e) => {
  const r = e && e.reason;
  const msg = (r && (r.message || (typeof r === 'string' ? r : r.toString()))) || 'unknown';
  recordFault('Unhandled promise rejection', msg);
});

// ─── Tauri v2 Compatibility Shim ───────────────────────────────────────────────
// Recreates the window.radium API from the Electron preload bridge using Tauri APIs.
// This allows the rest of app.js to remain unchanged.
// ── Network selection ────────────────────────────────────────────────────
// Which revival the launcher is pointed at. Read by the IPC shim below so every
// backend call carries it, instead of threading the value through ~30 call
// sites. Set from config at startup by setNetwork().
let activeNetwork = 'receos';

/// What the backend was last told about whether Stella is in use (see
/// syncStellaInUse()). Up here because applyNetworkUI() runs during startup,
/// before the rest of this file has been evaluated.
let stellaInUseSent = null;

/// Whether the tray panel is up (the backend says so: `tray-panel`). It shows
/// the launcher's live state, so while it is up the launcher is being looked
/// at even from the tray. See onTrayPanel.
let trayPanelOpen = false;

/// Tell the backend whether Stella is in use: the network picked, in a window
/// on screen (or in the tray panel). Only then does it sign in with Steam
/// (which Steam shows to your friends as playing Rec Room) or keep Stella's
/// live friends connection open; hidden in the tray or on another network, it
/// does neither. See stella_api::IN_USE.
function syncStellaInUse() {
  const inUse = activeNetwork === 'stella' && (!document.hidden || trayPanelOpen);
  if (inUse === stellaInUseSent) return;
  stellaInUseSent = inUse;
  window.radium?.stellaSetInUse?.(inUse);
}

// Per-network descriptors. `capabilities` records what a network's API can
// actually do — Vanilla has no activity feed and no presence — so the UI hides
// controls rather than showing dead ones.
const NETWORKS = {
  // The main network, so first: the switcher, the tray and Settings list
  // the networks in this order.
  receos: {
    label: 'REC EOS',
    logo: 'assets/receos-icon.png',
    site: 'https://rec-eos.com/',
    downloadPage: 'https://rec-eos.com/discord',
    // Images come back as absolute addresses (receos_api::image_url).
    imageBase: '',
    // Rooms, people, profiles and photos from rec-eos.com's public API
    // (receos_api.rs). It sorts by cheers or by newest only, and lists
    // players only for a search.
    hasFilters: true,
    hasSort: true,
    hasFeed: false,
    hasPresence: false,
    hasPhotoFeed: false,
    hasInventions: false,
    // Before a search, players online now, then the most recently seen.
    hasPeopleBrowse: true,
    needsConfiguredDownloadUrl: false,
    // The client is the stock game; the Rec EOS mod goes in with UPDATE.
    hasPatch: true,
    patchLabel: 'Rec EOS'
  },
  radium: {
    label: 'RADIUM',
    logo: 'logo.png',
    site: 'https://www.radie.app/',
    downloadPage: 'https://www.radie.app/',
    imageBase: 'https://img.radie.app',
    hasFilters: true,
    hasSort: true,
    hasFeed: true,
    hasPresence: true,
    // Radium's API could serve one, but the FEED tab was asked for on Vanilla
    // only — this flag is the switch if that ever changes.
    hasPhotoFeed: false,
    // Radium resolves its own download URL from the recroom.baby page.
    needsConfiguredDownloadUrl: false
  },
  vanilla: {
    label: 'VANILLA',
    logo: 'assets/vanilla-logo.png',
    site: 'https://vanillarec.net/',
    downloadPage: 'https://vanillarec.net/download/',
    imageBase: '',
    // Both true since rooms moved to the bulk /ws set: filtering and sorting
    // happen over the whole room list in the backend, so a tag and a typed
    // search compose instead of overwriting each other, and a sort orders every
    // room rather than the first page the API happened to return.
    hasFilters: true,
    hasSort: true,
    hasFeed: false,
    hasPresence: false,
    hasPhotoFeed: true,
    // Vanilla has not shipped a client; the URL comes from Settings.
    needsConfiguredDownloadUrl: true
  },
  stella: {
    label: 'STELLA',
    logo: 'assets/stella-logo.png',
    site: 'https://discord.com/invite/stella-rr',
    downloadPage: 'https://discord.com/invite/stella-rr',
    // Stella serves images openly at img.stellaonline.org/img/<name>; the
    // backend attaches absolute ThumbUrl/AvatarUrl, so no imageBase is needed.
    imageBase: '',
    // Stella's game API (rooms, people, profiles) is reached by signing in with
    // the player's Steam account — see stella_api.rs. Rooms and People are live;
    // there is no network-wide photo feed, and presence exists only for friends.
    hasSocial: true,
    hasFilters: true,
    // Sorted in the backend over the whole room list (stella_api::sort_rooms).
    hasSort: true,
    hasFeed: false,
    hasPresence: false,
    hasPhotoFeed: false,
    // A player's published inventions (stella_api::fetch_user_inventions).
    hasInventions: true,
    // There is no list of everyone, so before something is typed People lists
    // the players online now, from the live hub (stella_api::browse_online).
    hasPeopleBrowse: true,
    needsConfiguredDownloadUrl: false,
    // The client is patched at launch; the patch has its own UPDATE button.
    hasPatch: true,
    patchLabel: "Stella's patch"
  }
};

/// Where `network`'s own settings live in config: Radium's are the flat fields,
/// every other network's are nested under its name (`config.vanilla`,
/// `config.stella`).
function networkState(network) {
  return network === 'radium' ? config : (config?.[network] || {});
}

/// Write `fields` into `network`'s own part of config.
function setNetworkState(network, fields) {
  if (!config) return;
  if (network === 'radium') Object.assign(config, fields);
  else config[network] = { ...(config[network] || {}), ...fields };
}

function networkInfo(name = activeNetwork) {
  return NETWORKS[name] || NETWORKS.receos;
}

/// Install directories are per-network: Radium's lives on the flat
/// `config.installDir`, Vanilla's on `config.vanilla.installDir`. Every read
/// and write has to go through these two helpers — touching the flat field
/// while Vanilla is active is what stamped the Vanilla folder onto Radium's
/// slot, which then sent Radium's download into the Vanilla client folder and
/// made Vanilla report that Radium's client was a Vanilla install.
function configInstallDir(network = activeNetwork) {
  if (!config) return '';
  return networkState(network).installDir || '';
}

/// Whether `network`'s client folders, where they are now, are excluded from
/// Windows Defender. The backend works it out from the folders it excluded,
/// so a client moved elsewhere reads as not excluded; the page only mirrors
/// it (syncAvState). A third-party antivirus is never excluded, only
/// acknowledged, in `thirdPartyAvAcknowledged`.
///
/// Per network, like the folder it describes: Radium's is the flat
/// `config.defenderExcluded`, Vanilla's `config.vanilla.defenderExcluded`. A
/// single flat flag meant excluding Radium's folder also skipped the check for
/// Vanilla's, which never was.
function avExcluded(network = activeNetwork) {
  if (!config) return false;
  return networkState(network).defenderExcluded === true;
}

/// Whether the launcher has folders of `network`'s excluded from Windows
/// Defender: its current ones, or ones from before the client moved, which
/// the next exclusion takes out. The backend keeps this (`defenderDirs`) and
/// works out `defenderExcluded` from it; see refresh_defender_flags in
/// config.rs.
function avExclusionOnRecord(network = activeNetwork) {
  const dirs = networkState(network).defenderDirs;
  return Array.isArray(dirs) && dirs.length > 0;
}

/// Take the backend's word for `network`'s Defender exclusion: whether its
/// folders are excluded, and which folders are on record.
function syncAvState(network, excluded, dirs) {
  setNetworkState(network, {
    defenderExcluded: excluded === true,
    defenderDirs: Array.isArray(dirs) ? dirs : [],
  });
  if (network === activeNetwork) setExcludeAvLabel(avExcluded());
}

/// Placeholder for the log/modal text before checkInstall() has resolved the
/// real path. Per-network, since each defaults to its own folder.
function defaultInstallDirHint(network = activeNetwork) {
  const folder = network === 'radium' ? 'client' : `client-${network}`;
  return `%APPDATA%\\com.radium.launcher\\${folder}`;
}

function setConfigInstallDir(dir, network = activeNetwork) {
  setNetworkState(network, { installDir: dir });
}

/// The Settings path span for a network. Every network's row exists at once,
/// so each read and write of a displayed path has to name which one it means.
function installDirSpan(network = activeNetwork) {
  return $(`cfgInstallDir${network.charAt(0).toUpperCase()}${network.slice(1)}`);
}

/// Best known install path for `network`: what Settings is showing (the
/// resolved path, once checkInstall() has filled it in), else the configured
/// override, else the default hint.
function shownInstallDir(network = activeNetwork) {
  return installDirSpan(network)?.textContent.trim()
    || configInstallDir(network)
    || defaultInstallDirHint(network);
}

/// Fill both install-location rows.
///
/// The inactive network has no checkInstall() result to draw on, so its
/// resolved folder comes straight from the backend — the same path its
/// download would use: the default, or a custom location's network subfolder.
async function refreshInstallDirRows() {
  for (const network of Object.keys(NETWORKS)) {
    const span = installDirSpan(network);
    if (!span) continue;
    let dir = '';
    try {
      dir = await window.radium?.resolveClientDir(network, configInstallDir(network));
    } catch (e) {
      console.error('resolveClientDir error:', e);
    }
    span.textContent = dir || configInstallDir(network) || defaultInstallDirHint(network);
  }
}

/// Mirrors `norm_dir` in config.rs: install paths reach the UI from the folder
/// picker, config.json and the backend's own resolver, which disagree on
/// separators and casing.
function normDir(path) {
  return String(path || '').replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();
}

/// Whether two install folders are one folder or one holds the other. Mirrors
/// `install_dirs_overlap` in config.rs, which resets a folder that does.
function installDirsOverlap(a, b) {
  const x = normDir(a), y = normDir(b);
  if (!x || !y) return false;
  return x === y || x.startsWith(`${y}/`) || y.startsWith(`${x}/`);
}

/// The first other network whose folder, as Settings is showing it, is `dir`
/// or holds it or sits inside it: `{ network, dir }`, or null.
function overlappingInstallDir(network, dir) {
  for (const other of Object.keys(NETWORKS)) {
    if (other === network) continue;
    const otherDir = installDirSpan(other)?.textContent.trim() || configInstallDir(other) || '';
    if (otherDir && installDirsOverlap(otherDir, dir)) return { network: other, dir: otherDir };
  }
  return null;
}

/// `cfg` without `glass.bgImage`. See saveConfig in the shim below.
function withoutBackdrop(cfg) {
  if (!cfg) return cfg;
  let out = cfg;
  if (out.glass && 'bgImage' in out.glass) {
    const { bgImage, ...glass } = out.glass;
    out = { ...out, glass };
  }
  // Home's banner pictures likewise: up to a megabyte each, written only by
  // setHomeBanner (cmd_set_home_banner), and kept by the backend on a save.
  if (out.home && 'banners' in out.home) {
    const { banners, ...home } = out.home;
    out = { ...out, home };
  }
  return out;
}

(function setupTauriShim() {
  // Every call waits for the last "is Stella in use" message to land first,
  // so a sign-in asked for straight after Stella is opened is never turned
  // down as a background one. Normally already settled: a microtask's wait.
  const rawInvoke = window.__TAURI__.core.invoke;
  let inUseSent = Promise.resolve();
  const invoke = (...args) => inUseSent.then(() => rawInvoke(...args));
  const { listen } = window.__TAURI__.event;
  const { getCurrentWindow } = window.__TAURI__.window;
  const { open } = window.__TAURI__.shell;

  const appWindow = getCurrentWindow();

  let unlistenMap = {};

  window.radium = {
    // Window controls
    minimize: () => appWindow.minimize(),
    maximize: () => appWindow.toggleMaximize(),
    close:    () => appWindow.close(),

    // Config. The glass backdrop can be a 1.4 MB data URI, so it is left out
    // of every whole-config save (the backend keeps the stored one) and is
    // written only through setGlassBackdrop — otherwise each autosave, toggle
    // and play-mode click shipped it over IPC and had it re-parsed.
    getConfig:  ()    => invoke('cmd_get_config'),
    saveConfig: (cfg) => invoke('cmd_save_config', { config: withoutBackdrop(cfg) }),
    setGlassBackdrop: (value) => invoke('cmd_set_glass_backdrop', { value: String(value || '') }),
    setHomeBanner: (network, value) => invoke('cmd_set_home_banner', { network: String(network || ''), value: String(value || '') }),
    // The picture at a typed address, as an ArrayBuffer. The CSP lets the
    // page load no remote image itself; see setGlassBackdropFromUrl().
    fetchGlassBackdrop: (url) => invoke('cmd_fetch_glass_backdrop', { url: String(url || '') }),

    // Server. Every command below takes the active network so the backend can
    // route to the right API without the call sites having to care.
    pingServer:     (url) => invoke('ping_server', { url }),
    getPlayerCount: ()    => invoke('get_player_count', { network: activeNetwork }),
    addDefenderExclusion:    () => invoke('add_defender_exclusion', { network: activeNetwork }),
    removeDefenderExclusion: () => invoke('remove_defender_exclusion', { network: activeNetwork }),
    detectAntivirus:         () => invoke('detect_antivirus'),

    // Install
    checkInstall: () => invoke('check_install', { network: activeNetwork }),
    checkClientUpdate: () => invoke('check_client_update', { network: activeNetwork }),

    // Download
    downloadClient:  () => invoke('download_client', { network: activeNetwork }),
    cancelDownload:  () => invoke('cancel_download', { network: activeNetwork }),
    pauseDownload:   () => invoke('pause_download'),
    resumableDownloadInfo: () => invoke('resumable_download_info', { network: activeNetwork }),
    uninstallClient: () => invoke('uninstall_client', { network: activeNetwork }),
    // Verify Files: check every installed file, then repair the ones that
    // failed — single files where the server allows, else from a full
    // download when `full` (see verify.rs).
    verifyClient: () => invoke('verify_client', { network: activeNetwork }),
    repairClient: (full = false) => invoke('repair_client', { network: activeNetwork, full }),
    // These take an explicit network: Settings lists every network's install
    // folder at once, so it has to address the inactive ones too.
    openClientFolder: (network = activeNetwork) => invoke('open_client_folder', { network }),
    selectFolder:     (network = activeNetwork) => invoke('select_folder', { network }),
    getDefaultClientDir: (network = activeNetwork) => invoke('get_default_client_dir', { network }),
    // The folder the client really goes in for a picked location: its
    // network's subfolder of it (`config::custom_client_dir`).
    resolveClientDir: (network = activeNetwork, folder = '') => invoke('resolve_client_dir', { network, folder }),
    onDownloadProgress: async (cb) => {
      if (unlistenMap['download-progress']) unlistenMap['download-progress']();
      unlistenMap['download-progress'] = await listen('download-progress', (event) => cb(event.payload));
    },

    // Stella's patch: whether it needs an UPDATE, and running one.
    stellaPatchStatus: () => invoke('stella_patch_status'),
    stellaUpdatePatch: () => invoke('stella_update_patch'),
    onStellaPatchProgress: async (cb) => {
      if (unlistenMap['stella-patch-progress']) unlistenMap['stella-patch-progress']();
      unlistenMap['stella-patch-progress'] = await listen('stella-patch-progress', (event) => cb(event.payload));
    },

    // Rec EOS account and what it opens (receos_social.rs).
    receosAuthStatus: () => invoke('receos_auth_status'),
    receosLogin: (username, password) => invoke('receos_login', { username, password }),
    receosLogout: () => invoke('receos_logout'),
    receosFriends: () => invoke('receos_friends'),
    receosSetFriendFavorite: (id, on) => invoke('receos_set_friend_favorite', { id: Number(id), on: !!on }),
    receosAcceptFriendRequest: (id) => invoke('receos_accept_friend_request', { id: Number(id) }),
    receosSendFriendRequest: (id) => invoke('receos_send_friend_request', { id: Number(id) }),
    receosNotifications: () => invoke('receos_notifications'),
    receosUpdates: (afterMessageId, afterChatMessageId) => invoke('receos_updates', { afterMessageId: afterMessageId ?? null, afterChatMessageId: afterChatMessageId ?? null }),
    receosChatThreads: () => invoke('receos_chat_threads'),
    receosChatMessages: (threadId, before) => invoke('receos_chat_messages', { threadId: Number(threadId), before: before ?? null }),
    receosChatSend: (threadId, text) => invoke('receos_chat_send', { threadId: Number(threadId), text: String(text) }),
    receosChatOpen: (playerIds, text) => invoke('receos_chat_open', { playerIds: playerIds.map(Number), text: text ?? null }),
    receosChatRead: (threadId, messageId) => invoke('receos_chat_read', { threadId: Number(threadId), messageId: Number(messageId) }),
    receosRoomInteraction: (roomId) => invoke('receos_room_interaction', { roomId: Number(roomId) }),
    receosSetRoomInteraction: (roomId, cheer, favorite) => invoke('receos_set_room_interaction', { roomId: Number(roomId), cheer: cheer ?? null, favorite: favorite ?? null }),

    // The Rec EOS mod: whether it needs an UPDATE, and running one.
    receosPatchStatus: () => invoke('receos_patch_status'),
    receosUpdatePatch: () => invoke('receos_update_patch'),
    onReceosPatchProgress: async (cb) => {
      if (unlistenMap['receos-patch-progress']) unlistenMap['receos-patch-progress']();
      unlistenMap['receos-patch-progress'] = await listen('receos-patch-progress', (event) => cb(event.payload));
    },

    // Game
    launchGame: (cfg) => invoke('launch_game', { config: { ...cfg, network: activeNetwork } }),
    killGame:   ()    => invoke('kill_game'),
    onGameState: async (cb) => {
      if (unlistenMap['game-state']) unlistenMap['game-state']();
      unlistenMap['game-state'] = await listen('game-state', (event) => cb(event.payload));
    },

    // Misc
    openUrl:    (url)  => open(url),
    getVersion: ()     => invoke('get_version'),
    checkSteam: ()     => invoke('check_steam'),
    checkRequiredSteamApp: () => invoke('check_required_steam_app'),
    checkSmartAppControl: () => invoke('check_smart_app_control'),

    // Auto-update. Only the installer the last check offered is accepted, and
    // the backend checks it against the digest GitHub published with it.
    checkForUpdate:  ()                            => invoke('check_for_update'),
    downloadUpdate:  (downloadUrl, placeOnDesktop) =>
      invoke('download_update', { url: downloadUrl, placeOnDesktop: !!placeOnDesktop }),

    // Data Fetching
    fetchRooms:           (args) => invoke('fetch_rooms', { args: { ...args, network: activeNetwork } }),
    fetchPeople:          (args) => invoke('fetch_people', { args: { ...args, network: activeNetwork } }),
    fetchFilters:         ()     => invoke('fetch_filters', { network: activeNetwork }),
    fetchRoomWebDetails:  (name) => invoke('fetch_room_web_details', { name: String(name), network: activeNetwork }),
    fetchUserWebDetails:  (name, accountId) => invoke('fetch_user_web_details', { name: String(name), network: activeNetwork, accountId: accountId ?? null }),
    fetchUserPhotos:      (args) => invoke('fetch_user_photos', { args: { ...args, network: activeNetwork } }),
    fetchRoomPhotos:      (args) => invoke('fetch_room_photos', { args: { ...args, network: activeNetwork } }),
    fetchUserRooms:       (args) => invoke('fetch_user_rooms', { args: { ...args, network: activeNetwork } }),
    fetchUserInventions:  (args) => invoke('fetch_user_inventions', { args: { ...args, network: activeNetwork } }),
    fetchUserFeed:        (args) => invoke('fetch_user_feed', { args: { ...args, network: activeNetwork } }),
    fetchRecentPhotos:    (args) => invoke('fetch_recent_photos', { args: { ...args, network: activeNetwork } }),
    // Fire-and-forget: warms the active network's caches so the first visit to
    // Rooms or People reads a list that is already downloaded. Callers don't
    // await it, so a failure is swallowed here rather than surfacing as an
    // unhandled rejection — there is nothing to tell the user, and the tab
    // that needs the data will fetch it itself.
    prefetchNetworkData:  ()     => invoke('prefetch_network_data', { network: activeNetwork }).catch(() => {}),
    fetchPhotoWebDetails: (photoId) => invoke('fetch_photo_web_details', { photoId: String(photoId), network: activeNetwork }),
    fetchPhotoComments:   (photoId) => invoke('fetch_photo_comments', { photoId: String(photoId), network: activeNetwork }),

    // Vanilla account. The session cookie stays in the backend: these return
    // only `{ authenticated, player }`, a token count and notification rows.
    vanillaLogin:         () => invoke('vanilla_login'),
    vanillaLogout:        () => invoke('vanilla_logout'),
    stellaAuthStatus:     () => invoke('stella_auth_status'),
    stellaLogin:          () => invoke('stella_login'),
    stellaLogout:         () => invoke('stella_logout'),
    stellaAccounts:       () => invoke('stella_accounts'),
    stellaUseAccount:     (accountId) => invoke('stella_use_account', { accountId: Number(accountId) }),
    stellaRoomInteraction:    (roomId) => invoke('stella_room_interaction', { roomId: Number(roomId) }),
    stellaSetRoomInteraction: (roomId, kind, on) => invoke('stella_set_room_interaction', { roomId: Number(roomId), kind, on }),
    stellaRoomPlayers:    (roomId) => invoke('stella_room_players', { roomId: Number(roomId) }),
    stellaTokens:         () => invoke('stella_tokens'),
    stellaCheeredPhotos:  (ids) => invoke('stella_cheered_photos', { ids: ids.map(Number) }),
    stellaSetPhotoCheer:  (photoId, on) => invoke('stella_set_photo_cheer', { photoId: Number(photoId), on }),
    stellaJoinCheck:      (playerId, roomId) => invoke('stella_join_check', { playerId: Number(playerId), roomId: Number(roomId) || 0 }),
    stellaRequestJoin:    (playerId) => invoke('stella_request_join', { playerId: Number(playerId) }),
    stellaAskToJoin:      (playerId) => invoke('stella_ask_to_join', { playerId: Number(playerId) }),
    stellaNotifications:  () => invoke('stella_notifications'),
    vanillaAuthStatus:    () => invoke('vanilla_auth_status'),
    vanillaAccount:       () => invoke('vanilla_account'),
    vanillaNotifications: () => invoke('vanilla_notifications'),
    vanillaRoomCheered:      (roomId)        => invoke('vanilla_room_cheered', { roomId }),
    vanillaSetRoomCheer:     (roomId, cheer) => invoke('vanilla_set_room_cheer', { roomId, cheer }),
    vanillaCheeredPhotos:    ()              => invoke('vanilla_cheered_photos'),
    vanillaTogglePhotoCheer: (photoId)       => invoke('vanilla_toggle_photo_cheer', { photoId: String(photoId) }),
    vanillaSubscribed:       (playerId)      => invoke('vanilla_subscribed', { playerId }),
    vanillaSetSubscribed:    (playerId, subscribe) => invoke('vanilla_set_subscribed', { playerId, subscribe }),
    vanillaJoinRoom:         (roomId)        => invoke('vanilla_join_room', { roomId }),
    vanillaCurrentRoom:      ()              => invoke('vanilla_current_room'),
    // Background (tray) and start with Windows.
    getAutostart: ()        => invoke('get_autostart'),
    setAutostart: (enabled) => invoke('set_autostart', { enabled }),
    onLauncherHidden: async (cb) => {
      if (unlistenMap['launcher-hidden']) unlistenMap['launcher-hidden']();
      unlistenMap['launcher-hidden'] = await listen('launcher-hidden', () => cb());
    },
    setTrayState: (network, gameRunning, style, info) => invoke('set_tray_state', { network, gameRunning, style, info }),
    showLauncher: () => invoke('show_launcher'),
    onTrayAction: async (cb) => {
      if (unlistenMap['tray-action']) unlistenMap['tray-action']();
      unlistenMap['tray-action'] = await listen('tray-action', (event) => cb(event.payload));
    },
    onTrayPanel: async (cb) => {
      if (unlistenMap['tray-panel']) unlistenMap['tray-panel']();
      unlistenMap['tray-panel'] = await listen('tray-panel', (event) => cb(event.payload === true));
    },

    // Desktop notification pop-up (a separate always-on-top window).
    desktopNotify:     (cards) => invoke('desktop_notify', { cards }),
    onDesktopNotifOpen: async (cb) => {
      if (unlistenMap['desktop-notif-open']) unlistenMap['desktop-notif-open']();
      unlistenMap['desktop-notif-open'] = await listen('desktop-notif-open', (event) => cb(event.payload));
    },
    stellaFriends:      () => invoke('stella_friends'),
    stellaFriendsStop:  () => invoke('stella_friends_stop'),
    stellaSetInUse: (inUse) => {
      inUseSent = inUseSent.then(() => rawInvoke('stella_set_in_use', { inUse })).catch(() => {});
      return inUseSent;
    },
    stellaPresence:     (playerId) => invoke('stella_presence', { playerId: Number(playerId) }),
    stellaSetFriendFavorite: (playerId, on) => invoke('stella_set_friend_favorite', { playerId: Number(playerId), on: !!on }),
    stellaAcceptFriendRequest: (playerId) => invoke('stella_accept_friend_request', { playerId: Number(playerId) }),
    onStellaFriends: async (cb) => {
      if (unlistenMap['stella-friends-changed']) unlistenMap['stella-friends-changed']();
      unlistenMap['stella-friends-changed'] = await listen('stella-friends-changed', () => cb());
    },
    onStellaMessage: async (cb) => {
      if (unlistenMap['stella-message']) unlistenMap['stella-message']();
      unlistenMap['stella-message'] = await listen('stella-message', () => cb());
    },
    onStellaInvite: async (cb) => {
      if (unlistenMap['stella-invite']) unlistenMap['stella-invite']();
      unlistenMap['stella-invite'] = await listen('stella-invite', (event) => cb(event.payload));
    },
    onStellaJoinRequest: async (cb) => {
      if (unlistenMap['stella-join-request']) unlistenMap['stella-join-request']();
      unlistenMap['stella-join-request'] = await listen('stella-join-request', (event) => cb(event.payload));
    },
    onVanillaAuth: async (cb) => {
      if (unlistenMap['vanilla-auth-changed']) unlistenMap['vanilla-auth-changed']();
      unlistenMap['vanilla-auth-changed'] = await listen('vanilla-auth-changed', (event) => cb(event.payload));
    },

    // Window state events
    onWindowMaximizedState: async (cb) => {
      if (unlistenMap['window-maximized-state']) unlistenMap['window-maximized-state']();
      unlistenMap['window-maximized-state'] = await listen('window-maximized-state', (event) => cb(event.payload));
    },

    // Logs and bug reports (logs.js)
    logPrevious:       () => invoke('log_previous'),
    logOpenFolder:     () => invoke('log_open_folder'),
    logSave:           (text, fileName) => invoke('log_save', { text, fileName }),
    bugReportPreview:  (diagnostics, logs) => invoke('bug_report_preview', { diagnostics, logs }),
    bugReportCooldown: () => invoke('bug_report_cooldown'),
    submitBugReport:   (report) => invoke('submit_bug_report', { report }),
  };
})();

// App state variables
let config                = {};
let isGameRunning         = false;
let isGameLaunching       = false;
/// When the game was seen to start, for the tray panel's "Playing · 12:34".
let gameStartedAt         = 0;
/// What was last handed to the tray panel (see syncTray), so a push that
/// would change nothing is not sent, and whether one is already queued.
let traySent              = '';
let trayQueued            = false;
// Set while a launch started from the tray is under way; see playFromTray().
let revealOnModal         = false;
let revealOnModalTimer    = null;
let isDownloading         = false;
// Cancel requested, but the backend loop only aborts at its next chunk and
// holds a global "download in progress" guard until it exits. Re-enabling
// Download before then makes the next click fail with "A download is already
// in progress.", so the button stays disabled through this window.
let isCancelling          = false;
let isPaused              = false;
// 'verify' or 'repair' while Verify Files is using the download panel (and
// `isDownloading`, so everything a download blocks, it blocks too). 'repair'
// covers both a repair that fetches single files and one that has to download
// the whole client. See runVerifyFiles().
let clientTask            = null;
// Whether the running download can be paused and resumed; see updateDlProgress().
// False until the backend has the server's answer, so Pause never shows for a
// moment on a download that turns out not to be able to pause (Stella's).
let dlResumable           = false;
let isInstalled           = false;
// The client is there but missing files, its executable among them (deleted,
// or quarantined by an antivirus). The hero offers REPAIR, which runs Verify
// Files, instead of a fresh download. See check_install's `needsRepair`.
let needsRepair           = false;
// Stella's patch: the last stella_patch_status result (null until one has run
// since the client was found), and whether an UPDATE is running.
let stellaPatch           = null;
let stellaPatchUpdating   = false;
// Which network `stellaPatch` was checked for (Stella or Rec EOS).
let stellaPatchNetwork    = null;
// Where Stella keeps its patch (from checkInstall); outside the client folder.
let stellaPatchDir        = '';
let playMode              = 'screen';
let launchAfterExclusion  = false;
let sacWarnedThisSession  = false;
// Last-known reachability from checkServerStatus(), surfaced in bug reports.
// null = not checked yet this session.
let lastServerStatus      = { apiOnline: null, cdnOnline: null };
/// The network lastServerStatus was taken on, so the tray panel doesn't call
/// the network just switched to online on the strength of the last one.
let serverStatusNetwork   = '';

// ── Image load handling ──────────────────────────────────────────────────
// Every thumbnail in the app wants the same two things: drop the shimmer class
// once it resolves, and swap to a local placeholder if it 404s. That used to be
// an `onload`/`onerror` attribute on each `<img>`, which forced the CSP to allow
// `script-src 'unsafe-inline'` — and with inline script permitted, any HTML
// injection anywhere becomes code execution inside a webview that can reach
// every backend command.
//
// `load` and `error` don't bubble, but they do reach document in the capture
// phase, so one pair of listeners covers every image including ones added
// later. Images opt into a fallback with `data-fallback`.
function handleImageSettled(e) {
  const el = e.target;
  if (!(el instanceof HTMLImageElement)) return;
  el.classList.remove('image-loading-placeholder');
  if (e.type !== 'error') return;

  const fallback = el.dataset.fallback;
  if (!fallback) return;
  // Cleared before assigning, so a fallback that is itself missing fires this
  // once and stops rather than looping.
  delete el.dataset.fallback;
  el.src = fallback;
}
document.addEventListener('load', handleImageSettled, true);
document.addEventListener('error', handleImageSettled, true);

// ── Image sources ────────────────────────────────────────────────────────
// The two networks serve images differently. Radium exposes a resizing CDN
// addressed by image *name* (`img.radie.app/<name>?width=N`); Vanilla returns a
// ready-made absolute URL that must be used verbatim, because some already
// carry a cachebuster query and its endpoint does no resizing. The backend
// normalizer attaches that URL as ThumbUrl / AvatarUrl, so the presence of
// those fields is what decides which form to use — Radium payloads simply
// don't have them and fall through to the existing behaviour.
const RADIUM_IMG_BASE = 'https://img.radie.app';

/// Base URL of the launcher's thumbnail cache.
///
/// Tauri serves a custom scheme as `http://<scheme>.localhost` on Windows and
/// `<scheme>://localhost` everywhere else. Rather than sniff the platform, ask
/// Tauri how it rewrites the asset protocol and take the same shape — the
/// backend reads only the query, so the host and path don't have to match.
const THUMB_BASE = (() => {
  try {
    // Hands back the fully-formed origin for this platform with the path we
    // passed on the end, which is exactly the shape we want — and it keeps
    // working if Tauri ever changes how it rewrites schemes.
    const built = window.__TAURI_INTERNALS__.convertFileSrc('thumb', 'radiumimg');
    if (built) return built;
  } catch (e) { /* fall through */ }
  return navigator.userAgent.includes('Windows')
    ? 'http://radiumimg.localhost/thumb'
    : 'radiumimg://localhost/thumb';
})();

/// Point an `<img>` at the launcher's thumbnail cache instead of the origin.
///
/// The backend fetches the image once, scales it to `width` and keeps the
/// result on disk. This matters most on Vanilla, which serves room images as
/// the file the game uploaded: measured 2026-09-09, a room thumbnail is a
/// 2560x1440 PNG of about 3 MB — drawn in a card roughly 200 px wide. Twelve of
/// those is ~35 MB off the network and well over a hundred megabytes of decoded
/// bitmap, and since the responses carry no cache headers whatsoever it was
/// paid again on every single visit to the tab.
///
/// `width` is the size the element is drawn at in CSS pixels. It is multiplied
/// by the display's pixel ratio here, in one place, so every caller can name
/// the size from its own stylesheet and not think about it. Windows defaults to
/// 125% or 150% scaling on most laptops, and asking for a 480 px image to fill
/// a 480 px box on a 1.5x screen is a 1.5x upscale — which reads as a blurry
/// thumbnail, not as a scaling setting.
///
/// Local paths (`./images.png`) and anything that isn't http(s) come back
/// untouched — there is nothing to fetch or shrink.
function thumbSrc(url, width) {
  if (!url || !/^https?:\/\//i.test(url)) return url;
  return `${THUMB_BASE}?url=${encodeURIComponent(url)}&w=${devicePx(width)}`;
}

/// Device pixels for a size given in CSS pixels.
///
/// Clamped at 3 so an unusual display setting can't turn a thumbnail request
/// into a demand for an enormous image. Shared with the Radium CDN URLs below,
/// which name their own size in the query string: asking the CDN for 96 px and
/// then asking the thumbnail cache for 144 got a 96 px image stretched to 144,
/// which is exactly the blur this was meant to remove.
function devicePx(cssWidth) {
  const ratio = Math.min(Math.max(window.devicePixelRatio || 1, 1), 3);
  return Math.round(cssWidth * ratio);
}

/// Source width needed to fill a *square* avatar slot `cssSize` across.
///
/// Every avatar in the app is a square box with `object-fit: cover`, and the
/// pictures behind them are usually not square. Vanilla lets a player use any
/// screenshot as a profile picture, and most are 16:9 — measured 2026-09-10,
/// two of three sampled profile pictures were 2560x1440, and only one was a
/// square 256x256. Cover crops to the shorter side, so a 16:9 source
/// contributes just 9/16 of its width to a square slot: sizing the request by
/// the slot's width gets a picture that is sharp in a landscape card and
/// visibly soft here. Square or portrait sources need less than this, and are
/// over-asked for slightly rather than the common case being under-asked.
const AVATAR_SOURCE_ASPECT = 16 / 9;
function avatarWidth(cssSize) {
  return Math.round(cssSize * AVATAR_SOURCE_ASPECT);
}

/// Where a room's image really lives, before the thumbnail cache. Used
/// directly only where the full-resolution original is the point.
function roomSourceUrl(room, width) {
  if (!room) return './images.png';
  if (room.ThumbUrl) return room.ThumbUrl;
  const name = room.ImageName || room.imageName || '';
  return name ? `${RADIUM_IMG_BASE}/${name}?width=${devicePx(width)}` : './images.png';
}

function roomThumbUrl(room, width) {
  return thumbSrc(roomSourceUrl(room, width), width);
}

/// Format a count the way the room cards and detail view show it.
function formatCount(n) {
  return Number(n).toLocaleString();
}

/// A count short enough for a card's corner: 1,234 → "1.2K".
const compactCountFormat = new Intl.NumberFormat('en', { notation: 'compact', maximumFractionDigits: 1 });
function compactCount(n) {
  return compactCountFormat.format(Number(n) || 0);
}

/// Cheers / favourites / visits already present on a room row, or null.
///
/// Both networks send these with the list — `vanilla::room_row_json` builds
/// them from the bulk snapshot, and Radium's rooms endpoint carries the same
/// PascalCase fields. A row that has them needs no per-card lookup at all.
/// Null (rather than zero) when the row is missing them entirely, so the caller
/// can fall back instead of printing a confident 0.
function roomStatsFromRow(room) {
  if (!room) return null;
  const pick = (...names) => {
    for (const name of names) {
      const v = room[name];
      if (v != null && v !== '' && Number.isFinite(Number(v))) return Number(v);
    }
    return null;
  };
  const cheers = pick('CheerCount', 'cheerCount');
  const visits = pick('VisitCount', 'visitCount');
  const favorites = pick('FavoriteCount', 'favoriteCount');
  if (cheers == null && visits == null) return null;
  return {
    cheers: formatCount(cheers ?? 0),
    visits: formatCount(visits ?? 0),
    favorites: favorites == null ? null : formatCount(favorites)
  };
}

/// `fetchRoomWebDetails` with a short-lived memo.
///
/// The underlying call scrapes a full HTML page on Radium. Without this,
/// scrolling a profile's room grid re-fetched the same pages, and reopening a
/// room detail view paid for it again. Keyed by network so switching networks
/// doesn't serve the other one's numbers.
const ROOM_DETAILS_TTL_MS = 5 * 60 * 1000;
const roomDetailsMemo = new Map();

async function getRoomWebDetails(roomName) {
  const key = `${activeNetwork}:${roomName}`;
  const hit = roomDetailsMemo.get(key);
  if (hit && Date.now() - hit.at < ROOM_DETAILS_TTL_MS) return hit.value;

  const value = await window.radium?.fetchRoomWebDetails(roomName);
  // Only successes are cached: a failed lookup should be retried, not
  // remembered for five minutes.
  if (value && value.success) {
    // Bounded, so a long session browsing rooms can't grow this without limit.
    if (roomDetailsMemo.size > 300) roomDetailsMemo.clear();
    roomDetailsMemo.set(key, { value, at: Date.now() });
  }
  return value;
}

/// See [roomSourceUrl].
function photoSourceUrl(photo, width) {
  if (!photo) return './images.png';
  if (photo.ThumbUrl) return photo.ThumbUrl;
  const name = photo.ImageName || photo.imageName || '';
  return name ? `${RADIUM_IMG_BASE}/${name}?width=${devicePx(width)}` : './images.png';
}

function photoImageUrl(photo, width) {
  return thumbSrc(photoSourceUrl(photo, width), width);
}

/// A person with no picture, or whose picture failed to load: the orange
/// outline mark, centred on a transparent square so a slot's cover crop never
/// clips it. What used to stand in here was images.png, which is the Radium
/// logo: on Vanilla every player without a picture, and every desktop pop-up,
/// wore Radium's brand.
/// The picture for a player who has none: Rec EOS's logo there (as its
/// website shows), the generic one elsewhere.
function placeholderAvatar() {
  return activeNetwork === 'receos' ? './assets/receos-icon.png' : './assets/default-avatar.png';
}

/// Placeholder avatar for a network. Radium has a real DefaultProfileImage on
/// its CDN; Vanilla has no such asset, so it gets the bundled one.
function defaultAvatarUrl(width) {
  if (activeNetwork !== 'radium') return placeholderAvatar();
  return thumbSrc(`${RADIUM_IMG_BASE}/DefaultProfileImage?width=${devicePx(width)}&cropSquare=1`, width);
}

/// `size` is the width of the square slot in CSS pixels, not the width of the
/// image to fetch — see [avatarWidth], which is what turns one into the other.
function personAvatarUrl(person, size) {
  if (!person) return defaultAvatarUrl(size);
  // Vanilla hands back the picture at whatever shape the player uploaded, so
  // the cover crop has to be paid for.
  if (person.AvatarUrl) return thumbSrc(person.AvatarUrl, avatarWidth(size));
  const name = person.profileImage || '';
  if (!name || name === 'DefaultProfileImage') return defaultAvatarUrl(size);
  // Radium's CDN crops to a square itself (`cropSquare=1`), so what comes back
  // already fills the slot and its width is the slot's width.
  return thumbSrc(`${RADIUM_IMG_BASE}/${name}?width=${devicePx(size)}&cropSquare=1`, size);
}

// ── Network photo feed ───────────────────────────────────────────────────
// Vanilla publishes a network-wide feed of the photos players are taking right
// now — the same thing its website's front page shows. Radium has no such tab
// by request, so the nav button is hidden there.

let feedSkip = 0;
const feedTake = 12;
let feedLoading = false;
let feedHasMore = false;
let feedObserver = null;
let feedSequenceId = 0;

/// Point a card's creator and room at real people/places, once known.
///
/// Split out because attribution arrives two different ways: embedded in the
/// photo row (Vanilla), or from a per-photo lookup (Radium). Both end here, so
/// the card behaves identically whichever way it was filled.
function applyPhotoAttribution(card, { creatorName, creatorUsername, roomName, avatar }) {
  const creatorEl = card.querySelector('.creator-name');
  if (creatorEl) {
    creatorEl.textContent = creatorName || 'Unknown Creator';
    const profile = creatorUsername || creatorName;
    if (profile && profile !== 'Unknown' && profile !== 'Unknown Creator') {
      creatorEl.addEventListener('click', (e) => {
        e.stopPropagation();
        showCreatorProfile(profile);
      });
    }
  }

  const roomEl = card.querySelector('.room-link');
  if (roomEl) {
    if (roomName && roomName.toLowerCase() !== 'none') {
      roomEl.textContent = roomName;
      roomEl.addEventListener('click', (e) => {
        e.stopPropagation();
        showRoomByName(roomName);
      });
    } else {
      // Drop the whole "in <room> •" run rather than leaving a dangling label.
      roomEl.previousElementSibling?.remove();
      roomEl.nextElementSibling?.remove();
      roomEl.remove();
    }
  }

  if (avatar) {
    const avatarEl = card.querySelector('.creator-avatar');
    if (avatarEl) {
      avatarEl.classList.add('image-loading-placeholder');
      avatarEl.src = thumbSrc(avatar, avatarWidth(36));
    }
  }
}

/// A photo as a square thumbnail, for the grid on a player's profile. Opens
/// the photo page, where the uploader, room, caption and cheers are.
function buildPhotoTile(photo, backToView) {
  const tile = document.createElement('div');
  tile.className = 'profile-photo-tile';
  tile.tabIndex = 0;
  tile.setAttribute('role', 'button');
  const caption = String(photo.Description || photo.description || '').trim();
  tile.setAttribute('aria-label', caption ? `Photo: ${caption}` : 'Photo');

  const img = document.createElement('img');
  img.className = 'image-loading-placeholder';
  img.alt = '';
  img.loading = 'lazy';
  img.decoding = 'async';
  img.dataset.fallback = './images.png';
  img.src = photoImageUrl(photo, 400);
  tile.appendChild(img);

  const open = () => showPhotoDetails(photo, backToView);
  tile.addEventListener('click', open);
  tile.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      open();
    }
  });
  return tile;
}

/// Build one feed card from a photo row.
///
/// The single renderer behind the FEED tab, a room's photos and a player's
/// feed — those used to carry their own copies of this markup. (A player's
/// photos are thumbnails: buildPhotoTile().)
///
/// Vanilla embeds the uploader, room and tagged players in every row, so the
/// card is complete on first paint. Radium's rows carry none of that, so the
/// card paints with a placeholder and fills itself in from the per-photo
/// lookup in the background.
function buildPhotoCard(photo, backToView) {
  const card = document.createElement('div');
  card.className = 'feed-post-card';

  // Coerced to numbers — these are interpolated into innerHTML below.
  const cheers = Number(photo.CheerCount ?? photo.cheerCount) || 0;
  const comments = Number(photo.CommentCount ?? photo.commentCount) || 0;
  // Vanilla has no comment count at all, so the stat is omitted rather than
  // shown as a hardcoded zero.
  const hasComments = (photo.CommentCount ?? photo.commentCount) != null;
  const caption = photo.Description || photo.description || '';
  const embedded = !!photo.CreatorUsername;
  const creator = photo.CreatorDisplayName || photo.CreatorUsername || '';
  const roomName = photo.RoomName || '';
  // On Vanilla and Stella the count is the cheer button, as on vanillarec.net.
  const cheerable = photoCheerable(photo);

  let dateStr = '';
  const createdAt = photo.CreatedAt || photo.createdAt;
  if (createdAt) {
    try { dateStr = relativeTime(createdAt); } catch (e) { dateStr = String(createdAt); }
  }

  // Whoever else is in the shot, minus the uploader (already named above).
  const tagged = (photo.TaggedPlayers || [])
    .filter(p => p.userName && p.userName !== photo.CreatorUsername);

  card.innerHTML = `
    <div class="feed-post-header">
      <img class="feed-post-avatar creator-avatar image-loading-placeholder"
           src="${escapeHtml(thumbSrc(photo.CreatorAvatarUrl, avatarWidth(36)) || defaultAvatarUrl(36))}"
           loading="lazy" decoding="async"
           data-fallback="${placeholderAvatar()}" />
      <div class="feed-post-header-text">
        <div class="feed-post-creator creator-name">${escapeHtml(embedded ? creator : 'Loading...')}</div>
        <div class="feed-post-meta">
          <span>in</span>
          <span class="feed-post-room room-link">${escapeHtml(embedded ? roomName : 'Loading...')}</span>
          <span class="feed-post-dot">•</span>
          <span class="feed-post-time">${escapeHtml(dateStr)}</span>
        </div>
      </div>
    </div>
    ${caption ? `<div class="feed-post-description">${escapeHtml(caption)}</div>` : ''}
    <div class="feed-post-image-wrap image-wrap">
      <img class="feed-post-image image-loading-placeholder"
           src="${escapeHtml(photoImageUrl(photo, 800))}"
           loading="lazy" decoding="async"
           data-fallback="./images.png" />
    </div>
    ${tagged.length ? `<div class="feed-post-tagged"><span class="feed-tagged-label">In this photo:</span></div>` : ''}
    <div class="feed-post-footer">
      ${cheerable ? '' : `<span class="feed-post-stat"><span class="cheers-count">${cheers}</span> Cheers</span>`}
      ${hasComments ? `<span class="feed-post-stat"><span class="comments-count">${comments}</span> Comments</span>` : ''}
    </div>
  `;

  card.querySelector('.image-wrap')?.addEventListener('click', () => {
    showPhotoDetails(photo, backToView);
  });

  if (cheerable) card.querySelector('.feed-post-footer')?.prepend(buildCardCheer(photo));

  // Names are attached as elements, not interpolated markup, so a display name
  // containing quotes can't break out of a JS string context.
  const taggedEl = card.querySelector('.feed-post-tagged');
  if (taggedEl) {
    tagged.forEach(p => {
      const link = document.createElement('span');
      link.className = 'feed-tagged-name';
      link.textContent = p.displayName || p.userName;
      link.addEventListener('click', (e) => {
        e.stopPropagation();
        showCreatorProfile(p.userName);
      });
      taggedEl.appendChild(link);
    });
  }

  if (embedded) {
    applyPhotoAttribution(card, {
      creatorName: creator,
      creatorUsername: photo.CreatorUsername,
      roomName,
      avatar: photo.CreatorAvatarUrl
    });
  } else {
    // Resolved in the background so the grid paints immediately.
    (async () => {
      const details = await getPhotoWebDetails(photo.Id || photo.id, photo);
      if (!details?.success) {
        applyPhotoAttribution(card, { creatorName: 'Unknown Creator', roomName: '' });
        return;
      }
      const name = details.creatorUsername || 'Unknown';
      // Prefer the avatar that came with the photo: it is keyed on the
      // uploader's id, whereas a username lookup can land on a different
      // account that happens to match the name.
      const avatar = details.creatorAvatar
        || (name !== 'Unknown' ? (await getUserWebDetails(name))?.avatar : '');
      applyPhotoAttribution(card, {
        creatorName: name,
        creatorUsername: name,
        roomName: details.roomName || '',
        avatar
      });
    })();
  }

  return card;
}

/// "3m ago" / "2h ago", falling back to an absolute date past half a day —
/// the same cutoff vanillarec.net uses, so the two read alike.
function relativeTime(timestamp) {
  const then = new Date(timestamp).getTime();
  if (isNaN(then)) return '';
  const diffMs = Date.now() - then;
  const diffHours = diffMs / 3600000;
  if (diffHours >= 0 && diffHours < 12) {
    const mins = Math.floor(diffMs / 60000);
    if (mins < 1) return 'Just now';
    if (mins < 60) return `${mins}m ago`;
    return `${Math.floor(diffHours)}h ago`;
  }
  return new Date(then).toLocaleString();
}

async function loadFeed(append = false, { refresh = false } = {}) {
  const grid = $('feedGrid');
  const empty = $('feedEmptyMsg');
  if (!grid || feedLoading) return;
  // Guard here rather than only at the observer, so no caller can append past
  // the end of the feed and duplicate the last page.
  if (append && !feedHasMore) return;

  feedSequenceId++;
  const seq = feedSequenceId;

  if (!append) {
    feedSkip = 0;
    feedHasMore = false;
    grid.innerHTML = '<div id="feedLoading" class="feed-loading">Loading feed...</div>';
    if (empty) empty.style.display = 'none';
  } else {
    const more = document.createElement('div');
    more.id = 'feedLoading';
    more.className = 'feed-loading';
    more.textContent = 'Loading more...';
    grid.appendChild(more);
  }

  feedLoading = true;
  let res = null;
  try {
    // `refresh` reaches the backend, which holds the recent-photo list briefly
    // so paging on scroll doesn't re-download the pages already on screen.
    // Pressing Refresh has to go past that or it would show the same photos.
    res = await window.radium?.fetchRecentPhotos({ skip: feedSkip, take: feedTake, refresh: refresh && !append });
  } catch (e) {
    console.error('loadFeed error:', e);
  }
  feedLoading = false;

  // A network switch (or a refresh) started a newer load; drop this response.
  if (seq !== feedSequenceId) return;

  $('feedLoading')?.remove();

  if (!res?.success || !res.data?.Results) {
    if (!append) {
      grid.innerHTML = '';
      if (empty) {
        empty.textContent = `Couldn't load the feed: ${res?.error || 'unknown error'}`;
        empty.style.display = 'block';
      }
    }
    feedHasMore = false;
    return;
  }

  const photos = res.data.Results;
  if (!append) grid.innerHTML = '';

  photos.forEach(photo => grid.appendChild(buildPhotoCard(photo, 'feed')));

  feedSkip += photos.length;
  feedHasMore = photos.length === feedTake;

  if (!append && photos.length === 0 && empty) {
    empty.textContent = 'No recent photos.';
    empty.style.display = 'block';
  }

  setupFeedObserver();
}

/// Page in the next batch when the sentinel below the list scrolls into view.
function setupFeedObserver() {
  const sentinel = $('feedSentinel');
  const root = $('feedScroll');
  if (!sentinel || !root) return;
  if (feedObserver) feedObserver.disconnect();

  feedObserver = new IntersectionObserver((entries) => {
    if (entries[0].isIntersecting && feedHasMore && !feedLoading) {
      loadFeed(true);
    }
  }, {
    root,
    // The sentinel is 1px tall and sits flush with the bottom of the scroller,
    // so with no margin it lands exactly on the root's edge and can report a
    // zero ratio. The margin both fixes that and starts the next page a screen
    // early, so scrolling doesn't stall waiting for it.
    rootMargin: '400px 0px',
    threshold: 0
  });
  feedObserver.observe(sentinel);
}

/// Drop feed state — used when switching networks, since the photos belong to
/// the network that was active when they loaded.
function resetFeed() {
  feedSequenceId++;
  feedSkip = 0;
  feedHasMore = false;
  feedLoading = false;
  if (feedObserver) { feedObserver.disconnect(); feedObserver = null; }
  const grid = $('feedGrid');
  if (grid) grid.innerHTML = '';
  const empty = $('feedEmptyMsg');
  if (empty) empty.style.display = 'none';
}

/// Staff roles a network reports for a player, most senior first.
///
/// Read from the API's own booleans rather than a bundled list of names, so it
/// stays correct as staff change. Radium's scraper exposes no equivalent, so
/// this is empty there and nothing renders.
/// Vanilla's owners. The API has no owner flag — the site's own badge list
/// (badgeConfig.js) names them by hand — so this mirrors it: Coach (1), Nilla
/// (2), liam (3). Founders, so it changes about never; update if the site's
/// owner list does.
const VANILLA_OWNER_IDS = new Set([1, 2, 3]);

/// A player's staff roles, most senior first. `key` names both the badge icon
/// file and its CSS class; `short` is the retro text chip; `full` is the label.
function playerRoles(person) {
  if (!person) return [];
  const roles = [];
  // Owner is Vanilla-only: the id list means different people on another
  // network, and the badge art is Vanilla's own.
  if (activeNetwork === 'vanilla' && VANILLA_OWNER_IDS.has(Number(person.id))) {
    roles.push({ key: 'owner', short: 'OWNER', full: 'Owner' });
  }
  if (person.isDeveloper)     roles.push({ key: 'developer',     short: 'DEV',  full: 'Developer' });
  if (person.isModerator)     roles.push({ key: 'moderator',     short: 'MOD',  full: 'Moderator' });
  if (person.isCommunityTeam) roles.push({ key: 'communityTeam', short: 'TEAM', full: 'Community Team' });
  return roles;
}

/// Render a player's roles into `el`, hiding it when they have none.
///
/// Vanilla wears its own badge icons (the bronze marks from its site); every
/// other network keeps the plain coloured text chip, since the icons are
/// Vanilla-branded and mean nothing elsewhere.
function renderPlayerRoles(el, person) {
  if (!el) return;
  const roles = playerRoles(person);
  el.innerHTML = '';
  el.hidden = roles.length === 0;
  const useIcons = activeNetwork === 'vanilla';
  roles.forEach(role => {
    let badge;
    if (useIcons) {
      badge = document.createElement('img');
      badge.className = `role-badge role-icon role-${role.key.toLowerCase()}`;
      badge.src = `assets/badges/${role.key}.svg`;
      badge.alt = role.full;
      badge.draggable = false;
    } else {
      badge = document.createElement('span');
      badge.className = `role-badge role-${role.short.toLowerCase()}`;
      badge.textContent = role.short;
    }
    // A screen-reader name, not a `title`: hover tooltips are off app-wide.
    badge.setAttribute('aria-label', role.full);
    el.appendChild(badge);
  });
}

/// Fill one profile stat tile, hiding it entirely when the network doesn't
/// publish that number.
///
/// An empty value means "this network has no such stat" — Vanilla's player
/// record carries only a follower count, with no friend or visit figure
/// anywhere in its API. Showing those as a blank box implies a real value of
/// zero (or a bug), so the tile is removed and the remaining ones take the
/// space. A dash is different and stays visible: it means the stat exists but
/// the lookup failed.
function setProfileStat(el, value) {
  if (!el) return;
  const tile = el.closest('.profile-stat-item');
  const known = value !== '' && value != null;
  el.textContent = known ? value : '';
  if (tile) tile.hidden = !known;
}

/// The profile's bio: shown in the player's own words, and left out
/// altogether when they wrote none, rather than a box saying so. A long one
/// is cut to a few lines with a toggle for the rest, so the numbers under it
/// stay in view.
function setProfileBio(text) {
  const el = $('peopleDetailBio');
  if (!el) return;
  const bio = String(text ?? '').trim();
  el.textContent = bio;
  const box = el.closest('.profile-bio-box');
  if (box) box.hidden = !bio;
  const more = $('peopleDetailBioMore');
  if (!more) return;
  el.classList.add('is-clamped');
  more.textContent = 'Show more';
  more.hidden = true;
  // Measured once it has been laid out: only a bio the clamp cut short
  // gets the toggle.
  if (bio) requestAnimationFrame(() => { more.hidden = el.scrollHeight <= el.clientHeight + 1; });
}

document.getElementById('peopleDetailBioMore')?.addEventListener('click', (e) => {
  const clamped = document.getElementById('peopleDetailBio')?.classList.toggle('is-clamped');
  e.currentTarget.textContent = clamped ? 'Show more' : 'Show less';
});

/// What a Stella profile shows besides the numbers, from the profile lookup:
/// the emoji beside the name, a Junior account, cheers by kind and the
/// friends you share. `null` (another network, or before the lookup
/// lands) hides them all.
function setProfileExtras(details) {
  const emoji = $('peopleDetailEmoji');
  if (emoji) {
    emoji.textContent = details?.emoji || '';
    emoji.hidden = !emoji.textContent;
  }
  const junior = $('peopleDetailJunior');
  if (junior) junior.hidden = details?.junior !== true;
  renderProfileCheers(details?.cheers);
  renderMutualFriends(details?.mutualFriends);
}

/// The kinds of cheer, in the game's order.
const CHEER_KINDS = [
  ['general', 'General'],
  ['helpful', 'Helpful'],
  ['creative', 'Creative'],
  ['greatHost', 'Great Host'],
  ['sportsman', 'Sportsman'],
];

/// Cheers by kind, each with a bar against the biggest. Hidden when the
/// network has none to give, or the player has none yet.
function renderProfileCheers(cheers) {
  const panel = $('peopleDetailCheers');
  const list = $('peopleDetailCheerList');
  if (!panel || !list) return;
  const counts = CHEER_KINDS.map(([key, label]) => [label, Math.max(0, Number(cheers?.[key]) || 0)]);
  const total = counts.reduce((sum, [, n]) => sum + n, 0);
  panel.hidden = !cheers || total === 0;
  if (panel.hidden) {
    list.replaceChildren();
    return;
  }
  const totalEl = $('peopleDetailCheersTotal');
  if (totalEl) totalEl.textContent = formatCount(total);
  const most = Math.max(...counts.map(([, n]) => n));
  list.replaceChildren(...counts.map(([label, n]) => {
    const row = document.createElement('div');
    row.className = 'profile-cheer';
    const bar = document.createElement('span');
    bar.className = 'profile-cheer-bar';
    const fill = document.createElement('span');
    fill.className = 'profile-cheer-fill';
    fill.style.width = `${Math.round((n / most) * 100)}%`;
    bar.append(fill);
    row.append(friendSpan('profile-cheer-name', label), bar, friendSpan('profile-cheer-count', formatCount(n)));
    return row;
  }));
}

/// Friends you and this player share, as chips that open their profiles.
/// The backend leaves the list empty on your own profile.
const MUTUAL_FRIENDS_SHOWN = 24;
function renderMutualFriends(people) {
  const panel = $('peopleDetailMutual');
  const list = $('peopleDetailMutualList');
  if (!panel || !list) return;
  const all = Array.isArray(people) ? people : [];
  panel.hidden = all.length === 0;
  const countEl = $('peopleDetailMutualCount');
  if (countEl) countEl.textContent = String(all.length);
  const chips = all.slice(0, MUTUAL_FRIENDS_SHOWN).map(p => {
    const name = p.displayName || p.userName || 'Player';
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'friend-chip profile-mutual-chip';
    const pic = document.createElement('span');
    pic.className = 'friend-avatar';
    const img = document.createElement('img');
    img.alt = '';
    img.loading = 'lazy';
    img.dataset.fallback = placeholderAvatar();
    img.src = p.AvatarUrl ? thumbSrc(p.AvatarUrl, avatarWidth(22)) : placeholderAvatar();
    pic.append(img);
    chip.append(pic, friendSpan('friend-chip-name', name));
    chip.addEventListener('click', () => showCreatorProfile(p.userName, {
      id: p.id,
      displayName: p.displayName,
      avatarUrl: p.AvatarUrl
    }));
    return chip;
  });
  if (all.length > MUTUAL_FRIENDS_SHOWN) {
    chips.push(friendSpan('profile-more-note', `+${all.length - MUTUAL_FRIENDS_SHOWN} more`));
  }
  list.replaceChildren(...chips);
}

/// "Jun 2026" for when an account was made, or '' for none (some old
/// accounts carry a placeholder year 1). English, as the rest of the UI is.
function joinedLabel(iso) {
  if (!iso) return '';
  const date = new Date(iso);
  if (Number.isNaN(date.getTime()) || date.getFullYear() < 2000) return '';
  return date.toLocaleDateString('en-US', { month: 'short', year: 'numeric' });
}

/// Backdrop for a profile header.
///
/// Vanilla has no per-user banner — its own site paints every profile with one
/// shared pattern — so the launcher uses that same pattern rather than leaving
/// the header blank. Radium keeps its themed gradient, which a scraped banner
/// then overrides where one exists.
function defaultProfileBanner() {
  if (activeNetwork === 'vanilla') return "url('assets/vanilla-pattern.png')";
  // Rec EOS: its own banner art, as on Home.
  if (activeNetwork === 'receos') return "url('assets/receos-pattern.jpg'), linear-gradient(#2f87d8, #2f87d8)";
  // Stella has no default banner image of its own, so it gets the Stella Home
  // hero: the logo on its maroon glow (style.css, .home-hero-art).
  if (activeNetwork === 'stella') {
    return "url('assets/stella-logo.png'), radial-gradient(ellipse at 50% 30%, #5a1a13 0%, #300c08 55%, #1a0604 100%)";
  }
  return 'linear-gradient(135deg, var(--green-dim), var(--green))';
}

/// `background-size` for [defaultProfileBanner]'s layers: Stella's logo is
/// drawn at a fixed share of the banner's height rather than covering it.
function defaultProfileBannerSize() {
  return activeNetwork === 'stella' ? 'auto 58%, cover' : 'cover';
}

/// Full-resolution avatar for the lightbox.
function personAvatarFullUrl(person) {
  if (person?.AvatarUrl) return person.AvatarUrl;
  const name = person?.profileImage || '';
  if (!name || name === 'DefaultProfileImage') {
    return activeNetwork === 'radium'
      ? `${RADIUM_IMG_BASE}/DefaultProfileImage`
      : placeholderAvatar();
  }
  return `${RADIUM_IMG_BASE}/${name}`;
}

// DOM shortcuts
const $ = id => document.getElementById(id);

/// What the backdrop field shows while the backdrop is a stored picture — a
/// picked file or a downloaded address. The data: URI itself would be up to
/// 1.4 MB of text in an input, which freezes it.
const SAVED_BACKDROP_LABEL = '(Saved picture)';

function setBgImageUI(val) {
  const el = $('theme-bgImage');
  if (!el) return;
  if (val && val.startsWith('data:image/')) {
    el.dataset.savedPicture = '1';
    el.value = SAVED_BACKDROP_LABEL;
  } else {
    delete el.dataset.savedPicture;
    el.value = val || '';
  }
}

// Formatting utility helpers
function formatBytes(b) {
  if (b < 1024)        return `${b} B`;
  if (b < 1048576)     return `${(b/1024).toFixed(1)} KB`;
  if (b < 1073741824)  return `${(b/1048576).toFixed(1)} MB`;
  return `${(b/1073741824).toFixed(2)} GB`;
}
function formatEta(secs) {
  if (secs < 0 || !isFinite(secs)) return '—';
  if (secs < 60)   return `${Math.round(secs)}s`;
  if (secs < 3600) return `${Math.floor(secs/60)}m ${Math.round(secs%60)}s`;
  return `${Math.floor(secs/3600)}h ${Math.floor((secs%3600)/60)}m`;
}

// Notification toasts
function toast(msg, type = 'info', ms = 3200) {
  const c = $('toastContainer');
  const el = document.createElement('div');
  el.className = `toast ${type}`;
  el.textContent = msg;
  c.appendChild(el);
  requestAnimationFrame(() => requestAnimationFrame(() => el.classList.add('show')));
  setTimeout(() => { el.classList.remove('show'); setTimeout(() => el.remove(), 300); }, ms);
}

// Tab routing switch.
//
// Scoped to the nav itself: `.nav-btn` is also worn by the network switcher and
// its menu options, purely so they inherit each theme's button styling. Those
// carry no data-tab and must not take part in tab routing — or in the
// deactivate sweep below, which would strip the selected network's highlight.
document.querySelectorAll('.sidebar-nav .nav-btn').forEach(btn => {
  btn.addEventListener('click', () => {
    document.querySelectorAll('.sidebar-nav .nav-btn').forEach(b => b.classList.remove('active'));
    document.querySelectorAll('.tab-panel').forEach(p => p.classList.remove('active'));
    btn.classList.add('active');
    const tabName = btn.dataset.tab;
    const p = $('tab-' + tabName);
    if (p) p.classList.add('active');

    // Lazy load data when switching tabs — but only data we don't already have.
    // Coming back to a tab you were just on now shows what was there, which is
    // both instant and where you left off; a page older than LIST_STALE_MS is
    // refreshed underneath the rows rather than in place of them.
    if (tabName === 'home') {
      loadHomeRooms();
    } else if (tabName === 'rooms') {
      loadFilters();
      if (!listIsFresh(roomsKey(), roomsRenderKey, roomsRenderAt)) loadRooms();
    } else if (tabName === 'people') {
      if (!listIsFresh(peopleKey(), peopleRenderKey, peopleRenderAt, peopleFreshMs)) loadPeople();
    } else if (tabName === 'feed') {
      // Only reload an empty feed, so returning to the tab keeps your place
      // in the list instead of jumping back to the top.
      if (!$('feedGrid')?.children.length) loadFeed();
    } else if (tabName === 'friends') {
      refreshStellaFriends();
    } else if (tabName === 'chats') {
      loadChatThreads();
    }
  });
});

// Titlebar actions
$('btnMinimize')?.addEventListener('click', () => window.radium?.minimize());
$('btnMaximize')?.addEventListener('click', () => window.radium?.maximize());
$('btnClose')?.addEventListener('click',    () => window.radium?.close());

window.radium?.onWindowMaximizedState((isMaximized) => {
  const btn = $('btnMaximize');
  if (btn) {
    btn.innerHTML = isMaximized ? '❐' : '▢';
    btn.setAttribute('aria-label', isMaximized ? 'Restore' : 'Maximize');
  }
});

// Sidebar logo image load failure fallback
const logoImg = $('sidebarLogo');
if (logoImg) {
  logoImg.addEventListener('error', () => {
    const combo = $('logoCombo');
    if (combo) combo.style.display = 'none';
    const lf = $('logoFallback'); if (lf) lf.style.display = 'flex';
  });
}

// The launcher's own version. Not shown in the sidebar: it is written to the
// LOGS tab at startup and sent with a bug report.
let launcherVersion = '';
async function loadVersion() {
  const v = await window.radium?.getVersion();
  if (v) launcherVersion = v;
  const about = $('aboutVersion');
  if (about) about.textContent = v ? `Version ${v}` : '';
}

async function loadConfig() {
  config = (await window.radium?.getConfig()) || {};

  // Defaults
  if (!config.apiUrl)   config.apiUrl   = 'https://api.radie.app/';
  if (!config.playMode) config.playMode = 'screen';

  // `=== true`, not `!== false`: minimize-on-launch is off by default now, so
  // a config that predates the field must read as off rather than on.
  setToggle('tgl-minimizeOnLaunch', config.minimizeOnLaunch === true);
  setToggle('tgl-closeOnLaunch',    config.closeOnLaunch    === true);
  setToggle('tgl-autoUpdate',       config.autoUpdate       !== false);
  setToggle('tgl-disableWarnings',   config.disableWarnings   === true);
  setToggle('tgl-runInBackground',   config.runInBackground   !== false);
  syncLaunchOptionLabel();
  refreshAutostartToggle();
  setToggle('tgl-notifPopups',       config.notifPopups       !== false);
  setToggle('tgl-notifSound',        config.notifSound        !== false);

  // Play mode
  playMode = config.playMode || 'screen';
  setModeUI(playMode);
  updateQsMode();

  // Theme. The backend migrates anything unrecognised — including the removed
  // custom theme — before it gets here, so this is always a skin that ships.
  const activeTheme = AVAILABLE_THEMES.includes(config.theme) ? config.theme : DEFAULT_THEME;
  setValue('cfgTheme', activeTheme);

  // Where Stella's friends list shows.
  applyFriendsView();
  // Home's section order, and the banner picture.
  applyHomeLayout();
  applyHomeBanner();

  // Liquid Glass, which is now an effect over that skin rather than a mode of
  // a custom theme.
  config.glass = config.glass || {};
  const glassOn = config.glass.enabled === true;
  setToggle('tgl-glassEnabled', glassOn);
  // On unless explicitly switched off, matching GlassSettings::default.
  setToggle('tgl-glassFull', config.glass.fullEffects !== false);
  setGlassTintUI(safeColor(config.glass.tint, DEFAULT_GLASS_TINT));
  setBgImageUI(config.glass.bgImage || '');
  updateGlassControls(glassOn);

  applyTheme(activeTheme);

  // A backdrop saved as an address, from before addresses were downloaded:
  // the page could never load it (see paintableBackdrop). Fetched once now and
  // stored as a picture; if that fails it stays as it is, to try again on the
  // next start, and glass keeps its tint meanwhile.
  const savedBackdrop = String(config.glass.bgImage || '').trim();
  if (savedBackdrop.startsWith('https://')) {
    setGlassBackdropFromUrl(savedBackdrop, { quiet: true });
  }

  // Network last, so the brand and capability gating are applied against a
  // fully-loaded config.
  applyNetworkUI(NETWORKS[config.network] ? config.network : 'radium');

  // Both install rows, not just the active network's — after applyNetworkUI so
  // the ACTIVE tag lands on the row the loaded config actually selected.
  await refreshInstallDirRows();
}

/// Marks the body while Liquid Glass is on; every glass rule hangs off it.
const GLASS_CLASS = 'glass-enabled';
/// Set while a backdrop picture is in use: it swaps the tint field out for the
/// picture in `--lg-backdrop`. See `skins/11-glass.css`.
const GLASS_IMAGE_CLASS = 'glass-has-image';
/// Full glass effects, on by default. Off gives the lighter look for weak GPUs.
const GLASS_FULL_CLASS = 'glass-full';

/// Enable or grey out the controls that only mean something under glass, and
/// grey out Active Skin in the other direction while glass has taken over from
/// it.
///
/// The tint and the backdrop shape the glass surfaces and do nothing without
/// them, so with glass off they are shown but inert — the same "say why"
/// treatment applies to Active Skin once glass is on: picking a different skin
/// would visibly do nothing, since glass repaints every surface itself.
function updateGlassControls(glassOn) {
  const note = $('glassOffNote');
  if (note) note.hidden = glassOn;
  for (const id of ['theme-glassBg', 'btnResetGlassTint', 'theme-bgImage', 'btnBrowseBgFile', 'btnClearBgImage']) {
    const el = $(id);
    if (el) el.disabled = !glassOn;
  }
  $('glassOptions')?.classList.toggle('is-off', !glassOn);

  const skinSelect = $('cfgTheme');
  if (skinSelect) skinSelect.disabled = glassOn;
  $('activeSkinRow')?.classList.toggle('is-off', glassOn);
  const skinNote = $('activeSkinLockNote');
  if (skinNote) skinNote.hidden = !glassOn;
}

/// A CSS colour that is safe to interpolate into a generated stylesheet.
///
/// Everything here ends up inside a `<style>` element, so a value carrying a
/// `;` or a `}` closes the declaration and opens a rule of its own — which in a
/// webview that can reach every backend command is not a cosmetic problem. The
/// colour pickers are `<input type="color">` and hand back `#rrggbb`, but these
/// same values are read straight from config.json at startup, and that file is
/// editable by hand and shared between people swapping themes.
///
/// `glassBg` and `bgImage` were already checked at their own use sites; the
/// eleven palette variables were the ones that weren't.
///
/// Only the four lengths CSS actually defines — #rgb, #rgba, #rrggbb,
/// #rrggbbaa. A looser `{3,8}` would also admit 5 and 7 digits, which are not
/// colours at all and would land in the sheet as a dead declaration; it would
/// also disagree with `is_hex_color` in config.rs, which repairs them. The two
/// validators have to say the same thing or a value accepted here gets
/// rewritten on the next load.
const HEX_COLOR = /^#(?:[0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$/;

function safeColor(value, fallback) {
  return HEX_COLOR.test(String(value ?? '').trim()) ? String(value).trim() : fallback;
}

/// Every skin that ships. Mirrors `AVAILABLE_THEMES` in config.rs and the
/// `<option>` list in index.html; a Rust test holds the latter two together.
const AVAILABLE_THEMES = [
  'steam-green', 'steam2010', 'win98', 'win95', 'winxp', 'royalenoir',
  'winvista', 'win7', 'macosclassic', 'macosaqua', 'moderndark',
  'modernlight', 'moderngreen', 'blackandwhite', 'blackandwhite-inverted'
];

/// The skin a fresh install starts on.
const DEFAULT_THEME = 'blackandwhite';

/// The class a skin is stamped as on <body>: `theme-<skin>`, except Modern
/// Neon Dark. `theme-moderndark` is Liquid Glass's layout (GLASS_LAYOUT_THEME
/// below) and style.css keeps those rules exactly as glass needs them, so that
/// skin is drawn under `theme-neondark` instead. Mirrored in boot.js.
function skinClass(skin) {
  return 'theme-' + (skin === 'moderndark' ? 'neondark' : skin);
}

/// The layout Liquid Glass is drawn on.
///
/// Glass has always been rounded: the editor locked the style base to "modern"
/// whenever it was switched on, which added this class. Its own rules assume
/// that geometry — the radii, the panel insets, the pill buttons — so it keeps
/// coming along now that glass is a setting in its own right.
const GLASS_LAYOUT_THEME = 'moderndark';

/// Point Liquid Glass at a tint, a backdrop and the full-effects switch.
///
/// The sheet itself is `skins/11-glass.css`, loaded with the other skins and
/// inert until `glass-enabled` is on <body>. Only these three things about it
/// were ever dynamic, so only these three are set here.
///
/// It used to be an ~80 KB template literal built in this file and injected as
/// a <style> on every applyTheme() call — which is once per keystroke in the
/// backdrop URL field. Each one rebuilt the string, re-parsed the whole sheet,
/// and wrote it to localStorage for boot.js with the backdrop's data: URI
/// inlined, up to 1.4 MB of base64 a keypress.
///
/// Glass overrides every colour token itself, which is why it never depended on
/// the custom palette it used to live next to and why it survived that editor's
/// removal unchanged.
///
/// Modelled on Apple's Liquid Glass rather than on generic glassmorphism, and
/// most of the difference is restraint:
///
/// - Glass needs something behind it. Over a flat near-black tint every blur
///   averages to the same grey and the panels read as dull cards, so without an
///   image the tint is spread into a soft field of colour for them to pick up.
/// - The edge is lit rather than bordered: a hairline that is brightest at two
///   opposite corners and fades along the sides, drawn as a masked gradient
///   ring. A uniform 1px white border is what makes glass look like plastic.
/// - Controls are plain capsules with a light rim. The split top-half gloss and
///   the shine sweeping across on hover were Aqua-era tells.
/// - Only the four framing surfaces blur — the sidebar, the status cards, the
///   settings groups and the download panel. Every blur is redone whenever
///   anything near it repaints, which is heavy on integrated GPUs; everything
///   else is tinted, which over this soft backdrop reads nearly the same.
///
/// Returns what was applied, so [`cacheThemeForBoot`] can hand boot.js the same
/// three values to replay before the first paint.
function applyGlassVars(tint, bgImage, fullEffects = false) {
  const safeGlassBg = safeColor(tint, DEFAULT_GLASS_TINT);
  // Only a stored picture is painted. An https:// address is one the CSP
  // refuses to load (img-src names no remote host), and `glass-has-image`
  // over a picture that never arrives left nothing but a faint scrim over a
  // transparent window — the desktop showing through the launcher. An address
  // is downloaded into a data: URI instead (setGlassBackdropFromUrl); until
  // that lands, glass keeps its tint field.
  const safeBgImage = paintableBackdrop(bgImage);

  const style = document.body.style;
  style.setProperty('--lg-tint', safeGlassBg);
  if (safeBgImage) {
    // A complete `url(...)`, because that is what the stylesheet substitutes.
    // `setProperty` does not escape, but `safeBackdrop` has already refused
    // every character that could close the declaration — and a value the
    // engine still cannot parse is dropped rather than reinterpreted.
    style.setProperty('--lg-backdrop', `url("${safeBgImage}")`);
  } else {
    style.removeProperty('--lg-backdrop');
  }
  document.body.classList.toggle(GLASS_IMAGE_CLASS, !!safeBgImage);
  document.body.classList.toggle(GLASS_FULL_CLASS, !!fullEffects);

  return { tint: safeGlassBg, bgImage: safeBgImage, fullEffects: !!fullEffects };
}

/// A backdrop URL that is safe to interpolate into `url('...')`.
///
/// Mirrors `GlassSettings::sanitize` in config.rs: the two shapes the picker
/// and the URL field produce, and no character that could close the
/// declaration and open a rule of its own.
const BACKDROP_FORBIDDEN = /['"(){}\\]/;
/// Whether a string carries a control character.
///
/// Spelled as a code-point test rather than a regex so the range is legible
/// and carries no escape sequences of its own.
function hasControlChars(s) {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c < 0x20 || c === 0x7f) return true;
  }
  return false;
}

function safeBackdrop(value) {
  const url = String(value ?? '').trim();
  if (!url) return '';

  const isData = url.startsWith('data:image/');
  if (!isData && !url.startsWith('https://')) return '';
  if (BACKDROP_FORBIDDEN.test(url) || hasControlChars(url)) return '';
  // A data: URI legitimately contains `;` — the `;base64` marker — so it is
  // exempt from that one character. Nothing else may carry it, since outside a
  // data URI a `;` is what ends the declaration.
  if (!isData && url.includes(';')) return '';
  return url;
}

/// A backdrop the page can actually draw: a safe `data:` picture, or ''.
/// Mirrored in boot.js.
function paintableBackdrop(value) {
  const url = safeBackdrop(value);
  return url.startsWith('data:image/') ? url : '';
}

/// Apply a skin, plus Liquid Glass over it when that is switched on.
///
/// Glass is an effect rather than a skin: it repaints every colour token and
/// brings its own layout, so while it is on it stands in for the selected skin
/// entirely. The dropdown keeps that selection for when it is switched off.
function applyTheme(theme) {
  // Strip only the theme classes and the glass marker. This used to whitelist
  // 'animations-enabled' and drop everything else, which silently wiped
  // unrelated state classes on <body> (e.g. 'client-installed', which gates the
  // PLAY button and the Manage Client panel) every time the theme changed.
  const GLASS_MARKERS = [GLASS_CLASS, GLASS_IMAGE_CLASS, GLASS_FULL_CLASS];
  document.body.className = document.body.className
    .split(' ')
    .filter(c => c && !c.startsWith('theme-') && !GLASS_MARKERS.includes(c))
    .join(' ');

  const skin = AVAILABLE_THEMES.includes(theme) ? theme : DEFAULT_THEME;
  const glass = config.glass || {};
  const glassOn = glass.enabled === true;

  if (glassOn) {
    document.body.classList.add('theme-' + GLASS_LAYOUT_THEME, GLASS_CLASS);
  } else {
    document.body.classList.add(skinClass(skin));
  }

  if (glassOn) {
    cacheThemeForBoot(skin, applyGlassVars(glass.tint, glass.bgImage, glass.fullEffects !== false));
  } else {
    // The variables are left on <body> deliberately — they cost nothing while
    // nothing matches `glass-enabled`, and clearing them would make switching
    // glass back on flash the default tint for a frame.
    cacheThemeForBoot(skin, null);
  }

  // Animations are always on; the setting that could turn them off is gone.
  // The class still gates them in the stylesheet, so it is set here — the
  // className rewrite above keeps it, but boot.js may not have run first.
  document.body.classList.add('animations-enabled');
  try {
    // A leftover 'false' from the old setting would otherwise sit in storage.
    localStorage.removeItem('radium-animations');
  } catch (e) {}

  syncTray();
}

/// Remember enough for boot.js to paint the right thing before the first frame.
///
/// Without this the window renders the base stylesheet until the config arrives
/// over IPC and then snaps to the real skin — a visible flash on every start.
///
/// `glass` is what [`applyGlassVars`] applied, or `null` with glass off. What
/// used to be stored here was the whole generated stylesheet — ~80 KB, plus the
/// backdrop's data: URI inlined — rewritten on every applyTheme() call. Now the
/// sheet is a file the webview caches and only these three values travel.
function cacheThemeForBoot(skin, glass) {
  try {
    localStorage.setItem('radium-theme', skin);
    if (glass) {
      localStorage.setItem('radium-glass', JSON.stringify(glass));
    } else {
      localStorage.removeItem('radium-glass');
    }
    // The pre-1.x key, which held the whole generated sheet. Dropped so an
    // upgrade gives back however many hundred kilobytes it was holding.
    localStorage.removeItem('radium-glass-css');
  } catch (e) {
    // Quota, or a webview with storage disabled. The only cost is the flash
    // this exists to avoid, so there is nothing to report.
  }
}

function setValue(id, val) { const el = $(id); if (el) el.value = val; }

function setToggle(id, val) {
  const el = $(id); if (!el) return;
  if (val) el.classList.add('on'); else el.classList.remove('on');
}
function getToggle(id) { return $(id)?.classList.contains('on') ?? false; }

// "Start with Windows" isn't in config.json: the registry entry is the truth,
// so the switch is read from and written to it directly.
async function refreshAutostartToggle() {
  try {
    setToggle('tgl-launchAtStartup', await window.radium.getAutostart());
  } catch (e) {}
}
$('tgl-launchAtStartup')?.addEventListener('click', async () => {
  const el = $('tgl-launchAtStartup');
  if (el.dataset.pending === 'true') return;
  el.dataset.pending = 'true';
  const want = !getToggle('tgl-launchAtStartup');
  setToggle('tgl-launchAtStartup', want);
  try {
    const now = await window.radium.setAutostart(want);
    setToggle('tgl-launchAtStartup', now);
    addLog(now ? 'Rec EOS Launcher will start with Windows' : 'Rec EOS Launcher will no longer start with Windows', 'info');
  } catch (err) {
    setToggle('tgl-launchAtStartup', !want);
    toast(String(err), 'error');
  } finally {
    el.dataset.pending = 'false';
  }
});

// The first time the window is closed into the tray, say so, once — otherwise
// the launcher just seems to have quit.
const TRAY_HINT_KEY = 'radium-tray-hint-shown';
let trayHintShown = false;
window.radium?.onLauncherHidden?.(() => {
  if (trayHintShown) return;
  // Not when the window stepped aside for a game starting ("Hide launcher
  // when game starts"): a topmost card over a game going full screen can
  // knock it out of full screen, which is why notification pop-ups wait for
  // the game too. The hint keeps for the next time the window is closed.
  if (isGameRunning || isGameLaunching) return;
  try {
    if (localStorage.getItem(TRAY_HINT_KEY)) { trayHintShown = true; return; }
    localStorage.setItem(TRAY_HINT_KEY, '1');
  } catch (e) {}
  trayHintShown = true;
  window.radium.desktopNotify([{
    id: 'tray-hint',
    sender: null,
    icon: 'bell',
    app: 'Rec EOS Launcher',
    style: notifPopStyle(),
    parts: [
      { t: 'Still running in the background. ', b: true },
      { t: 'Open it or quit from the tray icon. You can turn this off in Settings.' },
    ],
  }]).catch(() => {});
});

['tgl-minimizeOnLaunch', 'tgl-closeOnLaunch', 'tgl-autoUpdate', 'tgl-disableWarnings',
 'tgl-runInBackground', 'tgl-notifPopups', 'tgl-notifSound'].forEach(id =>
  $(id)?.addEventListener('click', () => {
    $(id).classList.toggle('on');
    // Minimising and hiding on launch are two answers to the same question,
    // so turning one on turns the other off.
    if (getToggle(id)) {
      if (id === 'tgl-minimizeOnLaunch') setToggle('tgl-closeOnLaunch', false);
      if (id === 'tgl-closeOnLaunch') setToggle('tgl-minimizeOnLaunch', false);
    }
    if (id === 'tgl-runInBackground') syncLaunchOptionLabel();
    autoSaveSettings();
  })
);

/// With tray mode on, closing only hides the window, so the launch option
/// says what it will actually do.
function syncLaunchOptionLabel() {
  const tray = getToggle('tgl-runInBackground');
  const label = $('lblCloseOnLaunch');
  if (label) label.textContent = tray ? 'Hide the launcher' : 'Close the launcher';
  const desc = $('descCloseOnLaunch');
  if (desc) {
    desc.textContent = tray
      ? 'Tucks it into the tray once the game is running.'
      : 'Quits the launcher once the game is running.';
  }
}

// ── Settings sections ───────────────────────────────────────────────────
// One section at a time, picked from the list at the left. Notifications
// (Vanilla and Stella, the networks with accounts) and Friends (Stella) exist
// on those networks only; on another one the section falls back to General.

let settingsSection = 'general';

function settingsSectionAvailable(btn) {
  if (btn.classList.contains('network-only-vanilla')) return activeNetwork === 'vanilla';
  if (btn.classList.contains('network-only-stella')) return activeNetwork === 'stella';
  if (btn.classList.contains('network-only-accounts')) return activeNetwork === 'vanilla' || activeNetwork === 'stella';
  return true;
}

function showSettingsSection(name, { focus = false } = {}) {
  const btns = [...document.querySelectorAll('.settings-nav-btn')];
  if (!btns.length) return;
  const btn = btns.find(b => b.dataset.section === name && settingsSectionAvailable(b)) || btns[0];
  const changed = btn.dataset.section !== settingsSection;
  settingsSection = btn.dataset.section;
  for (const b of btns) {
    const on = b === btn;
    b.classList.toggle('active', on);
    b.setAttribute('aria-selected', on ? 'true' : 'false');
    b.tabIndex = on ? 0 : -1;
  }
  document.querySelectorAll('.settings-page').forEach(page => {
    const on = page.id === `setpage-${settingsSection}`;
    page.hidden = !on;
    page.classList.toggle('active', on);
  });
  if (changed) {
    // A menu or the tint picker open on the page being left goes with it.
    closeCselect();
    closeTintPicker();
    const body = document.querySelector('#tab-settings .settings-body');
    if (body) body.scrollTop = 0;
  }
  if (focus) btn.focus();
}

document.querySelectorAll('.settings-nav-btn').forEach(btn => {
  btn.addEventListener('click', () => showSettingsSection(btn.dataset.section));
});

// ── Home layout and banner (Settings → Home) ────────────────────────────
// config.home: `order` (Home's sections from the top) and `hidden` (the ones
// switched off) are saved with the rest of the settings; `banners` holds a
// picture of the user's own per network and is written only through
// setHomeBanner (see withoutBackdrop in the shim). The backend repairs both
// (config::HomeSettings::sanitize); the same rules are applied here so the
// page never acts on a name it doesn't know.

const HOME_SECTIONS = ['status', 'friends', 'rooms'];
const HOME_SECTION_INFO = {
  status:  { label: 'Status',            desc: 'Servers, players online and the game client.' },
  friends: { label: 'Friends',           desc: 'Who is online, with JOIN. Off puts them in a tab of their own.' },
  rooms:   { label: 'Popular right now', desc: "The network's busiest rooms at the moment." },
};

/// Home's order and hidden list, cleaned up: known names, each once, every
/// section in the order.
function homeLayout() {
  const raw = config?.home || {};
  const order = [];
  for (const name of Array.isArray(raw.order) ? raw.order : []) {
    if (HOME_SECTIONS.includes(name) && !order.includes(name)) order.push(name);
  }
  for (const name of HOME_SECTIONS) if (!order.includes(name)) order.push(name);
  const hidden = (Array.isArray(raw.hidden) ? raw.hidden : []).filter(n => HOME_SECTIONS.includes(n));
  return { order, hidden };
}

/// Whether a section appears in Settings' list on the network in use:
/// Friends exists on Stella only.
function homeSectionAvailable(name) {
  return name !== 'friends' || activeNetwork === 'stella';
}

/// Home's layouts (config::HOME_STYLES); the first is the default.
const HOME_STYLES = ['showcase', 'spotlight', 'compact', 'dashboard'];
/// The sections Dashboard puts in its side column when none were moved.
const HOME_SIDE_DEFAULT = ['status', 'friends'];

/// The sections Dashboard puts in its column beside the game (config.home.side);
/// the rest go under the game.
function homeSide() {
  const side = config?.home?.side;
  return Array.isArray(side) ? side.filter(n => HOME_SECTIONS.includes(n)) : HOME_SIDE_DEFAULT;
}

/// Home's layout. Before the config arrives, the one boot.js replayed.
function homeStyle() {
  const style = config?.home?.style;
  if (HOME_STYLES.includes(style)) return style;
  return HOME_STYLES.find(s => document.body.classList.contains(`home-style-${s}`)) || HOME_STYLES[0];
}

/// Make `parent`'s children `els`, in that order, moving only the ones out of
/// place: a node that is moved loses keyboard focus and restarts its
/// animations, so the hero is left where it is unless it has to go.
function arrangeChildren(parent, els) {
  let ref = parent.firstElementChild;
  for (const el of els) {
    if (el === ref) {
      ref = ref.nextElementSibling;
      continue;
    }
    parent.insertBefore(el, ref);
  }
}

/// Lay Home out in its style, put its sections in the saved order and hide
/// the ones switched off. The hero and the download panel always lead.
/// Moved rather than reordered with CSS `order`, so keyboard focus follows
/// what is on screen. Dashboard puts them in two columns (.home-columns):
/// the hero and the download panel lead the wide one, the sections in
/// homeSide() go in the narrow one beside it (status and friends unless
/// moved), the rest under the hero, each column in the saved order.
function applyHomeLayout() {
  const tab = $('tab-home');
  if (!tab) return;
  const style = homeStyle();
  for (const s of HOME_STYLES) document.body.classList.toggle(`home-style-${s}`, s === style);
  // Replayed by boot.js before the next start's first paint.
  try { localStorage.setItem('radium-home-style', style); } catch (e) {}
  const lead = [tab.querySelector('.home-hero-wrap'), $('downloadSection')].filter(Boolean);
  const nodes = {
    status: tab.querySelector('.home-stats'),
    friends: $('stellaFriendsCard'),
    rooms: $('homeRoomsSection'),
  };
  const { order, hidden } = homeLayout();
  const sections = order.map(name => nodes[name]).filter(Boolean);
  let columns = $('homeColumns');
  if (style === 'dashboard') {
    if (!columns) {
      columns = document.createElement('div');
      columns.id = 'homeColumns';
      columns.className = 'home-columns';
      columns.innerHTML = '<div class="home-col home-main"></div><div class="home-col home-side"></div>';
    }
    const [main, side] = columns.children;
    const inSide = homeSide();
    arrangeChildren(tab, [columns]);
    arrangeChildren(main, [...lead, ...order.filter(n => !inSide.includes(n)).map(n => nodes[n]).filter(Boolean)]);
    arrangeChildren(side, order.filter(n => inSide.includes(n)).map(n => nodes[n]).filter(Boolean));
  } else {
    arrangeChildren(tab, [...lead, ...sections]);
    columns?.remove();
  }
  syncHomeStylePicker();
  // Friends' place on Home is friendsView's to decide (body.friends-view-*).
  nodes.status?.classList.toggle('is-turned-off', hidden.includes('status'));
  nodes.rooms?.classList.toggle('is-turned-off', hidden.includes('rooms'));
  renderHomeLayoutList();
}

/// Settings' style picker: the option in use is the checked radio, and the
/// one Tab lands on.
function syncHomeStylePicker() {
  const style = homeStyle();
  document.querySelectorAll('#homeStylePicker .home-style-option').forEach(opt => {
    const on = opt.dataset.style === style;
    opt.setAttribute('aria-checked', String(on));
    opt.tabIndex = on ? 0 : -1;
  });
}

function setHomeStyle(style) {
  if (!HOME_STYLES.includes(style) || style === homeStyle()) return;
  config.home = { ...(config.home || {}), style };
  applyHomeLayout();
  addLog(`Home style: ${style}.`, 'info');
  autoSaveSettings();
}

document.querySelectorAll('#homeStylePicker .home-style-option').forEach(opt => {
  opt.addEventListener('click', () => setHomeStyle(opt.dataset.style));
});
// Arrow keys move the choice, as in any radio group.
$('homeStylePicker')?.addEventListener('keydown', (e) => {
  const keys = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 };
  if (!(e.key in keys) && e.key !== 'Home' && e.key !== 'End') return;
  e.preventDefault();
  const opts = [...document.querySelectorAll('#homeStylePicker .home-style-option')];
  const i = Math.max(0, opts.findIndex(o => o.dataset.style === homeStyle()));
  const j = e.key === 'Home' ? 0 : e.key === 'End' ? opts.length - 1 : (i + keys[e.key] + opts.length) % opts.length;
  setHomeStyle(opts[j].dataset.style);
  opts[j].focus();
});
// Laid out now in the style boot.js replayed, rather than when the config
// arrives, so Dashboard opens in its columns.
applyHomeLayout();

/// Settings' list of Home's sections: arrows to move each one, and its
/// switch. Rebuilt from config whenever either changes.
/// The sections Settings lists, top to bottom, on the network in use. On
/// Dashboard they come in its two columns, under the game then beside it,
/// and `split` is where the second begins; elsewhere it is the length.
function homeLayoutRows() {
  const { order } = homeLayout();
  const shown = order.filter(homeSectionAvailable);
  if (homeStyle() !== 'dashboard') return { rows: shown, split: shown.length };
  const side = homeSide();
  const main = shown.filter(n => !side.includes(n));
  return { rows: [...main, ...shown.filter(n => side.includes(n))], split: main.length };
}

function renderHomeLayoutList() {
  const list = $('homeLayoutList');
  if (!list) return;
  const { hidden } = homeLayout();
  const { rows: shown, split } = homeLayoutRows();
  const columns = homeStyle() === 'dashboard';
  const isOn = name => name === 'friends' ? friendsView() === 'home' : !hidden.includes(name);
  // Same rows in the same places: only a switch changed, so it is flipped
  // where it is. Rebuilt, the clicked switch was replaced by one already in
  // its new state, and the knob jumped across instead of sliding.
  const shape = JSON.stringify([shown, split, columns]);
  if (list.dataset.shape === shape) {
    list.querySelectorAll('.home-layout-row').forEach(row => {
      const on = isOn(row.dataset.section);
      const toggle = row.querySelector('.toggle-wrap');
      toggle.classList.toggle('on', on);
      toggle.setAttribute('aria-checked', String(on));
    });
    return;
  }
  list.dataset.shape = shape;
  list.textContent = '';
  // Dashboard: a heading over each column, and a word in an empty one on
  // how to fill it.
  const group = (text, empty) => {
    const head = document.createElement('div');
    head.className = 'home-layout-group';
    head.textContent = text;
    list.appendChild(head);
    if (!empty) return;
    const note = document.createElement('div');
    note.className = 'home-layout-empty';
    note.textContent = empty;
    list.appendChild(note);
  };
  if (columns) group('Under the game', split === 0 ? 'Nothing here. Move a section up into this column.' : '');
  shown.forEach((name, i) => {
    if (columns && i === split) group('Beside the game', '');
    const info = HOME_SECTION_INFO[name];
    const on = isOn(name);
    // Past the end of its column, an arrow takes a section into the other one.
    const crossUp = columns && i === split;
    const crossDown = columns && i === split - 1;
    const upLabel = crossUp ? `Move ${info.label} under the game` : `Move ${info.label} up`;
    const downLabel = crossDown ? `Move ${info.label} beside the game` : `Move ${info.label} down`;
    const upOff = i === 0 && !crossUp;
    const downOff = i === shown.length - 1 && !crossDown;
    const row = document.createElement('div');
    row.className = 'sg-toggle-row home-layout-row';
    row.dataset.section = name;
    row.innerHTML = `
      <span class="home-layout-move">
        <button type="button" class="btn-open-folder layout-move" data-dir="-1" aria-label="${escapeHtml(upLabel)}"${upOff ? ' disabled' : ''}>
          <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M7.41 15.41 12 10.83l4.59 4.58L18 14l-6-6-6 6z"/></svg>
        </button>
        <button type="button" class="btn-open-folder layout-move" data-dir="1" aria-label="${escapeHtml(downLabel)}"${downOff ? ' disabled' : ''}>
          <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M7.41 8.59 12 13.17l4.59-4.58L18 10l-6 6-6-6z"/></svg>
        </button>
      </span>
      <span class="sg-text">
        <span class="sg-label">${escapeHtml(info.label)}</span>
        <span class="sg-desc">${escapeHtml(info.desc)}</span>
      </span>
      <div class="toggle-wrap${on ? ' on' : ''}" role="switch" tabindex="0" aria-checked="${on}" aria-label="Show ${escapeHtml(info.label)} on Home"><div class="tgl-knob"></div></div>
    `;
    row.querySelectorAll('.layout-move').forEach(btn => btn.addEventListener('click', (e) => {
      e.stopPropagation();
      moveHomeSection(name, Number(btn.dataset.dir));
    }));
    const toggle = row.querySelector('.toggle-wrap');
    const flip = () => setHomeSectionShown(name, !toggle.classList.contains('on'));
    toggle.addEventListener('click', flip);
    toggle.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); flip(); }
    });
    list.appendChild(row);
  });
  if (columns && split === shown.length) group('Beside the game', 'Nothing here. Move a section down into this column.');
}

/// Move a section one place up (-1) or down (1) among the ones this network
/// shows — past a section that is only on another network, never just onto it.
/// On Dashboard, moving down off the end of the column under the game puts
/// the section at the top of the one beside it, and moving up off the top of
/// that one puts it back at the bottom of the first.
function moveHomeSection(name, dir) {
  const { order, hidden } = homeLayout();
  const { rows, split } = homeLayoutRows();
  const i = rows.indexOf(name);
  if (i < 0) return;
  let side = homeSide();
  const crossing = homeStyle() === 'dashboard'
    && ((dir > 0 && i === split - 1) || (dir < 0 && i === split));
  if (crossing) {
    // Placed in the saved order next to where it lands, so it shows at that
    // end of its new column.
    order.splice(order.indexOf(name), 1);
    if (dir > 0) {
      side = [...side, name];
      const first = rows[split];
      order.splice(first ? order.indexOf(first) : order.length, 0, name);
    } else {
      side = side.filter(n => n !== name);
      const last = rows[split - 1];
      order.splice(last ? order.indexOf(last) + 1 : 0, 0, name);
    }
  } else {
    const j = i + dir;
    if (j < 0 || j >= rows.length) return;
    const a = order.indexOf(rows[i]);
    const b = order.indexOf(rows[j]);
    [order[a], order[b]] = [order[b], order[a]];
  }
  config.home = { ...(config.home || {}), order, hidden, side };
  applyHomeLayout();
  // Keep the moved row's arrow under the keyboard.
  const btn = document.querySelector(`.home-layout-row[data-section="${name}"] .layout-move[data-dir="${dir}"]`);
  (btn && !btn.disabled ? btn : document.querySelector(`.home-layout-row[data-section="${name}"] .layout-move:not(:disabled)`))?.focus();
  autoSaveSettings();
}

/// Show or hide a section. Friends' switch is where the friends list goes:
/// on Home, or a tab of its own (Settings → Friends can also hide it).
function setHomeSectionShown(name, show) {
  if (name === 'friends') {
    config.stella = { ...(config.stella || {}), friendsView: show ? 'home' : 'tab' };
    applyFriendsView();
    autoSaveSettings();
    return;
  }
  const { order, hidden } = homeLayout();
  const next = show ? hidden.filter(n => n !== name) : [...new Set([...hidden, name])];
  config.home = { ...(config.home || {}), order, hidden: next };
  applyHomeLayout();
  document.querySelector(`.home-layout-row[data-section="${name}"] .toggle-wrap`)?.focus();
  if (name === 'rooms' && show) loadHomeRooms();
  autoSaveSettings();
}

/// Longest edge, and size, a banner picture is stored at. The hero is at
/// most ~1,700 CSS px wide; 1,920 keeps it sharp. Matches
/// config::HOME_BANNER_MAX_BYTES (a little under, for the backend's check).
const BANNER_MAX_EDGE = 1920;
const BANNER_MAX_STORED_BYTES = 950_000;

/// The picture the network in use shows at the top of Home, if the user set
/// one and it is safe to paint.
function homeBannerFor(network = activeNetwork) {
  return paintableBackdrop(config?.home?.banners?.[network] || '');
}

/// Paint the network in use's banner (or its own art), mirror it for the
/// pre-paint bootstrap, and bring Settings' preview up to date.
function applyHomeBanner() {
  const image = homeBannerFor();
  const body = document.body;
  body.classList.toggle('has-home-banner', !!image);
  if (image) body.style.setProperty('--home-banner', `url("${image}")`);
  else body.style.removeProperty('--home-banner');
  // One picture, the active network's: boot.js shows it before the config
  // arrives instead of flashing the network's art first.
  try {
    if (image) localStorage.setItem('radium-home-banner', JSON.stringify({ network: activeNetwork, image }));
    else localStorage.removeItem('radium-home-banner');
  } catch (e) {}

  const label = networkInfo().label;
  const name = label.charAt(0) + label.slice(1).toLowerCase();
  const desc = $('bannerDesc');
  if (desc) {
    desc.textContent = image
      ? `Your own picture, behind the logo on ${name}.`
      : `${name}'s own art. Each network can have a picture of your own.`;
  }
  const preview = $('bannerPreview');
  if (preview) {
    preview.dataset.network = activeNetwork;
    preview.classList.toggle('is-custom', !!image);
    preview.style.backgroundImage = image ? `url("${image}")` : '';
    preview.setAttribute('aria-label', image ? `Your banner on ${name}` : `${name}'s own banner art`);
  }
  const reset = $('btnResetBanner');
  if (reset) reset.disabled = !image;
}

/// Store `value` as the network in use's banner (empty for its own art).
async function setHomeBanner(value) {
  const network = activeNetwork;
  const before = config.home?.banners?.[network] || '';
  config.home = { ...(config.home || {}), banners: { ...(config.home?.banners || {}), [network]: value } };
  applyHomeBanner();
  try {
    const stored = await window.radium?.setHomeBanner(network, value);
    if (value && stored !== value) throw new Error('That picture could not be used.');
    addLog(value ? `Home banner set for ${networkInfo().label} (${formatBytes(value.length)}).` : `Home banner for ${networkInfo().label} back to its own art.`, 'ok');
  } catch (e) {
    config.home = { ...(config.home || {}), banners: { ...(config.home?.banners || {}), [network]: before } };
    if (network === activeNetwork) applyHomeBanner();
    toast(`Could not save the banner: ${e.message || e}`, 'error', 5000);
    addLog(`Home banner not saved: ${e.message || e}`, 'error');
  }
}

$('btnChooseBanner')?.addEventListener('click', () => {
  const picker = $('bannerFilePicker');
  if (!picker) return;
  picker.value = '';
  picker.click();
});
$('bannerFilePicker')?.addEventListener('change', async (e) => {
  const file = e.target.files?.[0];
  if (!file) return;
  try {
    const encoded = await prepareBackgroundImage(file, { maxEdge: BANNER_MAX_EDGE, maxBytes: BANNER_MAX_STORED_BYTES });
    await setHomeBanner(encoded);
  } catch (err) {
    toast(err.message || 'Could not use that picture.', 'error', 5000);
  }
});
$('btnResetBanner')?.addEventListener('click', () => setHomeBanner(''));
// Up and Down move through the list, as in any vertical tab list.
document.querySelector('.settings-nav')?.addEventListener('keydown', (e) => {
  if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key)) return;
  const btns = [...document.querySelectorAll('.settings-nav-btn')].filter(settingsSectionAvailable);
  if (!btns.length) return;
  e.preventDefault();
  const i = Math.max(0, btns.findIndex(b => b.dataset.section === settingsSection));
  const j = e.key === 'Home' ? 0
    : e.key === 'End' ? btns.length - 1
    : (i + (e.key === 'ArrowDown' ? 1 : -1) + btns.length) % btns.length;
  showSettingsSection(btns[j].dataset.section, { focus: true });
});

/// Persist the theme and glass settings without touching anything else.
///
/// Its own saver rather than a call to autoSaveSettings() so a theme change
/// does not sweep up whatever else the settings form happens to be showing.
async function saveThemeSettings() {
  if (!config || Object.keys(config).length === 0) return; // config not loaded yet
  const updated = {
    ...config,
    theme: config.theme,
    // Kept in step with `theme`: nothing sits underneath a skin any more, but
    // the field still exists and the backend repairs a mismatch, so writing a
    // stale value here would just be undone on the next load.
    baselineTheme: config.theme,
    glass: config.glass
  };
  try {
    const ok = await window.radium?.saveConfig(updated);
    if (ok) config = updated;
  } catch (e) {
    console.warn('saveThemeSettings: failed to persist', e);
  }
}

/// Debounced, for the colour picker — it fires `input` continuously while
/// dragging, and each save is an IPC call plus a config.json write.
let _themeSaveTimer = null;
function debouncedSaveThemeSettings() {
  clearTimeout(_themeSaveTimer);
  _themeSaveTimer = setTimeout(saveThemeSettings, 800);
}

$('cfgFriendsView')?.addEventListener('change', () => {
  config.stella = { ...(config.stella || {}), friendsView: $('cfgFriendsView').value || 'home' };
  applyFriendsView();
  autoSaveSettings();
});

$('cfgTheme')?.addEventListener('change', () => {
  config.theme = $('cfgTheme').value || DEFAULT_THEME;
  applyTheme(config.theme);
  saveThemeSettings();
});

$('tgl-glassEnabled')?.addEventListener('click', () => {
  const glassOn = !getToggle('tgl-glassEnabled');
  setToggle('tgl-glassEnabled', glassOn);
  config.glass = { ...(config.glass || {}), enabled: glassOn };
  updateGlassControls(glassOn);
  applyTheme(config.theme);
  saveThemeSettings();
});

// Full glass effects: on by default; off gives the lighter look for weak GPUs. The row is inert while
// glass itself is off (see .ct-body.is-off), and checked here as well so a
// keyboard or scripted click cannot flip it then either.
$('tgl-glassFull')?.addEventListener('click', () => {
  if (!getToggle('tgl-glassEnabled')) return;
  const full = !getToggle('tgl-glassFull');
  setToggle('tgl-glassFull', full);
  config.glass = { ...(config.glass || {}), fullEffects: full };
  applyTheme(config.theme);
  saveThemeSettings();
});

/// The tint a fresh install starts with. Mirrors `GlassSettings::default` in
/// config.rs.
const DEFAULT_GLASS_TINT = '#0b0c14';

// ─── Glass tint picker ──────────────────────────────────────────────────────
// Drawn by the launcher rather than `<input type="color">`, whose popup is
// Chromium's own white dialog and matched none of the skins.
//
// It also used to be slow to drag: every `input` event rebuilt the whole glass
// stylesheet and wrote it to localStorage. Now a drag is read at most once per
// frame, and each frame only sets `--lg-tint` on <body> — which is all the
// sheet (skins/11-glass.css) ever needed from it. The boot cache is written
// once, after the picker closes, so it gets the final colour.

/// Dark tints that suit the glass field: the default, then a spread of hues.
const TINT_PRESETS = ['#0b0c14', '#1b1036', '#0a1d3f', '#06302e', '#0d2a16', '#3b0f22', '#3a2208', '#2a2b31'];

/// Hue is kept separately from the hex so dragging to grey or black and back
/// does not snap the hue slider to red.
const tintState = { h: 0, s: 0, v: 0, dirty: false };

const clamp01 = (n) => Math.min(1, Math.max(0, n));

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` as [r, g, b]; alpha is ignored.
/// The short forms are legal tints (safeColor accepts them), and reading their
/// first six characters as `rrggbb` put the picker on the wrong colour.
function hexToRgb(hex) {
  let digits = String(hex).replace(/^#/, '');
  if (digits.length <= 4) digits = [...digits.slice(0, 3)].map(c => c + c).join('');
  const n = parseInt(digits.slice(0, 6), 16) || 0;
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function rgbToHsv(r, g, b) {
  r /= 255; g /= 255; b /= 255;
  const max = Math.max(r, g, b);
  const d = max - Math.min(r, g, b);
  let h = 0;
  if (d) {
    if (max === r) h = ((g - b) / d + 6) % 6;
    else if (max === g) h = (b - r) / d + 2;
    else h = (r - g) / d + 4;
    h *= 60;
  }
  return { h, s: max ? d / max : 0, v: max };
}

function hsvToHex(h, s, v) {
  const channel = (n) => {
    const k = (n + h / 60) % 6;
    return Math.round((v - v * s * Math.max(0, Math.min(k, 4 - k, 1))) * 255);
  };
  return '#' + [channel(5), channel(3), channel(1)].map(c => c.toString(16).padStart(2, '0')).join('');
}

/// `#abc` or `abcdef`, with or without the hash, as `#aabbcc`; null otherwise.
function parseHexInput(raw) {
  const m = /^#?([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(String(raw).trim());
  if (!m) return null;
  const digits = m[1].length === 3 ? [...m[1]].map(c => c + c).join('') : m[1];
  return '#' + digits.toLowerCase();
}

/// Show a tint on the swatch button, without applying it.
function setGlassTintUI(hex) {
  $('theme-glassBg')?.style.setProperty('--swatch', hex);
}

function syncTintStateFromHex(hex) {
  const { h, s, v } = rgbToHsv(...hexToRgb(hex));
  // A grey carries no hue of its own; keep the one the slider is on.
  if (s > 0 && v > 0) tintState.h = h;
  tintState.s = s;
  tintState.v = v;
}

function paintTintPicker(hex, { syncHexField = true } = {}) {
  const picker = $('tintPicker');
  if (!picker) return;
  const { h, s, v } = tintState;
  picker.style.setProperty('--tint-current', hex);
  picker.style.setProperty('--tint-hue-color', `hsl(${h} 100% 50%)`);

  const svThumb = $('tintSvThumb');
  if (svThumb) {
    svThumb.style.left = `${s * 100}%`;
    svThumb.style.top = `${(1 - v) * 100}%`;
  }
  const hueThumb = $('tintHueThumb');
  if (hueThumb) hueThumb.style.left = `${(h / 360) * 100}%`;

  $('tintSv')?.setAttribute('aria-valuetext', hex.toUpperCase());
  $('tintHue')?.setAttribute('aria-valuenow', String(Math.round(h)));
  if (syncHexField && $('tintHex')) $('tintHex').value = hex.toUpperCase();

  for (const chip of $('tintPresets')?.children || []) {
    chip.classList.toggle('active', chip.dataset.value === hex);
  }
}

/// Live preview: cheap enough to run every frame of a drag.
function previewGlassTint(hex) {
  setGlassTintUI(hex);
  if (config.glass?.tint === hex) return;
  config.glass = { ...(config.glass || {}), tint: hex };
  document.body.style.setProperty('--lg-tint', hex);
  tintState.dirty = true;
  debouncedSaveThemeSettings();
}

function applyTintState(opts) {
  const hex = hsvToHex(tintState.h, tintState.s, tintState.v);
  paintTintPicker(hex, opts);
  previewGlassTint(hex);
}

/// Pointer drags on the square and the hue bar. Pointer capture keeps the drag
/// alive when a fast flick leaves the control; the latest position is applied
/// once per animation frame however many events arrive in between.
function bindTintDrag(el, onPoint) {
  if (!el) return;
  let rect = null;
  let last = null;
  let frame = 0;
  const flush = () => {
    frame = 0;
    if (rect && last) onPoint(last, rect);
  };
  const queue = (e) => {
    last = { x: e.clientX, y: e.clientY };
    if (!frame) frame = requestAnimationFrame(flush);
  };
  el.addEventListener('pointerdown', (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    rect = el.getBoundingClientRect();
    el.setPointerCapture(e.pointerId);
    el.focus({ preventScroll: true });
    queue(e);
  });
  el.addEventListener('pointermove', (e) => {
    if (el.hasPointerCapture(e.pointerId)) queue(e);
  });
}

bindTintDrag($('tintSv'), (p, r) => {
  tintState.s = clamp01((p.x - r.left) / r.width);
  tintState.v = 1 - clamp01((p.y - r.top) / r.height);
  applyTintState();
});
bindTintDrag($('tintHue'), (p, r) => {
  tintState.h = clamp01((p.x - r.left) / r.width) * 360;
  applyTintState();
});

$('tintSv')?.addEventListener('keydown', (e) => {
  const step = e.shiftKey ? 0.1 : 0.01;
  const moves = { ArrowLeft: ['s', -step], ArrowRight: ['s', step], ArrowDown: ['v', -step], ArrowUp: ['v', step] };
  const move = moves[e.key];
  if (!move) return;
  e.preventDefault();
  tintState[move[0]] = clamp01(tintState[move[0]] + move[1]);
  applyTintState();
});
$('tintHue')?.addEventListener('keydown', (e) => {
  const step = e.shiftKey ? 10 : 1;
  const delta = { ArrowLeft: -step, ArrowDown: -step, ArrowRight: step, ArrowUp: step }[e.key];
  if (delta === undefined) return;
  e.preventDefault();
  tintState.h = Math.min(360, Math.max(0, tintState.h + delta));
  applyTintState();
});

$('tintHex')?.addEventListener('input', (e) => {
  const hex = parseHexInput(e.target.value);
  if (!hex) return;
  syncTintStateFromHex(hex);
  // The field is left as typed until it is committed.
  paintTintPicker(hex, { syncHexField: false });
  previewGlassTint(hex);
});
$('tintHex')?.addEventListener('change', (e) => {
  e.target.value = safeColor(config.glass?.tint, DEFAULT_GLASS_TINT).toUpperCase();
});
$('tintHex')?.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    closeTintPicker(true);
  }
});

(function buildTintPresets() {
  const host = $('tintPresets');
  if (!host) return;
  for (const hex of TINT_PRESETS) {
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'tint-preset';
    chip.dataset.value = hex;
    chip.style.setProperty('--swatch', hex);
    chip.setAttribute('aria-label', hex.toUpperCase());
    chip.addEventListener('click', () => {
      syncTintStateFromHex(hex);
      paintTintPicker(hex);
      previewGlassTint(hex);
    });
    host.appendChild(chip);
  }
})();

function openTintPicker() {
  const btn = $('theme-glassBg');
  const picker = $('tintPicker');
  if (!btn || !picker || btn.disabled) return;

  const hex = safeColor(config.glass?.tint, DEFAULT_GLASS_TINT);
  syncTintStateFromHex(hex);
  paintTintPicker(hex);

  // Lifted over the next settings group, as an open custom select is.
  picker.closest('.settings-group')?.classList.add('tint-host');
  picker.classList.remove('opens-up');
  showDropdown(picker);

  // Open upward when there is not room below inside the settings scroller.
  const bounds = scrollParentOf(picker.parentElement).getBoundingClientRect();
  const t = btn.getBoundingClientRect();
  const below = bounds.bottom - t.bottom;
  const above = t.top - bounds.top;
  if (picker.offsetHeight + 8 > below && above > below) picker.classList.add('opens-up');

  btn.setAttribute('aria-expanded', 'true');
  btn.classList.add('is-open');
  $('tintSv')?.focus({ preventScroll: true });
}

function closeTintPicker(refocus = false) {
  const btn = $('theme-glassBg');
  const picker = $('tintPicker');
  if (!picker || picker.hidden || picker.classList.contains('is-closing')) return;

  hideDropdown(picker);
  btn?.setAttribute('aria-expanded', 'false');
  btn?.classList.remove('is-open');
  if (refocus) btn?.focus();

  // After the exit animation, so the one full pass — which writes the boot
  // cache and saves — does not land on top of it. Skipped if the picker was
  // reopened in the meantime.
  setTimeout(() => {
    if (!picker.hidden) return;
    picker.closest('.settings-group')?.classList.remove('tint-host');
    if (!tintState.dirty) return;
    tintState.dirty = false;
    applyTheme(config.theme);
    clearTimeout(_themeSaveTimer);
    saveThemeSettings();
  }, MENU_EXIT_MS);
}

$('theme-glassBg')?.addEventListener('click', () => {
  if ($('theme-glassBg').getAttribute('aria-expanded') === 'true') closeTintPicker();
  else openTintPicker();
});

// Dismissal on press rather than click: a drag that starts on the square and
// is released outside the picker must not count as a click outside it.
document.addEventListener('pointerdown', (e) => {
  const picker = $('tintPicker');
  if (!picker || picker.hidden) return;
  if (picker.contains(e.target) || $('theme-glassBg')?.contains(e.target)) return;
  closeTintPicker();
}, true);
document.addEventListener('keydown', (e) => {
  const picker = $('tintPicker');
  if (!picker || picker.hidden || e.key !== 'Escape') return;
  e.preventDefault();
  closeTintPicker(true);
});

$('btnResetGlassTint')?.addEventListener('click', () => {
  setGlassTintUI(DEFAULT_GLASS_TINT);
  config.glass = { ...(config.glass || {}), tint: DEFAULT_GLASS_TINT };
  applyTheme(config.theme);
  // Not debounced: a click is one change, and a pending picker save would
  // otherwise land after it and write the old colour back.
  clearTimeout(_themeSaveTimer);
  saveThemeSettings();
});

/// Set the glass backdrop, redraw, and persist.
///
/// Saved through its own command, never with the rest of the config (see
/// saveConfig in the shim). `persistDelay` lets the field wait for typing to
/// stop instead of writing config.json on every keystroke.
let _backdropSaveTimer = null;
/// Bumped by every backdrop change, so an address that finishes downloading
/// after something newer was chosen is dropped instead of saved over it.
let backdropFetchSeq = 0;
function setGlassBackdrop(value, persistDelay = 0) {
  config.glass = { ...(config.glass || {}), bgImage: value };
  // A save of a newer choice wins over an address still downloading.
  backdropFetchSeq++;

  // Live preview is the two CSS variables and nothing else: the sheet is a
  // file now, and the class list hasn't changed, so there is nothing for
  // applyTheme() to redo. The full pass — which writes the boot cache, and the
  // backdrop with it — waits alongside the save: a picked image is a data:
  // URI of up to 1.4 MB, so doing either of those per keystroke was the whole
  // cost when the field saved as you typed.
  const glass = config.glass;
  if (glass.enabled === true) {
    applyGlassVars(glass.tint, glass.bgImage, glass.fullEffects !== false);
  }

  clearTimeout(_backdropSaveTimer);
  _backdropSaveTimer = setTimeout(async () => {
    applyTheme(config.theme);
    try {
      await window.radium?.setGlassBackdrop(value);
    } catch (e) {
      addLog(`Could not save the glass backdrop: ${e}`, 'error');
    }
  }, persistDelay);
}

// ─── Glass backdrop ─────────────────────────────────────────────────────────
// The surface the frosted panels sit over. Optional: with none set, glass
// paints gradients from the tint instead.

/// Largest file the backdrop picker will accept, before downscaling.
const BG_IMAGE_MAX_INPUT_BYTES = 20 * 1024 * 1024;

/// Longest edge the stored backdrop is resized to.
///
/// It is stretched over the window with `background-size: cover`, so anything
/// past a large display's width is detail nobody can see. 2560 covers a
/// maximised launcher on a 4K screen at 150% scaling.
const BG_IMAGE_MAX_EDGE = 2560;

/// Ceiling on the encoded data URI that ends up in config.json.
///
/// This string is stored in the config, interpolated into the generated glass
/// stylesheet, cached in localStorage for the pre-paint replay, and sent back
/// over IPC on every save — and `ensure_config` reads the config on nearly
/// every backend command. A picked 5 MB wallpaper used to become ~6.7 MB of
/// base64 doing all of that, which blew the localStorage quota and silently
/// disabled the boot cache. Re-encoding to fit keeps the whole chain cheap.
const BG_IMAGE_MAX_STORED_BYTES = 1_400_000;

/// Decode a picked file into something a canvas can draw.
///
/// Never through `URL.createObjectURL`: the CSP's `img-src` has no `blob:`, so
/// an `<img>` pointed at a blob URL is refused and fires `error` — which is
/// why every picked file, JPEG included, used to be reported as "not an image".
/// `createImageBitmap` decodes the bytes directly and is not subject to
/// `img-src`; a `data:` URL (which the CSP does allow) covers anything it
/// declines, such as SVG.
async function decodeImageFile(file) {
  if (typeof createImageBitmap === 'function') {
    try {
      return await createImageBitmap(file);
    } catch (e) {
      // Fall through to the data: URL path.
    }
  }
  const dataUrl = await new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
  const img = new Image();
  img.src = dataUrl;
  await img.decode();
  return img;
}

/// Downscale and re-encode a picked image to something worth storing.
///
/// Resolves to a JPEG data URI inside [`BG_IMAGE_MAX_STORED_BYTES`], stepping
/// the quality down until it fits. Rejects rather than storing something that
/// cannot be made small enough.
async function prepareBackgroundImage(file, { maxEdge = BG_IMAGE_MAX_EDGE, maxBytes = BG_IMAGE_MAX_STORED_BYTES } = {}) {
  if (file.size > BG_IMAGE_MAX_INPUT_BYTES) {
    throw new Error(`That image is ${formatBytes(file.size)}. Pick one under ${formatBytes(BG_IMAGE_MAX_INPUT_BYTES)}.`);
  }

  let source;
  try {
    source = await decodeImageFile(file);
  } catch (e) {
    throw new Error('That file could not be read as an image.');
  }

  try {
    const width = source.width;
    const height = source.height;
    if (!width || !height) throw new Error('That file could not be read as an image.');

    const scale = Math.min(1, maxEdge / Math.max(width, height));
    const canvas = document.createElement('canvas');
    canvas.width = Math.max(1, Math.round(width * scale));
    canvas.height = Math.max(1, Math.round(height * scale));
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('Could not process that image.');
    ctx.drawImage(source, 0, 0, canvas.width, canvas.height);

    // JPEG throughout: a backdrop sits behind a dark scrim and has no
    // transparency to preserve, and PNG at this size is several times larger.
    for (const quality of [0.82, 0.7, 0.6, 0.5, 0.4]) {
      const encoded = canvas.toDataURL('image/jpeg', quality);
      if (encoded.length <= maxBytes) return encoded;
    }
    throw new Error('That image is too detailed to store. Try a smaller one.');
  } finally {
    source.close?.();
  }
}

/// Download the picture at `url` and store it the way a picked file is stored.
///
/// The address itself can't be the backdrop: the CSP lets the page load no
/// remote image, so the backend fetches it (`cmd_fetch_glass_backdrop`, HTTPS
/// and public hosts only) and it is downscaled into a data: URI here. Returns
/// whether it was applied. `quiet` skips the toasts, for the one-time upgrade
/// of an address saved before this existed.
async function setGlassBackdropFromUrl(url, { quiet = false } = {}) {
  const seq = ++backdropFetchSeq;
  if (!quiet) toast('Downloading the picture...', 'info', 2500);
  try {
    const bytes = await window.radium.fetchGlassBackdrop(url);
    const data = bytes instanceof ArrayBuffer ? bytes : new Uint8Array(bytes);
    const encoded = await prepareBackgroundImage(new Blob([data]));
    if (seq !== backdropFetchSeq) return false;
    setBgImageUI(encoded);
    setGlassBackdrop(encoded);
    addLog(`Glass backdrop downloaded from ${url} (${formatBytes(encoded.length)} stored).`, 'ok');
    return true;
  } catch (err) {
    if (seq !== backdropFetchSeq) return false;
    const msg = err?.message || String(err);
    addLog(`Glass backdrop from ${url} not used: ${msg}`, 'warn');
    if (!quiet) toast(msg, 'error', 5000);
    return false;
  }
}

$('theme-bgImage')?.addEventListener('input', (e) => {
  const field = e.target;
  if (field.value !== SAVED_BACKDROP_LABEL) delete field.dataset.savedPicture;
  // Emptied: back to the tint straight away. An address waits until it is
  // committed (Enter, or leaving the field), so a half-typed one isn't
  // downloaded — see `change`.
  if (!field.value.trim()) setGlassBackdrop('', 600);
});

$('theme-bgImage')?.addEventListener('change', (e) => {
  const field = e.target;
  const value = field.value.trim();
  if (!value || field.dataset.savedPicture) return;
  if (paintableBackdrop(value)) {
    // A data: URI pasted in whole. Rare, but it is exactly what gets stored —
    // so it is held to the same ceiling a picked file is shrunk to fit, or a
    // pasted wallpaper would ride along in config.json at full size.
    if (value.length > BG_IMAGE_MAX_STORED_BYTES) {
      toast('That picture is too large to paste. Use Browse to pick the file instead.', 'error', 5000);
      return;
    }
    setBgImageUI(value);
    setGlassBackdrop(value);
    return;
  }
  if (!value.startsWith('https://') || !safeBackdrop(value)) {
    toast('Enter the address of a picture, starting with https://', 'error', 4000);
    return;
  }
  setGlassBackdropFromUrl(value);
});

$('btnBrowseBgFile')?.addEventListener('click', () => {
  const picker = $('theme-bgFilePicker');
  if (picker) picker.value = '';
  picker?.click();
});

$('theme-bgFilePicker')?.addEventListener('change', async (e) => {
  const file = e.target.files?.[0];
  if (!file) return;

  try {
    const encoded = await prepareBackgroundImage(file);
    setBgImageUI(encoded);
    setGlassBackdrop(encoded);
    addLog(`Glass backdrop set (${formatBytes(encoded.length)} stored).`, 'ok');
  } catch (err) {
    toast(err.message || 'Could not use that image.', 'error', 5000);
    addLog(`Glass backdrop rejected: ${err.message}`, 'warn');
  }
});

$('btnClearBgImage')?.addEventListener('click', () => {
  setBgImageUI('');
  if ($('theme-bgFilePicker')) $('theme-bgFilePicker').value = '';
  setGlassBackdrop('');
});

// ─── Auto-save Settings ─────────────────────────────────────────────────────
// Replaces the old manual Save button. Collects the full settings state and
// persists it to config.json. A small indicator in the header gives feedback.

function showAutosaveIndicator(state, text) {
  const el = $('autosaveIndicator');
  if (!el) return;
  el.style.display = '';
  el.className = `autosave-indicator visible ${state}`;
  el.textContent = text;
}

async function autoSaveSettings() {
  if (!config || Object.keys(config).length === 0) return;

  const updated = {
    ...config,
    apiUrl:           config.apiUrl || 'https://api.radie.app/',
    minimizeOnLaunch: getToggle('tgl-minimizeOnLaunch'),
    closeOnLaunch:    getToggle('tgl-closeOnLaunch'),
    autoUpdate:       getToggle('tgl-autoUpdate'),
    disableWarnings:  getToggle('tgl-disableWarnings'),
    runInBackground:  getToggle('tgl-runInBackground'),
    notifPopups:      getToggle('tgl-notifPopups'),
    notifSound:       getToggle('tgl-notifSound'),
    // No `installDir` here on purpose. The install-dir span shows whichever
    // network is active, so copying it into the flat (Radium) field on every
    // autosave silently repointed Radium at the Vanilla folder. The Change /
    // Reset Folder buttons own that setting and save it themselves, into the
    // active network's slot; the `...config` spread carries both slots through
    // untouched.
    playMode,
    // Theme and glass are owned by saveThemeSettings(), which the skin
    // dropdown and the glass controls call directly. The `...config` spread
    // carries the current values through untouched — writing them from this
    // form would let a stale read revert a change made a moment ago.
    network:          activeNetwork
    // No `vanilla` key here either, for the same reason as `installDir`: every
    // field in that sub-object is owned by the Change / Reset Folder buttons or
    // by the backend, so the `...config` spread carries it through untouched.
  };

  showAutosaveIndicator('saving', 'Saving...');

  let ok = false;
  try {
    ok = await window.radium?.saveConfig(updated);
  } catch (e) {
    console.error('autoSaveSettings error:', e);
  }

  if (ok) {
    config = updated;
    showAutosaveIndicator('saved', '✓ Saved');
    addLog('Configuration auto-saved.', 'ok');
    await checkInstall();
  } else {
    showAutosaveIndicator('error', '✕ Save failed');
    addLog('Auto-save failed.', 'error');
  }

  // Fade the indicator out after 2 seconds
  setTimeout(() => {
    const el = $('autosaveIndicator');
    if (el) el.classList.remove('visible');
  }, 2000);
}

// Check client installation state
/// One log line per session about a set-aside folder, not one per autosave.
let orphanedDirReported = false;

/// checkInstall() re-runs on every settings autosave, and its verdict almost
/// never changes between two of them, so each toggle clicked logged the same
/// "Game client found" line again. The log is what a bug report carries —
/// 2,000 lines of it — and a session of settings changes pushed everything
/// else out. Logged when the verdict changes instead: `kind` names which
/// verdict ('client', or 'outdated' beside it), so the two lines an outdated
/// client gets don't take turns being "new".
const lastInstallLog = new Map();
function logInstallState(kind, msg, level) {
  if (lastInstallLog.get(kind) === msg) return;
  lastInstallLog.set(kind, msg);
  addLog(msg, level, 'install');
}

/// The outdated-client dialog, once per client rather than once per
/// checkInstall(): it used to come back on every settings autosave for as long
/// as the client stayed outdated. Keyed by network and build, so switching
/// network, or a new install that is still outdated, asks again.
let outdatedPromptKey = '';

async function checkInstall() {
  const network = activeNetwork;
  let result = null;
  try {
    result = await window.radium?.checkInstall();
  } catch (e) {
    console.error('checkInstall error:', e);
  }
  // An answer about a network switched away from while it was on its way.
  // A settings autosave and a network switch can both ask at once, and the
  // old network's answer used to land last and paint its install state over
  // the new one's. The switch runs a check of its own for the new network.
  if (network !== activeNetwork) return;
  // The Defender exclusion follows the folder, which a settings change or
  // another window may have moved since the page last looked.
  if (result) syncAvState(network, result.avExcluded, result.defenderDirs);
  isInstalled = result?.installed ?? false;
  needsRepair = !isInstalled && !!result?.needsRepair;
  if (result?.patchDir) stellaPatchDir = result.patchDir;
  const qscC = $('qsc-client');
  // Only the active network's row: `result` describes the network checkInstall
  // was called for. The other row is filled by refreshInstallDirRows().
  const activeDirSpan = installDirSpan();

  if (activeDirSpan && result?.clientDir) {
    activeDirSpan.textContent = result.clientDir;
  }

  // The backend renames a client folder aside when it finds one network holding
  // another network's client. Say so once a session — checkInstall() re-runs on
  // every settings autosave, and the folder sticks around until the user deletes it.
  if (result?.orphanedClientDir && !orphanedDirReported) {
    orphanedDirReported = true;
    addLog(`Found a client in the wrong network's folder. Moved it aside to ${result.orphanedClientDir} — you can delete that folder.`, 'warn', 'install');
  }

  if (isInstalled) {
    // Show launch panel, hide download section
    const ds = $('downloadSection'); if (ds) ds.style.display = 'none';
    document.body.classList.add('client-installed');
    const qi = $('qsInstalled'); if (qi) qi.textContent = 'Installed';
    // Reapply the last known update result (if any) instead of flashing back
    // to "Check for Updates" and re-fetching — checkInstall() re-runs on
    // every settings autosave, so re-checking every time would spam requests
    // and flicker the button/card each time it resolves.
    if (qscC) {
      qscC.classList.add('installed');
      qscC.classList.remove('not-installed', 'update-available');
      if (clientUpdateInfo?.hasUpdate) {
        qscC.classList.add('update-available');
        const qi2 = $('qsInstalled'); if (qi2) qi2.textContent = `Update to v${clientUpdateInfo.latestVersion}`;
      }
    }
    setClientUpdateButton(clientUpdateInfo?.hasUpdate ? 'update' : 'check', clientUpdateInfo);
    const verLabel = result?.clientVersion ? `v${result.clientVersion}` : 'unknown version';
    const buildLabel = result?.clientBuild || 'unrecorded build';
    logInstallState('client', `Game client found (${verLabel}, build ${buildLabel}): ${result.exePath || 'client dir'}`, 'ok');

    // Check if the game is already running on startup. Only log the transition:
    // checkInstall() re-runs on every settings autosave, so logging every time
    // filled the log with one line per 800ms while the game was open.
    if (result?.isRunning) {
      const wasRunning = isGameRunning;
      setGameRunning(true);
      if (!wasRunning) addLog('Game is already running.', 'ok', 'game');
    }

    // An outdated client (left over from a previous launcher version) must be
    // re-downloaded to match the new Radium build.
    if (!result?.clientOutdated) lastInstallLog.delete('outdated');
    if (result?.clientOutdated && !result?.isRunning) {
      logInstallState('outdated', `Installed client is outdated — build '${result?.clientBuild || 'unrecorded'}' ≠ required '${result?.requiredBuild || 'unknown'}'. Update required.`, 'warn');
      const promptKey = `${activeNetwork}|${result?.clientBuild || ''}|${result?.exePath || ''}`;
      if (promptKey !== outdatedPromptKey) {
        outdatedPromptKey = promptKey;
        showModal($('clientUpdateModal'));
      }
    } else if (networkInfo().hasPatch) {
      // Stella's and Rec EOS's clients have no version feed; what updates is
      // the patch (Stella's DLL, the Rec EOS mod), which has its own
      // Steam-style UPDATE button. Throttled inside.
      refreshStellaPatch();
    } else if (!result?.isRunning && !clientUpdateAutoChecked) {
      // Live version check against recroom.baby (Steam-style update prompt).
      // Only auto-run this once per session — the manual button handles re-checks.
      clientUpdateAutoChecked = true;
      checkForClientUpdate();
    }
  } else {
    // Show download section, hide launch panel
    const ds = $('downloadSection'); if (ds) ds.style.display = 'flex';
    document.body.classList.remove('client-installed');
  // The gear that opens it lives in the installed-only bar and has just
  // disappeared; an open menu would be left hanging over the download CTA.
  closeManageMenu();
    const qi = $('qsInstalled'); if (qi) qi.textContent = 'Not installed';
    if (qscC) {
      qscC.classList.add('not-installed');
      qscC.classList.remove('installed', 'update-available');
    }
    setClientUpdateButton('hidden');
    // No client installed — clear any cached update state so a fresh
    // install triggers a real re-check instead of reapplying stale info.
    clientUpdateInfo = null;
    clientUpdateAutoChecked = false;

    // A network that has not shipped a client yet is a different state from
    // "you haven't installed it": there is nothing to install. Say so rather
    // than offering a Download that can only fail.
    if (needsRepair) {
      if (qi) qi.textContent = 'NEEDS REPAIR';
      logInstallState('client', `${networkInfo().label} is installed but missing files, including the game itself. Press REPAIR to check every file and get the missing ones back.`, 'warn');
    } else if (!clientDownloadAvailable()) {
      if (qi) qi.textContent = 'Not released yet';
      logInstallState('client', `${networkInfo().label} has not published a client yet.`, 'info');
    } else if (result?.incomplete) {
      // The launcher closed (or the PC went off) partway through unpacking.
      logInstallState('client', 'The last install stopped before it finished — download the client again.', 'warn');
    } else {
      logInstallState('client', `${networkInfo().label} client not found — download required.`, 'info');
    }
  }

  if (!isInstalled) stellaPatch = null;
  applyStellaPatchUI();
  updateDownloadCta();
}

/// Whether the active network has something the launcher can actually install.
///
/// Vanilla has published no client, so this is false and its hero button opens
/// their download page instead. The install pipeline behind it is complete and
/// keyed on `config.vanilla.clientUrl`; there is deliberately no UI for that
/// field while there is nothing to point it at, so it is set in config.json (or
/// a Settings row is added back) once Vanilla ships a build.
function clientDownloadAvailable() {
  const info = networkInfo();
  if (!info.needsConfiguredDownloadUrl) return true;
  return !!(config?.vanilla?.clientUrl || '').trim();
}

/// Point the hero's DOWNLOAD button at the right thing, and explain it when
/// that thing is a website rather than an install.
function updateDownloadCta() {
  const btn = $('btnDownload');
  const note = $('heroDownloadNote');
  if (needsRepair) {
    if (btn) btn.textContent = '\u2b07 REPAIR';
    if (note) {
      note.style.display = 'block';
      note.textContent = 'Some game files are missing. Repair checks every file and gets back the ones that are gone.';
    }
    return;
  }
  const available = clientDownloadAvailable();
  const info = networkInfo();

  if (btn) {
    btn.textContent = available ? '\u2b07 DOWNLOAD' : `\u2b07 GET ${info.label}`;
  }
  if (note) {
    note.style.display = available ? 'none' : 'block';
    note.textContent = available
      ? ''
      : `${info.label} hasn't released a client yet. This opens their download page.`;
  }
}

// Advance the Download → Extract → Done phase stepper. Steps before the
// active one are marked 'done'; the active one 'active'; later ones idle.
function setDlStep(active) {
  const order = ['download', 'extract', 'done'];
  const activeIdx = order.indexOf(active);
  document.querySelectorAll('#dlSteps .dlp-step').forEach((el) => {
    const idx = order.indexOf(el.dataset.step);
    el.classList.remove('active', 'done');
    if (idx < activeIdx) el.classList.add('done');
    else if (idx === activeIdx) el.classList.add('active');
  });
}

/// Relabel the three steps of the progress panel. Verify Files borrows it,
/// so its steps read Verify / Repair / Done; a download puts them back.
function setDlStepLabels(first = 'Download', second = 'Extract', third = 'Done') {
  const labels = { download: first, extract: second, done: third };
  document.querySelectorAll('#dlSteps .dlp-step').forEach((el) => {
    const idx = el.querySelector('.dlp-step-idx');
    el.replaceChildren(...(idx ? [idx] : []), document.createTextNode(labels[el.dataset.step] || ''));
  });
}

// In-app client download downloader state
// Sync the Download / Pause button labels to the current state.
function updateDlButtons() {
  const dlBtn = $('btnDownload');
  const pauseBtn = $('btnPauseDl');
  if (dlBtn) {
    dlBtn.disabled = isDownloading || isPaused || isCancelling;
    dlBtn.textContent = isCancelling  ? '⬇ CANCELLING...'
                      : clientTask === 'verify' ? '✓ VERIFYING...'
                      : clientTask === 'repair' ? '⬇ REPAIRING...'
                      : isDownloading ? '⬇ DOWNLOADING...'
                      : isPaused      ? '⬇ PAUSED'
                      : needsRepair   ? '⬇ REPAIR'
                      :                 '⬇ DOWNLOAD';
  }
  if (pauseBtn) pauseBtn.textContent = isPaused ? '▶ Resume' : '⏸ Pause';
  // For skins that draw a paused bar differently (Vista/7's yellow one).
  $('dlProgressBlock')?.classList.toggle('is-paused', isPaused);
}

// `opts.resuming` keeps the current bar position instead of snapping back to 0%,
// so continuing a paused/interrupted download doesn't visibly flash to zero.
function setDownloadUI(downloading, opts = {}) {
  isDownloading = downloading;
  if (downloading) { isPaused = false; isCancelling = false; extractEta = null; dlResumable = false; }
  const block = $('dlProgressBlock');
  const pauseBtn = $('btnPauseDl');
  if (downloading) {
    if (block) block.style.display = 'block';
    // Shown once the server is known to let it continue; see updateDlProgress().
    if (pauseBtn) pauseBtn.style.display = 'none';
    setStatLabels('Speed', 'Transferred', 'ETA');
    if (!clientTask) setDlStepLabels();
    setDlStep('download');
    if (!opts.resuming) {
      const fill = $('dlBarFill'); if (fill) { fill.classList.remove('indeterminate'); applyBarFill(fill, 0); }
      const pctEl = $('dlPctLabel'); if (pctEl) pctEl.textContent = '0%';
    }
  } else if (!isPaused) {
    // Fully idle — hide the panel. (A paused download keeps its panel visible;
    // see setPausedUI.)
    if (block) block.style.display = 'none';
  }
  updateDlButtons();
}

// Show the panel frozen in a paused/resumable state. `info` may carry
// { downloaded, total } (e.g. a resume offered on launcher startup) so the bar
// reflects real progress before the download is running again.
function setPausedUI(info = {}) {
  isDownloading = false;
  isPaused = true;
  const block = $('dlProgressBlock');
  if (block) block.style.display = 'block';
  const pauseBtn = $('btnPauseDl'); if (pauseBtn) pauseBtn.style.display = '';
  setStatLabels('Speed', 'Transferred', 'ETA');
  const phaseEl = $('dlPhaseLabel'); if (phaseEl) phaseEl.textContent = 'Paused';
  const speedEl = $('dlSpeedLabel'); if (speedEl) speedEl.textContent = '—';
  const etaEl   = $('dlEtaLabel');   if (etaEl)   etaEl.textContent   = '—';
  if (typeof info.total === 'number' && info.total > 0 && typeof info.downloaded === 'number') {
    const pct = Math.min(99, Math.floor((info.downloaded / info.total) * 100));
    const fill = $('dlBarFill'); if (fill) { fill.classList.remove('indeterminate'); applyBarFill(fill, pct); }
    const pctEl = $('dlPctLabel'); if (pctEl) pctEl.textContent = `${pct}%`;
    const sizeEl = $('dlSizeLabel'); if (sizeEl) sizeEl.textContent = `${formatBytes(info.downloaded)} / ${formatBytes(info.total)}`;
  }
  setDlStep('download');
  updateDlButtons();
}

// The retro-family themes draw the progress bar as discrete blocks, via a
// repeating gradient with a 10px period (8px block + 2px gap). A plain
// percentage width slices the final block mid-cube, so snap the fill to whole
// blocks. Themes that paint a solid bar (the modern family, Vista/7) are
// detected from the computed background and keep the exact percentage.
function applyBarFill(fill, pct) {
  // Must match the repeating-gradient period in .dlp-bar-fill (8px block + 2px gap).
  const SEGMENT_PX = 10;
  if (!fill) return;
  fill.dataset.pct = String(pct);
  const wrap = fill.parentElement;
  const segmented = getComputedStyle(fill).backgroundImage.includes('repeating-linear-gradient');
  if (!segmented || !wrap) { fill.style.width = `${pct}%`; return; }

  const ws = getComputedStyle(wrap);
  const track = wrap.clientWidth - parseFloat(ws.paddingLeft || 0) - parseFloat(ws.paddingRight || 0);
  if (!(track > 0)) { fill.style.width = `${pct}%`; return; }

  // 100% must fill the track exactly, even when it isn't a whole number of
  // blocks, or the bar would stop just short of the end.
  if (pct >= 100) { fill.style.width = '100%'; return; }
  let blocks = Math.floor((track * pct / 100) / SEGMENT_PX);
  if (pct > 0 && blocks < 1) blocks = 1;   // any progress shows at least one block
  fill.style.width = `${blocks * SEGMENT_PX}px`;
}

/// The bar for a download of unknown size (Stella's server sends none). On
/// the block-drawn skins it is a marquee: a run of whole blocks stepping
/// across the track one block at a time, as Windows draws one (see
/// `.dlp-bar-fill.indeterminate.segmented` in style.css). The steps have to
/// land on the block grid, so the track's width in whole blocks is handed to
/// the CSS here — and again on resize, since it is in pixels.
function applyIndeterminateBar(fill) {
  const SEGMENT_PX = 10;   // as in applyBarFill
  const wrap = fill?.parentElement;
  if (!wrap) return;
  const segmented = getComputedStyle(fill).backgroundImage.includes('repeating-linear-gradient');
  fill.classList.toggle('segmented', segmented);
  if (!segmented) return;
  const ws = getComputedStyle(wrap);
  const track = wrap.clientWidth - parseFloat(ws.paddingLeft || 0) - parseFloat(ws.paddingRight || 0);
  if (!(track > 0)) return;
  const blocks = Math.ceil(track / SEGMENT_PX);
  // Set only on a change: a new duration would restart the marquee, and
  // this runs on every progress event.
  const trackPx = `${blocks * SEGMENT_PX}px`;
  if (fill.style.getPropertyValue('--bar-track') !== trackPx) {
    fill.style.setProperty('--bar-track', trackPx);
    fill.style.setProperty('--bar-steps', String(blocks + 8));   // 8: the marquee's own length
  }
}

// A snapped width is in pixels, so it goes stale when the window resizes or a
// theme swaps the bar between segmented and solid. Re-apply on both.
if (typeof ResizeObserver !== 'undefined') {
  const ro = new ResizeObserver(() => {
    const fill = $('dlBarFill');
    if (fill && fill.classList.contains('indeterminate')) {
      applyIndeterminateBar(fill);
    } else if (fill && fill.dataset.pct != null) {
      applyBarFill(fill, Number(fill.dataset.pct));
    }
  });
  const startBarObserver = () => {
    const wrap = $('dlBarFill')?.parentElement;
    if (wrap) ro.observe(wrap);
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', startBarObserver);
  } else {
    startBarObserver();
  }
}

// Extraction ETA state. The backend reports only done/total entries for this
// phase, so the estimate is derived from the rate entries are being written at.
let extractEta = null;   // { t0, done0 }

// Relabel the three stat cards (they show download stats vs. extraction stats
// depending on the phase).
function setStatLabels(a, b, c) {
  const l1 = $('dlSpeedStatLabel'), l2 = $('dlSizeStatLabel'), l3 = $('dlEtaStatLabel');
  if (l1) l1.textContent = a;
  if (l2) l2.textContent = b;
  if (l3) l3.textContent = c;
}

// Estimate remaining extraction time from the entries-per-second rate observed
// since extraction started. Needs a moment of data before it can say anything.
function extractEtaText(done, totalEntries) {
  if (!(totalEntries > 0)) return 'Working…';
  const now = Date.now();
  // First sample, or the counter went backwards (a new extraction began).
  if (!extractEta || done < extractEta.done0) {
    extractEta = { t0: now, done0: done };
    return 'Estimating…';
  }
  const remaining = totalEntries - done;
  if (remaining <= 0) return '0s';
  const elapsed   = (now - extractEta.t0) / 1000;
  const processed = done - extractEta.done0;
  if (elapsed < 1 || processed <= 0) return 'Estimating…';
  return formatEta(remaining / (processed / elapsed));
}

function updateDlProgress({ phase, pct = 0, downloaded = 0, total = 0, speed = 0, eta = -1, status, entry = '', done = 0, totalEntries = 0, resumable, restarted, pauseRefused }) {
  // Whether this download can be paused and picked up later: the server has
  // to answer byte ranges, and Stella's doesn't. Told once, at the start.
  if (phase === 'download' && typeof resumable === 'boolean') {
    dlResumable = resumable;
    // A pause the backend let go of: put the button back as it was.
    if (pauseRefused) {
      const b = $('btnPauseDl'); if (b) b.textContent = '⏸ Pause';
      toast("This download can't be paused. It's still going.", 'info', 4000);
    }
    if (restarted) {
      addLog(`${networkInfo().label}'s server can't continue a download, so it started again from the beginning.`, 'warn', 'install');
      toast("This server can't resume downloads, so it started over.", 'warn', 5000);
    } else if (!resumable) {
      addLog(`${networkInfo().label}'s server can't continue a paused download, so this one can't be paused.`, 'info', 'install');
    }
  }
  // Paused: freeze the panel at the current progress with a Resume button.
  if (phase === 'paused') {
    setPausedUI({ downloaded, total });
    return;
  }
  // Pure "initializing" ping emitted before the download URL is resolved. It
  // carries no real numbers, so don't let it snap a resumed bar back to 0%.
  if (phase === 'download' && downloaded === 0 && total === 0) {
    setDlStep('download');
    const ph = $('dlPhaseLabel'); if (ph) ph.textContent = 'Downloading...';
    return;
  }

  const fill  = $('dlBarFill');
  const pctEl = $('dlPctLabel');
  const phase_el = $('dlPhaseLabel');
  const speedEl  = $('dlSpeedLabel');
  const sizeEl   = $('dlSizeLabel');
  const etaEl    = $('dlEtaLabel');

  if (fill) {
    if (pct >= 0) {
      fill.classList.remove('indeterminate');
      applyBarFill(fill, pct);
    } else {
      // Unknown total (server sent no Content-Length): show an animated
      // indeterminate bar instead of a full one, which would wrongly read as
      // "download complete".
      fill.classList.add('indeterminate');
      fill.style.width = '';
      applyIndeterminateBar(fill);
    }
  }
  if (pctEl)   pctEl.textContent = pct >= 0 ? `${pct}%` : '—';

  if (phase === 'extract') {
    setDlStep('extract');
    if (phase_el) phase_el.textContent = 'Extracting...';
    // Speed/ETA are meaningless while unzipping — repurpose the three cards to
    // show extraction progress instead of leaving them as empty dashes.
    setStatLabels('Files', 'Current File', 'ETA');
    if (speedEl) speedEl.textContent = totalEntries > 0 ? `${done} / ${totalEntries}` : '—';
    if (sizeEl)  sizeEl.textContent  = entry
      || (status ? status.replace(/^Extracting:\s*/, '') : 'Preparing…');
    if (etaEl)   etaEl.textContent   = extractEtaText(done, totalEntries);
    // Pause applies only during the download phase.
    const pauseBtn = $('btnPauseDl'); if (pauseBtn) pauseBtn.style.display = 'none';
    return;
  }

  if (phase === 'verify' || phase === 'repair') {
    // The first two steps are relabelled for these (see setDlStepLabels):
    // Verify, then Repair — or Download, then Repair, for a repair that
    // downloads the whole client.
    setDlStep(phase === 'verify' ? 'download' : 'extract');
    if (phase_el) phase_el.textContent = phase === 'verify' ? 'Verifying files...' : 'Repairing files...';
    setStatLabels('Files', 'Current File', 'ETA');
    if (speedEl) speedEl.textContent = totalEntries > 0 ? `${done} / ${totalEntries}` : '—';
    if (sizeEl)  sizeEl.textContent  = entry || '—';
    if (etaEl)   etaEl.textContent   = eta >= 0 ? formatEta(eta) : 'Estimating…';
    const pauseBtn = $('btnPauseDl'); if (pauseBtn) pauseBtn.style.display = 'none';
    return;
  }

  if (phase === 'done') {
    setDlStep('done');
    if (phase_el) phase_el.textContent = 'Complete!';
    setStatLabels('Files', 'Current File', 'ETA');
    if (speedEl) speedEl.textContent = totalEntries > 0 ? `${totalEntries} / ${totalEntries}` : '—';
    if (sizeEl)  sizeEl.textContent  = 'Done';
    if (etaEl)   etaEl.textContent   = '0s';
    extractEta = null;
    const pauseBtn = $('btnPauseDl'); if (pauseBtn) pauseBtn.style.display = 'none';
    return;
  }

  setDlStep('download');
  extractEta = null;
  if (phase_el) phase_el.textContent = 'Downloading...';
  setStatLabels('Speed', 'Transferred', 'ETA');
  // A repair's download can't be paused: what it downloads is never kept.
  // Nor can one from a server that can't continue it.
  const pauseBtn = $('btnPauseDl'); if (pauseBtn) pauseBtn.style.display = clientTask || !dlResumable ? 'none' : '';
  if (speedEl)  speedEl.textContent  = speed > 0 ? `${formatBytes(speed)}/s` : '—';
  if (sizeEl)   sizeEl.textContent   = total > 0
    ? `${formatBytes(downloaded)} / ${formatBytes(total)}`
    : formatBytes(downloaded);
  if (etaEl)    etaEl.textContent    = eta >= 0 ? formatEta(eta) : '—';
}

async function runClientDownload({ resuming = false } = {}) {
  if (isDownloading) return;

  // Reveal the download/progress UI. When updating an already-installed client
  // the launch panel is showing and the download section (which holds the
  // progress block) is hidden, so the status would otherwise be invisible.
  const ds = $('downloadSection'); if (ds) ds.style.display = 'flex';
  document.body.classList.remove('client-installed');
  // The gear that opens it lives in the installed-only bar and has just
  // disappeared; an open menu would be left hanging over the download CTA.
  closeManageMenu();

  setDownloadUI(true, { resuming });
  if (resuming) {
    addLog('Resuming download...', 'info', 'install');
    toast('Resuming download...', 'info', 2500);
  } else {
    addLog({
      radium: 'Starting download from recroom.baby (downloads page)...',
      vanilla: 'Starting download from the configured Vanilla client URL...',
      // Sent without a length, so the bar can't show a percentage.
      stella: "Starting download from Stella (about 4.7 GB; Stella's server doesn't send the size)...",
      receos: 'Starting download of the Rec EOS client from MediaFire (about 3.1 GB)...',
    }[activeNetwork] || 'Starting download...', 'info', 'install');
    toast('Download started!', 'info', 2500);
  }

  const dlStart = Date.now();
  let result = null;
  try {
    result = await window.radium?.downloadClient();
  } catch (e) {
    console.error('downloadClient error:', e);
    result = { success: false, error: e.toString() };
  }

  // The backend call has unwound, so its download guard is released and a new
  // download may start again.
  isCancelling = false;

  const elapsed = ((Date.now() - dlStart) / 1000).toFixed(1);
  if (result?.success) {
    setDownloadUI(false);
    addLog(`Download & extraction complete in ${elapsed}s.`, 'ok', 'install');
    addLog(`Exe: ${result.exePath || 'Found in client dir'}`, 'ok', 'install');
    toast(`${networkInfo().label} client installed!`, 'ok', 4000);
    // Stella: the install also fetched the patch, installing it if it was a
    // checked build. Either way the next checkInstall() asks afresh, so a
    // patch still to accept shows up as UPDATE straight away.
    if (activeNetwork === 'stella') {
      stellaPatch = null;
      if (result.patchInstalled) addLog("Stella's patch installed (a checked build).", 'ok', 'install');
    }
    if (activeNetwork === 'receos') {
      stellaPatch = null;
      addLog(result.patchInstalled
        ? 'Rec EOS installed into the client.'
        : "The client is installed, but Rec EOS couldn't be added yet. Press UPDATE on Home.", result.patchInstalled ? 'ok' : 'warn', 'install');
    }
    // The download stamped a new client build id / version / ETag directly into
    // config.json. Re-sync our in-memory copy from disk so the next settings
    // autosave (which writes the whole config back) doesn't revert those to the
    // stale values loaded at startup — which would flag the just-installed
    // client as "outdated" and kick off an endless re-download loop.
    config = (await window.radium?.getConfig()) || config;
    // The client just changed — any cached "update available" state is now
    // stale, so force a fresh check next time checkInstall() runs below.
    clientUpdateInfo = null;
    clientUpdateAutoChecked = false;
    await checkInstall();
  } else {
    const err = result?.error || 'Unknown error';
    if (err === 'Paused') {
      // Paused by the user — keep the panel visible and frozen so it can be
      // resumed. The partial file is preserved on disk by the backend.
      isDownloading = false;
      isPaused = true;
      setPausedUI();
      addLog(`Download paused at ${elapsed}s. Click Resume to continue.`, 'info', 'install');
      return; // don't run checkInstall — the download isn't finished or gone
    }
    setDownloadUI(false);
    if (err === 'Cancelled') {
      // User-initiated cancel — the cancel handler already logged/toasted it,
      // so don't also report it as a failure.
      addLog(`Download stopped after ${elapsed}s (cancelled by user).`, 'info', 'install');
    } else {
      addLog(`Download failed after ${elapsed}s: ${err}`, 'error', 'install');
      toast(`Failed: ${err}`, 'error', 5000);
    }
    // Restore the correct panel (e.g. back to the launch panel if still installed).
    await checkInstall();
    // A stall or a dropped connection keeps what arrived, and the error says
    // "Resume to continue" — so show the Resume button it means rather than
    // leaving only a DOWNLOAD that reads as starting over.
    if (err !== 'Cancelled') await offerResumeIfAny({ quiet: true });
  }
}

$('btnDownload')?.addEventListener('click', () => {
  // A damaged install is repaired, not downloaded again from scratch.
  if (needsRepair) {
    runVerifyFiles();
    return;
  }
  // Nothing to install on this network yet — send the user to the source
  // instead of starting a download that would immediately fail.
  if (!clientDownloadAvailable()) {
    const info = networkInfo();
    addLog(`Opening ${info.downloadPage} — no ${info.label} client to install yet.`, 'info', 'install');
    window.radium?.openUrl(info.downloadPage);
    return;
  }
  runClientDownload();
});

// On startup, offer to continue a download that was interrupted last session
// (paused, or the launcher was closed mid-download). The partial file + resume
// metadata persist on disk, so the backend can pick up exactly where it left off.
/// `quiet` leaves out the toast, for a failure that has just shown its own.
async function offerResumeIfAny({ quiet = false } = {}) {
  if (isDownloading || isPaused || isInstalled) return;
  let info = null;
  try {
    info = await window.radium?.resumableDownloadInfo();
  } catch (e) {
    console.error('resumableDownloadInfo error:', e);
    return;
  }
  if (!info?.resumable) return;

  const ds = $('downloadSection'); if (ds) ds.style.display = 'flex';
  document.body.classList.remove('client-installed');
  // The gear that opens it lives in the installed-only bar and has just
  // disappeared; an open menu would be left hanging over the download CTA.
  closeManageMenu();
  setPausedUI({ downloaded: info.downloaded, total: info.total });

  const pctTxt = info.total > 0 ? ` (${Math.floor((info.downloaded / info.total) * 100)}%)` : '';
  addLog(`Found an interrupted download${pctTxt}. Click Resume to continue.`, 'info', 'install');
  if (!quiet) toast('Resume your interrupted download', 'info', 4500);
}

// ─── Outdated-client (post-launcher-update) prompt ──────────────────────────
const clientUpdateModal = $('clientUpdateModal');
const closeClientUpdateModal = () => hideModal(clientUpdateModal);
$('clientUpdateLaterBtn')?.addEventListener('click', closeClientUpdateModal);
$('clientUpdateModalClose')?.addEventListener('click', closeClientUpdateModal);
$('clientUpdateModal')?.addEventListener('click', (e) => { if (e.target === clientUpdateModal) closeClientUpdateModal(); });
$('clientUpdateNowBtn')?.addEventListener('click', () => {
  closeClientUpdateModal();
  switchTab('home');
  runClientDownload();
});

// ─── Live client version update (Steam-style "Update Available" prompt) ────
let clientUpdateInfo = null;
// checkInstall() re-runs after every settings autosave, game state change,
// etc. — track whether we've already done the network-based version check
// this session so we only reapply cached state (no flicker) instead of
// re-fetching and resetting the button/card on every call.
let clientUpdateAutoChecked = false;

function formatPatchNotes(notes) {
  if (!Array.isArray(notes) || notes.length === 0) return '(No patch notes available.)';
  return notes
    .map((n) => {
      const header = n.date ? `${n.version} — ${n.date}` : n.version;
      const bullets = (n.notes || []).map((item) => `• ${item}`).join('\n');
      return bullets ? `${header}\n${bullets}` : header;
    })
    .join('\n\n');
}

// Manual update button embedded in the "Client Status" quick-stat card.
// States: 'hidden' (no client installed), 'check' (installed, no known
// update — click to re-check), 'checking' (check in progress),
// 'update' (update available — click to install it).
/// Drive the Check for Updates row in the gear menu.
///
/// It lives in that menu rather than in the Client Status tile, where it used
/// to share a line with the status value. That tile is one equal third of the
/// stats row, so the button never reliably fitted beside the value — there was
/// a whole `fitUpdateLabel()` routine picking the longest of three wordings
/// that would fit at the current window width, re-run on every resize. A menu
/// row is as wide as the menu, so the full wording simply fits and all of that
/// is gone.
///
/// Hidden through the attribute, not an inline `display`: the menu's layout
/// rule forces `display: flex !important`, which an inline style cannot beat.
function setClientUpdateButton(state, info) {
  const btn = $('btnClientUpdateAction');
  if (!btn) return;
  if (state === 'hidden') {
    btn.hidden = true;
    return;
  }
  btn.hidden = false;
  btn.disabled = state === 'checking';

  // The row is an icon plus a label, so only the label is rewritten — setting
  // textContent here would drop the icon with it.
  const label = btn.querySelector('span');
  if (state === 'checking') {
    if (label) label.textContent = 'Checking...';
    btn.dataset.mode = 'checking';
  } else if (state === 'update') {
    if (label) label.textContent = 'Install Update';
    btn.dataset.mode = 'update';
  } else {
    if (label) label.textContent = 'Check for Updates';
    btn.dataset.mode = 'check';
  }
}

$('btnClientUpdateAction')?.addEventListener('click', () => {
  const btn = $('btnClientUpdateAction');
  if (btn?.dataset.mode === 'update') {
    switchTab('home');
    runClientDownload();
    return;
  }
  if (btn?.dataset.mode === 'checking') return;
  if (networkInfo().hasPatch) {
    refreshStellaPatch({ manual: true });
    return;
  }
  setClientUpdateButton('checking');
  toast('Checking for client updates...', 'info', 2000);
  checkForClientUpdate(true);
});

function showClientVersionUpdateModal(info) {
  clientUpdateInfo = info;
  const cv = $('clientUpdateCurrentVer'); if (cv) cv.textContent = info.versionKnown ? `v${info.installedVersion}` : 'Unknown';
  const lv = $('clientUpdateLatestVer'); if (lv) lv.textContent = `v${info.latestVersion}`;
  const notes = $('clientUpdateNotes');
  if (notes) {
    let preface = '';
    if (!info.versionKnown) {
      preface = `Your installed client's version couldn't be confirmed (it was installed before update tracking was added). Updating now will sync it to the latest build and enable accurate update checks going forward.\n\n`;
    } else if (info.sameVersionRebuilt) {
      preface = `The site rebuilt v${info.latestVersion} without changing the version number — your download doesn't match the current build fingerprint.\n\n`;
    }
    notes.textContent = preface + formatPatchNotes(info.patchNotes);
  }
  const status = $('clientUpdateStatus'); if (status) status.style.display = 'none';
  const nowBtn = $('clientVersionUpdateNowBtn');
  if (nowBtn) { nowBtn.disabled = false; nowBtn.textContent = '⬇ Update Now'; }
  showModal($('clientVersionUpdateModal'));

  const qscC = $('qsc-client');
  if (qscC) qscC.classList.add('update-available');
  const qi = $('qsInstalled'); if (qi) qi.textContent = `Update to v${info.latestVersion}`;
  setClientUpdateButton('update', info);
}

const closeClientVersionUpdateModal = () => hideModal($('clientVersionUpdateModal'));
$('clientVersionUpdateLaterBtn')?.addEventListener('click', closeClientVersionUpdateModal);
$('clientVersionUpdateModalClose')?.addEventListener('click', closeClientVersionUpdateModal);
$('clientVersionUpdateModal')?.addEventListener('click', (e) => { if (e.target === $('clientVersionUpdateModal')) closeClientVersionUpdateModal(); });
$('clientVersionUpdateNowBtn')?.addEventListener('click', () => {
  closeClientVersionUpdateModal();
  switchTab('home');
  runClientDownload();
});

async function checkForClientUpdate(manual = false) {
  try {
    const info = await window.radium?.checkClientUpdate();
    if (!info?.success) {
      const reason = info?.error || 'Unknown error';
      if (manual) { addLog(`Client update check failed: ${reason}`, 'error', 'install'); toast(`Update check failed: ${reason}`, 'error', 4000); }
      setClientUpdateButton('check');
      return;
    }
    if (!info.hasUpdate) {
      const curLabel = info.versionKnown ? `v${info.installedVersion}` : 'unknown version';
      if (manual) { addLog(`Client is up to date (${curLabel}, latest v${info.latestVersion || '—'}).`, 'ok', 'install'); toast('Client is up to date.', 'ok', 3000); }
      setClientUpdateButton('check');
      return;
    }
    const fromLabel = info.versionKnown ? `v${info.installedVersion}` : 'unknown version';
    const changeNote = info.sameVersionRebuilt ? ' (same version, new build detected)' : '';
    addLog(`Client update available: ${fromLabel} → v${info.latestVersion}${changeNote}`, 'warn', 'install');
    toast('Client update available!', 'ok', 4000);
    showClientVersionUpdateModal(info);
  } catch (e) {
    console.error('checkClientUpdate error:', e);
    if (manual) toast('Update check failed.', 'error', 3000);
    setClientUpdateButton('check');
  }
}

// ─── Stella patch UPDATE (Steam-style) ─────────────────────────────────────
// Stella's client is patched at launch by a small DLL that changes far more
// often than the client. A new build of it is offered the way Steam offers a
// game update: a blue UPDATE button in PLAY's place with an "UPDATE QUEUED /
// 0% Complete" readout, rather than the launcher-update dialog, so it is never
// mistaken for an update to the launcher. Pressing it is what accepts that
// build; the backend then injects only a patch matching what was accepted.

/// How often an installed Stella client is re-checked for a new patch while
/// the launcher stays open.
const STELLA_PATCH_RECHECK_MS = 3 * 60 * 60 * 1000;
let stellaPatchCheckedAt = 0;
let stellaPatchCheckSeq = 0;

function stellaUpdateNeeded() {
  return networkInfo().hasPatch && stellaPatchNetwork === activeNetwork && isInstalled
    && (stellaPatchUpdating || stellaPatch?.updateAvailable === true);
}

/// The backend calls for the active network's patch.
function patchApi(network = activeNetwork) {
  return network === 'receos'
    ? { status: () => window.radium?.receosPatchStatus(), update: () => window.radium?.receosUpdatePatch() }
    : { status: () => window.radium?.stellaPatchStatus(), update: () => window.radium?.stellaUpdatePatch() };
}

function setStellaUpdateProgress(title, pct) {
  const clamped = Math.max(0, Math.min(100, Math.round(pct)));
  const t = $('stellaUpdateTitle'); if (t) t.textContent = title;
  const p = $('stellaUpdatePct'); if (p) p.textContent = `${clamped}% Complete`;
  const f = $('stellaUpdateFill'); if (f) f.style.width = `${clamped}%`;
}

/// Swap PLAY for UPDATE (or back) to match the last patch check.
function applyStellaPatchUI() {
  const needed = stellaUpdateNeeded();
  document.body.classList.toggle('stella-update-needed', needed);
  document.body.classList.toggle('stella-updating', needed && stellaPatchUpdating);
  const btn = $('btnStellaUpdate');
  if (btn) btn.disabled = stellaPatchUpdating;
  if (!stellaPatchUpdating) {
    // Steam says QUEUED for an update waiting on you and REQUIRED when the
    // game can't run without it; a missing patch is the second, and so is a
    // Rec EOS build its server no longer lets in.
    const required = !stellaPatch?.installed || stellaPatch?.required === true;
    setStellaUpdateProgress(required ? 'UPDATE REQUIRED' : 'UPDATE QUEUED', 0);
  }
}

/// Ask whether Stella's patch needs installing or has a new build. Automatic
/// checks are throttled; a manual one (Check for Updates) always runs.
async function refreshStellaPatch({ manual = false } = {}) {
  const net = activeNetwork;
  if (!networkInfo(net).hasPatch || !isInstalled || stellaPatchUpdating) return;
  // A check made for the other patch network says nothing about this one.
  if (stellaPatchNetwork !== net) { stellaPatch = null; stellaPatchCheckedAt = 0; }
  if (!manual && stellaPatch && Date.now() - stellaPatchCheckedAt < STELLA_PATCH_RECHECK_MS) {
    applyStellaPatchUI();
    return;
  }
  const seq = ++stellaPatchCheckSeq;
  if (manual) setClientUpdateButton('checking');
  let status = null;
  try {
    status = await patchApi(net).status();
  } catch (e) {
    status = { success: false, error: String(e) };
  }
  // A switch away, or a newer check, while this one was in flight.
  if (seq !== stellaPatchCheckSeq || activeNetwork !== net) return;
  stellaPatchCheckedAt = Date.now();
  stellaPatch = status;
  stellaPatchNetwork = net;
  if (isInstalled) setClientUpdateButton('check');
  applyStellaPatchUI();

  const name = net === 'receos' ? 'Rec EOS' : 'Stella';
  if (!status?.success) {
    const reason = status?.error || 'Unknown error';
    addLog(`Couldn't check for a ${name} update: ${reason}`, 'warn', 'install');
    if (manual) toast(`Update check failed: ${reason}`, 'error', 4000);
  } else if (status.updateAvailable) {
    if (net === 'receos') {
      addLog(status.installed
        ? `Rec EOS has an update (build ${status.latestBuild}${status.installedBuild ? `, you have ${status.installedBuild}` : ''}). Press UPDATE on Home to install it.`
        : 'Rec EOS needs to be added to the game before it can be played. Press UPDATE on Home.', 'warn', 'install');
    } else {
      addLog(status.installed
        ? `Stella has an update (patch ${status.latestSha256.slice(0, 12)}…). Press UPDATE on Home to install it.`
        : 'Stella needs its patch before it can be played. Press UPDATE on Home to install it.', 'warn', 'install');
    }
    if (manual) toast(`${name} has an update.`, 'ok', 3000);
  } else if (manual) {
    addLog(`${name} is up to date.`, 'ok', 'install');
    toast(`${name} is up to date.`, 'ok', 3000);
  }
}

async function runStellaPatchUpdate() {
  const net = activeNetwork;
  if (stellaPatchUpdating || !networkInfo(net).hasPatch) return;
  const name = net === 'receos' ? 'Rec EOS' : 'Stella';
  if (isGameRunning || isGameLaunching) {
    toast('Close the game before updating.', 'warn', 4000);
    return;
  }
  stellaPatchUpdating = true;
  applyStellaPatchUI();
  setStellaUpdateProgress('UPDATING', 0);
  addLog(`Updating ${name}...`, 'info', 'install');

  let result = null;
  try {
    result = await patchApi(net).update();
  } catch (e) {
    result = { success: false, error: String(e) };
  }
  stellaPatchUpdating = false;

  if (result?.success) {
    stellaPatch = {
      ...(stellaPatch || {}),
      success: true,
      installed: true,
      updateAvailable: false,
      installedSha256: result.sha256,
      latestSha256: result.sha256,
    };
    if (net === 'receos') {
      stellaPatch.required = false;
      stellaPatch.installedBuild = result.build;
      stellaPatch.latestBuild = result.build;
    }
    stellaPatchCheckedAt = Date.now();
    stellaPatchNetwork = net;
    // The backend recorded the accepted build in config.json; keep the next
    // settings autosave from carrying a stale copy (it preserves the field
    // anyway, but the in-memory config should say what is on disk).
    config = (await window.radium?.getConfig()) || config;
    if (net === 'receos') addLog(`Rec EOS updated to build ${result.build}.`, 'ok', 'install');
    else addLog(`Stella updated (patch ${String(result.sha256).slice(0, 12)}…${result.pinned ? ', a checked build' : ''}).`, 'ok', 'install');
    toast(`${name} is up to date.`, 'ok', 3000);
  } else {
    const err = result?.error || 'Unknown error';
    addLog(`${name} update failed: ${err}`, 'error', 'install');
    toast(`Update failed: ${err}`, 'error', 5000);
  }
  applyStellaPatchUI();
}

$('btnStellaUpdate')?.addEventListener('click', () => runStellaPatchUpdate());

function onPatchProgress(p) {
  if (!stellaPatchUpdating || !p) return;
  if (p.phase === 'download') setStellaUpdateProgress('UPDATING', p.pct || 0);
  else if (p.phase === 'install') setStellaUpdateProgress('INSTALLING', p.pct || 99);
  else if (p.phase === 'done') setStellaUpdateProgress('INSTALLING', 100);
}
window.radium?.onStellaPatchProgress(onPatchProgress);
window.radium?.onReceosPatchProgress(onPatchProgress);

// A launcher left open for days still finds a new patch.
setInterval(() => { refreshStellaPatch(); }, STELLA_PATCH_RECHECK_MS);

$('btnCancelDl')?.addEventListener('click', () => {
  const wasPaused = isPaused;
  const wasActive = isDownloading;
  window.radium?.cancelDownload();
  isPaused = false;
  if (wasActive) {
    // The backend is still mid-chunk and won't release its download guard
    // until it unwinds. Hold the panel in a "cancelling" state so Download
    // can't be re-clicked into a rejection; runClientDownload() clears this
    // as soon as the in-flight call actually settles.
    isCancelling = true;
    isDownloading = false;
    const phaseEl = $('dlPhaseLabel'); if (phaseEl) phaseEl.textContent = 'Cancelling...';
    const pauseBtn = $('btnPauseDl'); if (pauseBtn) pauseBtn.style.display = 'none';
    updateDlButtons();
  } else {
    setDownloadUI(false);
  }
  const what = clientTask === 'verify' ? 'File check' : clientTask === 'repair' ? 'Repair' : 'Download';
  addLog(`${what} cancelled.`, 'info', 'install');
  toast(`${what} cancelled.`, 'info');
  // A cancelled *active* download's checkInstall() runs when its promise
  // rejects; a cancelled *paused* download has no pending promise, so restore
  // the view here.
  if (wasPaused) checkInstall();
});

// Pause / Resume toggle.
$('btnPauseDl')?.addEventListener('click', () => {
  // Hidden for a download that can't be continued, and a press that lands
  // anyway does nothing: the backend lets it go and the download carries on.
  if (!isPaused && !dlResumable) return;
  if (isPaused) {
    // Resume — re-invoke the download, which continues from the .part file
    // (whether it was paused this session or left over from a previous run).
    runClientDownload({ resuming: true });
  } else if (isDownloading) {
    // Pause — the backend stops the loop and keeps the partial file. The UI
    // flips to the paused state when the 'paused' event / Paused result lands;
    // update the label now so the click feels responsive.
    window.radium?.pauseDownload();
    addLog('Pausing download...', 'info', 'install');
    const b = $('btnPauseDl'); if (b) b.textContent = '▶ Resume';
  }
});

// Reinstall logic with Modal
const reinstallModal = $('reinstallModal');
const closeReinstallModal = () => hideModal(reinstallModal);
$('reinstallCancelBtn')?.addEventListener('click', closeReinstallModal);
$('reinstallModalClose')?.addEventListener('click', closeReinstallModal);
$('reinstallModal')?.addEventListener('click', (e) => {
  if (e.target === reinstallModal) {
    closeReinstallModal();
  }
});

$('btnReinstall')?.addEventListener('click', () => {
  if (isDownloading) {
    toast('Download already in progress.', 'error');
    return;
  }
  if (isGameRunning) {
    toast('Cannot reinstall while the game is running.', 'error');
    return;
  }
  // Show the current install directory in the modal so the user can confirm
  const dirSpan = $('reinstallModalInstallDir');
  if (dirSpan) {
    dirSpan.textContent = shownInstallDir();
  }
  showModal(reinstallModal);
});

$('reinstallConfirmBtn')?.addEventListener('click', () => {
  closeReinstallModal();
  const ds = $('downloadSection'); if (ds) ds.style.display = 'flex';
  document.body.classList.remove('client-installed');
  // The gear that opens it lives in the installed-only bar and has just
  // disappeared; an open menu would be left hanging over the download CTA.
  closeManageMenu();
  isInstalled = false;
  addLog('Reinstall initiated.', 'info', 'install');
  toast('Starting reinstall...', 'info');
  $('btnDownload')?.click();
});

// ─── Verify Files ────────────────────────────────────────────────────────────
// Steam's "Verify integrity of game files". Every installed file is read back
// and checked against the size and CRC-32 its zip recorded at install, and
// the ones that fail are fetched again (verify.rs). It borrows the download
// panel and `isDownloading`, so everything a download blocks, it blocks too.

const verifyModal = $('verifyModal');
let verifyModalAnswer = null;

/// Ask a Verify Files question. Resolves true for the confirm button and
/// false for anything else that closes the dialog.
function askVerify({ title, lead, text, confirm = 'OK', cancel = 'Cancel' }) {
  $('verifyModalTitle').textContent = title;
  $('verifyModalLead').textContent = lead;
  $('verifyModalText').textContent = text;
  $('verifyModalConfirm').textContent = confirm;
  $('verifyModalCancel').textContent = cancel;
  verifyModalAnswer?.(false);
  showModal(verifyModal);
  return new Promise((resolve) => { verifyModalAnswer = resolve; });
}
function answerVerify(yes) {
  const answer = verifyModalAnswer;
  verifyModalAnswer = null;
  hideModal(verifyModal);
  answer?.(yes);
}
$('verifyModalConfirm')?.addEventListener('click', () => answerVerify(true));
$('verifyModalCancel')?.addEventListener('click', () => answerVerify(false));
$('verifyModalClose')?.addEventListener('click', () => answerVerify(false));
verifyModal?.addEventListener('click', (e) => { if (e.target === verifyModal) answerVerify(false); });

/// Put the download panel up for a check or a repair, its steps named `steps`.
function beginClientTask(kind, steps) {
  clientTask = kind;
  const ds = $('downloadSection'); if (ds) ds.style.display = 'flex';
  document.body.classList.remove('client-installed');
  // The gear that opens it lives in the installed-only bar, which is going.
  closeManageMenu();
  setDlStepLabels(...steps);
  setDownloadUI(true);
  const phaseEl = $('dlPhaseLabel');
  if (phaseEl) phaseEl.textContent = kind === 'verify' ? 'Verifying files...' : 'Repairing files...';
  setStatLabels('Files', 'Current File', 'ETA');
  for (const id of ['dlSpeedLabel', 'dlSizeLabel', 'dlEtaLabel']) { const el = $(id); if (el) el.textContent = '—'; }
}

/// Take the panel down again, and show the client as it now is.
async function endClientTask() {
  clientTask = null;
  isCancelling = false;
  setDownloadUI(false);
  setDlStepLabels();
  await checkInstall();
}

const VERIFY_PROBLEMS = { missing: 'missing', size: 'wrong size', content: 'changed', unreadable: "can't be read" };
const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

async function runVerifyFiles() {
  if (isDownloading || isPaused || isCancelling) {
    toast('Finish or cancel the current download first.', 'warn', 4000);
    return;
  }
  if (isGameRunning || isGameLaunching) {
    toast('Close the game before verifying its files.', 'warn', 4000);
    return;
  }
  const label = networkInfo().label;
  beginClientTask('verify', ['Verify', 'Repair', 'Done']);
  addLog(`Verifying ${label} game files...`, 'info', 'install');
  const started = Date.now();
  let result;
  try {
    result = await window.radium?.verifyClient();
  } catch (e) {
    result = { status: 'error', error: String(e) };
  }
  const secs = ((Date.now() - started) / 1000).toFixed(1);

  if (result?.status === 'ok') {
    addLog(`All ${result.checked} files validated in ${secs}s.`, 'ok', 'install');
    toast(`All ${result.checked} files successfully validated.`, 'ok', 5000);
    await endClientTask();
    return;
  }
  if (result?.status === 'cancelled') {
    addLog(`File check stopped after ${secs}s.`, 'info', 'install');
    await endClientTask();
    return;
  }
  if (result?.status === 'no-manifest') {
    await endClientTask();
    addLog(`This ${label} install has no file list to verify against; it predates Verify Files.`, 'warn', 'install');
    const reinstall = await askVerify({
      title: 'VERIFY FILES',
      lead: `This ${label} install has no file list to check against.`,
      text: "The launcher records every file's checksum when it installs a client, and this one was "
          + 'installed before it did. Reinstall once (the client is downloaded again) and Verify Files '
          + 'works from then on.',
      confirm: 'Reinstall',
      cancel: 'Not now',
    });
    // The question above was the confirmation, so straight to the reinstall.
    if (reinstall) $('reinstallConfirmBtn')?.click();
    return;
  }
  if (result?.status !== 'damaged') {
    const err = result?.error || 'Unknown error';
    addLog(`File check failed: ${err}`, 'error', 'install');
    toast(`File check failed: ${err}`, 'error', 5000);
    await endClientTask();
    return;
  }

  const damaged = result.damaged || [];
  addLog(`${damaged.length} of ${result.checked} files failed to validate (${formatBytes(result.damagedBytes || 0)}):`, 'warn', 'install');
  for (const f of damaged.slice(0, 20)) addLog(`  ${f.path}: ${VERIFY_PROBLEMS[f.problem] || f.problem}`, 'warn', 'install');
  if (damaged.length > 20) addLog(`  ...and ${damaged.length - 20} more.`, 'warn', 'install');
  toast(`${plural(damaged.length, 'file')} failed to validate and will be reacquired.`, 'warn', 5000);
  await runRepair(damaged.length);
}

/// Fetch again the files the check just failed. Single files where the
/// server allows it; otherwise, once asked, the whole client, taking only
/// those files from it.
async function runRepair(count) {
  const label = networkInfo().label;
  clientTask = 'repair';
  updateDlButtons();
  setDlStep('extract');
  const phaseEl = $('dlPhaseLabel'); if (phaseEl) phaseEl.textContent = 'Repairing files...';
  let r;
  try {
    r = await window.radium?.repairClient(false);
  } catch (e) {
    r = { success: false, error: String(e) };
  }

  if (r?.needsFullDownload) {
    await endClientTask();
    const size = r.total ? formatBytes(r.total) : (activeNetwork === 'stella' ? 'about 4.7 GB' : 'the whole client');
    // Either way the repair reads the client only as far as the last damaged
    // file. With the files' places in the zip recorded at install, how far
    // that is is known up front; without, it is found on the way.
    const text = r.needed
      ? `${label}'s server can't send single files, so the repair reads the client from the start and stops `
        + `once it has the damaged ones: about ${formatBytes(r.needed)} of ${size}. Only those files are replaced.`
      : `${label}'s server can't send single files, so the repair reads the client from the start and stops `
        + `once it has the damaged ones. This install doesn't record where they are in the ${size} download, `
        + "so how much that is shows as it goes, and it's all of it only if one is near the end. "
        + 'Only those files are replaced.';
    const go = await askVerify({
      title: 'REPAIR FILES',
      lead: `${plural(count, 'file')} failed to validate.`,
      text,
      confirm: r.needed ? `Download ${formatBytes(r.needed)} and repair` : 'Repair',
    });
    if (!go) {
      addLog('Repair skipped. Run Verify Files again to repair.', 'info', 'install');
      return;
    }
    beginClientTask('repair', ['Download', 'Repair', 'Done']);
    addLog(r.needed
      ? `Reading the first ${formatBytes(r.needed)} of the ${label} client to repair ${plural(count, 'file')}...`
      : `Reading the ${label} client from the start, as far as needed, to repair ${plural(count, 'file')}...`, 'info', 'install');
    try {
      r = await window.radium?.repairClient(true);
    } catch (e) {
      r = { success: false, error: String(e) };
    }
  }

  if (r?.success) {
    addLog(`Repaired ${plural(r.repaired, 'file')}.`, 'ok', 'install');
    toast(`Repaired ${plural(r.repaired, 'file')}. ${label} is ready to play.`, 'ok', 5000);
  } else if (r?.error === 'Cancelled') {
    addLog('Repair stopped; the damaged files were left as they were. Run Verify Files to try again.', 'info', 'install');
  } else {
    const err = r?.error || 'Unknown error';
    addLog(`Repair failed: ${err}`, 'error', 'install');
    toast(`Repair failed: ${err}`, 'error', 6000);
  }
  await endClientTask();
}

$('btnVerifyFiles')?.addEventListener('click', () => {
  closeManageMenu();
  runVerifyFiles();
});

// Stop Game logic with Modal
const stopGameModal = $('stopGameModal');
const closeStopGameModal = () => hideModal(stopGameModal);
$('stopGameCancelBtn')?.addEventListener('click', closeStopGameModal);
$('stopGameModalClose')?.addEventListener('click', closeStopGameModal);
$('stopGameConfirmBtn')?.addEventListener('click', async () => {
  closeStopGameModal();
  addLog('User confirmed stop game request. Killing game process...', 'info', 'game');
  toast(`Stopping ${networkInfo().label}...`, 'info', 2000);
  await window.radium?.killGame();
  setGameRunning(false);
});
$('stopGameModal')?.addEventListener('click', (e) => {
  if (e.target === stopGameModal) {
    closeStopGameModal();
  }
});
function showStopGameModal() {
  showModal(stopGameModal);
}

// Uninstall logic with Modal
const uninstallModal = $('uninstallModal');
const closeUninstallModal = () => hideModal(uninstallModal);
$('uninstallCancelBtn')?.addEventListener('click', closeUninstallModal);
$('uninstallModalClose')?.addEventListener('click', closeUninstallModal);

$('btnUninstall')?.addEventListener('click', () => {
  if (isGameRunning) {
    toast('Cannot uninstall while the game is running.', 'error');
    return;
  }
  const label = networkInfo().label;
  const question = $('uninstallQuestion');
  if (question) question.textContent = `Are you sure you want to uninstall the ${label.charAt(0)}${label.slice(1).toLowerCase()} client?`;
  // Offered only when there is an exclusion to remove, and ticked: a folder
  // left excluded with nothing in it is one Defender never scans again.
  const avRow = $('uninstallAvRow');
  if (avRow) avRow.style.display = avExclusionOnRecord() ? 'flex' : 'none';
  const removeAv = $('uninstallRemoveAv');
  if (removeAv) removeAv.checked = true;
  showModal(uninstallModal);
});

$('uninstallConfirmBtn')?.addEventListener('click', async () => {
  const removeAv = avExclusionOnRecord() && $('uninstallRemoveAv')?.checked === true;
  closeUninstallModal();
  addLog('Uninstalling client...', 'info', 'install');
  toast('Uninstalling...', 'info');

  const result = await window.radium?.uninstallClient();
  if (result?.success) {
    addLog('Client uninstalled successfully.', 'ok', 'install');
    toast(`${networkInfo().label} client uninstalled.`, 'ok');
    if (removeAv && !(await removeAvExclusion())) {
      addLog('Windows Defender still excludes the client folder. Remove it in Windows Security (Virus & threat protection → Exclusions), or with UNExclude AV after reinstalling.', 'warn', 'game');
    }
    await loadConfig();
    await checkInstall();
  } else {
    const err = result?.error || 'Unknown error';
    addLog(`Uninstall failed: ${err}`, 'error', 'install');
    toast(`Uninstall failed: ${err}`, 'error');
  }
});

// Open client folder button
$('btnOpenFolder')?.addEventListener('click', async () => {
  addLog('Opening client folder...', 'info', 'install');
  const ok = await window.radium?.openClientFolder();
  if (ok) {
    toast('Client folder opened!', 'ok');
  } else {
    toast('Failed to open client folder (does it exist?).', 'error');
  }
});

/// Open / change / reset act on the network named by the clicked button, not
/// on the active one — that is what lets Vanilla's folder be set while Radium
/// is selected, and the other way round.
function installRowNetwork(btn, attr) {
  const name = btn.getAttribute(attr);
  return NETWORKS[name] ? name : activeNetwork;
}

/// After `network`'s install folder moved: the Defender exclusion stays on the
/// old folder (the backend no longer counts the new one as excluded), so take
/// the backend's word for it and say so. The next Exclude AV, or the check
/// when PLAY is pressed, moves it across.
async function noteAvAfterFolderMove(network) {
  if (!avExclusionOnRecord(network)) return;
  let fresh = null;
  try {
    fresh = await window.radium?.getConfig();
  } catch (e) {
    console.error('getConfig error:', e);
  }
  if (!fresh) return;
  const state = network === 'radium' ? fresh : (fresh[network] || {});
  syncAvState(network, state.defenderExcluded, state.defenderDirs);
  if (avExclusionOnRecord(network) && !avExcluded(network)) {
    addLog(`Windows Defender still excludes ${networkInfo(network).label}'s old folder (${networkState(network).defenderDirs.join(', ')}). Excluding the new one, from Exclude AV or when you press PLAY, takes that off.`, 'warn', 'game');
  }
}

/// A folder change only invalidates launcher state when it moved the client
/// the launcher is currently pointed at.
async function afterInstallDirChange(network) {
  await noteAvAfterFolderMove(network);
  if (network !== activeNetwork) return;
  // The client at this new location may be a different install entirely — any
  // cached update-check result is meaningless here, so force a fresh check.
  clientUpdateInfo = null;
  clientUpdateAutoChecked = false;
  await checkInstall();
}

document.querySelectorAll('[data-open-folder]').forEach(btn => {
  btn.addEventListener('click', async () => {
    const network = installRowNetwork(btn, 'data-open-folder');
    const label = networkInfo(network).label;
    addLog(`Opening ${label} client folder from settings...`, 'info', 'install');
    const ok = await window.radium?.openClientFolder(network);
    if (ok) {
      toast(`${label} client folder opened!`, 'ok');
    } else {
      toast(`Failed to open the ${label} client folder (does it exist?).`, 'error');
    }
  });
});

document.querySelectorAll('[data-change-folder]').forEach(btn => {
  btn.addEventListener('click', async () => {
    const network = installRowNetwork(btn, 'data-change-folder');
    const label = networkInfo(network).label;
    addLog(`Selecting ${label} install directory...`, 'info', 'install');
    const newDir = await window.radium?.selectFolder(network);
    if (!newDir) {
      addLog(`${label} install directory selection cancelled.`, 'info', 'install');
      return;
    }
    // The backend refuses to let both networks resolve to one folder, or to
    // one inside the other — they install different games from different
    // sources, so a shared folder means each download overwrites the other's
    // client, and the install check (which searches beneath the folder) finds
    // the other network's game in a folder that holds it. It silently clears
    // the colliding entry on the next config read, so catch it here instead
    // and leave the user's existing setting alone.
    // The client goes in the network's own subfolder of the pick, which is
    // what gets checked and shown. The pick itself is what gets saved.
    let clientDir = newDir;
    try {
      clientDir = (await window.radium?.resolveClientDir(network, newDir)) || newDir;
    } catch (e) {
      console.error('resolveClientDir error:', e);
    }
    const clash = overlappingInstallDir(network, clientDir);
    if (clash) {
      const otherLabel = networkInfo(clash.network).label;
      const why = normDir(clash.dir) === normDir(clientDir) ? 'is already' : 'overlaps';
      toast(`That folder ${why} ${otherLabel}'s install location. Pick a different one.`, 'error', 4000);
      addLog(`Rejected ${label} install directory: ${clientDir} ${why} ${otherLabel}'s (${clash.dir}).`, 'warn', 'install');
      return;
    }

    const span = installDirSpan(network);
    if (!span) return;
    span.textContent = clientDir;
    setConfigInstallDir(newDir, network);
    const ok = await window.radium?.saveConfig(config);
    if (ok) {
      toast(`${label} install location updated and saved!`, 'ok');
      addLog(`Selected and saved ${label} install directory: ${clientDir}`, 'info', 'install');
    } else {
      toast(`Failed to save the ${label} install location.`, 'error');
    }
    await afterInstallDirChange(network);
  });
});

document.querySelectorAll('[data-reset-folder]').forEach(btn => {
  btn.addEventListener('click', async () => {
    const network = installRowNetwork(btn, 'data-reset-folder');
    const label = networkInfo(network).label;
    addLog(`Resetting ${label} install directory...`, 'info', 'install');
    const defaultDir = await window.radium?.getDefaultClientDir(network);
    if (!defaultDir) return;
    const span = installDirSpan(network);
    if (!span) return;
    span.textContent = defaultDir;
    // Stored as "" rather than the resolved path: an empty slot means "use
    // this network's default", so a reset keeps tracking the default instead
    // of pinning a literal path a later network switch could misapply.
    setConfigInstallDir('', network);
    const ok = await window.radium?.saveConfig(config);
    if (ok) {
      toast(`${label} install location reset and saved!`, 'ok');
      addLog(`Reset and saved ${label} install directory: ${defaultDir}`, 'info', 'install');
    } else {
      toast(`Failed to save the reset ${label} location.`, 'error');
    }
    await afterInstallDirChange(network);
  });
});

// Exclude AV Warning Modal Actions
function showExcludeAvModal() {
  showModal($('excludeAvModal'));
}

function hideExcludeAvModal() {
  hideModal($('excludeAvModal'));
  launchAfterExclusion = false; // Reset whenever the modal is hidden
  isGameLaunching = false;
}

$('excludeAvModalClose')?.addEventListener('click', hideExcludeAvModal);
$('btnExcludeAvCancel')?.addEventListener('click', hideExcludeAvModal);
$('btnExcludeAvAnyway')?.addEventListener('click', async () => {
  hideModal($('excludeAvModal'));
  launchAfterExclusion = false; // Reset since we are launching now anyway
  await proceedAfterAvCheck();
});
$('excludeAvModal')?.addEventListener('click', (e) => {
  if (e.target === $('excludeAvModal')) {
    hideExcludeAvModal();
  }
});

/// Set the Exclude AV menu row's label without touching the rest of the row.
///
/// The button holds an icon and a <span> for its text. Assigning its
/// textContent, which every caller used to do, replaced both with bare text —
/// so after the config loaded the row lost its icon and its text fell out of
/// line with the other rows in the manage menu.
function setExcludeAvLabel(excluded) {
  const btn = $('btnExcludeAv');
  if (!btn) return;
  const label = excluded ? 'UNExclude AV' : 'Exclude AV';
  const span = btn.querySelector('span');
  if (span) span.textContent = label;
  else btn.textContent = label;
}

async function executeExcludeAv() {
  const btn = $('btnExcludeAv');
  if (!btn) return;
  addLog('Requesting Windows Defender exclusion for client folder...', 'info', 'game');
  toast('Please approve the Administrator prompt...', 'info');
  const result = await window.radium?.addDefenderExclusion();
  if (result && result.success) {
    // The backend has recorded the folders; nothing for the page to save.
    syncAvState(activeNetwork, result.excluded !== false, result.dirs);
    toast('Defender exclusion added!', 'ok');
    addLog('Exclusion successfully added to Windows Defender.', 'ok', 'game');
    if (result.movedFrom?.length) {
      addLog(`Took the exclusion off the folder the client was in before: ${result.movedFrom.join(', ')}`, 'info', 'game');
    }

    // Check if we need to proceed to Smart App Control and Steam check and launch
    if (launchAfterExclusion) {
      launchAfterExclusion = false;
      await proceedAfterAvCheck();
    }
  } else {
    const err = result?.error || 'UAC elevation cancelled or failed';
    toast('Failed to add exclusion.', 'error');
    addLog(`Exclusion failed: ${err}`, 'error', 'game');
    launchAfterExclusion = false;
    isGameLaunching = false;
  }
}

/// Take the active network's Defender exclusion back out: exactly the folders
/// the backend has on record, through one administrator prompt. Returns
/// whether it worked.
async function removeAvExclusion() {
  addLog('Requesting Windows Defender exclusion removal for client folder...', 'info', 'game');
  toast('Please approve the Administrator prompt...', 'info');
  const network = activeNetwork;
  const result = await window.radium?.removeDefenderExclusion();
  if (result && result.success) {
    syncAvState(network, false, []);
    toast('Defender exclusion removed!', 'ok');
    addLog('Exclusion successfully removed from Windows Defender.', 'ok', 'game');
    return true;
  }
  const err = result?.error || 'UAC elevation cancelled or failed';
  toast('Failed to remove exclusion.', 'error');
  addLog(`Exclusion removal failed: ${err}`, 'error', 'game');
  return false;
}

$('btnExcludeAvConfirm')?.addEventListener('click', () => {
  // Set display directly to avoid resetting launchAfterExclusion inside hideExcludeAvModal
  hideModal($('excludeAvModal'));
  executeExcludeAv();
});

/// Every folder an antivirus has to leave alone for the active network: the
/// client folder, plus Stella's patch folder (the same pair the Defender
/// exclusion covers; see `exclusion_dirs` in defender.rs).
function avExclusionFolders() {
  const folders = [shownInstallDir()];
  if (activeNetwork === 'stella' && stellaPatchDir) folders.push(stellaPatchDir);
  return folders.filter(Boolean);
}

// Third-Party AV Warning Modal Actions
function showThirdPartyAvModal(thirdPartyAvs) {
  const m = $('thirdPartyAvModal');
  if (!m) return;

  const avNames = $('detectedAvNames');
  if (avNames) {
    // Dedup names defensively (the backend already dedups).
    avNames.textContent = [...new Set(thirdPartyAvs.map(av => av.name))].join(', ');
  }

  const folders = avExclusionFolders();
  const clientPathCode = $('tpClientFolderPath');
  if (clientPathCode) {
    clientPathCode.textContent = folders.join('\n');
  }
  const intro = $('tpFolderIntro');
  if (intro) {
    const label = networkInfo().label;
    intro.textContent = folders.length > 1
      ? `both ${label} folders below (the game, and the patch it needs)`
      : `the ${label} client folder`;
  }

  // The primary button doubles as "continue to launch" (from the Play flow) and
  // "acknowledge" (from the manual Exclude AV button). Only say "Launch Anyway"
  // when a launch is actually pending.
  const anywayBtn = $('btnThirdPartyAvAnyway');
  if (anywayBtn) anywayBtn.textContent = launchAfterExclusion ? 'Launch Anyway' : 'OK, Got It';

  // Reflect the saved "don't warn again on launch" choice.
  const dontWarn = $('tpDontWarnAgain');
  if (dontWarn) dontWarn.checked = config.thirdPartyAvAcknowledged === true;

  showModal(m);
}

function hideThirdPartyAvModal() {
  hideModal($('thirdPartyAvModal'));
  launchAfterExclusion = false; // Reset whenever the modal is hidden
  isGameLaunching = false;
}

$('thirdPartyAvModalClose')?.addEventListener('click', hideThirdPartyAvModal);
$('btnThirdPartyAvCancel')?.addEventListener('click', hideThirdPartyAvModal);

$('btnCopyTpPath')?.addEventListener('click', async () => {
  const folders = avExclusionFolders();
  if (folders.length) {
    try {
      await navigator.clipboard.writeText(folders.join('\n'));
      toast(folders.length > 1 ? 'Folder paths copied!' : 'Client folder path copied!', 'ok');
    } catch (e) {
      toast('Failed to copy path.', 'error');
    }
  }
});

$('btnThirdPartyAvOpenFolder')?.addEventListener('click', async () => {
  const ok = await window.radium?.openClientFolder();
  if (ok) {
    toast('Client folder opened!', 'ok');
  } else {
    toast('Failed to open folder.', 'error');
  }
});

$('btnThirdPartyAvAnyway')?.addEventListener('click', async () => {
  hideModal($('thirdPartyAvModal'));

  // Persist only the "don't warn again on launch" choice. We intentionally do
  // NOT set defenderExcluded — a third-party AV can't be auto-excluded, so the
  // Exclude AV button must stay "Exclude AV" (guidance-only), never flip to
  // "UNExclude AV" and never trigger a Defender removal.
  const dontWarn = $('tpDontWarnAgain');
  config.thirdPartyAvAcknowledged = !!(dontWarn && dontWarn.checked);
  await window.radium?.saveConfig(config);

  addLog(config.thirdPartyAvAcknowledged
    ? 'Third-party AV acknowledged — launch warning disabled.'
    : 'Third-party AV warning dismissed.', 'info', 'game');

  if (launchAfterExclusion) {
    launchAfterExclusion = false;
    await proceedAfterAvCheck();
  }
});

$('thirdPartyAvModal')?.addEventListener('click', (e) => {
  if (e.target === $('thirdPartyAvModal')) {
    hideThirdPartyAvModal();
  }
});

$('btnExcludeAv')?.addEventListener('click', async () => {
  const btn = $('btnExcludeAv');
  if (!btn) return;

  if (avExcluded()) {
    // The flag means a real Defender exclusion now: the backend works it out
    // from the folders it excluded (a third-party antivirus is only ever
    // acknowledged, in thirdPartyAvAcknowledged), so there is one to remove.
    await removeAvExclusion();
  } else {
    // Detect third party AV
    const avs = await window.radium?.detectAntivirus() || [];
    const thirdPartyAvs = avs.filter(av => !av.isDefender);
    if (thirdPartyAvs.length > 0) {
      showThirdPartyAvModal(thirdPartyAvs);
    } else {
      executeExcludeAv();
    }
  }
});

// Play mode configuration
function setModeUI(mode) {
  $('modeScreen')?.classList.toggle('active', mode === 'screen');
  $('modeVR')?.classList.toggle('active',     mode === 'vr');
  const wrap = document.querySelector('.mode-toggle-wrap');
  if (wrap) {
    wrap.classList.toggle('vr-active', mode === 'vr');
  }
}

function updateQsMode() {
  // Mode display placeholder for future quick stats card
}

document.querySelectorAll('.mode-btn').forEach(btn => {
  btn.addEventListener('click', () => {
    playMode = btn.dataset.mode;
    config.playMode = playMode;
    setModeUI(playMode);
    updateQsMode();
    window.radium?.saveConfig(config);
    toast(`Mode: ${playMode.toUpperCase()}`, 'info', 1500);
    addLog(`Play mode set to ${playMode}.`, 'info', 'game');
  });
});

// Periodically ping server and update status labels
async function checkServerStatus(silent = false) {
  // Radium's API host is user-configurable; Vanilla's is fixed. The CDN check
  // is Radium's download host, so it is only meaningful there.
  const network = activeNetwork;
  const isRadium = network === 'radium';
  const apiUrl = isRadium
    ? (config.apiUrl || 'https://api.radie.app/')
    : { vanilla: 'https://api.vanillarec.net', stella: 'https://api.stellaonline.org', receos: 'https://api.rec-eos.com' }[activeNetwork];
  const cdnUrl = 'https://cdn.recroomarchive.org';

  // Immediately show CHECKING... in quick stats while pings are in-flight
  const qsS = $('qsStatus');
  if (qsS) qsS.textContent = 'Checking…';

  if (!silent) addLog('Checking server status...', 'info', 'server');

  // Run both pings in parallel — max wait is 5s instead of 10s
  let apiResult, cdnResult;
  try {
    [apiResult, cdnResult] = await Promise.all([
      window.radium?.pingServer(apiUrl),
      isRadium ? window.radium?.pingServer(cdnUrl) : Promise.resolve(null),
    ]);
  } catch (e) {
    console.error('pingServer error:', e);
  }

  const apiOnline = apiResult?.online ?? false;
  // Tri-state: null means "not applicable to this network", which the bug
  // reporter renders differently from a real offline.
  const cdnOnline = isRadium ? (cdnResult?.online ?? false) : null;
  lastServerStatus = { apiOnline, cdnOnline };
  serverStatusNetwork = network;
  syncTray();

  // Quick stats card on home tab
  // Rec EOS also says when it is closed to all but its testers, or down for
  // maintenance.
  if (qsS) qsS.textContent = apiOnline
    ? (apiResult?.closedBeta ? 'Closed beta' : 'Online')
    : (apiResult?.maintenance ? 'Maintenance' : 'Offline');
  const qscS = $('qsc-status');
  if (qscS) {
    qscS.classList.toggle('online',  apiOnline);
    qscS.classList.toggle('offline', !apiOnline);
  }

  if (!silent) {
    // A server being unreachable is a status, not a launcher error — log it as a
    // warning so genuine errors stay distinct in the log.
    addLog(`API Gateway (${apiUrl}): ${apiOnline ? 'ONLINE' : 'OFFLINE'}`, apiOnline ? 'ok' : 'warn', 'server');
    if (isRadium) {
      addLog(`CDN Server (${cdnUrl}): ${cdnOnline ? 'ONLINE' : 'OFFLINE'}`, cdnOnline ? 'ok' : 'warn', 'server');
    }
  }
}

// Periodically fetch player count and update stats card
/// While Stella's count is still settling, it is asked for again sooner than
/// the minute-long poll.
let playerCountRetry = null;
/// Bumped by every count asked for, so an answer that arrives after a newer
/// question — the network switched while a slow API was being asked — is
/// dropped rather than painted over the newer one.
let playerCountSeq = 0;

async function updatePlayerCount(silent = false) {
  const qsPlayers = $('qsPlayers');
  const qscPlayers = $('qsc-players');
  if (!qsPlayers) return;
  clearTimeout(playerCountRetry);
  const seq = ++playerCountSeq;
  if (!silent) {
    qsPlayers.textContent = 'Loading…';
    addLog('Fetching online player count...', 'info', 'server');
  }

  try {
    const result = await window.radium?.getPlayerCount();
    if (seq !== playerCountSeq) return;
    // Stella isn't counting while the launcher is hidden or minimized: keep
    // the card as it was until the window is back (see stella_api::in_use).
    if (activeNetwork === 'stella' && result?.idle) return;
    // Stella's count comes from its live presence (stella_hub.rs), which has
    // states the other networks' counts don't.
    if (activeNetwork === 'stella' && result && !result.success && (result.counting || result.paused || result.signedOut)) {
      qscPlayers?.classList.remove('online', 'offline');
      if (result.counting) {
        // A floor that rises as players are heard from; settled after a
        // minute, when the "+" goes. Stella reports each player every half
        // minute or so.
        qsPlayers.textContent = result.soFar > 0 ? `${result.soFar}+` : 'Counting…';
        // Not while hidden: the count isn't kept then, and coming back on
        // screen asks again anyway.
        playerCountRetry = setTimeout(() => { if (!document.hidden) updatePlayerCount(true); }, 10000);
      } else if (result.paused) {
        // One sign-in at a time can listen for players, and the game is
        // using it; this comes back when the game closes.
        qsPlayers.textContent = 'Paused';
      } else {
        qsPlayers.textContent = 'Signed out';
      }
      return;
    }
    if (result?.unsupported) {
      // Stella publishes no player count: say so, rather than OFFLINE.
      qsPlayers.textContent = 'Not published';
      qscPlayers?.classList.remove('online', 'offline');
      if (!silent) addLog(`${networkInfo().label} doesn't publish a player count.`, 'info', 'server');
    } else if (result && result.success) {
      qsPlayers.textContent = Number.isFinite(Number(result.count)) ? formatCount(result.count) : result.count;
      if (qscPlayers) {
        qscPlayers.classList.add('online');
        qscPlayers.classList.remove('offline');
      }
      if (!silent) addLog(`Players online: ${result.count}`, 'ok', 'server');
    } else {
      qsPlayers.textContent = 'Unavailable';
      if (qscPlayers) {
        qscPlayers.classList.add('offline');
        qscPlayers.classList.remove('online');
      }
      if (!silent) addLog(`Failed to fetch player count: ${result?.error || 'Unknown error'}`, 'error', 'server');
    }
  } catch (err) {
    if (seq !== playerCountSeq) return;
    qsPlayers.textContent = 'Unavailable';
    if (qscPlayers) {
      qscPlayers.classList.add('offline');
      qscPlayers.classList.remove('online');
    }
    if (!silent) addLog(`Failed to fetch player count: ${err?.message || err}`, 'error', 'server');
  }
}

// Game execution and process monitoring
function setGameRunning(running) {
  if (running && !isGameRunning) gameStartedAt = Date.now();
  isGameRunning = running;
  if (running) {
    isGameLaunching = false;
    revealOnModal = false;
  }
  syncTray();
  const btn = $('btnPlay');
  if (running) {
    btn?.classList.add('running');
    const pt = $('playText'); if (pt) pt.textContent = 'STOP';
    const pi = $('playIcon'); if (pi) pi.textContent = '■';
  } else {
    btn?.classList.remove('running');
    const pt = $('playText'); if (pt) pt.textContent = 'PLAY';
    const pi = $('playIcon'); if (pi) pi.textContent = '▶';
  }
}

// Steam Modal actions
function showSteamModal() {
  showModal($('steamModal'));
}

function hideSteamModal(cancelLaunch = true) {
  hideModal($('steamModal'));
  if (cancelLaunch) isGameLaunching = false;
}

$('steamModalClose')?.addEventListener('click', () => hideSteamModal(true));
$('steamModal')?.addEventListener('click', (e) => {
  if (e.target === $('steamModal')) {
    hideSteamModal(true);
  }
});

// Final gate before launching: make sure the required Rec Room Steam app
// (steam://install/92) is installed. If it already is, this is invisible.
async function executeLaunch() {
  // Codename Gordon is what the older clients lean on for Steam; Stella's
  // 2024 client doesn't need it.
  if (config.disableWarnings !== true && activeNetwork !== 'stella') {
    let installed = true;
    try {
      installed = await window.radium?.checkRequiredSteamApp();
    } catch (e) { /* on error, don't block launch */ }
    if (installed === false) {
      addLog('Required Rec Room Steam app (steam://install/92) is not installed. Prompting user...', 'info', 'game');
      showSteamAppModal();
      return;
    }
  }
  await doLaunch();
}

function showSteamAppModal() {
  showModal($('steamAppModal'));
}
function hideSteamAppModal(cancelLaunch = true) {
  hideModal($('steamAppModal'));
  if (cancelLaunch) isGameLaunching = false;
}
$('steamAppModalClose')?.addEventListener('click', () => hideSteamAppModal(true));
$('steamAppCancelBtn')?.addEventListener('click', () => hideSteamAppModal(true));
$('steamAppModal')?.addEventListener('click', (e) => {
  if (e.target === $('steamAppModal')) hideSteamAppModal(true);
});
$('steamAppInstallBtn')?.addEventListener('click', () => {
  addLog('Opening Steam to install the required app...', 'info', 'game');
  toast('Opening Steam to install…', 'info', 3000);
  window.radium?.openUrl('steam://install/92');
  hideSteamAppModal(true);
});
$('steamAppAnywayBtn')?.addEventListener('click', () => {
  hideSteamAppModal(false);
  doLaunch();
});

/// Where the next launch goes on Stella, used up by that launch: a friend
/// (`{ id, name }`, from their JOIN; with `ask`, one in a private room, who is
/// sent a join request once the game is in) or a room (`{ room, name }`, from
/// the room's PLAY). See joinStellaFriend() and playStellaRoom().
let pendingJoin = null;

async function doLaunch() {
  setGameRunning(true);
  const join = stellaLike() ? pendingJoin : null;
  pendingJoin = null;
  addLog(join ? `Launching game to join ${join.name}...` : 'Launching game...', 'info', 'game');
  toast(join ? `Launching ${networkInfo().label} to join ${join.name}...` : `Launching ${networkInfo().label}...`, 'info', 2000);

  let result = null;
  try {
    // Only what launch_game reads (the shim adds `network`). Spreading the
    // whole config also sent the glass backdrop — up to 1.4 MB — on every PLAY.
    result = await window.radium?.launchGame({
      playMode,
      gameExePath: config.gameExePath || '',
      minimizeOnLaunch: config.minimizeOnLaunch === true,
      closeOnLaunch: config.closeOnLaunch === true,
      joinPlayerId: join?.ask ? null : (join?.id ?? null),
      joinRoomName: join?.room ?? null,
      requestJoinPlayerId: join?.ask ? join.id : null,
    });
  } catch (e) {
    console.error('launchGame error:', e);
    result = { success: false, error: e.toString() };
  }

  isGameLaunching = false;
  if (!result?.success) {
    setGameRunning(false);
    const err = result?.error || 'Unknown error';
    addLog(`Launch failed: ${err}`, 'error', 'game');
    toast(`Launch failed: ${err}`, 'error', 5000);
  } else {
    // .bat launches report no PID (the cmd.exe wrapper's PID is meaningless).
    const pidPart = (result.pid !== null && result.pid !== undefined) ? ` (PID ${result.pid})` : '';
    addLog(`Game running${pidPart} — mode: ${playMode}`, 'ok', 'game');
    toast(`${networkInfo().label} launched in ${playMode.toUpperCase()} mode!`, 'ok');
    // launch_game has already hidden the window (tray mode) or quit; this
    // only logs it. It also used to close the window again a second later,
    // which in tray mode was a second hide, and a second "launcher hidden"
    // for the page to react to.
    if (config.closeOnLaunch === true) {
      addLog(config.runInBackground !== false
        ? 'Launcher set to step aside on game start. Hidden to the tray.'
        : 'Launcher configured to exit on game start. Exiting...', 'info', 'game');
    }
  }
}

$('steamAnywayBtn')?.addEventListener('click', () => {
  hideSteamModal(false);
  executeLaunch();
});

$('steamLaunchBtn')?.addEventListener('click', async () => {
  hideSteamModal(false);
  addLog('Launching Steam...', 'info', 'game');
  toast('Launching Steam...', 'info', 2000);
  window.radium?.openUrl('steam://');
  
  // Wait 3 seconds to let Steam start initializing before starting the game
  addLog('Waiting for Steam to start (3s)...', 'info', 'game');
  setTimeout(() => {
    executeLaunch();
  }, 3000);
});

async function checkSteamAndLaunch() {
  addLog('Checking if Steam is running...', 'info', 'game');
  const steamRunning = await window.radium?.checkSteam();

  if (steamRunning) {
    addLog('Steam is running.', 'ok', 'game');
    executeLaunch();
  } else {
    if (config.disableWarnings === true) {
      addLog('Steam is not running. Warning skipped (disabled by user).', 'info', 'game');
      executeLaunch();
    } else {
      addLog('Steam is not running. Prompting user...', 'info', 'game');
      showSteamModal();
    }
  }
}

async function proceedAfterAvCheck() {
  await checkSacAndLaunch();
}

// Launch-time antivirus check: offer to exclude the client folder from
// Windows Defender (or warn about a third-party AV) before launching, so the
// game's patched files aren't quarantined. Flows into the Smart App Control
// check, then the Steam check, then the actual launch.
async function checkAvAndLaunch() {
  if (config.disableWarnings === true) {
    addLog('AV exclusion check skipped (disabled by user).', 'info', 'game');
    await proceedAfterAvCheck();
    return;
  }

  let avs = [];
  try {
    avs = await window.radium?.detectAntivirus() || [];
  } catch (e) {
    console.error('detectAntivirus error:', e);
  }

  const thirdPartyAvs = avs.filter(av => !av.isDefender);
  if (thirdPartyAvs.length > 0) {
    // Third-party AV can't be auto-excluded — only a manual folder exclusion in
    // the AV itself helps. Warn (with a manual guide) unless the user opted out.
    if (config.thirdPartyAvAcknowledged === true) {
      addLog('Third-party AV present; launch warning suppressed by user.', 'info', 'game');
      await proceedAfterAvCheck();
      return;
    }
    addLog('Third-party antivirus detected. Prompting user...', 'info', 'game');
    launchAfterExclusion = true;
    showThirdPartyAvModal(thirdPartyAvs);
    return;
  }

  // Windows Defender path — this one CAN be auto-excluded.
  if (avExcluded()) {
    await proceedAfterAvCheck();
    return;
  }
  if (avs.some(av => av.isDefender)) {
    addLog('Windows Defender active and folder not excluded. Prompting user...', 'info', 'game');
    launchAfterExclusion = true;
    showExcludeAvModal();
    return;
  }

  // No antivirus detected — nothing to exclude.
  addLog('No antivirus requiring exclusion detected.', 'ok', 'game');
  await proceedAfterAvCheck();
}

// ── Press feedback that survives a quick click ───────────────────────────
// `:active` only lasts while the button is held. A fast click lets go within a
// frame or two, before the press transition has moved at all, so the big
// buttons seemed to ignore the click. `.is-pressed` holds the same press styles
// (every skin lists it beside :active) for at least PRESS_MIN_MS.

const PRESS_MIN_MS = 170;
const PRESSABLE = '.btn-play, .btn-download-big, .room-play-btn';
const pressTimers = new WeakMap();

function holdPressed(btn, heldSince) {
  clearTimeout(pressTimers.get(btn));
  btn.classList.add('is-pressed');
  return () => {
    const left = Math.max(0, PRESS_MIN_MS - (performance.now() - heldSince));
    pressTimers.set(btn, setTimeout(() => btn.classList.remove('is-pressed'), left));
  };
}

document.addEventListener('pointerdown', (e) => {
  if (e.button !== 0) return;
  const btn = e.target.closest?.(PRESSABLE);
  if (!btn || btn.disabled) return;
  const release = holdPressed(btn, performance.now());
  const done = () => {
    window.removeEventListener('pointerup', done);
    window.removeEventListener('pointercancel', done);
    release();
  };
  window.addEventListener('pointerup', done);
  window.addEventListener('pointercancel', done);
});

// Enter / Space fire `click` with no pointer at all (detail is 0).
document.addEventListener('click', (e) => {
  if (e.detail !== 0) return;
  const btn = e.target.closest?.(PRESSABLE);
  if (btn && !btn.disabled) holdPressed(btn, performance.now())();
});

$('btnPlay')?.addEventListener('click', async () => {
  if (isGameRunning) {
    showStopGameModal();
    return;
  }
  if (isGameLaunching || !isInstalled) return;
  if (stellaUpdateNeeded()) return;
  isGameLaunching = true;
  pendingJoin = null;

  // Full pre-launch safety chain: AV exclusion → Smart App Control → Steam →
  // launch.
  await checkAvAndLaunch();
});

/// Start Stella with `target` as where it goes (see pendingJoin), through the
/// same checks PLAY runs. `what` names it in the messages.
function launchStellaInto(target, what) {
  const label = activeNetwork === 'receos' ? 'Rec EOS' : 'Stella';
  if (isGameRunning) {
    toast(`${label} is already running. Go to ${what} from the game.`, 'info', 5000);
    return;
  }
  if (isGameLaunching) return;
  if (!isInstalled) {
    toast(`Download ${label} first, from Home.`, 'info');
    return;
  }
  if (stellaUpdateNeeded()) {
    toast(`${networkInfo().patchLabel || label} needs an update first. Press UPDATE on Home.`, 'info', 5000);
    return;
  }
  isGameLaunching = true;
  pendingJoin = target;
  checkAvAndLaunch();
}

/// A room's PLAY on Stella: start the game in that room (`+roomname:`).
function playStellaRoom(room) {
  const name = String(room?.Name || '').replace(/^\^/, '');
  if (!name) return;
  launchStellaInto({ room: name, name: `^${name}` }, `^${name}`);
}

/// A friend's JOIN: start Stella straight into their room, through the same
/// checks PLAY runs. The game does the joining (`+join:<id>`, as Steam's "Join
/// Game" does), so whether they can be joined is the server's call.
///
/// One thing can be known first: whether the copy of the room they are in is
/// full. Stella then answers the game with "Room Full" and it lands the player
/// in their dorm, so that is asked before starting (see stella_join_check) and,
/// when it is full, a warning offers their own copy of the room instead.
let joinCheckBusy = false;
async function joinStellaFriend(friend) {
  if (activeNetwork !== 'stella' || !friend?.id) return;
  const name = friend.displayName || friend.userName || 'your friend';
  const go = () => launchStellaInto({ id: friend.id, name }, name);
  // Invited: a plain JOIN gets in, private room or not (see stellaInvites).
  if (stellaInvitedBy(friend.id)) {
    go();
    return;
  }
  if (friend.private) {
    askToJoinStellaFriend(friend, name);
    return;
  }
  // Nothing will start (launchStellaInto says why), so there's nothing to check.
  if (isGameRunning || isGameLaunching || !isInstalled || stellaUpdateNeeded() || joinCheckBusy) {
    if (!joinCheckBusy) go();
    return;
  }
  joinCheckBusy = true;
  let check = null;
  try {
    // A slow answer isn't worth holding the JOIN for: it goes ahead unchecked.
    check = await Promise.race([
      window.radium.stellaJoinCheck(friend.id, friend.roomId),
      sleep(5000).then(() => null)
    ]);
  } catch (e) {}
  joinCheckBusy = false;
  if (activeNetwork !== 'stella') return;
  if (check?.known && check.full) {
    showRoomFullModal(friend, name, check);
    return;
  }
  go();
}

/// A friend in a private copy of a room can't be joined outright: Stella
/// answers the game "Room does not exist". So JOIN asks them instead, as the
/// game's "Request to Join" does, and their game answers with an invite.
/// With the game closed the invite comes to the launcher's own hub
/// connection and pops up with a JOIN of its own (showStellaInvite); once
/// invited, a plain JOIN gets in. If the hub can't be reached, nothing could
/// hear the invite, so Stella starts into the dorm and the launcher asks once
/// the game is in (stella_api::request_join_after_launch, answered as
/// onStellaJoinRequest), and the invite shows up in the game. With the game
/// already open, the request goes now and the invite shows up there.
async function askToJoinStellaFriend(friend, name) {
  if (isGameRunning) {
    try {
      await window.radium.stellaRequestJoin(friend.id);
      stellaJoinAsked(name);
    } catch (e) {
      stellaJoinNotAsked(name, e);
    }
    return;
  }
  // The game couldn't start to use an invite (launchStellaInto says why).
  if (isGameLaunching || !isInstalled || stellaUpdateNeeded()) {
    launchStellaInto({ id: friend.id, name }, name);
    return;
  }
  if (joinCheckBusy) return;
  joinCheckBusy = true;
  let res = null;
  let err = null;
  try {
    res = await window.radium.stellaAskToJoin(friend.id);
  } catch (e) {
    err = e;
  }
  joinCheckBusy = false;
  if (activeNetwork !== 'stella') return;
  if (err) {
    stellaJoinNotAsked(name, err);
    return;
  }
  if (res?.sent) {
    addLog(`Sent ${name} a join request.`, 'ok', 'game');
    toast(`Asked ${name} to let you in. Their invite will pop up here.`, 'ok', 6000);
    stellaAsked.set(Number(friend.id), Date.now());
    setTimeout(() => {
      if (!stellaAsked.has(Number(friend.id)) || stellaInvitedBy(friend.id)) return;
      stellaAsked.delete(Number(friend.id));
      toast(`${name} hasn't answered your join request.`, 'info', 5000);
    }, ASK_ANSWER_MS);
    return;
  }
  // A launcher that quits as the game starts can't wait for it to land; and
  // without an invite, `+join:` would only meet "Room does not exist".
  const quitsOnLaunch = config.closeOnLaunch === true && config.runInBackground === false;
  launchStellaInto(quitsOnLaunch ? { name } : { id: friend.id, name, ask: true }, name);
}

// Invites ─────────────────────────────────────────────────────────────────

/// When each player last invited the signed-in one, by id. An invite lets a
/// plain JOIN into that friend's room even when it's private (the user found,
/// 2026-10-04). How long one lasts on Stella isn't known, so it counts for
/// INVITE_FRESH_MS; past that, JOIN asks again.
const stellaInvites = new Map();
const INVITE_FRESH_MS = 10 * 60 * 1000;
/// Join requests waiting for an answer, by friend id, and how long one may.
const stellaAsked = new Map();
const ASK_ANSWER_MS = 45 * 1000;

/// Invites in Stella's notification list count from when they were sent, so
/// one that came while the launcher wasn't listening still lets JOIN in.
function noteStellaInvites(items) {
  for (const n of items) {
    if (n.type !== 6 || !n.senderId) continue;
    const at = Date.parse(n.sentTime || '');
    const id = Number(n.senderId);
    if (!isNaN(at) && at > (stellaInvites.get(id) || 0)) stellaInvites.set(id, at);
  }
}

function stellaInvitedBy(id) {
  const at = stellaInvites.get(Number(id));
  return at != null && Date.now() - at < INVITE_FRESH_MS;
}

/// An invite heard on the hub (with the game closed: the game hears its own).
function showStellaInvite(invite) {
  const id = Number(invite?.fromPlayerId);
  if (!id || activeNetwork !== 'stella') return;
  stellaInvites.set(id, Date.now());
  const answered = stellaAsked.delete(id);
  const friend = stellaFriendsById.get(id);
  const name = friend?.displayName || friend?.userName || 'A player';
  const room = String(invite.roomName || '');
  const where = !room ? 'their room' : /'s Dorm$/.test(room) ? room : `^${room}`;
  addLog(`${name} invited you to join them in ${where}.`, 'info', 'game');
  // Shown whatever the pop-up setting when it answers the player's own JOIN:
  // they are waiting for it.
  const prefs = notifPrefs();
  if (!prefs.popups && !answered) {
    toast(`${name} invited you to join them in ${where}. Press JOIN on their row to go.`, 'info', 8000);
    return;
  }
  window.radium.desktopNotify([{
    id: `stella-invite:${id}`,
    sender: null,
    icon: null,
    avatar: friend?.AvatarUrl ? thumbSrc(friend.AvatarUrl, avatarWidth(40)) : placeholderAvatar(),
    app: 'Stella',
    parts: [{ t: name, b: true }, { t: ` invited you to join them in ${where}.` }],
    action: 'JOIN',
    style: notifPopStyle(),
  }]).catch(() => {});
  if (prefs.sound) playNotifChime();
  refreshStellaFriends();
}
window.radium?.onStellaInvite?.(showStellaInvite);

/// JOIN on an invite's pop-up.
async function joinFromStellaInvite(id) {
  if (activeNetwork !== 'stella') {
    await setNetwork('stella');
    if (activeNetwork !== 'stella') return;
  }
  const friend = stellaFriendsById.get(Number(id));
  const name = friend?.displayName || friend?.userName || 'your friend';
  launchStellaInto({ id: Number(id), name }, name);
}

function stellaJoinAsked(name) {
  addLog(`Sent ${name} a join request.`, 'ok', 'game');
  toast(`Asked ${name} to let you in. Their invite will show up in the game.`, 'ok', 6000);
}

function stellaJoinNotAsked(name, err) {
  const why = String(err).trim().replace(/([^.!?])$/, '$1.');
  addLog(`Couldn't send ${name} a join request: ${why}`, 'error', 'game');
  toast(`Couldn't ask ${name} to let you in: ${why} You can still ask from the game's friends list.`, 'error', 7000);
}

window.radium?.onStellaJoinRequest?.((res) => {
  const friend = stellaFriendsById.get(Number(res?.playerId));
  const name = friend?.displayName || friend?.userName || 'your friend';
  if (res?.ok) stellaJoinAsked(name);
  else stellaJoinNotAsked(name, res?.error || 'Unknown error.');
});

/// The friend JOIN is waiting on this warning: `{ friend, name, room }`.
let roomFullJoin = null;

function showRoomFullModal(friend, name, check) {
  const room = String(friend.roomName || '').replace(/^[\^@]/, '');
  // Only a plain room name can be started into (`+roomname:`), so a dorm
  // ("abod124's Dorm") has no "play it yourself" button.
  const playable = /^[A-Za-z0-9_-]+$/.test(room);
  roomFullJoin = { friend, name, room: playable ? room : '' };
  $('roomFullTitle').textContent = `${name}'s room is full`;
  $('roomFullText').textContent = room
    ? `Their ${playable ? `^${room} room` : room} already has ${check.players} players, its limit. Stella will turn you away and send you to your dorm.`
    : `The room they're in already has ${check.players} players, its limit. Stella will turn you away and send you to your dorm.`;
  const play = $('roomFullPlayBtn');
  play.hidden = !playable;
  if (playable) play.textContent = `Play ^${room}`;
  play.setAttribute('aria-label', playable ? `Start Stella in your own ^${room} room` : '');
  showModal($('roomFullModal'));
}

function hideRoomFullModal() {
  roomFullJoin = null;
  hideModal($('roomFullModal'));
}
$('roomFullModalClose')?.addEventListener('click', hideRoomFullModal);
$('roomFullCancelBtn')?.addEventListener('click', hideRoomFullModal);
$('roomFullModal')?.addEventListener('click', (e) => {
  if (e.target === $('roomFullModal')) hideRoomFullModal();
});
$('roomFullAnywayBtn')?.addEventListener('click', () => {
  const job = roomFullJoin;
  hideRoomFullModal();
  if (job) launchStellaInto({ id: job.friend.id, name: job.name }, job.name);
});
$('roomFullPlayBtn')?.addEventListener('click', () => {
  const job = roomFullJoin;
  hideRoomFullModal();
  if (job?.room) playStellaRoom({ Name: job.room });
});

// Tray menu ────────────────────────────────────────────────────────────────
// The tray icon's panel (traymenu.js): the launcher at a glance — the network,
// its servers and players, PLAY with whatever download or update is under
// way, Stella friends to join — and the pages, the network and Quit. What is
// picked there comes back through onTrayAction below.
//
// The tray's Play launches without opening the window, like Steam's game
// entries. If the launch then needs the user (a warning, the Steam check), the
// first dialog it opens brings the window up.

/// Tell the tray panel what to show and what the current skin looks like.
///
/// Many things call this, often several in one go (a network switch touches
/// most of the page, a download reports several times a second), so the push
/// waits for the end of the current task and goes once, and only when
/// something in it has changed. A microtask rather than a timer: a hidden
/// window's timers are throttled to a second, and a network picked in the
/// panel would wait that long for its Play label.
function syncTray() {
  if (trayQueued) return;
  trayQueued = true;
  queueMicrotask(() => {
    trayQueued = false;
    const style = trayMenuStyle();
    const info = trayInfo();
    const key = JSON.stringify([activeNetwork, isGameRunning, style, info]);
    if (key === traySent) return;
    traySent = key;
    window.radium?.setTrayState(activeNetwork, isGameRunning, style, info).catch(() => { traySent = ''; });
  });
}

/// What the panel shows, read from the same state Home is drawn from.
function trayInfo() {
  return {
    server: trayServer(),
    players: trayPlayers(),
    game: trayGame(),
    mode: playMode === 'vr' ? 'vr' : 'screen',
    // Pop-ups are Vanilla's and Stella's notifications; Radium has none.
    popups: activeNetwork === 'radium' ? null : getToggle('tgl-notifPopups'),
    friends: trayFriends(),
    // Whether Stella shows friends at all (Settings: Friends → Hidden), for
    // sizing the panel on any network: Stella's is the tallest.
    stellaFriends: friendsView() !== 'hidden',
  };
}

/// 'online', 'offline' or 'checking', for the network on screen. A re-check
/// keeps the last answer rather than flickering to "checking" every minute.
function trayServer() {
  if (serverStatusNetwork !== activeNetwork || lastServerStatus.apiOnline == null) return 'checking';
  return lastServerStatus.apiOnline ? 'online' : 'offline';
}

/// The player count as Home's status strip shows it: `{ count }` for a
/// number, `{ note }` for anything else ('' when there is nothing to say).
const TRAY_PLAYER_NOTES = {
  '': '', '—': '', 'Not published': '',
  'Loading…': 'Counting players…',
  'Counting…': 'Counting players…',
  'Paused': 'Count paused while you play',
  'Signed out': 'Sign in to count players',
  'Unavailable': 'Player count unavailable',
};
function trayPlayers() {
  const text = ($('qsPlayers')?.textContent || '').trim();
  const counting = text === 'Counting…' || text.endsWith('+');
  if (!counting && !Object.hasOwn(TRAY_PLAYER_NOTES, text)) {
    trayLastCount = { network: activeNetwork, text, at: Date.now() };
  }
  // Stella counts its players afresh each time its friends connection comes
  // back (opening the panel brings it back; see onTrayPanel), from nothing up
  // over a minute. Meanwhile the last full count, if it is recent, says more
  // than "Counting…" or a floor below it, so the panel keeps it until the
  // new count passes it.
  const last = trayLastCount;
  if (counting && last?.network === activeNetwork && Date.now() - last.at < 10 * 60_000) {
    const floor = Number(text.replace(/\D/g, '')) || 0;
    const known = Number(last.text.replace(/\D/g, ''));
    if (Number.isFinite(known) && known > floor) return { count: last.text };
  }
  return Object.hasOwn(TRAY_PLAYER_NOTES, text) ? { note: TRAY_PLAYER_NOTES[text] } : { count: text };
}

/// The last player count the panel showed that wasn't still counting.
let trayLastCount = null;

/// What PLAY is right now, and the progress of a download or update.
function trayGame() {
  const text = (id) => ($(id)?.textContent || '').trim();
  if (isGameRunning) return { state: 'stop', startedAt: gameStartedAt };
  if (networkInfo().hasPatch && stellaPatchUpdating) {
    const pct = parseFloat($('stellaUpdateFill')?.style.width);
    return { state: 'progress', title: `Updating ${networkInfo().patchLabel || networkInfo().label}`, pct: Number.isFinite(pct) ? Math.round(pct) : 0, detail: '', canPause: false };
  }
  if (isDownloading || isPaused || isCancelling) {
    const fill = $('dlBarFill');
    const pct = fill?.classList.contains('indeterminate') ? -1 : Number(fill?.dataset.pct || 0);
    const detail = isPaused || isCancelling ? '' : [text('dlSpeedLabel'), text('dlEtaLabel')].filter(v => v && v !== '—').join(' · ');
    const title = isCancelling ? 'Cancelling…' : isPaused ? 'Paused' : text('dlPhaseLabel').replace(/\.\.\.$/, '…');
    const pauseBtn = $('btnPauseDl');
    return {
      state: 'progress', title: title || 'Downloading…', pct: Number.isFinite(pct) ? pct : 0, detail,
      paused: isPaused, canPause: !isCancelling && !!pauseBtn && pauseBtn.style.display !== 'none',
    };
  }
  if (!isInstalled) return { state: 'download', repair: needsRepair };
  if (stellaUpdateNeeded()) return { state: 'update' };
  if (isGameLaunching) return { state: 'starting' };
  return { state: 'play' };
}

/// Stella only, where Home shows them: up to three friends, the online ones
/// first, or a note saying why there are none.
const TRAY_FRIEND_ROWS = 3;
function trayFriends() {
  if (activeNetwork !== 'stella' || friendsView() === 'hidden') return null;
  if (!stellaPlayer) {
    return { note: stellaAuthPending ? 'Signing in…'
      : stellaAuthError === STELLA_NO_ACCOUNT ? 'Play Stella once to make your account.'
      : 'Log in to Stella to see your friends.' };
  }
  const res = stellaFriendsRes;
  if (!res || (res.success && !res.loaded)) return { note: 'Loading friends…' };
  if (!res.success) return { note: res.error || "Couldn't load your friends." };
  const friends = res.friends || [];
  if (!friends.length) return { note: "No friends yet. Add some in game and they'll show up here." };
  const online = friends.filter(f => f.status === 'online');
  const rows = [...online, ...friends.filter(f => f.status !== 'online')].slice(0, TRAY_FRIEND_ROWS);
  const summary = res.paused ? 'Paused while you play'
    : !res.connected && res.error ? 'Reconnecting…'
    : `${online.length} of ${friends.length} online`;
  return {
    summary,
    rows: rows.map(f => {
      const where = friendWhere(f);
      return {
        id: String(f.id),
        name: f.displayName || f.userName || 'Player',
        where: where.text,
        locked: where.locked,
        status: f.status === 'online' ? 'online' : f.status === 'offline' ? 'offline' : 'unknown',
        favorite: !!f.favorite,
        avatar: f.AvatarUrl ? thumbSrc(f.AvatarUrl, avatarWidth(28)) : '',
      };
    }),
  };
}

// The panel shows what Home shows, and these are the parts of Home that
// change on their own — a ping, a count, a download's progress, a friend
// coming online — so a change to any of them is a change to the panel.
// Everything else that matters (the network, the skin, the game starting)
// already calls syncTray() itself.
{
  const watch = new MutationObserver(() => syncTray());
  for (const id of ['qsPlayers', 'qsc-status', 'dlPhaseLabel', 'dlSpeedLabel', 'dlEtaLabel', 'dlBarFill',
                    'btnPauseDl', 'stellaUpdateFill', 'stellaFriendsList', 'modeScreen', 'tgl-notifPopups']) {
    const el = $(id);
    if (el) watch.observe(el, { childList: true, characterData: true, subtree: true, attributes: true, attributeFilter: ['class', 'style', 'data-pct'] });
  }
  // client-installed, stella-update-needed and friends-view-* live here.
  watch.observe(document.body, { attributes: true, attributeFilter: ['class'] });
}

/// Only the inset layers of a `box-shadow`: the bevels the retro skins draw
/// their edges with. The drop shadow is left out of the tray menu and the
/// notification pop-up, which sit straight on the desktop.
function insetShadows(shadow) {
  const layers = (shadow || '').split(/,(?![^(]*\))/).map(s => s.trim()).filter(s => /\binset\b/.test(s));
  return layers.length ? layers.join(', ') : 'none';
}

/// The skin's own controls, for the panel's buttons, switches and progress
/// bar: the design tokens every skin sets on <body> (skins/00-tokens.css),
/// passed on as they are. Liquid Glass sets none of them — its look is not
/// token-driven — so under glass the list comes back empty and traymenu.css
/// draws glass itself.
const TRAY_TOKENS = [
  'accent', 'on-accent', 'muted', 'line', 'ok', 'err', 'dur', 'ease',
  'btn-bg', 'btn-fg', 'btn-border', 'btn-radius', 'btn-shadow', 'btn-tshadow', 'btn-font', 'btn-weight',
  'btn-hover-bg', 'btn-hover-fg', 'btn-hover-border', 'btn-hover-shadow',
  'btn-active-bg', 'btn-active-fg', 'btn-active-border', 'btn-active-shadow', 'btn-disabled-fg',
  'pri-bg', 'pri-fg', 'pri-border', 'pri-radius', 'pri-shadow', 'pri-tshadow', 'pri-font', 'pri-weight',
  'pri-hover-bg', 'pri-hover-fg', 'pri-hover-border', 'pri-hover-shadow',
  'pri-active-bg', 'pri-active-border', 'pri-active-shadow',
  'run-bg', 'run-fg', 'run-border', 'run-shadow', 'run-hover-bg',
  'seg-bg', 'seg-border', 'seg-radius', 'seg-shadow', 'seg-pad', 'seg-fg', 'seg-divider', 'seg-btn-radius',
  'seg-active-bg', 'seg-active-fg', 'seg-active-shadow',
  'tgl-w', 'tgl-h', 'tgl-bw', 'tgl-radius', 'tgl-bg', 'tgl-border', 'tgl-shadow', 'tgl-on-bg', 'tgl-on-border',
  'knob', 'knob-inset', 'knob-radius', 'knob-bg', 'knob-border', 'knob-shadow', 'knob-on-bg',
  'bar-h', 'bar-bg', 'bar-border', 'bar-radius', 'bar-shadow', 'bar-pad',
  'bar-fill', 'bar-fill-size', 'bar-fill-radius', 'bar-fill-shadow',
];

/// What a menu looks like under the current skin, for the tray menu's window
/// to copy (traymenu.css). Measured from the Manage Client menu, which is in
/// the page, hidden, whatever tab is showing — so it follows every skin and
/// Liquid Glass. Hover can't be measured, so it comes from the skin's tokens,
/// with a tint of the text colour where a look sets none.
function trayMenuStyle() {
  const menu = $('manageMenu');
  const row = menu?.querySelector('.manage-item:not(.danger)');
  const sep = menu?.querySelector('.manage-sep');
  if (!menu || !row) return null;
  const box = getComputedStyle(menu);
  const item = getComputedStyle(row);
  const token = (name, fallback) => item.getPropertyValue(name).trim() || fallback;
  const side = (s, edge) => `${s[`border${edge}Width`]} ${s[`border${edge}Style`]} ${s[`border${edge}Color`]}`;
  const fg = item.color;
  const style = {
    'bg': box.backgroundColor,
    'bg-image': box.backgroundImage,
    'border': side(box, 'Top'),
    'radius': box.borderTopLeftRadius,
    'shadow': insetShadows(box.boxShadow),
    'pad': box.padding,
    'fg': fg,
    'font': item.fontFamily,
    'size': item.fontSize,
    'weight': item.fontWeight,
    'item-pad': item.padding,
    'item-radius': item.borderTopLeftRadius,
    'hover-bg': token('--item-hover-bg', `color-mix(in srgb, ${fg} 14%, transparent)`),
    'hover-fg': token('--item-hover-fg', fg),
    'sel-weight': token('--item-sel-weight', '700'),
    'sel-bg': token('--item-sel-bg', 'transparent'),
    'sel-fg': token('--item-sel-fg', fg),
    // Opens the way the skin's own menus do: most retro skins set no
    // entrance (`--menu-in: none`) and pop their menus up instantly, and XP
    // to 7 fade theirs in. Windows 98's unroll is left out: it unrolls from
    // the edge a menu hangs off, and the tray menu hangs off no edge.
    motion: document.body.classList.contains('animations-enabled')
      && !/^(none|sk-fade-in|sk-unroll)\b/.test(token('--menu-in', '')),
    fade: document.body.classList.contains('animations-enabled')
      && token('--menu-in', '').startsWith('sk-fade-in'),
    glass: document.body.classList.contains('glass-enabled'),
  };
  if (sep) {
    const s = getComputedStyle(sep);
    Object.assign(style, {
      'sep-top': side(s, 'Top'),
      'sep-bottom': side(s, 'Bottom'),
      'sep-shadow': s.boxShadow,
      'sep-bg': s.backgroundColor,
      'sep-height': s.height,
      'sep-margin': s.margin,
    });
  }
  // PLAY's label as Home's hero sets it. Not a token: the XP skins' PLAY is
  // the Start button, in bold italic, by a rule of their own (.play-text).
  const playText = $('playText');
  if (playText) {
    const p = getComputedStyle(playText);
    Object.assign(style, {
      'play-font': p.fontFamily,
      'play-style': p.fontStyle,
      'play-weight': p.fontWeight,
      'play-ls': p.letterSpacing,
    });
  }
  const skin = getComputedStyle(document.body);
  // The system's own icons the launcher draws under this skin
  // (skins/12-system-icons.css), named by their folder, for the panel to use
  // the same ones; none for the modern skins and Liquid Glass.
  style.icons = (/assets\/icons\/([a-z0-9]+)\//.exec(skin.getPropertyValue('--ico-home')) || [])[1] || '';
  style.tokens = {};
  for (const name of TRAY_TOKENS) {
    const value = skin.getPropertyValue(`--${name}`).trim();
    if (value) style.tokens[name] = value;
  }
  // The modern skins draw their segmented control for Home's dark hero scrim:
  // translucent white, which vanishes on a light skin's panel. Those colours
  // are left out, and traymenu.css takes them from the menu's own text and
  // the skin's primary button instead.
  if (/^rgba\(255, 255, 255, 0?\.\d+\)$/.test(style.tokens['seg-bg'] || '')) {
    for (const name of ['seg-bg', 'seg-fg', 'seg-border', 'seg-active-bg', 'seg-active-fg']) delete style.tokens[name];
  }
  return style;
}

/// Start the reveal-on-dialog window for a launch begun from the tray: the
/// window stays hidden unless the launch needs the user, and then the first
/// dialog brings it up (showModal). A launch that fails without a dialog
/// must not leave a later, unrelated dialog popping the window open, hence
/// the minute's limit.
function revealOnNextModal() {
  revealOnModal = true;
  clearTimeout(revealOnModalTimer);
  revealOnModalTimer = setTimeout(() => { revealOnModal = false; }, 60000);
}

function playFromTray() {
  const btn = $('btnPlay');
  // Anything but a plain launch needs the window: the stop confirmation, a
  // missing install, a download in the way.
  if (isGameRunning || !isInstalled || isDownloading || isPaused || !btn || btn.disabled
      || stellaUpdateNeeded()) {
    window.radium?.showLauncher();
    switchTab('home');
    if (isGameRunning) btn?.click();
    return;
  }
  if (isGameLaunching) return;
  revealOnNextModal();
  btn.click();
}

/// A friend's JOIN in the tray panel. A plain join starts the game with the
/// window left hidden, as the panel's PLAY does. Anything that answers in a
/// toast instead — a private room's "asked them", the game already running,
/// nothing installed — brings the window up first, since a toast behind a
/// hidden window says nothing.
function joinFromTray(id) {
  const friend = stellaFriendsById.get(Number(id));
  if (activeNetwork !== 'stella' || !friend) return;
  if (friend.private || isGameRunning || isGameLaunching || !isInstalled || stellaUpdateNeeded()) {
    window.radium?.showLauncher();
  } else {
    pendingJoin = null;
    revealOnNextModal();
  }
  joinStellaFriend(friend);
}

window.radium?.onTrayAction?.(async ({ action, value }) => {
  if (action === 'play') { pendingJoin = null; playFromTray(); }
  else if (action === 'tab') switchTab(value);
  else if (action === 'network') {
    // Switching network from the tray leaves the window where it is — the menu
    // treats those rows as a selection and stays open. The exception is a
    // switch setNetwork() refuses (mid-download, or with the game running):
    // it says why in a toast, which is no use behind a hidden window, so that
    // is the one case worth bringing the launcher forward for.
    await setNetwork(value);
    if (activeNetwork !== value) window.radium?.showLauncher();
  }
  // The panel's Screen / VR switch, as the hero's.
  else if (action === 'mode') {
    if ((value === 'screen' || value === 'vr') && playMode !== value) $(value === 'vr' ? 'modeVR' : 'modeScreen')?.click();
  }
  // Settings → Notifications → Show pop-ups.
  else if (action === 'popups') {
    if (getToggle('tgl-notifPopups') !== (value === 'on')) $('tgl-notifPopups')?.click();
  }
  // A download's Pause / Resume, and Stella's patch UPDATE: all carry on
  // in the background, so the panel stays up to show it.
  else if (action === 'dl') {
    const pause = $('btnPauseDl');
    if (value === 'update') {
      if (stellaUpdateNeeded() && !stellaPatchUpdating) $('btnStellaUpdate')?.click();
    } else if ((value === 'pause' && !isPaused) || (value === 'resume' && isPaused)) {
      if (pause && pause.style.display !== 'none') pause.click();
    }
  }
  // DOWNLOAD may ask where to put the game; the backend has already brought
  // the window up for it.
  else if (action === 'download') {
    switchTab('home');
    $('btnDownload')?.click();
  }
  else if (action === 'join') joinFromTray(value);
  else if (action === 'friend') {
    const friend = stellaFriendsById.get(Number(value));
    if (friend) openFriendProfile(friend);
  }
  else if (action === 'friends') switchTab(friendsView() === 'tab' ? 'friends' : 'home');
});

/// When what the tray panel shows was last brought up to date for it.
let trayRefreshedAt = 0;

/// The tray panel opening or closing. Hidden in the tray, the launcher checks
/// its servers and player count only once a minute, and Stella not at all
/// (its sign-in and live friends wait for the launcher to be looked at; see
/// syncStellaInUse) — so the panel opened on whatever was last seen. Opening
/// it now counts as looking: the servers, the player count and Stella's
/// friends are asked for at once, and the panel redraws as the answers land.
/// Not more than every ten seconds, for a panel opened and closed in a row.
window.radium?.onTrayPanel?.((open) => {
  trayPanelOpen = open;
  syncStellaInUse();
  if (!open || Date.now() - trayRefreshedAt < 10_000) return;
  trayRefreshedAt = Date.now();
  checkServerStatus(true);
  updatePlayerCount(true);
  if (activeNetwork !== 'stella') return;
  // As resumeStella: only a sign-in that was waiting for the launcher to be
  // looked at (or for Steam) is tried again.
  if (!stellaPlayer && ['', STELLA_NOT_IN_USE, STEAM_NOT_RUNNING].includes(stellaAuthError)) refreshStellaAuth();
  refreshStellaFriends();
});

function showSacModal() {
  showModal($('sacModal'));
}

function hideSacModal(cancelLaunch = true) {
  hideModal($('sacModal'));
  if (cancelLaunch) isGameLaunching = false;
}

$('sacModalClose')?.addEventListener('click', () => hideSacModal(true));
$('sacAnywayBtn')?.addEventListener('click', async () => {
  hideSacModal(false);
  sacWarnedThisSession = true;
  await checkSteamAndLaunch();
});
$('sacSettingsBtn')?.addEventListener('click', async () => {
  hideSacModal(false);
  sacWarnedThisSession = true;
  window.radium?.openUrl('windowsdefender://appbrowser');
  await checkSteamAndLaunch();
});
$('sacModal')?.addEventListener('click', (e) => {
  if (e.target === $('sacModal')) {
    hideSacModal(true);
  }
});

async function checkSacAndLaunch() {
  addLog('Checking Smart App Control status...', 'info', 'game');
  const sac = await window.radium?.checkSmartAppControl();
  if (sac && sac.enabled && !sacWarnedThisSession) {
    if (config.disableWarnings === true) {
      addLog('Smart App Control is active. Warning skipped (disabled by user).', 'info', 'game');
      await checkSteamAndLaunch();
    } else {
      addLog('Smart App Control is active. Prompting user...', 'info', 'game');
      showSacModal();
    }
  } else {
    if (sac && sac.enabled) {
      addLog('Smart App Control is active (previously acknowledged this session).', 'info', 'game');
    } else {
      addLog('Smart App Control is not active.', 'ok', 'game');
    }
    await checkSteamAndLaunch();
  }
}


window.radium?.onGameState((data) => {
  if (data.running === false) {
    setGameRunning(false);
    const code = data.exitCode !== undefined ? ` (exit ${data.exitCode})` : '';
    addLog(`Game closed${code}`, 'info', 'game');
    toast('Game closed.', 'info', 2000);
    stellaRecheckAfterGame();
  } else if (data.running === true) {
    setGameRunning(true);
  }
  if (data.error) {
    addLog(`Error: ${data.error}`, 'error', 'game');
    toast(`Error: ${data.error}`, 'error', 4000);
  }
});

window.radium?.onDownloadProgress(updateDlProgress);

// Auto-update: check for a newer launcher release on GitHub
let updateInfo = null;

function showUpdateModal(info) {
  updateInfo = info;
  const ucv = $('updateCurrentVer'); if (ucv) ucv.textContent = `v${info.currentVersion}`;
  const ulv = $('updateLatestVer'); if (ulv) ulv.textContent = info.latestVersion;
  const un = $('updateNotes'); if (un) un.textContent = info.releaseNotes || '(No release notes provided.)';
  const status = $('updateStatus');
  if (status) status.style.display = 'none';
  const nowBtn = $('updateNowBtn');
  // No installer to run from here (a release without one, or without its
  // signature): the button opens the release page instead, and says so.
  if (nowBtn) { nowBtn.disabled = false; nowBtn.textContent = info.downloadUrl ? '⬇ Update Now' : 'Open Release Page'; }
  if (info.signed === false) {
    addLog(`${info.latestVersion} was published without its signature, so the launcher won't install it itself. Its release page has the installer.`, 'warn', 'update');
  }
  showModal($('updateModal'));
}

function hideUpdateModal() {
  hideModal($('updateModal'));
}

$('updateModalClose')?.addEventListener('click', hideUpdateModal);
$('updateLaterBtn')?.addEventListener('click', hideUpdateModal);

$('updateNowBtn')?.addEventListener('click', async () => {
  if (!updateInfo?.downloadUrl) {
    // No direct download — open the release page in browser
    window.radium?.openUrl(updateInfo?.releaseUrl || 'https://github.com/abod124-sudo/Radium-Launcher/releases/latest');
    hideUpdateModal();
    return;
  }
  const nowBtn = $('updateNowBtn');
  const status = $('updateStatus');
  if (nowBtn) { nowBtn.disabled = true; nowBtn.textContent = '⬇ Downloading...'; }
  if (status) { status.style.display = 'block'; status.style.color = ''; status.textContent = 'Downloading update, please wait...'; }
  addLog(`Downloading update ${updateInfo.latestVersion}...`, 'info', 'update');

  // Desktop shortcut placement is always on now — the opt-out checkbox was removed.
  // Every failure comes back as a rejection (a bad digest, a network error), so
  // it is caught here: left to reject, the button stayed on "Downloading..."
  // for good.
  let result;
  try {
    result = await window.radium?.downloadUpdate(updateInfo.downloadUrl, true);
  } catch (e) {
    result = { success: false, error: String(e) };
  }
  if (result?.success) {
    if (status) status.textContent = 'Update downloaded! Launching installer...';
    addLog('Launcher update started — restarting.', 'ok', 'update');
    // App will quit shortly from main process
  } else {
    const err = result?.error || 'Unknown error';
    if (status) { status.textContent = `Error: ${err}`; status.style.color = '#ff6666'; }
    if (nowBtn) { nowBtn.disabled = false; nowBtn.textContent = '⬇ Update Now'; }
    addLog(`Update failed: ${err}`, 'error', 'update');
    toast(`Update failed: ${err}`, 'error', 5000);
  }
});
/// The launcher lives in the tray between play sessions, often for days, so
/// one check at startup — which, started with Windows, is usually before the
/// network is up — could leave it never hearing about a release. It checks
/// again every so often, and soon after a check that couldn't reach GitHub.
const LAUNCHER_UPDATE_RECHECK_MS = 12 * 60 * 60 * 1000;
const LAUNCHER_UPDATE_RETRY_MS = 15 * 60 * 1000;
let launcherUpdateTimer = null;
/// The release already offered this session. A later automatic check that
/// finds the same one leaves the user's "Later" alone.
let offeredLauncherVersion = null;

function scheduleLauncherUpdateCheck(ms) {
  clearTimeout(launcherUpdateTimer);
  launcherUpdateTimer = setTimeout(checkForLauncherUpdate, ms);
}

async function checkForLauncherUpdate() {
  scheduleLauncherUpdateCheck(LAUNCHER_UPDATE_RECHECK_MS);
  // Only check if autoUpdate is enabled in settings
  if (config.autoUpdate === false) return;
  addLog('Checking for launcher updates...', 'info', 'update');
  try {
    const info = await window.radium?.checkForUpdate();
    if (!info) return;
    if (info.error) {
      addLog(`Update check failed: ${info.error}`, 'info', 'update');
      scheduleLauncherUpdateCheck(LAUNCHER_UPDATE_RETRY_MS);
      return;
    }
    if (info.hasUpdate) {
      if (offeredLauncherVersion === info.latestVersion) return;
      offeredLauncherVersion = info.latestVersion;
      addLog(`New version available: ${info.latestVersion} (current: v${info.currentVersion})`, 'ok', 'update');
      toast('Update available!', 'ok', 5000);
      showUpdateModal(info);
    } else {
      addLog(`Launcher is up to date (v${info.currentVersion}).`, 'info', 'update');
    }
  } catch (e) {
    addLog(`Update check error: ${e?.message || e}`, 'info', 'update');
    scheduleLauncherUpdateCheck(LAUNCHER_UPDATE_RETRY_MS);
  }
}

$('btnCheckUpdates')?.addEventListener('click', async () => {
  const btn = $('btnCheckUpdates');
  if (!btn || btn.disabled) return;
  btn.disabled = true;
  const resultEl = $('updateCheckResult');
  if (resultEl) {
    resultEl.textContent = 'Checking…';
    resultEl.className = 'sg-desc test-result';
  }
  addLog('Manual launcher update check initiated.', 'info', 'update');
  toast('Checking for updates...', 'info', 2000);
  
  try {
    const info = await window.radium?.checkForUpdate();
    if (!info) {
      if (resultEl) {
        resultEl.textContent = 'No answer from GitHub. Try again in a minute.';
        resultEl.className = 'sg-desc test-result error';
      }
      toast('Update check failed.', 'error');
      return;
    }
    if (info.error) {
      if (resultEl) {
        resultEl.textContent = "Couldn’t check right now. Try again in a minute.";
        resultEl.className = 'sg-desc test-result error';
      }
      addLog(`Update check failed: ${info.error}`, 'info', 'update');
      toast('Update check failed.', 'error');
      return;
    }
    if (info.hasUpdate) {
      if (resultEl) {
        resultEl.textContent = `Version ${info.latestVersion} is out.`;
        resultEl.className = 'sg-desc test-result ok';
      }
      offeredLauncherVersion = info.latestVersion;
      addLog(`New version available: ${info.latestVersion} (current: v${info.currentVersion})`, 'ok', 'update');
      toast('Update available!', 'ok', 5000);
      showUpdateModal(info);
    } else {
      if (resultEl) {
        resultEl.textContent = `You have the latest version (${info.currentVersion}).`;
        resultEl.className = 'sg-desc test-result ok';
      }
      addLog(`Launcher is up to date (v${info.currentVersion}).`, 'info', 'update');
      toast('Launcher is up to date.', 'ok');
    }
  } catch (e) {
    if (resultEl) {
      resultEl.textContent = "Couldn’t check right now. Try again in a minute.";
      resultEl.className = 'sg-desc test-result error';
    }
    addLog(`Update check error: ${e.message}`, 'info', 'update');
    toast('Update check error.', 'error');
  } finally {
    btn.disabled = false;
  }
});

// Launcher entrypoint initialization
// ── Network switcher ─────────────────────────────────────────────────────

/// Apply everything that is purely presentational about a network. Split out
/// from setNetwork() so startup can brand the UI without triggering a reload
/// of data that init() is about to fetch anyway.
function applyNetworkUI(name) {
  activeNetwork = NETWORKS[name] ? name : 'receos';
  const info = networkInfo();

  for (const name of Object.keys(NETWORKS)) {
    document.body.classList.toggle(`network-${name}`, activeNetwork === name);
  }
  document.body.classList.toggle('network-no-social', info.hasSocial === false);
  // The UPDATE button belongs to the network it was checked for (Stella or
  // Rec EOS); another network never inherits it.
  if (stellaPatchNetwork !== activeNetwork && !stellaPatchUpdating) {
    stellaPatch = null;
    stellaPatchCheckedAt = 0;
  }
  applyStellaPatchUI();
  // Who is signed in to Stella, and their friends. Deferred: at startup this
  // runs before the account section of this file has been evaluated. Leaving
  // Stella closes the friends connection, and the backend is told first
  // whether Stella is in use, so these calls may sign in.
  syncStellaInUse();
  if (activeNetwork === 'stella') {
    setTimeout(() => { refreshStellaAuth(); refreshStellaFriends(); }, 0);
  } else if (activeNetwork === 'receos') {
    window.radium?.stellaFriendsStop?.().catch(() => {});
    setTimeout(() => { refreshStellaFriends(); }, 0);
  } else {
    window.radium?.stellaFriendsStop?.().catch(() => {});
  }
  // Mirrored so the pre-paint bootstrap in index.html can brand the window
  // before CSS loads on the next launch.
  try { localStorage.setItem('radium-network', activeNetwork); } catch (e) {}

  const nameEl = $('networkName');
  if (nameEl) nameEl.textContent = info.label;

  // The hero's brand: Radium's mark is its wordmark; the square marks get
  // the name beside them.
  const heroName = $('heroName');
  if (heroName) heroName.textContent = activeNetwork === 'radium' ? '' : info.label;
  document.querySelector('.home-hero-mark')?.setAttribute('aria-label', info.label);
  // Settings' Notifications and Friends belong to one network each.
  showSettingsSection(settingsSection);
  // Each network has its own Home banner, and Friends is in Home's list on
  // Stella only.
  applyHomeBanner();
  renderHomeLayoutList();

  const logoEl = $('sidebarLogo');
  if (logoEl) logoEl.src = info.logo;

  document.querySelectorAll('#networkMenu .network-option').forEach(opt => {
    const selected = opt.dataset.network === activeNetwork;
    opt.setAttribute('aria-selected', String(selected));
    // `active` is the class every theme already styles as "this is the current
    // one", so the selected network is highlighted the same way the current
    // tab is, in whichever skin is applied.
    opt.classList.toggle('active', selected);
  });

  // The exclusion is per network, as the folder it names is.
  setExcludeAvLabel(avExcluded());
  updateDownloadCta();
  syncTray();
}

/// Mark the switcher as just-changed for one animation.
///
/// Only on a real switch, not on the startup branding pass, so the sidebar
/// doesn't flash every launch. The class is removed when the animation ends
/// (and on a timer as a backstop, since `animationend` never fires when the
/// active theme defines no animation for it) so a second switch replays it.
function pulseNetworkSwitcher() {
  const btn = $('networkSwitcher');
  if (!btn) return;

  btn.classList.remove('network-just-switched');
  // Forces the class removal to take effect before it is re-added, otherwise
  // the browser coalesces both into no change at all and nothing replays.
  void btn.offsetWidth;
  btn.classList.add('network-just-switched');

  const clear = () => btn.classList.remove('network-just-switched');
  btn.addEventListener('animationend', clear, { once: true });
  setTimeout(clear, 1200);
}

function closeNetworkMenu() {
  const menu = $('networkMenu');
  const btn = $('networkSwitcher');
  hideDropdown(menu);
  if (btn) {
    btn.setAttribute('aria-expanded', 'false');
    btn.classList.remove('active');
  }
}

function openNetworkMenu() {
  const menu = $('networkMenu');
  const btn = $('networkSwitcher');
  showDropdown(menu);
  if (btn) {
    btn.setAttribute('aria-expanded', 'true');
    // Borrows the current-tab look while open, which every theme already
    // defines for `.nav-btn.active`.
    btn.classList.add('active');
  }
  menu?.querySelector('.network-option.active')?.focus();
}

/// Switch networks: rebrand, persist, and reload everything that is
/// network-scoped.
async function setNetwork(name) {
  if (!NETWORKS[name] || name === activeNetwork) {
    closeNetworkMenu();
    return;
  }

  // A download and a running game are both single-instance globals bound to one
  // client, so switching underneath them would leave the UI describing a client
  // it is no longer pointing at.
  if (isDownloading || isPaused) {
    toast(clientTask
      ? 'Finish or cancel the file check before switching networks.'
      : 'Finish or cancel the current download before switching networks.', 'warn', 4000);
    closeNetworkMenu();
    return;
  }
  if (isGameRunning || isGameLaunching) {
    toast('Close the game before switching networks.', 'warn', 4000);
    closeNetworkMenu();
    return;
  }

  closeNetworkMenu();
  applyNetworkUI(name);
  pulseNetworkSwitcher();
  addLog(`Switched network to ${networkInfo().label} (${networkInfo().site}).`, 'ok');

  // Persist. Written directly rather than through autoSaveSettings() so the
  // switch survives even if the user never touches the settings form.
  config.network = activeNetwork;
  try {
    await window.radium?.saveConfig({ ...config, network: activeNetwork });
  } catch (e) {
    console.error('saveConfig (network) error:', e);
  }

  // Drop everything scoped to the previous network. Bumping the sequence ids
  // makes the existing race guards discard any responses still in flight.
  roomsSequenceId++;
  peopleSequenceId++;
  roomsSkip = 0;
  peopleSkip = 0;
  activeRoomsTag = '';
  activeRoomsSort = 0;
  // The dropdown follows, or it keeps naming the old network's sort (and Most
  // Players, which is Stella's, would stay picked while hidden).
  setValue('roomsSort', '0');
  roomsSearchQuery = '';
  peopleSearchQuery = '';
  setValue('roomsSearch', '');
  setValue('peopleSearch', '');
  hideRoomDetails();
  hidePlayerDetails();
  resetFeed();

  // Rooms and People are rendered per network, so drop what the old one left
  // on screen. Without this the reload below would dim the previous network's
  // rows and leave them readable while the new network's list downloads.
  roomsRenderKey = '';
  peopleRenderKey = '';
  filtersRenderKey = '';
  const roomsGridEl = $('roomsGrid');
  if (roomsGridEl) {
    roomsGridEl.innerHTML = '';
    delete roomsGridEl.dataset.listPlaceholder;
    endListLoad(roomsGridEl);
  }
  const peopleGridEl = $('peopleGrid');
  if (peopleGridEl) {
    peopleGridEl.innerHTML = '';
    delete peopleGridEl.dataset.listPlaceholder;
    endListLoad(peopleGridEl);
  }
  // Home's Popular row is the old network's rooms too.
  resetHomeRooms();

  // Cached client-update state belongs to the old network's client.
  clientUpdateInfo = null;
  clientUpdateAutoChecked = false;
  // And the install verdicts logged for it: the new network's are news.
  lastInstallLog.clear();

  // These caches are keyed by username / photo id alone, which is only unique
  // *within* a network — the same name is a different person on each. Without
  // this, opening a profile on one network then the same name on the other
  // serves the first network's stats.
  userWebDetailsCache.clear();
  photoWebDetailsCache.clear();

  await checkInstall();
  checkServerStatus(true);
  updatePlayerCount(true);

  // Only refetch a list the user is actually looking at.
  const openTab = document.querySelector('.tab-panel.active')?.id;
  if ((openTab === 'tab-rooms' || openTab === 'tab-people') && networkInfo().hasSocial === false) {
    // Their nav buttons just disappeared (Stella); don't strand the user there.
    switchTab('home');
  } else if (openTab === 'tab-rooms') {
    loadFilters();
    loadRooms();
  } else if (openTab === 'tab-people') {
    loadPeople();
  } else if (openTab === 'tab-feed') {
    // FEED only exists on networks that publish one; leaving the user parked
    // on a tab whose nav button just disappeared would strand them.
    if (networkInfo().hasPhotoFeed) loadFeed(false, { refresh: true });
    else switchTab('home');
  } else if (openTab === 'tab-friends' && !stellaLike()) {
    // Likewise FRIENDS, which is Stella's and Rec EOS's.
    switchTab('home');
  } else if (openTab === 'tab-chats' && activeNetwork !== 'receos') {
    // And CHATS, Rec EOS's.
    switchTab('home');
  } else if (openTab === 'tab-home') {
    loadHomeRooms();
  }

  // Warm the new network's bulk sets now, while the user is reading whatever
  // tab they're on, rather than on the click that opens Rooms or People.
  window.radium?.prefetchNetworkData();
}

$('networkSwitcher')?.addEventListener('click', (e) => {
  e.stopPropagation();
  const expanded = $('networkSwitcher')?.getAttribute('aria-expanded') === 'true';
  if (expanded) closeNetworkMenu(); else openNetworkMenu();
});

document.querySelectorAll('#networkMenu .network-option').forEach(opt => {
  opt.addEventListener('click', (e) => {
    e.stopPropagation();
    setNetwork(opt.dataset.network);
  });
});


// ── Dropdown show/hide ───────────────────────────────────────────────────
// Shared by the two dropdowns in the launcher: the gear on the hero and the
// network switcher in the sidebar.

/// How long a menu's exit animation is given before the element is hidden,
/// when the animation cannot say how long it runs. Most skins exit in 140ms;
/// the retro ones that let a chosen item fade or blink take longer, and
/// hideDropdown() reads their length off the animation itself.
const MENU_EXIT_MS = 140;

/// Per-menu counter, bumped by every show and every hide, so a hide that is
/// still waiting out its animation can tell whether it is still the current
/// one. Keyed by element rather than by id so both menus share one mechanism.
const menuCloseTokens = new WeakMap();

/// Hide a dropdown, letting its exit animation play first where there is one.
///
/// `hidden` removes the element outright, so a close cannot be animated by CSS
/// alone — the element has to stay in the layout until the animation is done.
/// It is marked `.is-closing`, which is what the exit keyframes hang off, and
/// hidden once that has had its time. Returns that time in ms (0 when it is
/// hidden at once), for callers that have their own tidying to do after it.
function hideDropdown(menu) {
  if (!menu || menu.hidden) return 0;

  const token = (menuCloseTokens.get(menu) || 0) + 1;
  menuCloseTokens.set(menu, token);

  const finish = () => {
    // Reopening during the exit bumps the token, so this timer belongs to a
    // close the user has already undone and must not hide anything.
    if (menuCloseTokens.get(menu) !== token) return;
    menu.classList.remove('is-closing');
    menu.querySelector('.is-picked')?.classList.remove('is-picked');
    menu.hidden = true;
  };

  // The item under the pointer is the one just chosen. Marked rather than
  // left to :hover, since the closing menu stops taking the pointer; the
  // retro skins that fade or blink a chosen item hang that off the mark.
  menu.querySelector('.is-picked')?.classList.remove('is-picked');
  menu.querySelector(':is(.network-option, .manage-item):hover')?.classList.add('is-picked');
  menu.classList.add('is-closing');

  // Most retro skins have no exit animation (only a chosen item's fade or
  // blink on some), and neither does anyone whose system asks for reduced
  // motion; waiting on a timer for them would only make dismissal feel
  // sluggish. getAnimations() flushes pending style, so the
  // class added a line above is already accounted for.
  const animations = typeof menu.getAnimations === 'function' ? menu.getAnimations() : [];
  if (animations.length === 0) {
    finish();
    return 0;
  }

  // A timer rather than an `animationend` listener: a minimised or hidden
  // window pauses animations, and that event would never arrive — leaving the
  // menu stuck open on screen the next time the window was restored.
  const wait = Math.min(1000, Math.max(...animations.map((a) => {
    const end = a.effect?.getComputedTiming?.().endTime;
    return Number.isFinite(end) ? end - (Number(a.currentTime) || 0) : MENU_EXIT_MS;
  })));
  setTimeout(finish, wait);
  return wait;
}

/// Show a dropdown, cancelling any exit still in flight.
function showDropdown(menu) {
  if (!menu) return;
  menuCloseTokens.set(menu, (menuCloseTokens.get(menu) || 0) + 1);
  menu.classList.remove('is-closing');
  menu.querySelector('.is-picked')?.classList.remove('is-picked');
  menu.hidden = false;
}

// ── Dialog show/hide ─────────────────────────────────────────────────────
// Every warning and confirmation dialog goes through these two, so a skin
// that animates dialogs out (the modern family, Liquid Glass) gets to play
// the exit, and the rest close instantly as they always did.

/// How long a dialog's exit animation is given before it is hidden, when the
/// animation cannot say how long it runs; hideModal() reads it off the
/// overlay's exit (180ms on the modern skins and glass, as on Vista/7).
const MODAL_EXIT_MS = 180;

/// Per-dialog counter, bumped by every show and hide, so a close still waiting
/// out its animation can tell it has been superseded by a reopen.
const modalCloseTokens = new WeakMap();

/// Show a dialog, cancelling any close still in flight.
function showModal(modal) {
  if (!modal) return;
  if (revealOnModal) {
    revealOnModal = false;
    window.radium?.showLauncher();
  }
  modalCloseTokens.set(modal, (modalCloseTokens.get(modal) || 0) + 1);
  modal.classList.remove('is-closing');
  modal.style.display = 'flex';
}

/// Hide a dialog, letting its exit animation play first where there is one.
///
/// `display: none` removes the dialog outright, so it is marked `.is-closing`
/// (which the exit keyframes hang off) and hidden once that has had its time.
/// Only the overlay's own animations are checked: the exit is declared there,
/// and a dialog can hold unrelated endless animations (a throbbing default
/// button, a loading placeholder) that must not make every close wait.
/// `onHidden` runs once it is really gone — not if it was reopened meanwhile.
function hideModal(modal, onHidden) {
  if (!modal) return;
  const token = (modalCloseTokens.get(modal) || 0) + 1;
  modalCloseTokens.set(modal, token);

  const finish = () => {
    if (modalCloseTokens.get(modal) !== token) return;
    modal.classList.remove('is-closing');
    modal.style.display = 'none';
    onHidden?.();
  };

  if (getComputedStyle(modal).display === 'none') { finish(); return; }

  modal.classList.add('is-closing');
  const animations = typeof modal.getAnimations === 'function' ? modal.getAnimations() : [];
  if (animations.length === 0) { finish(); return; }
  // A timer, not animationend: see hideDropdown() for why. As long as the
  // skin's exit runs, read off the animation itself.
  setTimeout(finish, Math.min(1000, Math.max(...animations.map((a) => {
    const end = a.effect?.getComputedTiming?.().endTime;
    return Number.isFinite(end) ? end - (Number(a.currentTime) || 0) : MODAL_EXIT_MS;
  }))));
}

// ── Manage-client menu ───────────────────────────────────────────────────
// The gear on the hero. Modelled on the network switcher above, down to the
// aria-expanded flag and the two ways out, so both dropdowns in the launcher
// behave the same.

function closeManageMenu() {
  const menu = $('manageMenu');
  const btn = $('btnManageClient');
  hideDropdown(menu);
  if (btn) {
    btn.setAttribute('aria-expanded', 'false');
    btn.classList.remove('active');
  }
}

function openManageMenu() {
  const menu = $('manageMenu');
  const btn = $('btnManageClient');
  showDropdown(menu);
  if (btn) {
    btn.setAttribute('aria-expanded', 'true');
    // Borrows the current-tab look while open, as the network switcher does.
    btn.classList.add('active');
  }
  menu?.querySelector('button')?.focus();
}

$('btnManageClient')?.addEventListener('click', (e) => {
  e.stopPropagation();
  const expanded = $('btnManageClient')?.getAttribute('aria-expanded') === 'true';
  if (expanded) closeManageMenu(); else openManageMenu();
});

// Every item starts something that takes over the screen — a folder, a modal, a
// re-download — so the menu closes behind the click rather than lingering over
// whatever it opened. Registered in the capture phase so it runs before each
// button's own handler, which may replace the button or navigate away.
$('manageMenu')?.addEventListener('click', (e) => {
  if (e.target.closest('button')) closeManageMenu();
}, true);

// Dismissal: anywhere outside, or Escape. Mirrors the network menu below.
document.addEventListener('click', (e) => {
  const menu = $('manageMenu');
  if (!menu || menu.hidden) return;
  if (menu.contains(e.target) || $('btnManageClient')?.contains(e.target)) return;
  closeManageMenu();
});
document.addEventListener('keydown', (e) => {
  const menu = $('manageMenu');
  if (!menu || menu.hidden || e.key !== 'Escape') return;
  closeManageMenu();
  $('btnManageClient')?.focus();
});

// Dismissal: anywhere outside, or Escape.
document.addEventListener('click', (e) => {
  const menu = $('networkMenu');
  if (!menu || menu.hidden) return;
  if (menu.contains(e.target) || $('networkSwitcher')?.contains(e.target)) return;
  closeNetworkMenu();
});
document.addEventListener('keydown', (e) => {
  const menu = $('networkMenu');
  if (!menu || menu.hidden) return;
  if (e.key === 'Escape') {
    closeNetworkMenu();
    $('networkSwitcher')?.focus();
    return;
  }
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    const opts = Array.from(menu.querySelectorAll('.network-option'));
    const idx = opts.indexOf(document.activeElement);
    const next = e.key === 'ArrowDown'
      ? (idx + 1) % opts.length
      : (idx - 1 + opts.length) % opts.length;
    opts[next]?.focus();
  }
});

// ── Vanilla account ──────────────────────────────────────────────────────
// Sign-in happens on Vanilla's own page in a separate window (see
// vanilla_auth.rs). Nothing here ever sees the password or the session; the
// backend hands over a display name, an avatar and a token count.

let vanillaPlayer = null;
/// Stella's signed-in account (a people row), or null. See the Stella account
/// section below; declared here because fitAccountName reads it.
let stellaPlayer = null;

/// Fit a long name into the account button: first a smaller face on one line
/// (down to 12px), then two lines, then 11px on two lines, and only after
/// that an ellipsis. Re-run whenever the name, the skin or the sidebar's width
/// changes, since each skin sets its own face and size.
function fitAccountName() {
  fitOneAccountName($('vanillaAccountName'), vanillaPlayer);
  fitOneAccountName($('stellaAccountName'), stellaPlayer);
}

function fitOneAccountName(name, player) {
  if (!name) return;
  name.style.fontSize = '';
  name.classList.remove('two-lines');
  if (!player || !name.clientWidth) return;

  const wide = () => name.scrollWidth > name.clientWidth + 0.5;
  const tall = () => name.scrollHeight > name.clientHeight + 0.5;
  let size = parseFloat(getComputedStyle(name).fontSize) || 14;
  while (wide() && size > 12) {
    size = Math.max(12, size - 0.5);
    name.style.fontSize = `${size}px`;
  }
  if (!wide()) return;
  name.classList.add('two-lines');
  if (tall()) name.style.fontSize = '11px';
}
if (window.ResizeObserver) {
  for (const id of ['vanillaAccountName', 'stellaAccountName']) {
    const nameEl = document.getElementById(id);
    if (nameEl) new ResizeObserver(() => fitAccountName()).observe(nameEl);
  }
}
// A skin switch changes the face without resizing the box.
new MutationObserver(() => requestAnimationFrame(fitAccountName))
  .observe(document.body, { attributes: true, attributeFilter: ['class'] });

function renderVanillaAccount() {
  const name = $('vanillaAccountName');
  const avatar = $('vanillaAccountAvatar');
  const btn = $('vanillaAccountBtn');
  const bell = $('vanillaNotifsBtn');
  if (!name || !avatar || !btn) return;

  if (vanillaPlayer) {
    const handle = vanillaPlayer.userName || vanillaPlayer.displayName || 'account';
    name.textContent = vanillaPlayer.displayName || handle;
    const shown = vanillaPlayer.displayName && vanillaPlayer.displayName !== handle
      ? `${vanillaPlayer.displayName} (@${handle})` : `@${handle}`;
    btn.setAttribute('aria-label', `Signed in to Vanilla as ${shown}`);
    avatar.dataset.fallback = placeholderAvatar();
    avatar.src = vanillaPlayer.AvatarUrl
      ? thumbSrc(vanillaPlayer.AvatarUrl, avatarWidth(28))
      : defaultAvatarUrl(28);
    avatar.hidden = false;
    btn.setAttribute('aria-haspopup', 'menu');
    $('vanillaAccountFullName').textContent = `@${handle}`;
    // The display name too, when it isn't just the username.
    const display = $('vanillaAccountDisplay');
    if (display) {
      const differs = vanillaPlayer.displayName && vanillaPlayer.displayName !== handle;
      display.textContent = differs ? vanillaPlayer.displayName : '';
      display.hidden = !differs;
    }
    if (bell) bell.hidden = false;
    fitAccountName();
  } else {
    name.textContent = 'LOG IN';
    fitAccountName();
    btn.setAttribute('aria-label', 'Sign in with your Vanilla account');
    avatar.hidden = true;
    avatar.removeAttribute('src');
    btn.removeAttribute('aria-haspopup');
    $('vanillaAccountTokens').textContent = '';
    if (bell) bell.hidden = true;
    resetNotifs(notifSources.vanilla);
    closeAccountMenu();
  }
}

/// Token balance and notifications, fetched after sign-in. Neither is worth an
/// error message of its own: an expired session is reported through the auth
/// event, and anything else just leaves them blank.
async function refreshVanillaExtras() {
  if (!vanillaPlayer) return;
  try {
    const acct = await window.radium.vanillaAccount();
    $('vanillaAccountTokens').textContent = `${Number(acct.tokens || 0).toLocaleString()} tokens`;
  } catch (e) {}
  loadVanillaNotifications();
}

/// A saved session Vanilla couldn't be asked about yet, retried until it
/// answers either way.
///
/// Started with Windows, the launcher is usually up before the network is,
/// so the startup check failed, the account button said LOG IN, and nothing
/// asked again that session: no notifications and no pop-ups until the
/// launcher was restarted, although the session was fine all along.
let authRetryTimer = null;
let authRetryDelay = 0;
const AUTH_RETRY_MAX_MS = 5 * 60 * 1000;

async function recheckVanillaAuth() {
  clearTimeout(authRetryTimer);
  authRetryTimer = null;
  try {
    applyVanillaAuth(await window.radium.vanillaAuthStatus());
  } catch (e) {
    scheduleAuthRetry();
  }
}

function scheduleAuthRetry() {
  clearTimeout(authRetryTimer);
  authRetryDelay = Math.min(authRetryDelay ? authRetryDelay * 2 : 15000, AUTH_RETRY_MAX_MS);
  authRetryTimer = setTimeout(recheckVanillaAuth, authRetryDelay);
}

// Back online: ask now rather than at the end of the current wait.
window.addEventListener('online', () => {
  if (authRetryTimer) recheckVanillaAuth();
});

function applyVanillaAuth(state) {
  const was = vanillaPlayer;
  vanillaPlayer = state && state.authenticated && state.player ? state.player : null;
  if (!vanillaPlayer && state?.hasSession) {
    scheduleAuthRetry();
  } else {
    clearTimeout(authRetryTimer);
    authRetryTimer = null;
    authRetryDelay = 0;
  }
  renderVanillaAccount();

  // Cheer and subscribe buttons on whatever is open reflect the new account.
  if (was?.id !== vanillaPlayer?.id) {
    cheeredPhotoIds = null;
    resetNotifs(notifSources.vanilla);
    refreshSocialButtons();
  }

  if (vanillaPlayer) {
    if (!was) addLog(`Signed in to Vanilla as @${vanillaPlayer.userName}`, 'ok', 'account');
    refreshVanillaExtras();
    return;
  }
  if (state && state.error && !state.hasSession) {
    toast(`Vanilla sign-in failed: ${state.error}`, 'error');
  }
  if (was) {
    const expired = state && state.reason === 'expired';
    addLog(expired ? 'Vanilla session expired, signed out' : 'Signed out of Vanilla', 'info', 'account');
    if (expired) toast('Your Vanilla session expired. Please sign in again.', 'info');
  }
}

function closeAccountMenu() {
  const btn = $('vanillaAccountBtn');
  hideDropdown($('vanillaAccountMenu'));
  if (btn) {
    btn.setAttribute('aria-expanded', 'false');
    btn.classList.remove('is-open');
  }
}

function openAccountMenu() {
  closeNotifPanel();
  const menu = $('vanillaAccountMenu');
  const btn = $('vanillaAccountBtn');
  showDropdown(menu);
  if (btn) {
    btn.setAttribute('aria-expanded', 'true');
    btn.classList.add('is-open');
  }
  menu?.querySelector('.network-option')?.focus();
}

$('vanillaAccountBtn')?.addEventListener('click', async () => {
  if (vanillaPlayer) {
    const menu = $('vanillaAccountMenu');
    if (menu && !menu.hidden) closeAccountMenu(); else openAccountMenu();
    return;
  }
  // Opens Vanilla's sign-in window; the result arrives as `vanilla-auth-changed`.
  try {
    await window.radium.vanillaLogin();
  } catch (err) {
    toast(String(err), 'error');
  }
});

$('vanillaProfileBtn')?.addEventListener('click', () => {
  closeAccountMenu();
  if (vanillaPlayer?.userName) showCreatorProfile(vanillaPlayer.userName);
});
$('vanillaLogoutBtn')?.addEventListener('click', async () => {
  closeAccountMenu();
  try {
    await window.radium.vanillaLogout();
  } catch (e) {
    toast(String(e), 'error');
  }
});

// ── Stella account ───────────────────────────────────────────────────────
// Stella has no password sign-in: the backend signs in with the player's Steam
// account (stella_api.rs), and this shows which Stella account that is. LOG
// OUT is remembered by the backend across restarts; until LOG IN, Rooms and
// People offer a sign-in instead of signing in on their own.

/// The error every Stella API call answers while logged out
/// (`stella_api::SIGNED_OUT_ERROR`).
const STELLA_SIGNED_OUT = 'Signed out of Stella.';
/// What a sign-in answers when Steam isn't running
/// (`stella_api::STEAM_NOT_RUNNING`).
const STEAM_NOT_RUNNING = "Steam isn't running.";
/// What a sign-in answers while the launcher is hidden or minimized, or Stella
/// isn't picked (`stella_api::NOT_IN_USE`).
const STELLA_NOT_IN_USE = 'Stella signs in when you open it.';
/// What a sign-in answers when the Steam account has no Stella account yet
/// (`stella_api::NO_ACCOUNT`). The game makes one the first time it runs.
const STELLA_NO_ACCOUNT = 'This Steam account has no Stella account yet.';
/// How the launcher says so, wherever it does.
const STELLA_NO_ACCOUNT_TEXT = "Your Steam account doesn't have a Stella account yet. Play Stella once to make one.";
let stellaAuthPending = false;
/// Why the last sign-in failed, if it did.
let stellaAuthError = '';
/// How many Stella accounts the Steam account holds (stella_accounts); SWITCH
/// ACCOUNT is offered when it is more than one.
let stellaAccountCount = 0;

function renderStellaAccount() {
  const name = $('stellaAccountName');
  const avatar = $('stellaAccountAvatar');
  const btn = $('stellaAccountBtn');
  if (!name || !avatar || !btn) return;

  if (stellaPlayer) {
    const handle = stellaPlayer.userName || stellaPlayer.displayName || 'account';
    name.textContent = stellaPlayer.displayName || handle;
    const differs = stellaPlayer.displayName && stellaPlayer.displayName !== handle;
    btn.setAttribute('aria-label', `Signed in to Stella as ${differs ? `${stellaPlayer.displayName} (@${handle})` : `@${handle}`}`);
    avatar.dataset.fallback = placeholderAvatar();
    avatar.src = stellaPlayer.AvatarUrl
      ? thumbSrc(stellaPlayer.AvatarUrl, avatarWidth(28))
      : placeholderAvatar();
    avatar.hidden = false;
    btn.setAttribute('aria-haspopup', 'menu');
    $('stellaAccountFullName').textContent = `@${handle}`;
    const display = $('stellaAccountDisplay');
    if (display) {
      display.textContent = differs ? stellaPlayer.displayName : '';
      display.hidden = !differs;
    }
    if ($('stellaNotifsBtn')) $('stellaNotifsBtn').hidden = false;
    if ($('stellaSwitchBtn')) $('stellaSwitchBtn').hidden = stellaAccountCount < 2;
  } else {
    if ($('stellaNotifsBtn')) $('stellaNotifsBtn').hidden = true;
    name.textContent = stellaAuthPending ? 'SIGNING IN…' : 'LOG IN';
    btn.setAttribute('aria-label', 'Sign in to Stella with your Steam account');
    avatar.hidden = true;
    avatar.removeAttribute('src');
    btn.removeAttribute('aria-haspopup');
    closeStellaAccountMenu();
  }
  fitAccountName();
}

/// Rooms and People belong to whoever is signed in (or to nobody): reload the
/// one on screen, and close a detail view the old account opened.
function reloadStellaLists() {
  if (activeNetwork !== 'stella') return;
  roomsRenderKey = '';
  peopleRenderKey = '';
  filtersRenderKey = '';
  userWebDetailsCache.clear();
  resetHomeRooms();
  const openTab = document.querySelector('.tab-panel.active')?.id;
  if (openTab === 'tab-home') {
    loadHomeRooms();
  } else if (openTab === 'tab-rooms') {
    hideRoomDetails();
    loadFilters();
    loadRooms();
  } else if (openTab === 'tab-people') {
    hidePlayerDetails();
    loadPeople();
  }
}

function applyStellaAuth(player) {
  const was = stellaPlayer;
  stellaPlayer = player || null;
  // Another account's (or nobody's) tokens.
  if (was?.id !== stellaPlayer?.id) $('stellaAccountTokens').textContent = '';
  renderStellaAccount();
  refreshStellaFriends();
  if (was?.id === stellaPlayer?.id) return;
  // An open room's cheer and favorite tiles, and the photo cheers on screen,
  // belong to the account.
  stellaCheerKnown = new Map();
  refreshSocialButtons();
  resetNotifs(notifSources.stella);
  if (stellaPlayer) loadNotifications(notifSources.stella);
  if (stellaPlayer) addLog(`Signed in to Stella as @${stellaPlayer.userName}`, 'ok', 'account');
  else if (was) addLog('Signed out of Stella', 'info', 'account');
  // Whether there are other accounts to switch to, and, the first time a
  // Steam account with several signs in, which one to use.
  if (stellaPlayer) syncStellaAccounts({ ask: true });
  reloadStellaLists();
}

/// Ask the backend who is signed in. Unless the user logged out, this signs in
/// with Steam (a second or two), so the button says so meanwhile. A failure
/// (Steam not running, say) just leaves LOG IN; pressing it says why.
async function refreshStellaAuth() {
  if (activeNetwork !== 'stella' || stellaPlayer || stellaAuthPending) return;
  stellaAuthPending = true;
  renderStellaAccount();
  let state = null;
  try {
    state = await window.radium.stellaAuthStatus();
  } catch (e) {}
  stellaAuthPending = false;
  stellaAuthError = state?.authenticated ? '' : (state?.error || '');
  applyStellaAuth(state?.authenticated ? state.player : null);
}

/// LOG IN. `quiet`: asked by the launcher rather than the player, so a
/// failure only shows where the lists and the friends card say why.
async function stellaSignIn({ quiet = false } = {}) {
  if (stellaAuthPending) return;
  stellaAuthPending = true;
  renderStellaAccount();
  try {
    const player = await window.radium.stellaLogin();
    stellaAuthPending = false;
    stellaAuthError = '';
    applyStellaAuth(player);
  } catch (e) {
    stellaAuthPending = false;
    stellaAuthError = String(e);
    renderStellaAccount();
    // Signed out stays cleared: the next list load tries again, and shows
    // the real reason (Steam not running) rather than a sign-in prompt.
    reloadStellaLists();
    refreshStellaFriends();
    if (quiet) return;
    toast(stellaAuthError === STEAM_NOT_RUNNING ? "Steam isn't running. Start Steam, then try again."
      : stellaAuthError === STELLA_NO_ACCOUNT ? STELLA_NO_ACCOUNT_TEXT
      : `Stella sign-in failed: ${e}`, stellaAuthError === STELLA_NO_ACCOUNT ? 'info' : 'error', 6000);
  }
}

/// The game has just closed on Stella while the Steam account had no Stella
/// account: it makes one on its first run, so ask again (the backend would
/// otherwise wait a while before asking, see stella_api::NO_ACCOUNT_AT).
function stellaRecheckAfterGame() {
  if (activeNetwork !== 'stella' || stellaPlayer || stellaAuthError !== STELLA_NO_ACCOUNT) return;
  stellaSignIn({ quiet: true });
}

// ── Stella account chooser ────────────────────────────────────────────────
// One Steam account can hold several Stella accounts. The backend signs in to
// the one picked here (kept across restarts), or until then the one played
// last; the first time a Steam account with several signs in, this asks.

/// Asked once per run at most, so closing the chooser is never met by it again.
let stellaAccountsAsked = false;
/// The chooser's rows as last fetched, and the account it was opened as.
let stellaAccountsShown = null;

/// Ask the backend which Stella accounts the Steam account holds: shows or
/// hides SWITCH ACCOUNT, and with `ask`, opens the chooser if there are
/// several and none was picked yet.
async function syncStellaAccounts({ ask = false } = {}) {
  let res;
  try {
    res = await window.radium.stellaAccounts();
  } catch (e) {
    return null;
  }
  if (activeNetwork !== 'stella' || !stellaPlayer) return null;
  const accounts = Array.isArray(res?.accounts) ? res.accounts : [];
  stellaAccountCount = accounts.length;
  renderStellaAccount();
  if (ask && accounts.length > 1 && !res.chosen && !stellaAccountsAsked) {
    stellaAccountsAsked = true;
    openStellaAccounts({ first: true, res });
  }
  return res;
}

/// "played 3 hours ago", from an ISO time; empty if it can't be read.
function playedAgo(iso) {
  const at = Date.parse(iso || '');
  if (!Number.isFinite(at)) return '';
  const mins = Math.max(0, Math.round((Date.now() - at) / 60000));
  if (mins < 1) return 'played just now';
  if (mins < 60) return `played ${mins} ${mins === 1 ? 'minute' : 'minutes'} ago`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `played ${hours} ${hours === 1 ? 'hour' : 'hours'} ago`;
  const days = Math.round(hours / 24);
  if (days < 30) return `played ${days} ${days === 1 ? 'day' : 'days'} ago`;
  return `played ${new Date(at).toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric' })}`;
}

/// Open the chooser. `first`: the launcher is asking by itself, so the intro
/// says why, and closing it keeps the account already signed in to.
async function openStellaAccounts({ first = false, res = null } = {}) {
  const modal = $('stellaAccountsModal');
  if (!modal) return;
  res = res || await syncStellaAccounts();
  if (!res || !stellaPlayer) return;
  stellaAccountsShown = { first, current: Number(res.current) || Number(stellaPlayer.id) };
  const count = (res.accounts || []).length;
  $('stellaAccountsIntro').textContent = first
    ? `Your Steam account has ${count} Stella accounts. Which one should the launcher use? You can change it later from the account menu.`
    : 'The Stella accounts on your Steam account. The launcher signs in to the one you pick.';
  renderStellaAccountChoices(res.accounts || []);
  showModal(modal);
  const current = $('stellaAccountsList')?.querySelector('[aria-current="true"]');
  (current || $('stellaAccountsList')?.querySelector('.account-choice'))?.focus();
}

function renderStellaAccountChoices(accounts) {
  const list = $('stellaAccountsList');
  if (!list) return;
  const current = stellaAccountsShown?.current;
  list.textContent = '';
  for (const a of accounts) {
    const id = Number(a.id);
    const handle = a.userName || '';
    const name = a.displayName || handle || `Account ${id}`;
    const isCurrent = id === current;
    const sub = [handle && handle !== name ? `@${handle}` : '', playedAgo(a.lastLogin)].filter(Boolean).join(' · ');
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'account-choice';
    btn.dataset.accountId = String(id);
    if (isCurrent) btn.setAttribute('aria-current', 'true');
    btn.setAttribute('aria-label', `${name}${handle && handle !== name ? ` (@${handle})` : ''}${isCurrent ? ', signed in' : ''}`);
    btn.innerHTML = `
      <img class="account-choice-avatar" alt="" />
      <span class="account-choice-text">
        <span class="account-choice-name">${escapeHtml(name)}</span>
        <span class="account-choice-sub">${escapeHtml(sub)}</span>
      </span>
      <span class="account-choice-state" aria-hidden="true">${isCurrent ? 'Signed in' : ''}</span>
    `;
    const img = btn.querySelector('img');
    img.dataset.fallback = placeholderAvatar();
    img.src = a.AvatarUrl ? thumbSrc(a.AvatarUrl, avatarWidth(40)) : placeholderAvatar();
    btn.addEventListener('click', () => pickStellaAccount(id, btn));
    list.appendChild(btn);
  }
}

/// Use account `id`: sign in to it, unless it is the one already signed in,
/// and keep it as the one picked.
async function pickStellaAccount(id, btn) {
  const list = $('stellaAccountsList');
  if (!list || list.classList.contains('is-busy')) return;
  const switching = id !== Number(stellaPlayer?.id);
  list.classList.add('is-busy');
  list.querySelectorAll('.account-choice').forEach(b => { b.disabled = true; });
  const state = btn.querySelector('.account-choice-state');
  if (switching && state) state.textContent = 'Signing in…';
  let player = null;
  try {
    player = await window.radium.stellaUseAccount(id);
  } catch (e) {
    list.classList.remove('is-busy');
    list.querySelectorAll('.account-choice').forEach(b => { b.disabled = false; });
    if (state) state.textContent = btn.getAttribute('aria-current') === 'true' ? 'Signed in' : '';
    const why = String(e).trim().replace(/([^.!?])$/, '$1.');
    addLog(`Couldn't switch Stella accounts: ${why}`, 'error', 'account');
    toast(`Couldn't sign in to that account: ${why}`, 'error', 7000);
    btn.focus();
    return;
  }
  list.classList.remove('is-busy');
  stellaAccountsShown = null;
  hideModal($('stellaAccountsModal'));
  if (switching && player) {
    applyStellaAuth(player);
    toast(`Signed in to Stella as @${player.userName || player.displayName}.`, 'ok', 3000);
  }
}

/// Closed without picking. Asked by the launcher itself, that keeps the
/// account signed in to, so it isn't asked again.
function closeStellaAccounts() {
  const shown = stellaAccountsShown;
  if ($('stellaAccountsList')?.classList.contains('is-busy')) return;
  stellaAccountsShown = null;
  hideModal($('stellaAccountsModal'));
  if (shown?.first && stellaPlayer) window.radium.stellaUseAccount(stellaPlayer.id).catch(() => {});
}
$('stellaAccountsClose')?.addEventListener('click', closeStellaAccounts);
$('stellaAccountsDone')?.addEventListener('click', closeStellaAccounts);
$('stellaAccountsModal')?.addEventListener('click', (e) => {
  if (e.target === e.currentTarget) closeStellaAccounts();
});
document.addEventListener('keydown', (e) => {
  const modal = $('stellaAccountsModal');
  if (e.key === 'Escape' && modal && modal.style.display !== 'none' && !modal.classList.contains('is-closing')) {
    e.preventDefault();
    closeStellaAccounts();
  }
});
$('stellaSwitchBtn')?.addEventListener('click', () => {
  closeStellaAccountMenu();
  openStellaAccounts();
});

/// Back on screen on Stella, from the tray or from being minimized: the
/// backend let Stella go meanwhile (see stella_api::in_use), so sign in if that
/// waited for it, and bring the friends, the player count and an open online
/// list back up to date. Also how Steam started while the launcher sat waiting
/// for it gets picked up: the user had to leave the window to start it.
function resumeStella() {
  syncStellaInUse();
  if (document.hidden || activeNetwork !== 'stella') return;
  // Only a sign-in that was waiting (for the window, or for Steam to start)
  // is tried again here: one that failed for good would otherwise start the
  // Steam API on every switch back to the window.
  if (!stellaPlayer && ['', STELLA_NOT_IN_USE, STEAM_NOT_RUNNING].includes(stellaAuthError)) refreshStellaAuth();
  refreshStellaFriends();
  updatePlayerCount(true);
  if (document.getElementById('tab-people')?.classList.contains('active') && !peopleSearchQuery
      && !listIsFresh(peopleKey(), peopleRenderKey, peopleRenderAt, peopleFreshMs)) {
    loadPeople();
  }
}

// A window hidden in the tray still reports its page visible, so `focus`
// (which a window brought back from the tray or the taskbar gets) is the
// signal that matters; `visibilitychange` covers a start hidden in the tray.
document.addEventListener('visibilitychange', resumeStella);
window.addEventListener('focus', resumeStella);

function closeStellaAccountMenu() {
  const btn = $('stellaAccountBtn');
  const menu = $('stellaAccountMenu');
  if (menu && !menu.hidden) hideDropdown(menu);
  if (btn) {
    btn.setAttribute('aria-expanded', 'false');
    btn.classList.remove('is-open');
  }
}

/// The token count in the account menu, asked for each time the menu opens
/// (tokens change while playing). The last count stays up meanwhile, and stays
/// if Stella can't be asked.
async function refreshStellaTokens() {
  const asked = stellaPlayer?.id;
  if (!asked) return;
  try {
    const tokens = await window.radium.stellaTokens();
    if (stellaPlayer?.id !== asked) return;
    $('stellaAccountTokens').textContent = `${Number(tokens).toLocaleString()} ${tokens === 1 ? 'token' : 'tokens'}`;
  } catch (e) {}
}

function openStellaAccountMenu() {
  const menu = $('stellaAccountMenu');
  const btn = $('stellaAccountBtn');
  refreshStellaTokens();
  showDropdown(menu);
  if (btn) {
    btn.setAttribute('aria-expanded', 'true');
    btn.classList.add('is-open');
  }
  menu?.querySelector('.network-option')?.focus();
}

$('stellaAccountBtn')?.addEventListener('click', () => {
  if (stellaPlayer) {
    const menu = $('stellaAccountMenu');
    if (menu && !menu.hidden) closeStellaAccountMenu(); else openStellaAccountMenu();
    return;
  }
  stellaSignIn();
});

$('stellaProfileBtn')?.addEventListener('click', () => {
  closeStellaAccountMenu();
  if (!stellaPlayer) return;
  showCreatorProfile(stellaPlayer.userName, {
    id: stellaPlayer.id,
    displayName: stellaPlayer.displayName,
    avatarUrl: stellaPlayer.AvatarUrl
  });
});

$('stellaLogoutBtn')?.addEventListener('click', async () => {
  closeStellaAccountMenu();
  try {
    await window.radium.stellaLogout();
  } catch (e) {
    toast(String(e), 'error');
    return;
  }
  applyStellaAuth(null);
});

// ── Stella friends (Home card) ───────────────────────────────────────────
// The backend keeps a live connection to Stella's notification hub while this
// is showing and reports each friend as online (with their room), offline, or
// still "checking" in the first minute, before their first update could have
// arrived. See stella_hub.rs.

let stellaFriendsQueued = null;
/// Your Stella friends by account id, as the friends card last had them: how
/// a profile knows to offer JOIN (see applyStellaPresence()).
let stellaFriendsById = new Map();

/// Where the friends list shows (Settings → FRIENDS): "home", "tab" or
/// "hidden". Anything else, including unset, is Home.
function friendsView() {
  const v = config?.stella?.friendsView;
  return v === 'tab' || v === 'hidden' ? v : 'home';
}

function applyFriendsView() {
  const view = friendsView();
  for (const v of ['home', 'tab', 'hidden']) {
    document.body.classList.toggle(`friends-view-${v}`, view === v);
  }
  setValue('cfgFriendsView', view);
  // Settings → Home has a switch for it too.
  renderHomeLayoutList();
  // Its nav button just went away: don't leave the user on a tab they can't
  // see the button for.
  if (view !== 'tab' && document.getElementById('tab-friends')?.classList.contains('active')) {
    switchTab('home');
  }
  // Deferred: at startup this runs from loadConfig, possibly before the
  // account section of this file (stellaPlayer) has been evaluated.
  setTimeout(refreshStellaFriends, 0);
}

async function refreshStellaFriends() {
  const net = activeNetwork;
  if (!stellaLike(net)) return;
  // Hidden: no list to fill, so no reason to hold the friends connection
  // open for it. (A profile page still starts it for its own status.)
  if (friendsView() === 'hidden') return;
  if (!socialPlayer()) {
    renderStellaFriends(null);
    return;
  }
  let res;
  try {
    res = net === 'receos' ? await window.radium.receosFriends() : await window.radium.stellaFriends();
  } catch (e) {
    res = { success: false, error: String(e) };
  }
  if (activeNetwork !== net || !socialPlayer()) return;
  if (net === 'receos' && res?.success) receosFriendsList = res.friends || [];
  renderStellaFriends(res);
}

/// Coalesces a burst of presence events into one redraw.
function queueStellaFriends() {
  clearTimeout(stellaFriendsQueued);
  stellaFriendsQueued = setTimeout(refreshStellaFriends, 250);
}

function friendStatusText(f) {
  if (f.status === 'checking') return 'Checking…';
  if (f.status === 'paused') return 'Shown in game';
  // Not heard from, and the friends connection is down (it keeps retrying).
  if (f.status === 'unknown') return 'Status unknown';
  if (f.status !== 'online') return 'Offline';
  if (f.roomName) return f.private ? `In ${f.roomName} · private` : `In ${f.roomName}`;
  if (f.private) return 'In a private room';
  return 'Online';
}

/// The line under a friend's name on their card: where they are, with a lock
/// for a private room, or how they are when not online.
function friendWhere(f) {
  if (f.status !== 'online') return { text: friendStatusText(f), locked: false };
  if (f.private) return { text: f.roomName || 'Private room', locked: true };
  return { text: f.roomName ? `In ${f.roomName}` : 'Online', locked: false };
}

/// What the group of friends who aren't online is called. Once the list has
/// settled they are simply offline; in its first moments some are still being
/// checked, and while the game runs it holds their status, not the launcher.
function friendsOthersLabel(others) {
  const statuses = new Set(others.map(f => f.status));
  if (statuses.has('paused')) return 'Shown in game';
  if (statuses.has('checking')) return 'Checking…';
  if (statuses.has('offline')) return 'Offline';
  return 'Status unknown';
}

/// A padlock, drawn in the text's own colour.
const FRIEND_LOCK_SVG = '<svg viewBox="0 0 12 12" aria-hidden="true"><path d="M3.6 5.2V3.9a2.4 2.4 0 0 1 4.8 0v1.3" fill="none" stroke="currentColor" stroke-width="1.5"/><rect x="2" y="5.2" width="8" height="5.8" rx="1.5" fill="currentColor"/></svg>';

/// The last list the backend sent, kept so typing in the FRIENDS tab's search
/// can redraw it without asking again.
let stellaFriendsRes = null;

/// Players who have sent a friend request, by id (from the last friends list),
/// and their ids as last drawn in the notifications list.
let stellaRequestsById = new Map();
let stellaRequestsKey = '';

/// Fill both places the list can show: the Home card and the FRIENDS tab.
/// Only one is visible at a time, and keeping both current means switching
/// the setting shows a full list straight away.
function renderStellaFriends(res) {
  if (res?.success && res.loaded) {
    stellaFriendsById = new Map((res.friends || []).map(f => [Number(f.id), f]));
    stellaRequestsById = new Map((res.requests || []).map(r => [Number(r.id), r]));
  }
  stellaFriendsRes = res;
  // Redrawing replaces the buttons, so keyboard focus on a star or ACCEPT is
  // put back on the same one afterwards.
  const focused = document.activeElement?.closest?.('[data-friend-action]');
  const refocus = focused && `[data-friend-action="${focused.dataset.friendAction}"]`;
  renderStellaFriendsInto($('stellaFriendsList'), $('stellaFriendsCount'), res, false);
  renderStellaFriendsInto($('stellaFriendsTabList'), $('stellaFriendsTabCount'), res, true);
  if (refocus) focused.closest('#stellaFriendsList, #stellaFriendsTabList')?.querySelector(refocus)?.focus();
  // The notifications list offers ACCEPT on a request still waiting, so it
  // is redrawn when those change.
  const requestsKey = [...stellaRequestsById.keys()].join(',');
  if (requestsKey !== stellaRequestsKey) {
    stellaRequestsKey = requestsKey;
    if (!$('vanillaNotifsPanel')?.hidden && panelSource === notifSources.stella) renderNotifPanel();
  }
}

/// Star or unstar a friend on Stella. Drawn at once, and put back if Stella
/// turns it down.
async function setStellaFavorite(f, on) {
  const id = Number(f.id);
  const name = f.displayName || f.userName || 'your friend';
  const mark = (value) => {
    const row = (stellaFriendsRes?.friends || []).find(x => Number(x.id) === id);
    if (row) row.favorite = value;
    const known = stellaFriendsById.get(id);
    if (known) known.favorite = value;
    if (stellaFriendsRes) renderStellaFriends(stellaFriendsRes);
    paintProfileFavorite(id);
  };
  mark(on);
  try {
    await (activeNetwork === 'receos' ? window.radium.receosSetFriendFavorite(id, on) : window.radium.stellaSetFriendFavorite(id, on));
  } catch (e) {
    mark(!on);
    const why = String(e).trim().replace(/([^.!?])$/, '$1.');
    addLog(`Couldn't ${on ? 'favorite' : 'unfavorite'} ${name}: ${why}`, 'error', 'account');
    toast(`Couldn't ${on ? 'favorite' : 'unfavorite'} ${name}: ${why}`, 'error');
    return;
  }
  addLog(on ? `Added ${name} to your ${socialLabel()} favorites.` : `Removed ${name} from your ${socialLabel()} favorites.`, 'ok', 'account');
}

/// Accept a friend request. `btn` shows it working.
async function acceptStellaRequest(r, btn) {
  const id = Number(r.id);
  const name = r.displayName || r.userName || 'them';
  if (btn) {
    btn.disabled = true;
    btn.dataset.label = btn.textContent;
    btn.textContent = 'ACCEPTING…';
  }
  try {
    await (activeNetwork === 'receos' ? window.radium.receosAcceptFriendRequest(id) : window.radium.stellaAcceptFriendRequest(id));
  } catch (e) {
    if (btn?.isConnected) {
      btn.disabled = false;
      btn.textContent = btn.dataset.label || 'ACCEPT';
    }
    const why = String(e).trim().replace(/([^.!?])$/, '$1.');
    addLog(`Couldn't accept ${name}'s friend request: ${why}`, 'error', 'account');
    toast(`Couldn't accept ${name}'s friend request: ${why}`, 'error');
    return;
  }
  stellaRequestsById.delete(id);
  addLog(`You and ${name} are now friends on ${socialLabel()}.`, 'ok', 'account');
  toast(`You and ${name} are now friends.`, 'ok');
  await refreshStellaFriends();
  if (activeNetwork === 'stella') paintStellaFriendActions(id, playerDetailSeq);
}

function openFriendProfile(f) {
  // A Rec EOS player's profile, rooms and photos are found by username.
  if (activeNetwork === 'receos') {
    showCreatorProfile(f.userName, { id: f.userName, displayName: f.displayName, avatarUrl: f.AvatarUrl });
    return;
  }
  showCreatorProfile(f.userName, {
    id: f.id,
    displayName: f.displayName,
    avatarUrl: f.AvatarUrl
  });
}

function friendSpan(className, text) {
  const el = document.createElement('span');
  el.className = className;
  el.textContent = text;
  return el;
}

/// A friend's picture, `size` CSS pixels square, with the presence dot on its
/// corner. The dot shows for online friends, and grey while their status is
/// still unknown; an offline friend has none.
function friendAvatar(f, size) {
  const pic = document.createElement('span');
  pic.className = 'friend-avatar';
  const img = document.createElement('img');
  img.alt = '';
  img.loading = 'lazy';
  img.dataset.fallback = placeholderAvatar();
  img.src = f.AvatarUrl ? thumbSrc(f.AvatarUrl, avatarWidth(size)) : placeholderAvatar();
  pic.append(img, friendSpan('friend-dot', ''));
  return pic;
}

/// One friend as a card: picture, name, where they are, and JOIN for someone
/// online. The tab's cards are bigger, carry the @handle too, and have the
/// star (`starToggle`); Home's narrower ones only mark a favorite, as its
/// chips do, and leave starring to the profile.
function buildFriendCard(f, size, withHandle, starToggle = false) {
  const name = f.displayName || f.userName || 'Player';
  const card = document.createElement('div');
  card.className = `friend-card is-${f.status}`;

  const main = document.createElement('button');
  main.type = 'button';
  main.className = 'friend-card-main';
  const text = document.createElement('span');
  text.className = 'friend-text';
  const nameEl = friendSpan('friend-name-row', '');
  nameEl.append(friendSpan('friend-name', name));
  if (f.favorite && !starToggle) {
    const mark = friendSpan('vn-icon vn-icon-star friend-chip-fav', '');
    mark.setAttribute('role', 'img');
    mark.setAttribute('aria-label', 'Favorite');
    nameEl.append(mark);
  }
  text.append(nameEl);
  if (withHandle && f.userName) text.append(friendSpan('friend-handle', `@${f.userName}`));
  const where = friendWhere(f);
  const whereEl = friendSpan('friend-where', '');
  if (where.locked) {
    const lock = friendSpan('friend-lock', '');
    lock.setAttribute('role', 'img');
    lock.setAttribute('aria-label', 'Private room:');
    lock.innerHTML = FRIEND_LOCK_SVG;
    whereEl.append(lock);
  }
  whereEl.append(friendSpan('friend-where-text', where.text));
  text.append(whereEl);
  main.append(friendAvatar(f, size), text);
  main.addEventListener('click', () => openFriendProfile(f));
  card.append(main);
  if (starToggle) card.append(buildFriendStar(f, name));

  // Only someone online can be joined.
  if (f.status === 'online') {
    const join = document.createElement('button');
    join.type = 'button';
    join.className = 'modal-btn modal-btn-primary friend-join';
    join.textContent = 'JOIN';
    if (activeNetwork === 'receos') {
      join.setAttribute('aria-label', `Start Rec EOS and join ${name}`);
      join.addEventListener('click', () => joinReceosFriend(f));
    } else {
      join.setAttribute('aria-label', f.private && !stellaInvitedBy(f.id) ? `Ask ${name} to let you into their private room` : `Start Stella and join ${name}`);
      join.addEventListener('click', () => joinStellaFriend(f));
    }
    card.append(join);
  }
  return card;
}

/// The star on a friend's card: starred friends come first, here and in the
/// game (it is saved on Stella).
function buildFriendStar(f, name) {
  const star = document.createElement('button');
  star.type = 'button';
  star.className = 'friend-fav';
  star.dataset.friendAction = `fav-${f.id}`;
  star.setAttribute('aria-pressed', String(!!f.favorite));
  star.setAttribute('aria-label', `Favorite ${name}`);
  const icon = document.createElement('span');
  icon.className = `vn-icon ${f.favorite ? 'vn-icon-star' : 'vn-icon-star-outline'}`;
  icon.setAttribute('aria-hidden', 'true');
  star.append(icon);
  star.addEventListener('click', () => setStellaFavorite(f, !f.favorite));
  return star;
}

/// A friend who isn't online, on the Home card: a small picture and the name,
/// and a star for a favorite. Starring is done on their card or profile.
function buildFriendChip(f) {
  const name = f.displayName || f.userName || 'Player';
  const chip = document.createElement('button');
  chip.type = 'button';
  chip.className = `friend-chip is-${f.status}`;
  chip.setAttribute('aria-label', `${name}, ${f.favorite ? 'favorite, ' : ''}${friendStatusText(f)}`);
  chip.append(friendAvatar(f, 22), friendSpan('friend-chip-name', name));
  if (f.favorite) {
    const star = friendSpan('vn-icon vn-icon-star friend-chip-fav', '');
    star.setAttribute('aria-hidden', 'true');
    chip.append(star);
  }
  chip.addEventListener('click', () => openFriendProfile(f));
  return chip;
}

/// Someone who has sent the player a friend request: their card, with ACCEPT.
function buildRequestCard(r, size) {
  const name = r.displayName || r.userName || 'Player';
  const card = document.createElement('div');
  card.className = 'friend-card friend-request is-request';
  const main = document.createElement('button');
  main.type = 'button';
  main.className = 'friend-card-main';
  const text = document.createElement('span');
  text.className = 'friend-text';
  text.append(friendSpan('friend-name', name));
  const where = friendSpan('friend-where', '');
  where.append(friendSpan('friend-where-text', 'Wants to be your friend'));
  text.append(where);
  main.append(friendAvatar(r, size), text);
  main.addEventListener('click', () => openFriendProfile(r));
  const accept = document.createElement('button');
  accept.type = 'button';
  accept.className = 'modal-btn modal-btn-primary friend-join friend-accept';
  accept.dataset.friendAction = `accept-${r.id}`;
  accept.textContent = 'ACCEPT';
  accept.setAttribute('aria-label', `Accept ${name}'s friend request`);
  accept.addEventListener('click', () => acceptStellaRequest(r, accept));
  card.append(main, accept);
  return card;
}

/// A titled group: "OFFLINE 5", then its cards or chips.
function friendsSection(label, count, body) {
  const section = document.createElement('div');
  section.className = 'friends-section';
  const head = document.createElement('div');
  head.className = 'friends-section-head';
  head.append(friendSpan('friends-section-label', label), friendSpan('friends-section-count', String(count)));
  section.append(head, body);
  return section;
}

function friendsGroup(className, children) {
  const group = document.createElement('div');
  group.className = className;
  group.append(...children);
  return group;
}

/// The Home card (`isTab` false): online friends as cards, everyone else as
/// chips under them. The FRIENDS tab: a section of cards for each, filtered
/// by the tab's search.
function renderStellaFriendsInto(list, count, res, isTab) {
  if (!list || !count) return;
  const note = (text) => {
    const el = document.createElement('div');
    el.className = 'friends-note';
    el.textContent = text;
    list.replaceChildren(el);
  };
  count.classList.remove('has-online');

  if (activeNetwork === 'receos' && !receosPlayer) {
    count.textContent = '';
    note('Sign in to Rec EOS to see your friends.');
    return;
  }
  if (activeNetwork !== 'receos' && !stellaPlayer) {
    count.textContent = '';
    note(stellaAuthPending ? 'Signing in…'
      : stellaAuthError === STELLA_NO_ACCOUNT ? STELLA_NO_ACCOUNT_TEXT
      : 'Log in to Stella to see your friends.');
    return;
  }
  if (!res?.success) {
    count.textContent = '';
    note(res?.error || "Couldn't load your friends.");
    return;
  }
  if (!res.loaded) {
    count.textContent = '';
    note(res.error || 'Loading friends…');
    return;
  }
  const friends = res.friends || [];
  const requests = res.requests || [];
  if (friends.length === 0 && requests.length === 0) {
    count.textContent = '';
    note("No friends yet. Add some in game and they'll show up here.");
    return;
  }

  const onlineCount = friends.filter(f => f.status === 'online').length;
  const total = `${friends.length} ${friends.length === 1 ? 'friend' : 'friends'}`;
  // Paused: one sign-in at a time can watch friends, and the game is using
  // it. It picks up again when the game closes.
  if (res.paused) count.textContent = 'Paused while you play';
  else if (!res.connected && res.error) count.textContent = 'Reconnecting…';
  else {
    count.textContent = `${onlineCount} online · ${total}`;
    count.classList.toggle('has-online', onlineCount > 0);
  }

  let shown = friends;
  let asking = requests;
  if (isTab) {
    const query = ($('stellaFriendsSearch')?.value || '').trim();
    const q = query.toLowerCase();
    const match = f => `${f.displayName || ''}\n${f.userName || ''}`.toLowerCase().includes(q);
    if (q) {
      shown = friends.filter(match);
      asking = requests.filter(match);
    }
    if (!shown.length && !asking.length) {
      note(`No friends match "${query}".`);
      return;
    }
  }
  // Requests waiting for an answer come first, in both places.
  const parts = [];
  if (asking.length) {
    parts.push(friendsSection('Friend requests', asking.length,
      friendsGroup('friends-grid', asking.map(r => buildRequestCard(r, isTab ? 44 : 40)))));
  }
  if (!shown.length) {
    list.replaceChildren(...parts);
    return;
  }

  // The tab gathers the starred friends at the top. On Home they come first
  // among the online cards and among the chips (the list arrives so sorted).
  const favorites = isTab ? shown.filter(f => f.favorite) : [];
  const rest = isTab ? shown.filter(f => !f.favorite) : shown;
  const online = rest.filter(f => f.status === 'online');
  const others = rest.filter(f => f.status !== 'online');

  if (isTab) {
    if (favorites.length) {
      parts.push(friendsSection('Favorites', favorites.length,
        friendsGroup('friends-grid friends-grid-online', favorites.map(f => buildFriendCard(f, 52, true, true)))));
    }
    if (online.length) {
      parts.push(friendsSection('Online', online.length,
        friendsGroup('friends-grid friends-grid-online', online.map(f => buildFriendCard(f, 52, true, true)))));
    }
    if (others.length) {
      parts.push(friendsSection(friendsOthersLabel(others), others.length,
        friendsGroup('friends-grid friends-grid-others', others.map(f => buildFriendCard(f, 36, false, true)))));
    }
  } else {
    if (online.length) {
      parts.push(friendsGroup('friends-grid', online.map(f => buildFriendCard(f, 40, false))));
    } else if (others.every(f => f.status === 'offline')) {
      const none = document.createElement('div');
      none.className = 'friends-note';
      none.textContent = 'None of your friends are online right now.';
      parts.push(none);
    }
    if (others.length) {
      parts.push(friendsSection(friendsOthersLabel(others), others.length,
        friendsGroup('friends-chips', others.map(buildFriendChip))));
    }
  }
  list.replaceChildren(...parts);
}

// The tab's search filters the list it already has.
$('stellaFriendsSearch')?.addEventListener('input', () => {
  renderStellaFriendsInto($('stellaFriendsTabList'), $('stellaFriendsTabCount'), stellaFriendsRes, true);
});
$('stellaFriendsSearch')?.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape' || !e.target.value) return;
  e.target.value = '';
  renderStellaFriendsInto($('stellaFriendsTabList'), $('stellaFriendsTabCount'), stellaFriendsRes, true);
});

window.radium?.onStellaFriends?.(queueStellaFriends);
// "Checking…" turns into "Offline", and a friend gone quiet into offline, by
// time passing rather than by an event, so the card is redrawn now and then.
setInterval(() => {
  if (stellaLike() && socialPlayer() && !document.hidden) refreshStellaFriends();
}, 20000);

// The LOG IN button a signed-out Rooms or People list shows.
document.addEventListener('click', (e) => {
  if (e.target.closest?.('[data-stella-login]')) stellaSignIn();
  if (e.target.closest?.('[data-stella-retry]')) {
    reloadStellaLists();
    refreshStellaAuth();
  }
  // No Stella account yet: the game makes one on its first run.
  if (e.target.closest?.('[data-stella-play]')) $('btnPlay')?.click();
});

document.addEventListener('click', (e) => {
  const menu = $('stellaAccountMenu');
  if (menu && !menu.hidden && !menu.contains(e.target) && !$('stellaAccountBtn')?.contains(e.target)) {
    closeStellaAccountMenu();
  }
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !$('stellaAccountMenu')?.hidden) {
    closeStellaAccountMenu();
    $('stellaAccountBtn')?.focus();
  }
});

// ── Rec EOS account ──────────────────────────────────────────────────────
// Signed in with the Rec EOS username and password (receos_social.rs). That
// opens what Stella's Steam sign-in opens there: friends and where they are,
// friend requests and game invites on the bell, JOIN, room cheers and
// favorites, and Rec EOS's own CHATS tab. The friends list, the bell and the
// room tiles are Stella's, pointed at Rec EOS (see stellaLike/socialPlayer).

let receosPlayer = null;
/// The last friends list, for starting a chat.
let receosFriendsList = [];

/// Stella and Rec EOS share the friends list, invites and room tiles.
function stellaLike(net = activeNetwork) {
  return net === 'stella' || net === 'receos';
}

/// Whoever is signed in to the network on show (Stella or Rec EOS).
function socialPlayer() {
  return activeNetwork === 'receos' ? receosPlayer : stellaPlayer;
}

function socialLabel() {
  return activeNetwork === 'receos' ? 'Rec EOS' : 'Stella';
}

function requireSocialLogin(what) {
  if (activeNetwork !== 'receos') return requireStellaLogin(what);
  if (receosPlayer) return true;
  toast(`Sign in to Rec EOS to ${what}.`, 'info');
  openReceosLogin();
  return false;
}

/// `{ cheered, favorited }` for a room, on Stella or Rec EOS.
async function roomInteraction(roomId) {
  if (activeNetwork === 'receos') {
    const s = await window.radium.receosRoomInteraction(roomId);
    return { cheered: !!s?.Cheered, favorited: !!s?.Favorited };
  }
  return window.radium.stellaRoomInteraction(roomId);
}

async function setRoomInteraction(roomId, kind, on) {
  if (activeNetwork === 'receos') {
    await window.radium.receosSetRoomInteraction(roomId, kind === 'cheer' ? on : null, kind === 'favorite' ? on : null);
    return roomInteraction(roomId);
  }
  return window.radium.stellaSetRoomInteraction(roomId, kind, on);
}

function renderReceosAccount() {
  const name = $('receosAccountName');
  const avatar = $('receosAccountAvatar');
  const btn = $('receosAccountBtn');
  const bell = $('receosNotifsBtn');
  if (!name || !avatar || !btn) return;
  if (receosPlayer) {
    const handle = receosPlayer.userName || 'account';
    const differs = receosPlayer.displayName && receosPlayer.displayName !== handle;
    name.textContent = receosPlayer.displayName || handle;
    btn.setAttribute('aria-label', `Signed in to Rec EOS as ${differs ? `${receosPlayer.displayName} (@${handle})` : `@${handle}`}`);
    avatar.dataset.fallback = placeholderAvatar();
    avatar.src = receosPlayer.AvatarUrl ? thumbSrc(receosPlayer.AvatarUrl, avatarWidth(28)) : placeholderAvatar();
    avatar.hidden = false;
    btn.setAttribute('aria-haspopup', 'menu');
    $('receosAccountFullName').textContent = `@${handle}`;
    const display = $('receosAccountDisplay');
    if (display) {
      display.textContent = differs ? receosPlayer.displayName : '';
      display.hidden = !differs;
    }
    if (bell) bell.hidden = false;
  } else {
    name.textContent = 'LOG IN';
    btn.setAttribute('aria-label', 'Sign in with your Rec EOS account');
    avatar.hidden = true;
    avatar.removeAttribute('src');
    btn.removeAttribute('aria-haspopup');
    if (bell) bell.hidden = true;
    closeReceosAccountMenu();
  }
  fitOneAccountName(name, receosPlayer);
}

function applyReceosAuth(player, { expired = false } = {}) {
  const was = receosPlayer;
  receosPlayer = player || null;
  renderReceosAccount();
  if (was?.id === receosPlayer?.id) return;
  resetNotifs(notifSources.receos);
  receosCursors = null;
  chatThreads = [];
  chatOpenId = null;
  receosFriendsList = [];
  stellaRequestsById = new Map();
  if (receosPlayer) {
    addLog(`Signed in to Rec EOS as @${receosPlayer.userName}`, 'ok', 'account');
    loadNotifications(notifSources.receos);
    pollReceos();
  } else if (was) {
    addLog(expired ? 'Rec EOS sign-in expired, signed out' : 'Signed out of Rec EOS', 'info', 'account');
    if (expired) toast('Your Rec EOS sign-in expired. Please sign in again.', 'info');
  }
  setChatsBadge(0);
  refreshStellaFriends();
  refreshSocialButtons();
  if (document.querySelector('#tab-chats.active')) loadChatThreads();
}

async function refreshReceosAuth() {
  try {
    const s = await window.radium.receosAuthStatus();
    if (s?.authenticated) applyReceosAuth(s.player);
    else if (!s?.hasSession) applyReceosAuth(null, { expired: s?.reason === 'expired' });
  } catch (e) {}
}

/// A call answered "Sign in to Rec EOS.": the token is gone (the backend has
/// already forgotten it).
function receosSignedOutBy(err) {
  if (String(err).includes('Sign in to Rec EOS') && receosPlayer) applyReceosAuth(null, { expired: true });
}

function openReceosLogin() {
  const modal = $('receosLoginModal');
  if (!modal) return;
  $('receosLoginError').hidden = true;
  $('receosLoginPass').value = '';
  showModal(modal);
  setTimeout(() => ($('receosLoginUser').value ? $('receosLoginPass') : $('receosLoginUser')).focus(), 50);
}

function closeReceosLogin() {
  $('receosLoginPass').value = '';
  hideModal($('receosLoginModal'));
}

$('receosLoginForm')?.addEventListener('submit', async (e) => {
  e.preventDefault();
  const user = $('receosLoginUser').value.trim();
  const pass = $('receosLoginPass').value;
  const err = $('receosLoginError');
  const submit = $('receosLoginSubmit');
  if (!user || !pass) return;
  submit.disabled = true;
  submit.textContent = 'Signing in…';
  err.hidden = true;
  try {
    const res = await window.radium.receosLogin(user, pass);
    closeReceosLogin();
    applyReceosAuth(res.player);
    toast(`Signed in as @${res.player.userName}.`, 'ok');
  } catch (e2) {
    err.textContent = String(e2);
    err.hidden = false;
    $('receosLoginPass').select();
  } finally {
    submit.disabled = false;
    submit.textContent = 'Sign in';
  }
});
$('receosLoginCancel')?.addEventListener('click', closeReceosLogin);
$('receosLoginClose')?.addEventListener('click', closeReceosLogin);

function closeReceosAccountMenu() {
  const btn = $('receosAccountBtn');
  hideDropdown($('receosAccountMenu'));
  if (btn) {
    btn.setAttribute('aria-expanded', 'false');
    btn.classList.remove('is-open');
  }
}

$('receosAccountBtn')?.addEventListener('click', () => {
  if (!receosPlayer) {
    openReceosLogin();
    return;
  }
  const menu = $('receosAccountMenu');
  if (menu && !menu.hidden) {
    closeReceosAccountMenu();
    return;
  }
  closeNotifPanel();
  showDropdown(menu);
  $('receosAccountBtn').setAttribute('aria-expanded', 'true');
  $('receosAccountBtn').classList.add('is-open');
});
$('receosProfileBtn')?.addEventListener('click', () => {
  closeReceosAccountMenu();
  if (receosPlayer?.userName) showCreatorProfile(receosPlayer.userName);
});
$('receosLogoutBtn')?.addEventListener('click', async () => {
  closeReceosAccountMenu();
  try { await window.radium.receosLogout(); } catch (e) {}
  applyReceosAuth(null);
});
document.addEventListener('click', (e) => {
  const menu = $('receosAccountMenu');
  if (menu && !menu.hidden && !menu.contains(e.target) && !$('receosAccountBtn')?.contains(e.target)) closeReceosAccountMenu();
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !$('receosAccountMenu')?.hidden) {
    closeReceosAccountMenu();
    $('receosAccountBtn')?.focus();
  }
});

// ── Rec EOS JOIN ─────────────────────────────────────────────────────────
// The game itself does the joining (`+join:<account id>`, the same launch
// argument Stella's game takes); the mod lets friends into private rooms too.

function joinReceosFriend(friend) {
  if (activeNetwork !== 'receos' || !friend?.id) return;
  const name = friend.displayName || friend.userName || 'your friend';
  launchStellaInto({ id: Number(friend.id), name }, name);
}

async function joinFromReceosInvite(id, name = null) {
  if (activeNetwork !== 'receos') {
    await setNetwork('receos');
    if (activeNetwork !== 'receos') return;
  }
  const friend = receosFriendsList.find(f => Number(f.id) === Number(id));
  const who = friend?.displayName || friend?.userName || name || 'your friend';
  launchStellaInto({ id: Number(id), name: who }, who);
}

// ── Rec EOS chats ────────────────────────────────────────────────────────
// The game's own chats: what is written here shows up in the game, and the
// other player's game is told at once (the server pings it to sync). New
// messages are looked for every 20 seconds while signed in, and pop up like
// notifications when the chat isn't open in front of the player.

let chatThreads = [];
let chatOpenId = null;
let chatMessagesShown = [];
let chatLoading = false;
/// `{ message, chat }` cursors for receos_updates; null until the first look.
let receosCursors = null;
const CHAT_POLL_MS = 20 * 1000;

function setChatsBadge(n) {
  const badge = $('chatsBadge');
  if (!badge) return;
  badge.hidden = !n;
  badge.textContent = n > 9 ? '9+' : String(n || '');
}

function chatName(t) {
  if (t.name) return t.name;
  const names = (t.members || []).map(m => m.displayName || m.userName).filter(Boolean);
  return names.length ? names.join(', ') : 'Chat';
}

function chatsNote(text) {
  const el = document.createElement('div');
  el.className = 'chats-note';
  el.textContent = text;
  return el;
}

async function loadChatThreads() {
  const list = $('chatThreadList');
  if (!list || activeNetwork !== 'receos') return;
  if (!receosPlayer) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'modal-btn modal-btn-primary';
    btn.textContent = 'LOG IN';
    btn.addEventListener('click', openReceosLogin);
    const note = chatsNote('Sign in to Rec EOS to see your chats.');
    note.append(document.createElement('br'), document.createElement('br'), btn);
    list.replaceChildren(note);
    $('chatMessages').replaceChildren();
    $('chatTitle').textContent = '';
    $('chatForm').hidden = true;
    return;
  }
  if (!chatThreads.length) list.replaceChildren(chatsNote('Loading…'));
  try {
    chatThreads = await window.radium.receosChatThreads();
  } catch (e) {
    receosSignedOutBy(e);
    list.replaceChildren(chatsNote(`Couldn't load your chats: ${e}`));
    return;
  }
  renderChatList();
  if (chatOpenId && chatThreads.some(t => t.id === chatOpenId)) {
    // Back on the tab: what came while away.
    openChat(chatOpenId);
  } else if (!chatOpenId) {
    renderChatEmpty();
  }
}

function renderChatList() {
  const list = $('chatThreadList');
  if (!list) return;
  // The open chat counts too while the CHATS tab isn't on screen.
  const shown = !!document.querySelector('#tab-chats.active');
  setChatsBadge(chatThreads.filter(t => t.unread && !(shown && t.id === chatOpenId)).length);
  if (!chatThreads.length) {
    list.replaceChildren(chatsNote('No chats yet. Start one with + New chat.'));
    return;
  }
  list.replaceChildren(...chatThreads.map(t => {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'chat-row';
    row.setAttribute('role', 'listitem');
    row.classList.toggle('is-open', t.id === chatOpenId);
    row.classList.toggle('is-unread', !!t.unread && !(shown && t.id === chatOpenId));
    const img = document.createElement('img');
    img.alt = '';
    img.dataset.fallback = placeholderAvatar();
    const pic = t.members?.[0]?.AvatarUrl;
    img.src = pic ? thumbSrc(pic, avatarWidth(34)) : placeholderAvatar();
    const text = document.createElement('span');
    text.className = 'chat-row-text';
    text.append(friendSpan('chat-row-name', chatName(t)));
    const last = t.latest;
    text.append(friendSpan('chat-row-last', last ? `${Number(last.senderId) === Number(receosPlayer?.id) ? 'You: ' : ''}${last.text}` : ''));
    row.append(img, text);
    row.addEventListener('click', () => openChat(t.id));
    return row;
  }));
}

function renderChatEmpty() {
  $('chatTitle').textContent = '';
  $('chatForm').hidden = true;
  $('chatMessages').replaceChildren(chatsNote(chatThreads.length ? 'Pick a chat.' : 'Your Rec EOS chats show up here.'));
}

function chatMessageEl(m) {
  const mine = Number(m.senderId) === Number(receosPlayer?.id);
  const el = document.createElement('div');
  el.className = 'chat-msg';
  el.classList.toggle('is-mine', mine);
  el.classList.toggle('is-other-kind', m.isText === false);
  el.append(friendSpan('chat-msg-bubble', m.text));
  const t = Date.parse(m.sentTime || '');
  const when = isNaN(t) ? '' : new Date(t).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  const who = mine ? '' : (m.senderDisplay || m.senderName || '');
  el.append(friendSpan('chat-msg-meta', [who, when].filter(Boolean).join(' · ')));
  return el;
}

function renderChatMessages({ keepScroll = false } = {}) {
  const box = $('chatMessages');
  if (!box) return;
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  box.replaceChildren(...(chatMessagesShown.length ? chatMessagesShown.map(chatMessageEl) : [chatsNote('No messages yet. Say hi!')]));
  if (!keepScroll || atBottom) box.scrollTop = box.scrollHeight;
}

async function openChat(threadId) {
  chatOpenId = threadId;
  const t = chatThreads.find(x => x.id === threadId);
  $('chatTitle').textContent = t ? chatName(t) : 'Chat';
  $('chatForm').hidden = false;
  renderChatList();
  $('chatMessages').replaceChildren(chatsNote('Loading…'));
  try {
    const msgs = await window.radium.receosChatMessages(threadId);
    if (chatOpenId !== threadId) return;
    chatMessagesShown = msgs;
  } catch (e) {
    receosSignedOutBy(e);
    $('chatMessages').replaceChildren(chatsNote(`Couldn't load this chat: ${e}`));
    return;
  }
  renderChatMessages();
  markChatRead(threadId);
  $('chatInput')?.focus();
}

function markChatRead(threadId) {
  const last = chatMessagesShown[chatMessagesShown.length - 1];
  const t = chatThreads.find(x => x.id === threadId);
  if (t) t.unread = false;
  renderChatList();
  if (last?.id) window.radium.receosChatRead(threadId, last.id).catch(() => {});
}

$('chatForm')?.addEventListener('submit', async (e) => {
  e.preventDefault();
  const input = $('chatInput');
  const text = input.value.trim();
  if (!text || !chatOpenId) return;
  const threadId = chatOpenId;
  $('chatSend').disabled = true;
  try {
    const sent = await window.radium.receosChatSend(threadId, text);
    input.value = '';
    if (chatOpenId === threadId) {
      chatMessagesShown.push({ ...sent, senderId: receosPlayer?.id });
      renderChatMessages();
    }
    const t = chatThreads.find(x => x.id === threadId);
    if (t) {
      t.latest = { ...sent, senderId: receosPlayer?.id };
      chatThreads = [t, ...chatThreads.filter(x => x !== t)];
      renderChatList();
    }
  } catch (err) {
    receosSignedOutBy(err);
    toast(`Couldn't send: ${err}`, 'error');
  } finally {
    $('chatSend').disabled = false;
    input.focus();
  }
});

/// + New chat: pick a friend; their chat opens (made if there is none).
$('btnNewChat')?.addEventListener('click', async () => {
  if (!requireSocialLogin('chat')) return;
  chatOpenId = null;
  chatMessagesShown = [];
  renderChatList();
  $('chatTitle').textContent = 'New chat';
  $('chatForm').hidden = true;
  const box = $('chatMessages');
  box.replaceChildren(chatsNote('Loading your friends…'));
  if (!receosFriendsList.length) {
    try {
      const res = await window.radium.receosFriends();
      if (res?.success) receosFriendsList = res.friends || [];
    } catch (e) {}
  }
  if (!receosFriendsList.length) {
    box.replaceChildren(chatsNote('Add friends in the game to chat with them here.'));
    return;
  }
  const pick = document.createElement('div');
  pick.className = 'chats-friend-pick';
  pick.append(...receosFriendsList.map(f => {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'chat-row';
    const img = document.createElement('img');
    img.alt = '';
    img.dataset.fallback = placeholderAvatar();
    img.src = f.AvatarUrl ? thumbSrc(f.AvatarUrl, avatarWidth(34)) : placeholderAvatar();
    const text = document.createElement('span');
    text.className = 'chat-row-text';
    text.append(friendSpan('chat-row-name', f.displayName || f.userName), friendSpan('chat-row-last', `@${f.userName}`));
    row.append(img, text);
    row.addEventListener('click', () => startChatWith(f));
    return row;
  }));
  box.replaceChildren(pick);
});

async function startChatWith(friend) {
  try {
    const t = await window.radium.receosChatOpen([friend.id]);
    if (!chatThreads.some(x => x.id === t.id)) chatThreads = [t, ...chatThreads];
    if (!document.querySelector('#tab-chats.active')) switchTab('chats');
    openChat(t.id);
  } catch (e) {
    receosSignedOutBy(e);
    toast(`Couldn't open a chat with ${friend.displayName || friend.userName}: ${e}`, 'error');
  }
}

async function openChatFromPopup(threadId) {
  if (activeNetwork !== 'receos') {
    await setNetwork('receos');
    if (activeNetwork !== 'receos') return;
  }
  switchTab('chats');
  if (!chatThreads.some(t => t.id === threadId)) await loadChatThreads();
  openChat(threadId);
}

/// New chat messages (and anything else new) since the last look.
async function pollReceos() {
  if (!receosPlayer) return;
  let res;
  try {
    res = await window.radium.receosUpdates(null, receosCursors ? receosCursors.chat : null);
  } catch (e) {
    receosSignedOutBy(e);
    return;
  }
  const first = !receosCursors;
  receosCursors = { chat: res.chatCursor };
  if (first || !res.chat?.length) return;
  const chatTabOpen = document.querySelector('#tab-chats.active') && activeNetwork === 'receos';
  const looking = chatTabOpen && document.hasFocus();
  for (const m of res.chat) {
    if (looking && m.threadId === chatOpenId) {
      chatMessagesShown.push(m);
    }
  }
  if (looking && res.chat.some(m => m.threadId === chatOpenId)) {
    renderChatMessages({ keepScroll: true });
    markChatRead(chatOpenId);
  }
  // The chat list, with its unread marks.
  if (activeNetwork === 'receos') {
    try { chatThreads = await window.radium.receosChatThreads(); } catch (e) {}
    if (chatOpenId && looking) {
      const t = chatThreads.find(x => x.id === chatOpenId);
      if (t) t.unread = false;
    }
    renderChatList();
  }
  // Pop-ups for what the player isn't looking at.
  const unseen = res.chat.filter(m => !(looking && m.threadId === chatOpenId));
  const prefs = notifPrefs();
  if (unseen.length && prefs.popups && !isGameRunning) {
    const style = notifPopStyle();
    window.radium.desktopNotify(unseen.slice(-3).map(m => {
      const friend = receosFriendsList.find(f => Number(f.id) === Number(m.senderId));
      return {
        id: `receos-chat:${m.threadId}`,
        sender: null,
        icon: null,
        avatar: friend?.AvatarUrl ? thumbSrc(friend.AvatarUrl, avatarWidth(40)) : placeholderAvatar(),
        app: 'Rec EOS',
        parts: [{ t: m.senderDisplay || m.senderName || 'A player', b: true }, { t: `: ${m.text}` }],
        style,
      };
    })).catch(() => {});
    if (prefs.sound) playNotifChime();
  }
  addLog(`${res.chat.length} new chat message${res.chat.length === 1 ? '' : 's'} on Rec EOS.`, 'info', 'account');
}

setInterval(() => {
  if (!receosPlayer) return;
  if (document.hidden && !notifPrefs().popups) return;
  pollReceos();
}, CHAT_POLL_MS);

// Whoever is signed in, from the last run.
setTimeout(refreshReceosAuth, 0);


// ── Vanilla notifications ────────────────────────────────────────────────
// A panel off the bell next to the account button, laid out like the one on
// vanillarec.net. Vanilla and Stella each have their own list and bell (in
// their account slots); the one panel shows whichever bell opened it. Neither
// keeps read state on its side (Vanilla's website keeps it in the browser),
// so neither does this: which ones have been seen is a per-account list in
// localStorage.

/// Each network's notifications. Stella's are the game's messages, with the
/// same type numbers as Vanilla's (both are Rec Room's): see
/// stella_api::stella_notifications.
const notifSources = {
  vanilla: {
    net: 'vanilla', label: 'Vanilla', items: [], loading: false,
    /// Ids already seen for the signed-in account; `null` until its first
    /// load, which only records a baseline (see announceNewNotifs).
    announced: null,
    bell: 'vanillaNotifsBtn', badge: 'vanillaNotifBadge',
    player: () => vanillaPlayer,
    fetch: () => window.radium.vanillaNotifications(),
  },
  stella: {
    net: 'stella', label: 'Stella', items: [], loading: false, announced: null,
    bell: 'stellaNotifsBtn', badge: 'stellaNotifBadge',
    player: () => stellaPlayer,
    fetch: () => window.radium.stellaNotifications(),
  },
  receos: {
    net: 'receos', label: 'Rec EOS', items: [], loading: false, announced: null,
    bell: 'receosNotifsBtn', badge: 'receosNotifBadge',
    player: () => receosPlayer,
    fetch: () => window.radium.receosNotifications(),
  },
};
/// The list the panel shows: the bell that last opened it.
let panelSource = notifSources.vanilla;

/// Wording per notification type, matching the website. `{s}` is the sender
/// and `{m}` a free-text message; both are inserted as text, never markup.
const VANILLA_NOTIF_TEXT = {
  1: '{s} declined your game invite.',
  2: 'Failed to join game.',
  3: 'Party switched activity.',
  4: '{s} sent you a friend request. Accept in-game.',
  5: 'A vote to kick was initiated.',
  6: '{s} invited you to play.',
  7: 'Party switched activity.',
  10: '{s} requested an invite to join you.',
  11: '{s} declined your invite request.',
  20: '{s} is now online.',
  30: '{s}: {m}',
  40: '{s} accepted your friend request.',
  50: '{s} cheered you.',
  51: 'An anonymous player cheered you.',
  60: 'You were added as a room co-owner.',
  61: 'You were removed as a room co-owner.',
  62: '{s} invited you to be a room co-owner.',
  70: '{s} published a new room.',
  80: '{s} is attending your event.',
  81: '{s} invited you to an event.',
  90: '{s} invited you to join a club.',
  91: '{s} joined your club.',
  100: 'Coach: {m}',
};

/// Types that aren't from a person, shown with an icon instead of an avatar.
const VANILLA_SYSTEM_NOTIFS = new Set([2, 3, 5, 7, 51, 60, 61, 100]);

const READ_NOTIFS_CAP = 500;

function readNotifsKey(src) {
  const player = src.player();
  return player ? `radium-${src.net}-read-notifs-${player.id}` : null;
}

function readNotifIds(src) {
  const key = readNotifsKey(src);
  if (!key) return new Set();
  try {
    const list = JSON.parse(localStorage.getItem(key) || '[]');
    return new Set(Array.isArray(list) ? list.map(String) : []);
  } catch (e) {
    return new Set();
  }
}

function markNotifsRead(src, ids) {
  const key = readNotifsKey(src);
  if (!key || !ids.length) return;
  const seen = readNotifIds(src);
  ids.forEach(id => seen.add(String(id)));
  try {
    localStorage.setItem(key, JSON.stringify([...seen].slice(-READ_NOTIFS_CAP)));
  } catch (e) {}
}

function unreadNotifs(src) {
  const seen = readNotifIds(src);
  return src.items.filter(n => n.id != null && !seen.has(String(n.id)));
}

function setNotifBadge(src) {
  const badge = $(src.badge);
  if (!badge) return;
  const count = unreadNotifs(src).length;
  badge.hidden = !count;
  badge.textContent = count > 99 ? '99+' : String(count || '');
  $(src.bell)?.setAttribute('aria-label', count ? `Notifications, ${count} unread` : 'Notifications');
}
function setVanillaBadge() { setNotifBadge(notifSources.vanilla); }

async function loadNotifications(src) {
  const player = src.player();
  if (!player || src.loading) return;
  src.loading = true;
  const panelOpen = () => !$('vanillaNotifsPanel')?.hidden && panelSource === src;
  try {
    const list = await src.fetch();
    if (src.player()?.id !== player.id) return;
    src.items = Array.isArray(list) ? list : [];
    if (src.net === 'stella') noteStellaInvites(src.items);
    setNotifBadge(src);
    announceNewNotifs(src, src.items);
    if (panelOpen()) renderNotifPanel();
  } catch (e) {
    if (panelOpen()) renderNotifPanel(String(e));
  } finally {
    src.loading = false;
  }
}
function loadVanillaNotifications() { return loadNotifications(notifSources.vanilla); }

/// A signed-in account changed: its list (and what was announced) is gone.
function resetNotifs(src) {
  src.items = [];
  src.announced = null;
  setNotifBadge(src);
  if (panelSource === src) closeNotifPanel();
}

function formatNotifTime(iso) {
  const t = Date.parse(iso || '');
  if (isNaN(t)) return '';
  const mins = Math.floor((Date.now() - t) / 60000);
  if (mins < 1) return 'Just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  return new Date(t).toLocaleDateString();
}

/// A Stella room as the game names it: "^RecCenter", or a dorm as it is.
function stellaRoomLabel(room) {
  room = String(room || '');
  if (!room) return 'their room';
  return /'s Dorm$/.test(room) ? room : `^${room}`;
}

/// The message as parts (`{ t, b }`, b for bold), the sender's name in bold.
/// Inserted as text, never markup, both here and in the desktop pop-up.
function notifParts(n, net) {
  // Stella's invites carry the room's name.
  const invite = n.type === 6 && n.message;
  const template = invite ? '{s} invited you to join them in {m}.'
    // Stella's friend requests are accepted here (ACCEPT on the row).
    : n.type === 4 && (net === 'stella' || net === 'receos') ? '{s} sent you a friend request.'
    : VANILLA_NOTIF_TEXT[n.type] || 'New notification from {s}.';
  return template.split(/(\{s\}|\{m\})/).filter(Boolean).map(part =>
    part === '{s}' ? { t: n.senderDisplay || n.senderName || 'A player', b: true }
      : part === '{m}' ? { t: invite ? stellaRoomLabel(n.message) : (n.message || '') }
      : { t: part });
}

function notifMessage(n, net) {
  const frag = document.createDocumentFragment();
  for (const part of notifParts(n, net)) {
    if (part.b) {
      const b = document.createElement('strong');
      b.textContent = part.t;
      frag.appendChild(b);
    } else {
      frag.appendChild(document.createTextNode(part.t));
    }
  }
  return frag;
}

/// The sender's picture, or an icon for notifications no person sent.
function notifAvatar(n, size) {
  if (VANILLA_SYSTEM_NOTIFS.has(n.type) || !n.senderId) {
    const icon = document.createElement('span');
    icon.className = 'notif-avatar notif-avatar-system';
    const glyph = document.createElement('span');
    glyph.className = `vn-icon ${n.type === 51 ? 'vn-icon-thumb' : 'vn-icon-info'}`;
    icon.appendChild(glyph);
    return icon;
  }
  const img = document.createElement('img');
  img.className = 'notif-avatar';
  img.alt = '';
  img.loading = 'lazy';
  // A picture that fails to load gets the placeholder, as every other avatar
  // does, rather than the webview's broken-image mark.
  img.dataset.fallback = placeholderAvatar();
  img.src = n.senderAvatar ? thumbSrc(n.senderAvatar, avatarWidth(size)) : placeholderAvatar();
  return img;
}

/// A Stella invite that can still be used: JOIN on it starts Stella there.
function joinableStellaInvite(src, n) {
  // Rec EOS: the game decides at the door, so any invite offers JOIN.
  if (src.net === 'receos') return n.type === 6 && !!n.senderId;
  return src.net === 'stella' && n.type === 6 && n.senderId && stellaInvitedBy(n.senderId);
}

/// A row's words and when it came.
function notifBody(n, net) {
  const body = document.createElement('span');
  body.className = 'notif-body';
  const text = document.createElement('span');
  text.className = 'notif-text';
  text.appendChild(notifMessage(n, net));
  const time = document.createElement('span');
  time.className = 'notif-time';
  time.textContent = formatNotifTime(n.sentTime);
  body.append(text, time);
  return body;
}

/// A Stella friend request still waiting for an answer: ACCEPT on it.
function acceptableStellaRequest(src, n) {
  return (src.net === 'stella' || src.net === 'receos') && src.net === activeNetwork && n.type === 4 && n.senderId
    ? stellaRequestsById.get(Number(n.senderId)) : null;
}

function notifRow(src, n, unread) {
  const join = joinableStellaInvite(src, n);
  const request = !join && acceptableStellaRequest(src, n);
  if (request) {
    // Two buttons side by side rather than one: the row opens their profile,
    // ACCEPT accepts, so a stray click on the row never accepts anyone.
    const wrap = document.createElement('div');
    wrap.className = 'notif-item notif-item-split';
    wrap.classList.toggle('unread', unread);
    const open = document.createElement('button');
    open.type = 'button';
    open.className = 'notif-item-open';
    open.append(notifAvatar(n, 32), notifBody(n, src.net));
    open.addEventListener('click', () => {
      closeNotifPanel();
      showCreatorProfile(n.senderName, { id: n.senderId, displayName: n.senderDisplay, avatarUrl: n.senderAvatar });
    });
    const accept = document.createElement('button');
    accept.type = 'button';
    accept.className = 'notif-join notif-accept';
    accept.dataset.friendAction = `accept-${request.id}`;
    accept.textContent = 'ACCEPT';
    accept.setAttribute('aria-label', `Accept ${n.senderDisplay || n.senderName || 'their'} friend request`);
    accept.addEventListener('click', () => acceptStellaRequest(request, accept));
    wrap.append(open, accept);
    return wrap;
  }
  const clickable = join || n.senderName;
  const row = document.createElement(clickable ? 'button' : 'div');
  row.className = 'notif-item';
  if (clickable) row.type = 'button';
  row.classList.toggle('unread', unread);
  row.append(notifAvatar(n, 32), notifBody(n, src.net));

  if (join) {
    // The whole row joins; this only says so.
    const chip = document.createElement('span');
    chip.className = 'notif-join';
    chip.textContent = 'JOIN';
    chip.setAttribute('aria-hidden', 'true');
    row.appendChild(chip);
    row.setAttribute('aria-label', `Join ${n.senderDisplay || n.senderName || 'them'} in ${stellaRoomLabel(n.message)}`);
    row.addEventListener('click', () => {
      closeNotifPanel();
      if (src.net === 'receos') joinFromReceosInvite(n.senderId, n.senderDisplay || n.senderName);
      else joinFromStellaInvite(n.senderId);
    });
  } else if (n.senderName) {
    row.addEventListener('click', () => {
      closeNotifPanel();
      // Stella opens a profile by id rather than by searching the name.
      if (src.net === 'stella') {
        showCreatorProfile(n.senderName, { id: n.senderId, displayName: n.senderDisplay, avatarUrl: n.senderAvatar });
      } else {
        showCreatorProfile(n.senderName);
      }
    });
  }
  return row;
}

function notifState(message, loading = false) {
  const el = document.createElement('div');
  el.className = 'notif-empty';
  if (!loading) {
    const icon = document.createElement('span');
    icon.className = 'vn-icon vn-icon-bell notif-empty-icon';
    el.appendChild(icon);
  }
  const text = document.createElement('span');
  text.textContent = message;
  el.appendChild(text);
  return el;
}

function renderNotifPanel(error) {
  const src = panelSource;
  const list = $('vanillaNotifsList');
  if (!list) return;
  const unread = unreadNotifs(src);
  const mark = $('vanillaNotifsMarkRead');
  if (mark) mark.disabled = unread.length === 0;

  if (error && !src.items.length) {
    list.replaceChildren(notifState(error));
    return;
  }
  if (!src.items.length) {
    list.replaceChildren(src.loading ? notifState('Loading…', true) : notifState('No new notifications'));
    return;
  }
  // Unread first, then newest first, as on the website.
  const unreadIds = new Set(unread.map(n => String(n.id)));
  const sorted = [...src.items].sort((a, b) => {
    const au = unreadIds.has(String(a.id)), bu = unreadIds.has(String(b.id));
    if (au !== bu) return au ? -1 : 1;
    return String(b.sentTime || '').localeCompare(String(a.sentTime || ''));
  });
  list.replaceChildren(...sorted.map(n => notifRow(src, n, unreadIds.has(String(n.id)))));
}

/// Place the panel beside the sidebar, bottom-aligned with the bell, and no
/// taller than the room above it (the title bar included).
function positionNotifPanel() {
  const panel = $('vanillaNotifsPanel');
  const bell = $(panelSource.bell);
  const sidebar = document.querySelector('.sidebar');
  if (!panel || !bell || !sidebar) return;
  const gap = 10;
  const bellBox = bell.getBoundingClientRect();
  const titlebar = $('titlebar')?.getBoundingClientRect().bottom || 0;
  panel.style.left = `${Math.round(sidebar.getBoundingClientRect().right + gap)}px`;
  panel.style.bottom = `${Math.max(gap, Math.round(window.innerHeight - bellBox.bottom))}px`;
  panel.style.maxHeight = `${Math.max(200, Math.min(460, Math.round(bellBox.bottom - titlebar - gap)))}px`;
}

function openNotifPanel(src = panelSource) {
  closeAccountMenu();
  closeStellaAccountMenu();
  closeReceosAccountMenu();
  if (panelSource !== src) closeNotifPanel();
  panelSource = src;
  const panel = $('vanillaNotifsPanel');
  const bell = $(src.bell);
  if (!panel) return;
  positionNotifPanel();
  renderNotifPanel();
  showDropdown(panel);
  bell?.setAttribute('aria-expanded', 'true');
  bell?.classList.add('is-open');
  loadNotifications(src);
}

/// Closing counts everything that was on show as seen, like the website.
function closeNotifPanel() {
  const panel = $('vanillaNotifsPanel');
  if (!panel || panel.hidden) return;
  const src = panelSource;
  markNotifsRead(src, src.items.map(n => n.id).filter(id => id != null));
  setNotifBadge(src);
  hideDropdown(panel);
  const bell = $(src.bell);
  bell?.setAttribute('aria-expanded', 'false');
  bell?.classList.remove('is-open');
}

for (const src of Object.values(notifSources)) {
  $(src.bell)?.addEventListener('click', () => {
    const panel = $('vanillaNotifsPanel');
    if (panel && !panel.hidden && panelSource === src) closeNotifPanel(); else openNotifPanel(src);
  });
}

$('vanillaNotifsMarkRead')?.addEventListener('click', () => {
  const src = panelSource;
  markNotifsRead(src, src.items.map(n => n.id).filter(id => id != null));
  setNotifBadge(src);
  renderNotifPanel();
});

window.addEventListener('resize', () => {
  if (!$('vanillaNotifsPanel')?.hidden) positionNotifPanel();
});

// Check for new ones every minute while signed in, so pop-ups arrive close to
// when the notification did. One small request each. While the launcher is
// minimised it only keeps checking if pop-ups are on. Stella's also come
// over its live hub the moment they are sent (onStellaMessage).
setInterval(() => {
  if (document.hidden && !notifPrefs().popups) return;
  for (const src of Object.values(notifSources)) {
    if (src.player()) loadNotifications(src);
  }
}, 60 * 1000);
window.radium?.onStellaMessage?.(() => loadNotifications(notifSources.stella));

// ── Notification pop-ups ─────────────────────────────────────────────────
// Steam-style cards for notifications that arrive while the launcher is
// running, always in their own window in the corner of the screen (see
// desktop_notify.rs), never inside the launcher. What was already there at
// sign-in is not announced.

const NOTIF_POP_MAX = 3;

function announceNewNotifs(src, list) {
  const withIds = list.filter(n => n.id != null);
  if (!src.announced) {
    src.announced = new Set(withIds.map(n => String(n.id)));
    return;
  }
  const fresh = withIds.filter(n => !src.announced.has(String(n.id)));
  fresh.forEach(n => src.announced.add(String(n.id)));

  // Announced whichever network the launcher is showing. These belong to the
  // signed-in account, and while another network was selected they used to
  // be marked as announced without ever popping up — and the bell that would
  // have shown them is hidden there. Opening one switches to its network.
  if (!fresh.length) return;
  const seen = readNotifIds(src);
  const show = fresh
    .filter(n => !seen.has(String(n.id)))
    // A Stella invite has popped up already, with its JOIN (showStellaInvite).
    .filter(n => !(src.net === 'stella' && n.type === 6))
    .sort((a, b) => String(a.sentTime || '').localeCompare(String(b.sentTime || '')));
  if (show.length) deliverNotifPops(src, show);
}

function notifPrefs() {
  return {
    popups: config?.notifPopups !== false,
    sound: config?.notifSound !== false,
  };
}

/// Show new notifications in the desktop pop-up, and play the chime once for
/// the batch. Skipped while the game is running (a topmost window over a
/// full-screen game can knock it out of full screen), and while the launcher
/// is in front with its notification list open, which already shows them.
function deliverNotifPops(src, list, { force = false } = {}) {
  const prefs = notifPrefs();
  if (!force) {
    if (!prefs.popups || isGameRunning) return;
    const listOpen = !$('vanillaNotifsPanel')?.hidden && panelSource === src;
    if (listOpen && document.hasFocus()) return;
  }

  // At most NOTIF_POP_MAX cards: the newest, led by an "N more" card.
  const items = list.length > NOTIF_POP_MAX
    ? [{ more: list.length - (NOTIF_POP_MAX - 1) }, ...list.slice(-(NOTIF_POP_MAX - 1))]
    : list;

  const style = notifPopStyle();
  window.radium.desktopNotify(items.map(it => desktopCard(src, it, style))).catch(() => {});
  if (prefs.sound) playNotifChime();
}

/// A card for the desktop pop-up: plain data, rendered there as text. Its id
/// says whose notification it is (`notif:<network>:<id>`), for when it's clicked.
function desktopCard(src, it, style) {
  // A Rec EOS game invite pops up with its JOIN.
  if (src.net === 'receos' && it.type === 6 && it.senderId) {
    return {
      id: `receos-invite:${it.senderId}`, sender: null, icon: null,
      avatar: it.senderAvatar ? thumbSrc(it.senderAvatar, avatarWidth(40)) : placeholderAvatar(),
      app: src.label, parts: notifParts(it, src.net), action: 'JOIN', style,
    };
  }
  if (it.more) {
    return { id: null, sender: null, icon: 'bell', app: src.label, style,
             parts: [{ t: `${it.more} more new notifications` }] };
  }
  const n = it;
  const system = VANILLA_SYSTEM_NOTIFS.has(n.type) || !n.senderId;
  return {
    id: n.id != null ? `notif:${src.net}:${n.id}` : null,
    sender: n.senderName || null,
    icon: system ? (n.type === 51 ? 'thumb' : 'info') : null,
    // Always the account's network, whichever is showing, so never Radium's
    // default picture; the placeholder rather than nothing, which the pop-up
    // used to fill with the Radium logo.
    avatar: system ? '' : (n.senderAvatar ? thumbSrc(n.senderAvatar, avatarWidth(40)) : placeholderAvatar()),
    app: src.label,
    parts: notifParts(n, src.net),
    style,
  };
}

/// What a pop-up card looks like under the current skin (the `.notif-pop`
/// rules in style.css, applied to a hidden probe), for the desktop window to
/// copy — so it matches whatever theme, or Liquid Glass, is on.
function notifPopStyle() {
  const host = $('toastContainer');
  if (!host) return null;
  const probe = document.createElement('div');
  probe.className = 'network-menu notif-pop';
  probe.style.cssText = 'visibility:hidden;animation:none';
  const avatar = document.createElement('span');
  avatar.className = 'notif-avatar';
  const app = document.createElement('span');
  app.className = 'notif-pop-app';
  const text = document.createElement('span');
  text.className = 'notif-pop-text';
  const name = document.createElement('strong');
  text.appendChild(name);
  probe.append(avatar, app, text);
  host.appendChild(probe);
  const box = getComputedStyle(probe);
  const style = {
    'bg': box.backgroundColor,
    'bg-image': box.backgroundImage,
    'border': `${box.borderTopWidth} ${box.borderTopStyle} ${box.borderTopColor}`,
    'radius': box.borderTopLeftRadius,
    'shadow': insetShadows(box.boxShadow),
    'fg': getComputedStyle(text).color,
    'muted': getComputedStyle(app).color,
    'accent': getComputedStyle(name).color,
    'font': getComputedStyle(text).fontFamily,
    'system-bg': `color-mix(in srgb, ${getComputedStyle(text).color} 12%, transparent)`,
    'avatar-radius': getComputedStyle(avatar).borderTopLeftRadius,
    motion: document.body.classList.contains('animations-enabled'),
    retro: isRetroSkin(),
    // How a retro skin's pop-up comes and goes (see --pop-motion in skins/).
    'retro-motion': getComputedStyle(document.body).getPropertyValue('--pop-motion').trim() || 'rise',
    // Liquid Glass: the pop-up window asks Windows for real blur behind it.
    glass: document.body.classList.contains('glass-enabled'),
  };
  probe.remove();
  return style;
}

// ── The chime ────────────────────────────────────────────────────────────
// Two soft sine notes, synthesised, so there's no sound file to ship. The
// audio context is created on the first click anywhere: a page may not start
// sound before the user has interacted with it.

let chimeCtx = null;
function ensureChimeCtx() {
  try {
    if (!chimeCtx) chimeCtx = new AudioContext();
    if (chimeCtx.state === 'suspended') chimeCtx.resume();
  } catch (e) {}
}
document.addEventListener('pointerdown', ensureChimeCtx, { once: true, capture: true });

let lastChimeAt = 0;
function playNotifChime() {
  const now = performance.now();
  if (now - lastChimeAt < 1500) return;
  lastChimeAt = now;
  ensureChimeCtx();
  if (!chimeCtx) return;
  try {
    const t = chimeCtx.currentTime + 0.01;
    [[880, 0], [1318.5, 0.1]].forEach(([freq, delay]) => {
      const osc = chimeCtx.createOscillator();
      const gain = chimeCtx.createGain();
      osc.type = 'sine';
      osc.frequency.value = freq;
      gain.gain.setValueAtTime(0.0001, t + delay);
      gain.gain.exponentialRampToValueAtTime(0.16, t + delay + 0.015);
      gain.gain.exponentialRampToValueAtTime(0.0001, t + delay + 0.4);
      osc.connect(gain).connect(chimeCtx.destination);
      osc.start(t + delay);
      osc.stop(t + delay + 0.45);
    });
  } catch (e) {}
}

// A desktop card was clicked: the launcher has already been brought forward.
window.radium?.onDesktopNotifOpen?.(async (card) => {
  if (card?.id === 'tray-hint') return;
  const invite = /^stella-invite:(\d+)$/.exec(String(card?.id ?? ''));
  if (invite) {
    joinFromStellaInvite(invite[1]);
    return;
  }
  const receosInvite = /^receos-invite:(\d+)$/.exec(String(card?.id ?? ''));
  if (receosInvite) {
    joinFromReceosInvite(receosInvite[1]);
    return;
  }
  const chat = /^receos-chat:(\d+)$/.exec(String(card?.id ?? ''));
  if (chat) {
    openChatFromPopup(Number(chat[1]));
    return;
  }
  const own = /^notif:(vanilla|stella|receos):(.+)$/.exec(String(card?.id ?? ''));
  // The pop-up hands back only the id and sender, so an id-less card ("N
  // more") is taken as Vanilla's.
  const src = notifSources[own?.[1] || 'vanilla'];
  if (own) {
    markNotifsRead(src, [own[2]]);
    setNotifBadge(src);
  }
  // Profiles and the notification list are the network's own. setNetwork()
  // refuses (and says why) during a download or while the game runs; the
  // launcher is already in front, so that is where it stops.
  if (activeNetwork !== src.net) {
    await setNetwork(src.net);
    if (activeNetwork !== src.net) return;
  }
  const n = own && src.items.find(x => String(x.id) === own[2]);
  if (src.net === 'stella' && n?.senderId) {
    showCreatorProfile(n.senderName, { id: n.senderId, displayName: n.senderDisplay, avatarUrl: n.senderAvatar });
  } else if (card?.sender) {
    showCreatorProfile(String(card.sender));
  } else if (src.player()) {
    openNotifPanel(src);
  }
});

// Settings → Send a test pop-up.
// From the network the page is open on, so the card says Vanilla or Stella.
$('btnTestNotif')?.addEventListener('click', () => {
  const src = notifSources[activeNetwork] || notifSources.vanilla;
  const player = src.player();
  deliverNotifPops(src, [{
    id: null,
    type: 4,
    senderId: player?.id || 1,
    senderName: player?.userName || 'Coach',
    senderAvatar: player?.AvatarUrl || '',
    sentTime: new Date().toISOString(),
  }], { force: true });
});

/// The retro skins, whose desktop pop-up slides up like Steam's old one
/// instead of fading in. Everything except the modern family and Liquid Glass.
const MODERN_SKIN_CLASSES = ['theme-neondark', 'theme-modernlight', 'theme-moderngreen',
                             'theme-blackandwhite', 'theme-blackandwhite-inverted'];
function isRetroSkin() {
  const cl = document.body.classList;
  return !cl.contains('glass-enabled') && !MODERN_SKIN_CLASSES.some(c => cl.contains(c));
}

document.addEventListener('click', (e) => {
  const menu = $('vanillaAccountMenu');
  if (menu && !menu.hidden && !menu.contains(e.target) && !$('vanillaAccountBtn')?.contains(e.target)) {
    closeAccountMenu();
  }
  const panel = $('vanillaNotifsPanel');
  if (panel && !panel.hidden && !panel.contains(e.target) && !$(panelSource.bell)?.contains(e.target)) {
    closeNotifPanel();
  }
});
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  if (!$('vanillaAccountMenu')?.hidden) {
    closeAccountMenu();
    $('vanillaAccountBtn')?.focus();
  }
  if (!$('vanillaNotifsPanel')?.hidden) {
    closeNotifPanel();
    $(panelSource.bell)?.focus();
  }
});

// ── Vanilla cheers, subscriptions and Play ───────────────────────────────
// All of these act as the signed-in account, so each one offers the sign-in
// window instead when nobody is signed in.

/// The room, photo and player whose detail views are open, so their buttons
/// can be repainted when the account changes.
let socialRoom = null;
let socialPhoto = null;
let socialPerson = null;
/// Ids (as strings) of photos the account has cheered; `null` until fetched.
let cheeredPhotoIds = null;
/// The Play request in progress, if any: `{ roomId, cancelled, label }`.
let roomJoin = null;

const sleep = ms => new Promise(r => setTimeout(r, ms));

function requireStellaLogin(what) {
  if (stellaPlayer) return true;
  toast(`Log in to Stella to ${what}.`, 'info');
  stellaSignIn();
  return false;
}

function requireVanillaLogin(what) {
  if (vanillaPlayer) return true;
  toast(`Sign in to Vanilla to ${what}.`, 'info');
  window.radium.vanillaLogin().catch(e => toast(String(e), 'error'));
  return false;
}

/// Cheer tiles: pressed state only; the icon and count stay as they are.
function setCheerPressed(btn, on) {
  if (!btn) return;
  btn.classList.toggle('is-on', on);
  btn.setAttribute('aria-pressed', String(on));
  // aria-pressed already says whether it is cheered; no `title` tooltip.
  if (btn.classList.contains('card-cheer')) btn.setAttribute('aria-label', `${btn.querySelector('.cheers-count')?.textContent || 0} cheers`);
}

/// Cheer tiles are only buttons on Vanilla and Stella; on Radium they are
/// plain stats.
function setCheerEnabled(btn, enabled) {
  if (!btn) return;
  btn.disabled = !enabled;
  if (!enabled) {
    setCheerPressed(btn, false);
  }
}

function bumpCount(el, delta) {
  // Only plain counts ("1,234"); an abbreviated or missing one is left alone.
  const text = String(el?.textContent ?? '').trim();
  if (!/^[\d,]+$/.test(text)) return;
  el.textContent = Math.max(0, parseInt(text.replace(/,/g, ''), 10) + delta).toLocaleString();
}

// Room cheer ─────────────────────────────────────────────────────────────

async function paintRoomCheer(room) {
  const btn = $('roomsDetailCheerBtn');
  const fav = $('roomsDetailFavoriteBtn');
  const stella = stellaLike();
  setCheerEnabled(btn, !!room);
  setCheerPressed(btn, false);
  // Favorite is Stella's only; there it takes the cheer tile's look too.
  fav?.classList.toggle('cheer-stat', stella);
  setCheerEnabled(fav, stella && !!room);
  setCheerPressed(fav, false);
  if (!btn || !room) return;
  if (stella) {
    if (!socialPlayer()) return;
    try {
      const state = await roomInteraction(room.RoomId);
      if (socialRoom === room) {
        setCheerPressed(btn, !!state.cheered);
        setCheerPressed(fav, !!state.favorited);
      }
    } catch (e) {}
    return;
  }
  if (!vanillaPlayer) return;
  try {
    const on = await window.radium.vanillaRoomCheered(room.RoomId);
    if (socialRoom === room) setCheerPressed(btn, on);
  } catch (e) {}
}

/// Stella: cheer or favorite the open room from its tile. The tile and count
/// change at once and are put back if Stella says no.
async function toggleStellaRoom(btn, kind, countEl) {
  const room = socialRoom;
  if (!room || btn.dataset.pending === 'true') return;
  if (!requireSocialLogin(kind === 'cheer' ? 'cheer rooms' : 'favorite rooms')) return;
  const next = btn.getAttribute('aria-pressed') !== 'true';
  btn.dataset.pending = 'true';
  setCheerPressed(btn, next);
  bumpCount(countEl, next ? 1 : -1);
  try {
    const state = await setRoomInteraction(room.RoomId, kind, next);
    const now = kind === 'cheer' ? !!state.cheered : !!state.favorited;
    // Stella's answer is the truth; it can differ if the tile was stale.
    if (socialRoom === room && now !== next) {
      setCheerPressed(btn, now);
      bumpCount(countEl, now ? 1 : -1);
    }
  } catch (err) {
    if (socialRoom === room) {
      setCheerPressed(btn, !next);
      bumpCount(countEl, next ? -1 : 1);
    }
    toast(`Couldn't update ${kind === 'cheer' ? 'cheer' : 'favorite'}: ${err}`, 'error');
  } finally {
    btn.dataset.pending = 'false';
  }
}

$('roomsDetailFavoriteBtn')?.addEventListener('click', (e) => {
  if (!stellaLike()) return;
  toggleStellaRoom(e.currentTarget, 'favorite', $('roomsDetailFavorites'));
});

$('roomsDetailCheerBtn')?.addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  if (stellaLike()) {
    toggleStellaRoom(btn, 'cheer', $('roomsDetailCheers'));
    return;
  }
  const room = socialRoom;
  if (!room || btn.dataset.pending === 'true') return;
  if (!requireVanillaLogin('cheer rooms')) return;

  const next = btn.getAttribute('aria-pressed') !== 'true';
  btn.dataset.pending = 'true';
  setCheerPressed(btn, next);
  bumpCount($('roomsDetailCheers'), next ? 1 : -1);
  try {
    await window.radium.vanillaSetRoomCheer(room.RoomId, next);
  } catch (err) {
    if (socialRoom === room) {
      setCheerPressed(btn, !next);
      bumpCount($('roomsDetailCheers'), next ? -1 : 1);
    }
    toast(`Couldn't update cheer: ${err}`, 'error');
  } finally {
    btn.dataset.pending = 'false';
  }
});

// Photo cheer ────────────────────────────────────────────────────────────
// The same photo can be on screen more than once (a feed card, a room's
// grid, the detail view), so every control for it is kept in step through
// `data-photo-id`, and one toggle at a time is allowed per photo.

let cheeredPhotosLoad = null;
const photoCheerPending = new Set();

const photoIdOf = photo => String(photo?.Id ?? photo?.id ?? '');

/// Whether a photo gets a cheer button: Vanilla and Stella can cheer photos.
const photoCheerable = photo =>
  (activeNetwork === 'vanilla' || activeNetwork === 'stella') && (photo?.Id ?? photo?.id) != null;

/// Stella: photo id → cheered, for the signed-in account. Stella has no list
/// of everything an account has cheered, only a per-photo check, so the
/// photos on screen are asked about together (see stellaPhotoCheered).
let stellaCheerKnown = new Map();
let stellaCheerBatch = null;

/// Whether the Stella account has cheered photo `id`. Every photo asked about
/// in the same moment (a page of cards) goes in one request.
function stellaPhotoCheered(id) {
  if (!stellaPlayer) return Promise.resolve(false);
  if (stellaCheerKnown.has(id)) return Promise.resolve(stellaCheerKnown.get(id));
  if (!stellaCheerBatch) {
    const account = stellaPlayer.id;
    const batch = { ids: new Set() };
    batch.done = sleep(0)
      .then(() => {
        if (stellaCheerBatch === batch) stellaCheerBatch = null;
        return window.radium.stellaCheeredPhotos([...batch.ids]);
      })
      .then(cheered => {
        if (stellaPlayer?.id !== account) return;
        const on = new Set(cheered.map(String));
        for (const i of batch.ids) stellaCheerKnown.set(i, on.has(i));
      })
      .catch(() => {});
    stellaCheerBatch = batch;
  }
  stellaCheerBatch.ids.add(id);
  return stellaCheerBatch.done.then(() => !!stellaCheerKnown.get(id));
}

/// Whether the signed-in account has cheered photo `id`, on either network.
async function photoCheered(id) {
  if (activeNetwork === 'stella') return stellaPhotoCheered(id);
  const set = await cheeredPhotoSet();
  return !!set && set.has(id);
}

/// The account's cheered photo ids, fetched once and shared by every card.
function cheeredPhotoSet() {
  if (!vanillaPlayer) return Promise.resolve(null);
  if (cheeredPhotoIds) return Promise.resolve(cheeredPhotoIds);
  if (!cheeredPhotosLoad) {
    const account = vanillaPlayer.id;
    cheeredPhotosLoad = window.radium.vanillaCheeredPhotos()
      .then(list => {
        if (vanillaPlayer?.id !== account) return null;
        cheeredPhotoIds = new Set(list);
        return cheeredPhotoIds;
      })
      .catch(() => null)
      .finally(() => { cheeredPhotosLoad = null; });
  }
  return cheeredPhotosLoad;
}

/// Every cheer control for one photo: its cards, and the detail view if open.
function photoCheerControls(id) {
  const controls = [...document.querySelectorAll('.card-cheer')].filter(el => el.dataset.photoId === id);
  const detail = $('photoDetailCheerBtn');
  if (detail && socialPhoto && photoIdOf(socialPhoto) === id) controls.push(detail);
  return controls;
}

function showPhotoCheer(id, cheered, count) {
  for (const btn of photoCheerControls(id)) {
    if (count != null) {
      const el = btn.querySelector('.cheers-count, #photoDetailCheers');
      if (el) el.textContent = Number(count).toLocaleString();
    }
    setCheerPressed(btn, cheered);
  }
}

async function togglePhotoCheer(photo) {
  const id = photoIdOf(photo);
  if (!id || photoCheerPending.has(id)) return;
  if (activeNetwork === 'stella') {
    toggleStellaPhotoCheer(photo, id);
    return;
  }
  if (!requireVanillaLogin('cheer photos')) return;

  photoCheerPending.add(id);
  try {
    // Vanilla toggles and reports the result, so nothing is guessed here.
    const res = await window.radium.vanillaTogglePhotoCheer(id);
    if (cheeredPhotoIds) {
      if (res.cheered) cheeredPhotoIds.add(id); else cheeredPhotoIds.delete(id);
    }
    if (res.cheerCount != null) photo.CheerCount = res.cheerCount;
    showPhotoCheer(id, res.cheered, res.cheerCount);
  } catch (err) {
    toast(`Couldn't update cheer: ${err}`, 'error');
  } finally {
    photoCheerPending.delete(id);
  }
}

/// Stella sets a cheer rather than toggling it, and answers with nothing, so
/// the new state comes from the one known before and the count is moved here.
async function toggleStellaPhotoCheer(photo, id) {
  if (!requireStellaLogin('cheer photos')) return;
  photoCheerPending.add(id);
  try {
    const next = !(await stellaPhotoCheered(id));
    await window.radium.stellaSetPhotoCheer(id, next);
    stellaCheerKnown.set(id, next);
    const count = Math.max(0, (Number(photo.CheerCount ?? photo.cheerCount) || 0) + (next ? 1 : -1));
    photo.CheerCount = count;
    showPhotoCheer(id, next, count);
  } catch (err) {
    // Most likely cheered or un-cheered somewhere else (the game) meanwhile:
    // ask again, so the button shows what Stella has.
    stellaCheerKnown.delete(id);
    showPhotoCheer(id, await stellaPhotoCheered(id));
    toast(`Couldn't update cheer: ${err}`, 'error');
  } finally {
    photoCheerPending.delete(id);
  }
}

/// The thumbs-up pill in a photo card's footer.
function buildCardCheer(photo) {
  const id = photoIdOf(photo);
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'card-cheer';
  btn.dataset.photoId = id;

  const icon = document.createElement('span');
  icon.className = 'vn-icon vn-icon-thumb cheer-icon';
  icon.setAttribute('aria-hidden', 'true');
  const count = document.createElement('span');
  count.className = 'cheers-count';
  count.textContent = (Number(photo.CheerCount ?? photo.cheerCount) || 0).toLocaleString();
  btn.append(icon, count);

  setCheerPressed(btn, false);
  paintCardCheer(btn);
  btn.addEventListener('click', (e) => {
    e.stopPropagation();
    togglePhotoCheer(photo);
  });
  return btn;
}

async function paintCardCheer(btn) {
  setCheerPressed(btn, await photoCheered(btn.dataset.photoId));
}

async function paintPhotoCheer(photo) {
  const btn = $('photoDetailCheerBtn');
  setCheerEnabled(btn, !!photo);
  setCheerPressed(btn, false);
  if (!btn || !photo) return;
  const cheered = await photoCheered(photoIdOf(photo));
  if (socialPhoto === photo) setCheerPressed(btn, cheered);
}

$('photoDetailCheerBtn')?.addEventListener('click', () => {
  if (socialPhoto) togglePhotoCheer(socialPhoto);
});

// Subscribe ──────────────────────────────────────────────────────────────

function setSubscribed(btn, on) {
  if (!btn) return;
  btn.setAttribute('aria-pressed', String(on));
  btn.textContent = on ? 'SUBSCRIBED' : 'SUBSCRIBE';
  // Filled while it is an invitation, quiet once it's done — as on the site.
  btn.classList.toggle('modal-btn-primary', !on);
  btn.classList.toggle('modal-btn-secondary', on);
}

async function paintSubscribe(person) {
  const btn = $('peopleDetailSubscribeBtn');
  if (!btn) return;
  setSubscribed(btn, false);
  const id = Number(person?.id);
  // Hidden on your own profile and on rows without a real id.
  btn.hidden = !person || !id || id === vanillaPlayer?.id;
  if (btn.hidden || !vanillaPlayer) return;
  try {
    const on = await window.radium.vanillaSubscribed(id);
    if (socialPerson === person) setSubscribed(btn, on);
  } catch (e) {}
}

$('peopleDetailSubscribeBtn')?.addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  const person = socialPerson;
  const id = Number(person?.id);
  if (!id || btn.dataset.pending === 'true') return;
  if (!requireVanillaLogin('subscribe to players')) return;

  const next = btn.getAttribute('aria-pressed') !== 'true';
  btn.dataset.pending = 'true';
  setSubscribed(btn, next);
  bumpCount($('peopleDetailSubscribers'), next ? 1 : -1);
  try {
    await window.radium.vanillaSetSubscribed(id, next);
  } catch (err) {
    if (socialPerson === person) {
      setSubscribed(btn, !next);
      bumpCount($('peopleDetailSubscribers'), next ? -1 : 1);
    }
    toast(`Couldn't update subscription: ${err}`, 'error');
  } finally {
    btn.dataset.pending = 'false';
  }
});

// Play (join a room) ─────────────────────────────────────────────────────

/// How long Play keeps retrying while the game starts and the player signs in.
const JOIN_WAIT_MS = 3 * 60 * 1000;
const JOIN_RETRY_MS = 5000;

function paintPlayButton() {
  const btn = $('roomsDetailPlay');
  if (!btn) return;
  const waiting = roomJoin && socialRoom && roomJoin.roomId === socialRoom.RoomId;
  const label = btn.querySelector('.room-play-label') || btn;
  label.textContent = waiting ? roomJoin.label : '▶ PLAY';
  btn.classList.toggle('is-busy', !!waiting);
  btn.setAttribute('aria-label', waiting ? 'Cancel joining this room' : 'Join this room in the game');
}

function setJoinLabel(job, label) {
  job.label = label;
  if (roomJoin === job) paintPlayButton();
}

/// Start the game through the same checks the Home Play button runs.
function startGameForJoin() {
  if (isGameRunning || isGameLaunching || !isInstalled) return;
  isGameLaunching = true;
  checkAvAndLaunch();
}

const normRoom = s => String(s ?? '').toLowerCase().replace('^', '').trim();

/// After Vanilla accepts the join, watch `/me` until the game is in the room.
async function confirmJoined(job, room) {
  const until = Date.now() + 30000;
  while (!job.cancelled && Date.now() < until) {
    try {
      const cur = await window.radium.vanillaCurrentRoom();
      if (cur && (normRoom(cur.id) === normRoom(room.RoomId) || normRoom(cur.name) === normRoom(room.Name))) {
        return true;
      }
    } catch (e) {}
    await sleep(1500);
  }
  return false;
}

async function playRoom(room) {
  const job = { roomId: room.RoomId, cancelled: false, label: 'JOINING…' };
  roomJoin = job;
  paintPlayButton();
  const roomName = room.Name || 'the room';
  let prompted = false;
  const deadline = Date.now() + JOIN_WAIT_MS;

  try {
    while (!job.cancelled) {
      let res;
      try {
        res = await window.radium.vanillaJoinRoom(room.RoomId);
      } catch (err) {
        toast(`Couldn't join: ${err}`, 'error');
        return;
      }
      if (job.cancelled) return;

      if (res.success) {
        setJoinLabel(job, 'JOINING…');
        addLog(`Vanilla is sending your game to ^${roomName}`, 'info', 'game');
        if (await confirmJoined(job, room)) {
          toast(`Joined ^${roomName}`, 'ok');
          addLog(`Joined ^${roomName}`, 'ok', 'game');
          setJoinLabel(job, '✓ JOINED');
          await sleep(2000);
        } else if (!job.cancelled) {
          toast(`Sent to ^${roomName}. Check your game.`, 'info');
        }
        return;
      }

      if (!res.notOnline) {
        toast(res.message || `Vanilla couldn't join ^${roomName}.`, 'error');
        return;
      }

      // The game isn't running, or isn't signed in yet. Start it once, then
      // keep asking until the player is in, or time runs out.
      if (!prompted) {
        prompted = true;
        if (!isGameRunning) {
          if (!isInstalled) {
            toast('Install the Vanilla client first.', 'error');
            return;
          }
          toast('Starting the game. Sign in to your Vanilla account in-game and you will join automatically.', 'info', 7000);
          startGameForJoin();
        } else {
          toast('Sign in to your Vanilla account in-game. You will join automatically.', 'info', 7000);
        }
      }
      if (Date.now() > deadline) {
        toast('Gave up waiting for the game. Press Play again once you are signed in in-game.', 'error', 6000);
        return;
      }
      setJoinLabel(job, 'WAITING FOR GAME…');
      await sleep(JOIN_RETRY_MS);
    }
  } finally {
    if (roomJoin === job) roomJoin = null;
    paintPlayButton();
  }
}

$('roomsDetailPlay')?.addEventListener('click', () => {
  const room = socialRoom;
  if (!room || !room.RoomId) return;
  if (stellaLike()) {
    playStellaRoom(room);
    return;
  }
  // A second click on the room already joining cancels it.
  if (roomJoin && roomJoin.roomId === room.RoomId) {
    roomJoin.cancelled = true;
    roomJoin = null;
    paintPlayButton();
    return;
  }
  if (!requireVanillaLogin('join rooms')) return;
  // Only one join at a time: a new room replaces the old request.
  if (roomJoin) roomJoin.cancelled = true;
  playRoom(room);
});

// Entry points from the detail views ────────────────────────────────────

function setupRoomSocial(room) {
  socialRoom = (activeNetwork === 'vanilla' || stellaLike()) && room?.RoomId ? room : null;
  paintPlayButton();
  paintRoomCheer(socialRoom);
}

function setupPhotoSocial(photo) {
  socialPhoto = photoCheerable(photo) ? photo : null;
  paintPhotoCheer(socialPhoto);
}

function setupPersonSocial(person) {
  socialPerson = activeNetwork === 'vanilla' ? person : null;
  paintSubscribe(socialPerson);
}

function refreshSocialButtons() {
  document.querySelectorAll('.card-cheer').forEach(btn => {
    setCheerPressed(btn, false);
    paintCardCheer(btn);
  });
  if (socialRoom) paintRoomCheer(socialRoom);
  if (socialPhoto) paintPhotoCheer(socialPhoto);
  if (socialPerson) paintSubscribe(socialPerson);
}

(async () => {
  if (!window.radium?.onVanillaAuth) return;
  await window.radium.onVanillaAuth(applyVanillaAuth);
  try {
    applyVanillaAuth(await window.radium.vanillaAuthStatus());
  } catch (e) {}
})();

// ── Custom selects ───────────────────────────────────────────────────────
// Every <select class="cfg-input"> is rebuilt as a button and a menu — see the
// .cselect rules in style.css for why. The native <select> is kept, hidden, as
// the source of truth: the existing code goes on reading .value, listening for
// `change` and toggling .disabled on it, and the custom control follows along.

/// The trigger, label and menu built for each enhanced select.
const cselectParts = new WeakMap();

/// The select whose menu is open. One at a time, like the other dropdowns.
let openCselect = null;

/// The nearest ancestor that scrolls vertically — the box an open menu is
/// clipped by — or <body> when nothing between does.
function scrollParentOf(el) {
  for (let node = el.parentElement; node && node !== document.body; node = node.parentElement) {
    const overflowY = getComputedStyle(node).overflowY;
    if (overflowY === 'auto' || overflowY === 'scroll') return node;
  }
  return document.body;
}

function enhanceSelects() {
  document.querySelectorAll('select.cfg-input').forEach(enhanceSelect);
}

function enhanceSelect(select) {
  if (cselectParts.has(select)) return;

  const wrap = document.createElement('div');
  wrap.className = 'cselect';

  const trigger = document.createElement('button');
  trigger.type = 'button';
  trigger.className = 'cfg-input cselect-trigger';
  // Inline sizing in the markup was written for the control that is drawn.
  const inline = select.getAttribute('style');
  if (inline) trigger.setAttribute('style', inline);
  trigger.setAttribute('aria-haspopup', 'listbox');
  trigger.setAttribute('aria-expanded', 'false');

  const label = document.createElement('span');
  label.className = 'cselect-label';
  const caret = document.createElement('span');
  caret.className = 'cselect-caret';
  caret.setAttribute('aria-hidden', 'true');
  trigger.append(label, caret);

  const menu = document.createElement('div');
  menu.className = 'cselect-menu';
  menu.setAttribute('role', 'listbox');
  menu.hidden = true;

  if (select.id) {
    menu.id = `${select.id}-menu`;
    trigger.setAttribute('aria-controls', menu.id);
    // The <label for> still names the hidden select; let it name the button.
    const lbl = document.querySelector(`label[for="${CSS.escape(select.id)}"]`);
    if (lbl) {
      if (!lbl.id) lbl.id = `${select.id}-label`;
      trigger.setAttribute('aria-labelledby', lbl.id);
    }
  }

  for (const opt of select.options) {
    const item = document.createElement('button');
    item.type = 'button';
    // .nav-btn so every skin paints the rows as it paints its nav rows. The
    // option's own classes come along, so one marked network-only-stella
    // (Rooms' Most Players) hides with its network.
    item.className = 'nav-btn cselect-option' + (opt.className ? ` ${opt.className}` : '');
    item.setAttribute('role', 'option');
    item.dataset.value = opt.value;
    item.textContent = opt.textContent;
    item.addEventListener('click', () => chooseCselectOption(select, opt.value));
    menu.appendChild(item);
  }

  select.parentNode.insertBefore(wrap, select);
  wrap.append(select, trigger, menu);
  select.tabIndex = -1;
  select.setAttribute('aria-hidden', 'true');
  cselectParts.set(select, { wrap, trigger, label, menu });

  // setValue() and anything else assigning .value fire no event, so the
  // assignment itself is intercepted to keep the label in step.
  const native = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value');
  Object.defineProperty(select, 'value', {
    configurable: true,
    get() { return native.get.call(this); },
    set(v) { native.set.call(this, v); syncCselect(this); },
  });
  select.addEventListener('change', () => syncCselect(select));
  // .disabled reflects to the attribute, which is what the bug-report
  // cooldown and the glass lock on Active Skin both toggle.
  new MutationObserver(() => syncCselect(select))
    .observe(select, { attributes: true, attributeFilter: ['disabled'] });

  trigger.addEventListener('click', () => {
    if (openCselect === select) closeCselect(); else openCselectMenu(select);
  });
  trigger.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      openCselectMenu(select);
    }
  });

  syncCselect(select);
}

function syncCselect(select) {
  const parts = cselectParts.get(select);
  if (!parts) return;
  const current = select.options[select.selectedIndex];
  parts.label.textContent = current ? current.textContent : '';
  parts.trigger.disabled = select.disabled;
  for (const item of parts.menu.children) {
    const on = item.dataset.value === select.value;
    item.classList.toggle('active', on);
    item.setAttribute('aria-selected', on ? 'true' : 'false');
  }
  if (select.disabled && openCselect === select) closeCselect();
}

function chooseCselectOption(select, value) {
  const changed = select.value !== value;
  select.value = value;
  closeCselect(true);
  if (changed) select.dispatchEvent(new Event('change', { bubbles: true }));
}

function openCselectMenu(select) {
  const parts = cselectParts.get(select);
  if (!parts || select.disabled) return;
  if (openCselect && openCselect !== select) closeCselect();
  openCselect = select;

  parts.wrap.closest('.settings-group')?.classList.add('cselect-host');
  parts.menu.classList.remove('opens-up');
  showDropdown(parts.menu);

  // Open upward when the menu would run past the bottom of whatever clips it
  // and there is more room above — the bug-report selects are the last thing
  // on Settings. Measured against the nearest ancestor that actually scrolls:
  // on Settings that is .settings-body, not the tab, whose rect also takes in
  // the page title the menu cannot draw over.
  const bounds = scrollParentOf(parts.wrap).getBoundingClientRect();
  const t = parts.trigger.getBoundingClientRect();
  const below = bounds.bottom - t.bottom;
  const above = t.top - bounds.top;
  if (parts.menu.offsetHeight + 4 > below && above > below) {
    parts.menu.classList.add('opens-up');
  }

  parts.trigger.setAttribute('aria-expanded', 'true');
  parts.trigger.classList.add('is-open');

  // Scrolled by hand rather than scrollIntoView(), which would also scroll the
  // Settings tab behind the menu.
  const selected = parts.menu.querySelector('.cselect-option.active') || parts.menu.firstElementChild;
  if (selected) {
    parts.menu.scrollTop = selected.offsetTop - (parts.menu.clientHeight - selected.offsetHeight) / 2;
    selected.focus({ preventScroll: true });
  }
}

function closeCselect(refocus = false) {
  const select = openCselect;
  if (!select) return;
  openCselect = null;
  const parts = cselectParts.get(select);
  const exitMs = hideDropdown(parts.menu);
  parts.trigger.setAttribute('aria-expanded', 'false');
  parts.trigger.classList.remove('is-open');
  // Held until the exit animation is over, so the closing menu is not dropped
  // under the next group mid-fade. Skipped if it reopened in the meantime.
  setTimeout(() => {
    if (openCselect !== select) parts.wrap.closest('.settings-group')?.classList.remove('cselect-host');
  }, exitMs);
  if (refocus) parts.trigger.focus();
}

document.addEventListener('click', (e) => {
  if (!openCselect) return;
  if (cselectParts.get(openCselect).wrap.contains(e.target)) return;
  closeCselect();
});
document.addEventListener('keydown', (e) => {
  if (!openCselect) return;
  const items = Array.from(cselectParts.get(openCselect).menu.children);
  const idx = items.indexOf(document.activeElement);
  if (e.key === 'Escape') {
    e.preventDefault();
    closeCselect(true);
  } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    const next = e.key === 'ArrowDown' ? Math.min(idx + 1, items.length - 1) : Math.max(idx - 1, 0);
    items[next]?.focus();
  } else if (e.key === 'Home' || e.key === 'End') {
    e.preventDefault();
    items[e.key === 'Home' ? 0 : items.length - 1]?.focus();
  } else if (e.key === 'Tab') {
    closeCselect();
  }
});

enhanceSelects();
// ── end custom selects ──

async function init() {
  addLog('Rec EOS Launcher started.', 'ok');
  await loadVersion();
  await loadConfig();

  // Launcher/session identity up front so an exported log is self-describing.
  addLog(`Launcher version: v${launcherVersion || 'unknown'} | platform: ${navigator.platform || 'unknown'}`, 'info');

  // Check for launcher updates first on startup (run in background, do not block initialization)
  checkForLauncherUpdate();

  addLog(`Network: ${networkInfo().label} (${networkInfo().site})`, 'info');
  // Radium's own setting; the server check logs every network's address.
  if (activeNetwork === 'radium') addLog(`API: ${config.apiUrl}`, 'info');
  addLog(`Install dir: ${shownInstallDir()}`, 'info');

  // Check install first (determines which panel to show)
  await checkInstall();

  // If a download was interrupted last session, offer to resume it.
  await offerResumeIfAny();

  // Home's Popular row. Not awaited; it fills in when the rooms arrive (and
  // waits for the window to be shown when the launcher started in the tray).
  loadHomeRooms();

  // Check server on startup (show results in log), then silently every 60s —
  // but only while the window is on screen. Both of these paint the Home tab's
  // quick-stats card and nothing else, so polling them behind a hidden window
  // is two requests a minute for numbers nobody can see, for however long the
  // launcher sits in the tray between play sessions. The notification poll
  // below already works this way; these did not.
  //
  // A hidden window is skipped rather than having its timer cleared, so the
  // tick that follows the window coming back is at most a minute away, and the
  // `visibilitychange` handler refreshes it at once so the card is never stale
  // on the frame it is shown.
  await checkServerStatus(false);
  const serverPollInterval = setInterval(() => {
    if (!document.hidden) checkServerStatus(true);
  }, 60_000);

  // Check player count on startup (show results in log), then silently every 60s
  await updatePlayerCount(false);
  const playerPollInterval = setInterval(() => {
    if (!document.hidden) updatePlayerCount(true);
  }, 60_000);

  // Back on screen: bring the card up to date now rather than on the next tick.
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) return;
    checkServerStatus(true);
    updatePlayerCount(true);
  });

  // Clear the polls if the window is closed
  window.addEventListener('beforeunload', () => {
    clearInterval(serverPollInterval);
    clearInterval(playerPollInterval);
  });

  // With startup done and nothing else competing for the connection, start
  // downloading the lists Rooms and People are going to want. Last, and
  // awaited by nothing: on Vanilla these are megabytes, and the point is that
  // they arrive while the user is still on Home rather than on the click that
  // opens the tab.
  //
  // Not while the launcher has started hidden in the tray, which with "start
  // with Windows" on is every sign-in: on Vanilla that was a 5 MB download
  // parsed into ~50 MB of player records for a window nobody had opened, which
  // the backend then dropped again after fifteen idle minutes. The first time
  // the window is shown does it instead.
  if (document.hidden) {
    const prefetchWhenShown = () => {
      if (document.hidden) return;
      document.removeEventListener('visibilitychange', prefetchWhenShown);
      window.radium?.prefetchNetworkData();
    };
    document.addEventListener('visibilitychange', prefetchWhenShown);
  } else {
    window.radium?.prefetchNetworkData();
  }

  // Disable default context menu
  document.addEventListener('contextmenu', e => e.preventDefault());
}

init().catch(err => {
  addLog(`Init error: ${err.message}`, 'error');
  console.error(err);
});

// Native Rooms & People Loading Controller
let roomsSkip = 0;
// Fills whole rows of the card grid at 2, 3, 4 or 6 across.
const roomsTake = 24;
let activeRoomsTag = '';
let activeRoomsSort = 0;
let roomsSearchQuery = '';
let roomsSequenceId = 0;

let peopleSkip = 0;
// Fills whole rows of the card grid at 2, 3, 4 or 6 across.
const peopleTake = 24;
let peopleSearchQuery = '';
let peopleSequenceId = 0;

// What the Rooms grid, People table and Filters rail are currently showing, and
// when. Opening a tab used to refetch unconditionally: the list blanked to
// "Loading...", the identical page came back over the identical round-trips, and
// the view you had just been reading rebuilt itself in front of you. A list is
// now refetched only when the query behind it changed, or when what is on screen
// has aged past LIST_STALE_MS.
let roomsRenderKey = '';
let roomsRenderAt = 0;
let peopleRenderKey = '';
let peopleRenderAt = 0;
let filtersRenderKey = '';

/// How old a rendered list may be before reopening its tab refreshes it.
const LIST_STALE_MS = 5 * 60 * 1000;

/// Everything that decides what a page of rooms contains. Two renders with the
/// same key are the same page, so the second one is not worth fetching.
function roomsKey() {
  return JSON.stringify([activeNetwork, roomsSkip, activeRoomsSort, roomsSearchQuery, activeRoomsTag]);
}

function peopleKey() {
  return JSON.stringify([activeNetwork, peopleSkip, peopleSearchQuery]);
}

/// How long the People list on screen stays fresh: Stella's list of who is
/// online, and in which room, ages faster than a search.
let peopleFreshMs = LIST_STALE_MS;
const ONLINE_LIST_STALE_MS = 60 * 1000;

function listIsFresh(key, renderedKey, renderedAt, maxAge = LIST_STALE_MS) {
  return key === renderedKey && Date.now() - renderedAt < maxAge;
}

/// Put a list into its loading state, showing the placeholder only when there
/// is nothing worth keeping on screen.
///
/// Blanking a populated list on every page click and every keystroke of a
/// search is most of why paging felt slow even when the data arrived quickly:
/// what people were reading vanished first, and the layout collapsed with it.
/// Dimming the rows that are there keeps the page still, and they are replaced
/// the moment the new ones land.
///
/// `data-list-placeholder` marks a container whose only content is a loading
/// line, an empty-state or an error — nothing a reader would mind losing, so
/// those are replaced rather than dimmed.
function beginListLoad(el, placeholder) {
  if (!el) return;
  if (!el.children.length || el.dataset.listPlaceholder === '1') {
    el.innerHTML = placeholder;
    el.dataset.listPlaceholder = '1';
  }
  el.classList.add('is-refreshing');
}

function endListLoad(el) {
  el?.classList.remove('is-refreshing');
}

/// The page numbers between a list's Previous and Next: every page when there
/// are few, otherwise the first, the last and the ones either side of the
/// current one, with a gap ("…") for what is left out — never more than seven
/// slots, so the row keeps its width as you page.
function pagerSlots(page, pages) {
  if (pages <= 7) return Array.from({ length: pages }, (_, i) => i + 1);
  if (page <= 4) return [1, 2, 3, 4, 5, '…', pages];
  if (page >= pages - 3) return [1, '…', pages - 4, pages - 3, pages - 2, pages - 1, pages];
  return [1, '…', page - 1, page, page + 1, '…', pages];
}

/// Fill a list's pager (`nav.list-pager`: Previous, `pagesEl`, Next) for the
/// page starting at `skip`, and call `onPage(n)` when a number is picked.
///
/// `totalKnown === false` means the backend is paging a source that never
/// reports its size (Vanilla enumerates its player roster, and its list
/// endpoints cap rows without saying how many were withheld). Numbering up to
/// a last page there would quote a total that grows every time you press
/// Next, so only the current page is shown, between the arrows.
///
/// A list that fits on one page has no pager at all.
function renderPager(pagesEl, prevBtn, nextBtn, { skip, take, total, totalKnown, onPage }) {
  if (!pagesEl) return;
  const page = Math.floor(skip / take) + 1;
  const known = totalKnown !== false;
  const pages = Math.max(1, Math.ceil((total || 0) / take));
  if (prevBtn) prevBtn.disabled = skip === 0;
  if (nextBtn) nextBtn.disabled = skip + take >= (total || 0);

  pagesEl.textContent = '';
  if (known) {
    for (const slot of pagerSlots(page, pages)) {
      if (slot === '…') {
        const gap = document.createElement('span');
        gap.className = 'pager-gap';
        gap.setAttribute('aria-hidden', 'true');
        gap.textContent = '…';
        pagesEl.appendChild(gap);
        continue;
      }
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'btn-refresh pager-btn pager-num';
      btn.textContent = formatCount(slot);
      btn.setAttribute('aria-label', `Page ${slot}`);
      if (slot === page) {
        btn.classList.add('is-current');
        btn.setAttribute('aria-current', 'page');
      } else {
        btn.addEventListener('click', () => onPage(slot));
      }
      pagesEl.appendChild(btn);
    }
  } else {
    const label = document.createElement('span');
    label.className = 'pager-label';
    label.textContent = `Page ${formatCount(page)}`;
    pagesEl.appendChild(label);
  }

  const nav = pagesEl.closest('.list-pager');
  if (nav) nav.hidden = known && pages <= 1 && skip === 0;
}

/// Placeholder cards for a list's first load, the shape of what is coming:
/// `n` copies of `html`.
function skeletonCards(html, n) {
  return html.repeat(n);
}
const ROOM_SKELETON = '<div class="room-card is-skeleton" aria-hidden="true"><div class="room-card-media"></div><div class="room-card-foot"><span class="skel-line"></span></div></div>';
const PLAYER_SKELETON = '<div class="player-card is-skeleton" aria-hidden="true"><span class="player-avatar"></span><span class="player-info"><span class="skel-line"></span><span class="skel-line is-short"></span></span></div>';

/// A message in place of a list's cards — loading trouble, nothing found —
/// across the whole grid. `html` is trusted markup (listErrorHtml escapes).
function listMessageHtml(html) {
  return `<div class="list-message">${html}</div>`;
}

/// A room as a card: its picture with the name over the bottom edge, the
/// live count in the corner (Stella), and under it who made it and its
/// cheers. The Rooms grid, Home's Popular row and a profile's ROOMS tab all
/// draw rooms with this, so a room looks the same wherever it appears.
///
/// `onOpen(room)` runs on a click, Enter or Space; the creator opens their
/// profile instead. `scrapeMissing` looks the cheers up when the row came
/// without them (a profile's rooms, on a network whose list leaves them out).
function buildRoomCard(room, { width = 560, onOpen, scrapeMissing = false } = {}) {
  const roomName = room.Name || room.name || 'Unknown Room';
  const creatorUsername = room.CreatorUsername || room.creatorUsername || '';
  const cheersRaw = room.CheerCount ?? room.cheerCount;
  const cheers = cheersRaw != null && cheersRaw !== '' && Number.isFinite(Number(cheersRaw)) ? Number(cheersRaw) : null;
  const avatar = room.CreatorAvatarUrl
    ? thumbSrc(room.CreatorAvatarUrl, avatarWidth(20))
    : defaultAvatarUrl(20);

  const card = document.createElement('div');
  card.className = 'room-card';
  card.tabIndex = 0;
  card.setAttribute('role', 'button');
  card.setAttribute('aria-label', roomName);
  card.innerHTML = `
    <div class="room-card-media">
      <img class="room-card-image image-loading-placeholder" loading="lazy" decoding="async" data-fallback="./images.png" src="${escapeHtml(roomThumbUrl(room, width))}" alt="" />
      ${liveBadgeHtml(room)}
      <div class="room-card-name">${escapeHtml(roomName)}</div>
    </div>
    <div class="room-card-foot">
      ${creatorUsername ? `<button type="button" class="room-card-creator" tabindex="-1">
        <img class="room-card-avatar image-loading-placeholder" loading="lazy" decoding="async" data-fallback="${escapeHtml(placeholderAvatar())}" src="${escapeHtml(avatar)}" alt="" />
        <span class="room-card-creator-name">${escapeHtml(creatorUsername)}</span>
      </button>` : '<span class="room-card-creator is-unknown">Unknown creator</span>'}
      <span class="room-card-cheers" role="img"${cheers == null ? ' hidden' : ''}><span class="vn-icon vn-icon-thumb" aria-hidden="true"></span><span class="room-card-cheers-n"></span></span>
    </div>
  `;

  const cheersEl = card.querySelector('.room-card-cheers');
  const paintCheers = (n) => {
    cheersEl.querySelector('.room-card-cheers-n').textContent = compactCount(n);
    cheersEl.setAttribute('aria-label', `${formatCount(n)} ${Number(n) === 1 ? 'cheer' : 'cheers'}`);
    cheersEl.hidden = false;
  };
  if (cheers != null) paintCheers(cheers);
  else if (scrapeMissing) {
    // The scrape is a fallback for a row that genuinely carries no counts;
    // every network's list sends them, so this is rare.
    getRoomWebDetails(roomName).then(d => {
      const n = Number(String(d?.cheers ?? '').replace(/[^\d]/g, ''));
      if (d?.success && d.cheers != null && Number.isFinite(n)) paintCheers(n);
    }).catch(() => {});
  }

  // Attached as closures rather than inline handlers so a username or room
  // name containing quotes can't break out of a JS-string context.
  card.querySelector('button.room-card-creator')?.addEventListener('click', (e) => {
    e.stopPropagation();
    showCreatorProfile(creatorUsername, {
      id: room.CreatorPlayerId ?? room.CreatorAccountId ?? null,
      displayName: creatorUsername,
      avatarUrl: room.CreatorAvatarUrl
    });
  });
  const open = () => onOpen?.(room);
  card.addEventListener('click', open);
  card.addEventListener('keydown', (e) => {
    if (e.target !== card) return;
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      open();
    }
  });
  return card;
}

/// Open a room from outside the Rooms tab (Home, a profile): the room page
/// lives on that tab.
function openRoomFromElsewhere(room) {
  switchTab('rooms');
  showRoomDetails(room);
}

// ── Home: Popular right now ─────────────────────────────────────────────
// The first rooms of the network's Hot list, drawn as the Rooms grid draws
// them. Asked for when Home is on screen and what it shows is another
// network's or older than HOME_ROOMS_STALE_MS (the live counts move), never
// while the window is hidden. A network that can't list rooms right now —
// signed out of Stella, its API down — gets no section rather than an error
// on the front page.

/// As many as the widest row shows; the stylesheet shows fewer on a narrower
/// window.
const HOME_ROOMS_COUNT = 6;
const HOME_ROOMS_STALE_MS = 2 * 60 * 1000;
let homeRoomsNetwork = '';
let homeRoomsAt = 0;
let homeRoomsSeq = 0;

/// Forget what the row shows (another network's rooms, another account's).
function resetHomeRooms() {
  homeRoomsSeq++;
  homeRoomsNetwork = '';
  homeRoomsAt = 0;
  const section = $('homeRoomsSection');
  if (section) section.hidden = true;
  const grid = $('homeRoomsGrid');
  if (grid) grid.textContent = '';
}

async function loadHomeRooms() {
  const section = $('homeRoomsSection');
  const grid = $('homeRoomsGrid');
  if (!section || !grid || document.hidden) return;
  // Switched off in Settings → Home: nothing to fetch it for.
  if (homeLayout().hidden.includes('rooms')) return;
  if (networkInfo().hasSocial === false) {
    section.hidden = true;
    return;
  }
  const network = activeNetwork;
  if (homeRoomsNetwork === network && Date.now() - homeRoomsAt < HOME_ROOMS_STALE_MS) return;

  const seq = ++homeRoomsSeq;
  // A first look gets placeholders; a refresh keeps the cards until the new
  // ones land.
  if (homeRoomsNetwork !== network) {
    grid.innerHTML = skeletonCards(ROOM_SKELETON, HOME_ROOMS_COUNT);
    section.hidden = false;
  }

  let res = null;
  try {
    res = await window.radium?.fetchRooms({ skip: 0, take: HOME_ROOMS_COUNT, sortBy: 0, query: '', tag: '' });
  } catch (e) { /* treated as no rooms below */ }
  if (seq !== homeRoomsSeq || network !== activeNetwork) return;

  const rooms = res?.success ? (res.data?.Results || []) : [];
  if (!rooms.length) {
    // Asked again the next time Home is opened.
    homeRoomsNetwork = '';
    section.hidden = true;
    grid.textContent = '';
    return;
  }
  grid.textContent = '';
  rooms.slice(0, HOME_ROOMS_COUNT).forEach(room =>
    grid.appendChild(buildRoomCard(room, { width: 400, onOpen: openRoomFromElsewhere })));
  section.hidden = false;
  homeRoomsNetwork = network;
  homeRoomsAt = Date.now();
}

$('homeRoomsAll')?.addEventListener('click', () => switchTab('rooms'));

// Back on screen from the tray or a minimise: the counts may have moved.
document.addEventListener('visibilitychange', () => {
  if (!document.hidden && $('tab-home')?.classList.contains('active')) loadHomeRooms();
});

function escapeHtml(str) {
  if (str == null) return '';
  return String(str)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#039;');
}

async function loadFilters() {
  const listEl = $('roomsFiltersList');
  if (!listEl) return;

  // The rail's contents depend on nothing but the network — on Vanilla it is a
  // tally over the whole cached room set — so once it is built, reopening the
  // tab has nothing to learn by building it again.
  if (filtersRenderKey === activeNetwork && listEl.children.length) return;

  // Chip-shaped placeholders while the tags load, so the strip keeps its
  // height and the grid under it doesn't jump when they arrive.
  listEl.innerHTML = '<span class="filter-btn is-skeleton" aria-hidden="true"></span>'.repeat(6);
  syncTagStrip();

  const res = await window.radium?.fetchFilters();
  if (res && res.success && res.data) {
    const pinned = res.data.PinnedFilters || [];
    const popular = res.data.PopularFilters || [];

    // De-duplicate tags
    const allTags = Array.from(new Set(['all', ...pinned, ...popular]));

    listEl.innerHTML = '';
    allTags.forEach(tag => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'filter-btn';
      if (tag === 'all') {
        btn.textContent = 'All rooms';
        if (!activeRoomsTag) btn.classList.add('active');
      } else {
        btn.textContent = tag;
        if (activeRoomsTag === tag) btn.classList.add('active');
      }
      btn.setAttribute('aria-pressed', btn.classList.contains('active') ? 'true' : 'false');

      btn.addEventListener('click', () => {
        document.querySelectorAll('#roomsFiltersList .filter-btn').forEach(b => {
          b.classList.remove('active');
          b.setAttribute('aria-pressed', 'false');
        });
        btn.classList.add('active');
        btn.setAttribute('aria-pressed', 'true');
        btn.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: 'smooth' });
        activeRoomsTag = (tag === 'all') ? '' : tag;
        // Mirror of the search handler: on Vanilla the two share one field, so
        // picking a tag clears whatever was typed.
        if (!networkInfo().hasFilters && activeRoomsTag && roomsSearchQuery) {
          roomsSearchQuery = '';
          setValue('roomsSearch', '');
        }
        roomsSkip = 0;
        loadRooms();
      });
      listEl.appendChild(btn);
    });
    filtersRenderKey = activeNetwork;
  } else {
    // Left unset so the next visit retries rather than caching the failure.
    filtersRenderKey = '';
    // No strip at all: signed out of Stella or no Steam, the rooms list under
    // it says so, and a failed tag list is not worth a line of its own.
    listEl.innerHTML = '';
  }
  $('roomsTagStrip')?.classList.toggle('is-empty', !listEl.children.length);
  syncTagStrip();
}

/// The tag strip scrolls sideways. Its arrows show only while there is more
/// that way (`can-left` / `can-right` on the strip), and the mouse wheel
/// scrolls it too, since most mice have no sideways wheel.
function syncTagStrip() {
  const row = $('roomsFiltersList');
  const strip = $('roomsTagStrip');
  if (!row || !strip) return;
  const max = row.scrollWidth - row.clientWidth;
  strip.classList.toggle('can-left', row.scrollLeft > 1);
  strip.classList.toggle('can-right', row.scrollLeft < max - 1);
}

{
  const row = $('roomsFiltersList');
  const strip = $('roomsTagStrip');
  if (row && strip) {
    row.addEventListener('scroll', syncTagStrip, { passive: true });
    row.addEventListener('wheel', (e) => {
      if (Math.abs(e.deltaY) <= Math.abs(e.deltaX) || row.scrollWidth <= row.clientWidth) return;
      e.preventDefault();
      row.scrollLeft += e.deltaY;
    }, { passive: false });
    const step = (dir) => row.scrollBy({ left: dir * Math.max(120, row.clientWidth * 0.7), behavior: 'smooth' });
    strip.querySelector('.tag-scroll-left')?.addEventListener('click', () => step(-1));
    strip.querySelector('.tag-scroll-right')?.addEventListener('click', () => step(1));
    // Also catches the tab being shown: the row measures 0 wide while hidden.
    new ResizeObserver(syncTagStrip).observe(row);
  }
}

/// What a failed Rooms or People load shows. Signed out of Stella isn't an
/// error, so it gets a way to sign in instead.
function listErrorHtml(res, what) {
  if (res?.error === STELLA_SIGNED_OUT) {
    return `Log in to Stella to see ${what}.<br><button type="button" class="btn-refresh stella-login-prompt" data-stella-login>LOG IN WITH STEAM</button>`;
  }
  if (String(res?.error || '').startsWith("Couldn't reach Stella")) {
    return `${escapeHtml(res.error)}<br><button type="button" class="btn-refresh stella-login-prompt" data-stella-retry>TRY AGAIN</button>`;
  }
  if (res?.error === STEAM_NOT_RUNNING) {
    // Lets the window's focus handler retry once Steam is up.
    stellaAuthError = STEAM_NOT_RUNNING;
    return `Start Steam to see ${what}. Stella signs in with your Steam account.<br><button type="button" class="btn-refresh stella-login-prompt" data-stella-retry>TRY AGAIN</button>`;
  }
  if (res?.error === STELLA_NO_ACCOUNT) {
    // The game closing asks again (stellaRecheckAfterGame).
    stellaAuthError = STELLA_NO_ACCOUNT;
    const play = document.body.classList.contains('client-installed')
      ? '<br><button type="button" class="btn-refresh stella-login-prompt" data-stella-play>PLAY STELLA</button>'
      : '';
    return `${escapeHtml(STELLA_NO_ACCOUNT_TEXT)}${play}`;
  }
  return `Error: ${escapeHtml(res?.error || `Failed to fetch ${what}`)}`;
}

async function loadRooms() {
  const gridEl = $('roomsGrid');
  const emptyEl = $('roomsEmptyMsg');
  if (!gridEl) return;

  roomsSequenceId++;
  const currentSeq = roomsSequenceId;
  const key = roomsKey();

  beginListLoad(gridEl, skeletonCards(ROOM_SKELETON, roomsTake));
  emptyEl?.classList.add('hidden');

  const res = await window.radium?.fetchRooms({
    skip: roomsSkip,
    take: roomsTake,
    sortBy: activeRoomsSort,
    query: roomsSearchQuery,
    tag: activeRoomsTag
  });

  if (currentSeq !== roomsSequenceId) return;
  endListLoad(gridEl);

  if (res && res.success && res.data) {
    // Signed in on the way (Steam was started after the startup check).
    if (activeNetwork === 'stella' && !stellaPlayer) refreshStellaAuth();
    const rooms = res.data.Results || [];
    const total = res.data.TotalResults || 0;
    // The same page again (a stale one refreshed) swaps its cards in place:
    // only a new page plays the cards' entrance.
    gridEl.classList.toggle('is-quiet', roomsRenderKey === key);
    roomsRenderKey = key;
    roomsRenderAt = Date.now();

    gridEl.innerHTML = '';
    if (rooms.length === 0) {
      gridEl.dataset.listPlaceholder = '1';
      // Most Players lists only rooms with someone in them, so an empty page
      // there means nobody is playing in one, not a search that missed.
      if (emptyEl) {
        emptyEl.textContent = activeRoomsSort === 5 && !roomsSearchQuery
          ? 'Nobody is in any of these rooms right now.'
          : 'No rooms matched your search.';
      }
      emptyEl?.classList.remove('hidden');
    } else {
      delete gridEl.dataset.listPlaceholder;
      // The grid is 3 columns, more on a wide window, so a card reaches about
      // 550 CSS px maximised — 480 was an upscale before the pixel ratio was
      // even applied.
      rooms.forEach(room => gridEl.appendChild(buildRoomCard(room, { width: 560, onOpen: showRoomDetails })));
    }
    // A new page starts at the top, not wherever the last one was scrolled to.
    const scroller = $('roomsScroll');
    if (scroller) scroller.scrollTop = 0;

    renderPager($('txtRoomsPage'), $('btnRoomsPrev'), $('btnRoomsNext'), {
      skip: roomsSkip, take: roomsTake, total, totalKnown: res.data.TotalKnown,
      onPage: (n) => { roomsSkip = (n - 1) * roomsTake; loadRooms(); }
    });
  } else {
    // Escaped: this string can carry text straight from a remote API (an error
    // object's message, or a prefix of an unparseable response body), and
    // unescaped markup here reaches a webview that can call every backend
    // command.
    // Cleared so the next visit retries instead of treating the error as the
    // rendered page.
    roomsRenderKey = '';
    gridEl.dataset.listPlaceholder = '1';
    gridEl.innerHTML = listMessageHtml(listErrorHtml(res, 'rooms'));
    renderPager($('txtRoomsPage'), $('btnRoomsPrev'), $('btnRoomsNext'), {
      skip: 0, take: roomsTake, total: 0, totalKnown: true, onPage: () => {}
    });
  }
}

/// Where a Stella player is: blank unless online. `private` is whether the
/// room is a private copy, which the card marks with a lock.
function stellaWhere(person) {
  if (person.isOnline !== true) return null;
  const isPrivate = person.roomPrivate === true;
  if (person.roomName) return { text: person.roomName, isPrivate };
  return isPrivate ? { text: 'A private room', isPrivate } : null;
}

/// A player as a card: picture (with a green dot while they're online), name,
/// @handle and staff badges, level and — on Stella — the room they're in, and
/// the first lines of their bio where the network sends one. Opens the
/// profile on a click, Enter or Space.
function buildPlayerCard(person) {
  const name = person.displayName || person.userName || 'Unknown';
  const online = person.isOnline === true;
  const where = activeNetwork === 'stella' ? stellaWhere(person) : null;
  const bio = String(person.bio || '').trim();

  const card = document.createElement('div');
  card.className = 'player-card' + (online ? ' is-online' : '');
  card.tabIndex = 0;
  card.setAttribute('role', 'button');
  card.setAttribute('aria-label', online ? `${name}, online` : name);
  card.innerHTML = `
    <span class="player-avatar">
      <img class="image-loading-placeholder" loading="lazy" decoding="async" data-fallback="${escapeHtml(defaultAvatarUrl(48))}" src="${escapeHtml(personAvatarUrl(person, 48))}" alt="" />
      ${online ? '<span class="status-dot online" aria-hidden="true"></span>' : ''}
    </span>
    <span class="player-info">
      <span class="player-name">${escapeHtml(name)}</span>
      <span class="player-handle"><span class="player-handle-text">@${escapeHtml(person.userName || '')}</span><span class="profile-roles inline-roles"></span></span>
      ${(person.level != null || where) ? `<span class="player-meta">
        ${person.level != null ? `<span class="player-level">Lv ${escapeHtml(String(person.level))}</span>` : ''}
        ${where ? `<span class="player-where">${where.isPrivate ? `<span class="friend-lock" role="img" aria-label="Private room">${FRIEND_LOCK_SVG}</span>` : ''}<span class="player-where-text">${escapeHtml(where.text)}</span></span>` : ''}
      </span>` : ''}
      ${bio ? `<span class="player-bio">${escapeHtml(bio)}</span>` : ''}
    </span>
  `;
  // Built after innerHTML so the pills are real elements rather than
  // interpolated markup.
  renderPlayerRoles(card.querySelector('.inline-roles'), person);

  const open = () => showPlayerDetails(person);
  card.addEventListener('click', open);
  card.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      open();
    }
  });
  return card;
}

/// Asks again while Stella's online list is still filling in (see
/// stella_api::browse_online's `partial`).
let peopleRefill = null;

async function loadPeople() {
  const gridEl = $('peopleGrid');
  if (!gridEl) return;
  clearTimeout(peopleRefill);

  peopleSequenceId++;
  const currentSeq = peopleSequenceId;
  const key = peopleKey();

  beginListLoad(gridEl, skeletonCards(PLAYER_SKELETON, 9));

  const res = await window.radium?.fetchPeople({
    skip: peopleSkip,
    take: peopleTake,
    query: peopleSearchQuery
  });

  if (currentSeq !== peopleSequenceId) return;
  endListLoad(gridEl);

  if (res && res.success && res.data) {
    // Signed in on the way (Steam was started after the startup check).
    if (activeNetwork === 'stella' && !stellaPlayer) refreshStellaAuth();
    const people = res.data.Results || [];
    const total = res.data.TotalResults || 0;
    // Stella's online list asks again every few seconds while it fills in;
    // the same page coming back swaps its cards in place rather than playing
    // their entrance each time.
    gridEl.classList.toggle('is-quiet', peopleRenderKey === key);
    peopleRenderKey = key;
    peopleRenderAt = Date.now();
    peopleFreshMs = activeNetwork === 'stella' && !peopleSearchQuery ? ONLINE_LIST_STALE_MS : LIST_STALE_MS;

    // Stella's online list is still filling in: ask again shortly, unless the
    // user has moved on to a search, another page or another tab by then.
    if (res.partial && !peopleSearchQuery && peopleSkip === 0) {
      peopleRefill = setTimeout(() => {
        if (currentSeq === peopleSequenceId && peopleKey() === key && !document.hidden
            && document.getElementById('tab-people')?.classList.contains('active')) {
          loadPeople();
        }
      }, 10000);
    }

    gridEl.innerHTML = '';
    if (people.length === 0) {
      gridEl.dataset.listPlaceholder = '1';
      // The backend's reason when it gave one (Stella's online list); an empty
      // box on a network with no browse list isn't a failed search either.
      const message = res.note
        ? escapeHtml(res.note)
        : (!peopleSearchQuery && networkInfo().hasPeopleBrowse === false
          ? 'Search for a player by name.'
          : 'No players found.');
      gridEl.innerHTML = listMessageHtml(message);
    } else {
      delete gridEl.dataset.listPlaceholder;
      people.forEach(person => gridEl.appendChild(buildPlayerCard(person)));
    }
    const scroller = $('peopleScroll');
    if (scroller) scroller.scrollTop = 0;

    renderPager($('txtPeoplePage'), $('btnPeoplePrev'), $('btnPeopleNext'), {
      skip: peopleSkip, take: peopleTake, total, totalKnown: res.data.TotalKnown,
      onPage: (n) => { peopleSkip = (n - 1) * peopleTake; loadPeople(); }
    });
  } else {
    // Escaped for the same reason as the rooms error above.
    peopleRenderKey = '';
    gridEl.dataset.listPlaceholder = '1';
    gridEl.innerHTML = listMessageHtml(listErrorHtml(res, 'players'));
    renderPager($('txtPeoplePage'), $('btnPeoplePrev'), $('btnPeopleNext'), {
      skip: 0, take: peopleTake, total: 0, totalKnown: true, onPage: () => {}
    });
  }
}

// Wired here rather than beside loadFeed(): the `$` helper is declared further
// down this file, so a top-level call up there hits its temporal dead zone.
$('btnFeedRefresh')?.addEventListener('click', () => loadFeed(false, { refresh: true }));

// Event listeners for Rooms search / sort / pagination
let roomsSearchTimeout;
$('roomsSearch')?.addEventListener('input', (e) => {
  clearTimeout(roomsSearchTimeout);
  roomsSearchTimeout = setTimeout(() => {
    roomsSearchQuery = e.target.value.trim();
    // Vanilla filters by tag through the same search field it matches names
    // with, and a multi-term query matches nothing there — so a typed search
    // replaces the tag rather than narrowing it. Radium filters server-side and
    // can hold both at once.
    if (!networkInfo().hasFilters && roomsSearchQuery) clearRoomsTag();
    roomsSkip = 0;
    loadRooms();
  }, 300);
});

/// Drop the active tag filter and un-highlight it in the rail.
function clearRoomsTag() {
  if (!activeRoomsTag) return;
  activeRoomsTag = '';
  document.querySelectorAll('#roomsFiltersList .filter-btn').forEach(b => b.classList.remove('active'));
  document.querySelector('#roomsFiltersList .filter-btn')?.classList.add('active'); // "All Rooms"
}

$('btnRoomsPrev')?.addEventListener('click', () => {
  if (roomsSkip >= roomsTake) {
    roomsSkip -= roomsTake;
    loadRooms();
  }
});
$('btnRoomsNext')?.addEventListener('click', () => {
  roomsSkip += roomsTake;
  loadRooms();
});

$('roomsSort')?.addEventListener('change', (e) => {
  activeRoomsSort = parseInt(e.target.value) || 0;
  roomsSkip = 0;
  loadRooms();
});

// Event listeners for People search / pagination
let peopleSearchTimeout;
$('peopleSearch')?.addEventListener('input', (e) => {
  clearTimeout(peopleSearchTimeout);
  peopleSearchTimeout = setTimeout(() => {
    peopleSearchQuery = e.target.value.trim();
    peopleSkip = 0;
    loadPeople();
  }, 300);
});

$('btnPeoplePrev')?.addEventListener('click', () => {
  if (peopleSkip >= peopleTake) {
    peopleSkip -= peopleTake;
    loadPeople();
  }
});
$('btnPeopleNext')?.addEventListener('click', () => {
  peopleSkip += peopleTake;
  loadPeople();
});

// Native Detail View Helpers
function switchTab(tabName) {
  const btn = $('nav-' + tabName);
  if (btn) {
    btn.click();
  }
}

/// Bumped by every profile lookup by name, so of two clicked in quick
/// succession the one clicked last is the one shown — not whichever lookup
/// happened to come back last.
let creatorLookupSeq = 0;

/// `creator`, when given, is the id/displayName/avatar a room or photo row
/// already carried for this person. It is used to build the profile directly,
/// skipping the by-name search below: the id is exact, and it saves a request.
async function showCreatorProfile(username, creator = null) {
  if (!username && !(creator && creator.id != null)) return;
  const seq = ++creatorLookupSeq;
  let person = null;

  if (creator && creator.id != null) {
    person = {
      id: creator.id,
      userName: username || creator.userName || '',
      displayName: creator.displayName || username || '',
      // An absolute URL the row already resolved; personAvatarUrl prefers it.
      AvatarUrl: creator.avatarUrl || '',
      profileImage: '',
      isOnline: null,
      bio: ''
    };
  }

  try {
    if (!person) {
      const res = await window.radium?.fetchPeople({ query: username });
      if (seq !== creatorLookupSeq) return;
      if (res && res.success && res.data && res.data.Results) {
        // A row can come back without a username; that one just isn't a match.
        person = res.data.Results.find(p => String(p.userName || '').toLowerCase() === username.toLowerCase());
        if (!person && res.data.Results.length > 0) {
          person = res.data.Results[0];
        }
      }
    }
  } catch (err) {
    console.error("Error fetching creator profile:", err);
  }
  if (seq !== creatorLookupSeq) return;
  if (!person) {
    person = {
      id: null,
      userName: username,
      displayName: username,
      profileImage: 'DefaultProfileImage',
      // Unknown, not offline: the lookup failed, and on a network with no
      // presence at all (Vanilla) "OFFLINE" would be a claim nobody made.
      isOnline: null,
      bio: ''
    };
  }
  switchTab('people');
  showPlayerDetails(person);
}

// Scraped Web Details Cache
const photoWebDetailsCache = new Map();
const userWebDetailsCache = new Map();

/// Attribution for a photo: who took it and where.
///
/// Radium has to scrape this from the photo's web page, one request per photo.
/// Vanilla's feed already embeds the uploader and room on every row (and has no
/// per-photo detail endpoint to scrape anyway), so when the photo object
/// carries them the answer is already in hand — passing `photo` avoids a
/// pointless round trip on Radium and is the only way it resolves on Vanilla.
async function getPhotoWebDetails(photoId, photo) {
  // Keyed on the uploader specifically: a row carrying only a room name would
  // otherwise short-circuit into an "Unknown Creator" card on Radium, where the
  // scrape is the only source of attribution.
  if (photo?.CreatorUsername) {
    return {
      success: true,
      creatorUsername: photo.CreatorUsername,
      roomName: photo.RoomName || '',
      creatorAvatar: photo.CreatorAvatarUrl || ''
    };
  }
  if (photoWebDetailsCache.has(photoId)) {
    return photoWebDetailsCache.get(photoId);
  }
  const res = await window.radium?.fetchPhotoWebDetails(photoId);
  if (res && res.success) {
    if (photoWebDetailsCache.size >= 200) {
      const oldestKey = photoWebDetailsCache.keys().next().value;
      if (oldestKey !== undefined) photoWebDetailsCache.delete(oldestKey);
    }
    photoWebDetailsCache.set(photoId, res);
  }
  return res;
}

async function getUserWebDetails(username, accountId = null) {
  // Stella resolves by account id, so a row with an id but no username is
  // still lookupable. Cache by id when present.
  if (!username && accountId == null) return null;
  const key = accountId != null ? `#${accountId}` : username.toLowerCase();
  if (userWebDetailsCache.has(key)) {
    return userWebDetailsCache.get(key);
  }
  const res = await window.radium?.fetchUserWebDetails(username || '', accountId);
  if (res && res.success) {
    if (userWebDetailsCache.size >= 200) {
      const oldestKey = userWebDetailsCache.keys().next().value;
      if (oldestKey !== undefined) userWebDetailsCache.delete(oldestKey);
    }
    userWebDetailsCache.set(key, res);
  }
  return res;
}

// Photos pagination state
let roomPhotosFeedSkip = 0;
// A room's photos are found by scanning the network-wide feed and filtering by
// room id — neither network exposes a per-room photo endpoint. Each scanned page
// costs a request (two on Vanilla, which resolves photo creators in a batch),
// and because Vanilla has no `skip` the backend re-fetches everything before the
// requested window, so deep pages get quadratically more expensive. Fewer, larger
// pages cover the same ground for half the round trips.
const roomPhotosFeedTake = 60;
const ROOM_PHOTO_MAX_PAGES = 3;
let currentRoomId = null;
let currentRoomPhotosCount = 0;
let roomPhotosHasMore = false;
let roomPhotosLoading = false;
let roomPhotosTotalPagesSearched = 0;

let playerPhotosSkip = 0;
const playerPhotosTake = 10;
let currentPlayerId = null;
let playerPhotosHasMore = false;
let playerPhotosLoading = false;

// Feeds pagination state
let playerFeedsSkip = 0;
const playerFeedsTake = 10;
let currentPlayerFeedsId = null;
let playerFeedsHasMore = false;
let playerFeedsLoading = false;

// Rooms pagination state
let playerRoomsSkip = 0;
const playerRoomsTake = 20;
let currentPlayerRoomsUserId = null;
let playerRoomsHasMore = false;
let playerRoomsLoading = false;

// Inventions pagination state
let playerInventionsSkip = 0;
const playerInventionsTake = 24;
let currentPlayerInventionsId = null;
let playerInventionsHasMore = false;
let playerInventionsLoading = false;

let currentBackToView = null;

// Bumped each time a detail view opens or closes. Each view fills in from
// lookups that can take a second or more, so a view opened after another had
// the first one's stats, bio and photos written over it when those arrived
// late. A lookup that finds the number moved on drops its result.
let roomDetailSeq = 0;
let playerDetailSeq = 0;
let photoDetailSeq = 0;

// IntersectionObserver instances
let roomPhotoObserver = null;
let playerPhotoObserver = null;
let playerFeedsObserver = null;
let playerRoomsObserver = null;
let playerInventionsObserver = null;

function setupRoomPhotoObserver() {
  if (roomPhotoObserver) roomPhotoObserver.disconnect();
  const sentinel = $('roomsDetailPhotosSentinel');
  if (!sentinel) return;
  roomPhotoObserver = new IntersectionObserver((entries) => {
    if (entries[0].isIntersecting && roomPhotosHasMore && !roomPhotosLoading && currentRoomId) {
      roomPhotosFeedSkip += roomPhotosFeedTake;
      loadRoomPhotos(currentRoomId, true);
    }
  }, { threshold: 0.1 });
  roomPhotoObserver.observe(sentinel);
}

function setupPlayerPhotoObserver() {
  if (playerPhotoObserver) playerPhotoObserver.disconnect();
  const sentinel = $('peopleDetailPhotosSentinel');
  if (!sentinel) return;
  playerPhotoObserver = new IntersectionObserver((entries) => {
    if (entries[0].isIntersecting && playerPhotosHasMore && !playerPhotosLoading && currentPlayerId) {
      playerPhotosSkip += playerPhotosTake;
      loadPlayerPhotos(currentPlayerId, true);
    }
  }, { threshold: 0.1 });
  playerPhotoObserver.observe(sentinel);
}

function setupPlayerFeedsObserver() {
  if (playerFeedsObserver) playerFeedsObserver.disconnect();
  const sentinel = $('peopleDetailFeedsSentinel');
  if (!sentinel) return;
  playerFeedsObserver = new IntersectionObserver((entries) => {
    if (entries[0].isIntersecting && playerFeedsHasMore && !playerFeedsLoading && currentPlayerFeedsId) {
      playerFeedsSkip += playerFeedsTake;
      loadPlayerFeeds(currentPlayerFeedsId, true);
    }
  }, { threshold: 0.1 });
  playerFeedsObserver.observe(sentinel);
}

function setupPlayerRoomsObserver() {
  if (playerRoomsObserver) playerRoomsObserver.disconnect();
  const sentinel = $('peopleDetailRoomsSentinel');
  if (!sentinel) return;
  playerRoomsObserver = new IntersectionObserver((entries) => {
    if (entries[0].isIntersecting && playerRoomsHasMore && !playerRoomsLoading && currentPlayerRoomsUserId) {
      playerRoomsSkip += playerRoomsTake;
      loadPlayerRooms(currentPlayerRoomsUserId, true);
    }
  }, { threshold: 0.1 });
  playerRoomsObserver.observe(sentinel);
}

function setupPlayerInventionsObserver() {
  if (playerInventionsObserver) playerInventionsObserver.disconnect();
  const sentinel = $('peopleDetailInventionsSentinel');
  if (!sentinel) return;
  playerInventionsObserver = new IntersectionObserver((entries) => {
    if (entries[0].isIntersecting && playerInventionsHasMore && !playerInventionsLoading && currentPlayerInventionsId) {
      playerInventionsSkip += playerInventionsTake;
      loadPlayerInventions(currentPlayerInventionsId, true);
    }
  }, { threshold: 0.1 });
  playerInventionsObserver.observe(sentinel);
}

// ── A player's inventions (Stella) ───────────────────────────────────────

/// "May 4, 2026", or '' for a date that won't parse.
function longDate(iso) {
  const date = new Date(iso || '');
  if (Number.isNaN(date.getTime())) return '';
  return date.toLocaleDateString('en-US', { month: 'short', day: 'numeric', year: 'numeric' });
}

function inventionPriceText(price) {
  const n = Number(price) || 0;
  return n > 0 ? `${formatCount(n)} ${n === 1 ? 'token' : 'tokens'}` : 'Free';
}

/// One invention in the profile's grid: its picture, name and downloads,
/// with what it costs where it isn't free. Opens the invention dialog.
function buildInventionCard(inv) {
  const card = document.createElement('div');
  card.className = 'invention-card';
  card.tabIndex = 0;
  card.setAttribute('role', 'button');
  const name = String(inv.Name || '').trim() || 'Untitled invention';
  card.setAttribute('aria-label', `Invention: ${name}`);

  const thumb = document.createElement('div');
  thumb.className = 'invention-thumb';
  const img = document.createElement('img');
  img.className = 'image-loading-placeholder';
  img.alt = '';
  img.loading = 'lazy';
  img.decoding = 'async';
  img.dataset.fallback = './images.png';
  img.src = thumbSrc(inv.ThumbUrl, 320) || './images.png';
  thumb.append(img);
  if (Number(inv.Price) > 0) thumb.append(friendSpan('invention-badge invention-price', inventionPriceText(inv.Price)));
  if (inv.Certified) thumb.append(friendSpan('invention-badge invention-certified', 'Certified'));

  const downloads = Number(inv.Downloads) || 0;
  card.append(
    thumb,
    friendSpan('invention-name', name),
    friendSpan('invention-meta', `${formatCount(downloads)} ${downloads === 1 ? 'download' : 'downloads'}`)
  );

  const open = () => showInventionDetails(inv);
  card.addEventListener('click', open);
  card.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      open();
    }
  });
  return card;
}

async function loadPlayerInventions(userId, append = false) {
  const grid = $('peopleDetailInventionsGrid');
  const empty = $('peopleDetailInventionsEmpty');
  if (!grid || playerInventionsLoading) return;

  if (!userId) {
    grid.replaceChildren();
    if (empty) empty.style.display = 'block';
    playerInventionsHasMore = false;
    return;
  }

  const loading = document.createElement('div');
  loading.className = 'profile-grid-note';
  loading.textContent = append ? 'Loading more...' : 'Loading inventions...';
  if (!append) {
    playerInventionsSkip = 0;
    playerInventionsHasMore = false;
    grid.replaceChildren(loading);
    if (empty) empty.style.display = 'none';
  } else {
    grid.append(loading);
  }

  currentPlayerInventionsId = userId;
  playerInventionsLoading = true;
  let res;
  try {
    res = await window.radium?.fetchUserInventions({ userId, skip: playerInventionsSkip, take: playerInventionsTake });
  } catch (e) {
    res = { success: false, error: String(e) };
  }
  // Another profile took over meanwhile; see loadPlayerPhotos().
  if (currentPlayerInventionsId !== userId) return;
  playerInventionsLoading = false;
  loading.remove();

  if (res?.success && res.data) {
    const inventions = res.data.Results || [];
    grid.append(...inventions.map(buildInventionCard));
    const any = grid.querySelector('.invention-card');
    if (empty) empty.style.display = any ? 'none' : 'block';
    // The backend says whether this was the last page; a page can be short
    // of `take` without being the last, once unlisted ones are dropped.
    playerInventionsHasMore = res.data.TotalKnown === false;
    setupPlayerInventionsObserver();
  } else if (!append) {
    const note = document.createElement('div');
    note.className = 'profile-grid-note';
    note.textContent = res?.error || "Couldn't load this player's inventions.";
    grid.replaceChildren(note);
  }
}

/// The invention dialog: the picture large, who made it, the description
/// when there is one, and its downloads, price and publish date.
function showInventionDetails(inv) {
  const modal = $('inventionModal');
  if (!modal) return;
  const name = String(inv.Name || '').trim() || 'Untitled invention';
  const img = $('inventionModalImage');
  if (img) {
    img.alt = name;
    img.dataset.fallback = './images.png';
    img.src = thumbSrc(inv.ThumbUrl, 640) || './images.png';
  }
  const nameEl = $('inventionModalName');
  if (nameEl) nameEl.textContent = name;
  // Opened from a profile's grid, so the maker is that profile's player.
  const creator = $('peopleDetailDisplayName')?.textContent || '';
  const creatorEl = $('inventionModalCreator');
  if (creatorEl) creatorEl.textContent = creator ? `by ${creator}` : '';
  const desc = $('inventionModalDescription');
  if (desc) {
    desc.textContent = String(inv.Description || '').trim();
    desc.hidden = !desc.textContent;
  }
  const facts = $('inventionModalFacts');
  if (facts) {
    const downloads = Number(inv.Downloads) || 0;
    const rows = [
      ['Downloads', formatCount(downloads)],
      ['Price', inventionPriceText(inv.Price)],
      ['Published', longDate(inv.CreatedAt)],
    ];
    if (Number(inv.CheerCount) > 0) rows.splice(1, 0, ['Cheers', formatCount(inv.CheerCount)]);
    if (inv.Certified) rows.push(['Certified', 'Yes']);
    facts.replaceChildren(...rows.filter(([, v]) => v).map(([label, value]) => {
      const fact = document.createElement('div');
      fact.className = 'invention-fact';
      fact.append(friendSpan('invention-fact-val', value), friendSpan('invention-fact-lbl', label));
      return fact;
    }));
  }
  showModal(modal);
  $('inventionModalDone')?.focus();
}

function hideInventionDetails() {
  const modal = $('inventionModal');
  hideModal(modal, () => {
    const img = $('inventionModalImage');
    if (img) img.src = 'data:,';
  });
}

$('inventionModalClose')?.addEventListener('click', hideInventionDetails);
$('inventionModalDone')?.addEventListener('click', hideInventionDetails);
$('inventionModal')?.addEventListener('click', (e) => {
  if (e.target === e.currentTarget) hideInventionDetails();
});
document.addEventListener('keydown', (e) => {
  const modal = $('inventionModal');
  if (e.key === 'Escape' && modal && modal.style.display !== 'none' && !modal.classList.contains('is-closing')) {
    e.preventDefault();
    hideInventionDetails();
  }
});

/// Bumped by every fresh room-photo load. A room opened while the previous
/// one's scan was still paging through the feed used to find the scan's
/// "loading" flag up and give up, so it showed no photos of its own — and the
/// old scan then wrote the previous room's photos into its grid.
let roomPhotosSeq = 0;

async function loadRoomPhotos(roomId, append = false) {
  const photosGrid = $('roomsDetailPhotosGrid');
  const photosEmpty = $('roomsDetailPhotosEmpty');
  if (!photosGrid) return;
  // Only a scroll-triggered page waits its turn; a new room starts over.
  if (append && roomPhotosLoading) return;
  const seq = append ? roomPhotosSeq : ++roomPhotosSeq;

  // Reset pages searched so infinite scroll can continue
  roomPhotosTotalPagesSearched = 0;

  if (!append) {
    roomPhotosFeedSkip = 0;
    currentRoomPhotosCount = 0;
    roomPhotosHasMore = false;
    photosGrid.innerHTML = '<div id="roomPhotosLoading" class="profile-grid-note" style="text-align: center; padding: 10px; font-size: 11px; color: var(--text-muted);">Loading photos...</div>';
    if (photosEmpty) photosEmpty.style.display = 'none';
  } else {
    const loadingEl = document.createElement('div');
    loadingEl.id = 'roomPhotosLoading';
    loadingEl.className = 'profile-grid-note';
    loadingEl.style.cssText = 'text-align: center; padding: 10px; font-size: 11px; color: var(--text-muted);';
    loadingEl.textContent = 'Loading more...';
    photosGrid.appendChild(loadingEl);
  }
  
  currentRoomId = roomId;
  roomPhotosLoading = true;

  // Networks with a direct per-room photo endpoint (Stella) use it. Others
  // answer `unsupported` and fall through to the network-wide feed scan below.
  {
    const directSkip = append ? roomPhotosFeedSkip : 0;
    const direct = await window.radium?.fetchRoomPhotos({ roomId, skip: directSkip, take: roomPhotosFeedTake });
    if (seq !== roomPhotosSeq) return;
    if (direct && direct.success) {
      const results = (direct.data && direct.data.Results) || [];
      if (!append && currentRoomPhotosCount === 0) photosGrid.innerHTML = '';
      const loadEl = $('roomPhotosLoading');
      if (loadEl) loadEl.remove();
      results.forEach(photo => { currentRoomPhotosCount++; photosGrid.appendChild(buildPhotoTile(photo, 'rooms-detail')); });
      roomPhotosFeedSkip = directSkip + roomPhotosFeedTake;
      roomPhotosHasMore = results.length >= roomPhotosFeedTake;
      roomPhotosLoading = false;
      if (photosEmpty) {
        if (currentRoomPhotosCount === 0) { photosEmpty.textContent = 'No photos yet.'; photosEmpty.style.display = 'block'; }
        else { photosEmpty.style.display = 'none'; }
      }
      setupRoomPhotoObserver();
      return;
    }
    // Not supported on this network: fall through to the feed scan.
  }

  let resultsLength = 0;
  let hasFailed = false;
  let pagesSearched = 0;
  let matchedInBatch = 0;
  
  while (true) {
    pagesSearched++;
    roomPhotosTotalPagesSearched++;
    const res = await window.radium?.fetchRecentPhotos({ skip: roomPhotosFeedSkip, take: roomPhotosFeedTake });
    // Another room took over the grid while this page was on its way.
    if (seq !== roomPhotosSeq) return;
    if (res && res.success && res.data && res.data.Results) {
      const results = res.data.Results || [];
      resultsLength = results.length;
      
      const matched = results.filter(p => (p.RoomId || p.roomId) === roomId);
      if (matched.length > 0) {
        if (!append && currentRoomPhotosCount === 0) {
          photosGrid.innerHTML = '';
        }
        const loadEl = $('roomPhotosLoading');
        if (loadEl) loadEl.remove();
        matched.forEach(photo => {
          currentRoomPhotosCount++;
          matchedInBatch++;
          
          photosGrid.appendChild(buildPhotoTile(photo, 'rooms-detail'));
        });
      }
      
      if (resultsLength < roomPhotosFeedTake) {
        roomPhotosHasMore = false;
        break;
      }
      if (matched.length > 0) {
        roomPhotosFeedSkip += roomPhotosFeedTake;
        roomPhotosHasMore = (roomPhotosTotalPagesSearched < ROOM_PHOTO_MAX_PAGES);
        break;
      }
      if (roomPhotosTotalPagesSearched >= ROOM_PHOTO_MAX_PAGES) {
        roomPhotosHasMore = false;
        break;
      }
      roomPhotosFeedSkip += roomPhotosFeedTake;
    } else {
      hasFailed = true;
      break;
    }
  }
  
  roomPhotosLoading = false;
  
  const loadingEl = $('roomPhotosLoading');
  if (loadingEl) loadingEl.remove();
  
  if (hasFailed) {
    if (!append) {
      photosGrid.innerHTML = '';
      if (photosEmpty) { photosEmpty.textContent = 'Error loading photos.'; photosEmpty.style.display = 'block'; }
    }
    return;
  }
  
  if (!append && currentRoomPhotosCount === 0) {
    if (photosEmpty) photosEmpty.style.display = 'block';
  } else {
    if (photosEmpty) photosEmpty.style.display = 'none';
  }

  // Re-observe sentinel for next scroll trigger
  setupRoomPhotoObserver();
}

async function loadPlayerPhotos(userId, append = false) {
  const photosGrid = $('peopleDetailPhotosGrid');
  const photosEmpty = $('peopleDetailPhotosEmpty');
  if (!photosGrid) return;
  if (playerPhotosLoading) return;
  
  if (!userId) {
    photosGrid.innerHTML = '';
    if (photosEmpty) photosEmpty.style.display = 'block';
    playerPhotosHasMore = false;
    return;
  }
  
  if (!append) {
    playerPhotosSkip = 0;
    playerPhotosHasMore = false;
    photosGrid.innerHTML = '<div id="playerPhotosLoading" class="profile-grid-note" style="text-align: center; padding: 10px; font-size: 11px; color: var(--text-muted);">Loading photos...</div>';
    if (photosEmpty) photosEmpty.style.display = 'none';
  } else {
    const loadingEl = document.createElement('div');
    loadingEl.id = 'playerPhotosLoading';
    // Across the whole grid, not in one thumbnail's cell.
    loadingEl.className = 'profile-grid-note';
    loadingEl.style.cssText = 'text-align: center; padding: 10px; font-size: 11px; color: var(--text-muted); width: 100%;';
    loadingEl.textContent = 'Loading more...';
    photosGrid.appendChild(loadingEl);
  }
  
  currentPlayerId = userId;
  playerPhotosLoading = true;
  
  const res = await window.radium?.fetchUserPhotos({ userId, skip: playerPhotosSkip, take: playerPhotosTake });
  // A different profile was opened while these were on their way; they are
  // not its photos. Its own load owns the grid and the loading flag.
  if (currentPlayerId !== userId) return;
  const loadingEl = $('playerPhotosLoading');
  if (loadingEl) loadingEl.remove();
  playerPhotosLoading = false;
  
  if (res && res.success && res.data && res.data.Results) {
    const photos = res.data.Results || [];
    
    if (!append) photosGrid.innerHTML = '';
    
    photos.forEach(photo => {
      photosGrid.appendChild(buildPhotoTile(photo, 'people-detail'));
    });

    const totalInGrid = photosGrid.querySelectorAll('.profile-photo-tile').length;
    if (totalInGrid === 0 && !append) {
      if (photosEmpty) photosEmpty.style.display = 'block';
    } else {
      if (photosEmpty) photosEmpty.style.display = 'none';
    }
    
    const totalCount = res.data.TotalResults;
    if (totalCount !== undefined) {
      playerPhotosHasMore = totalInGrid < totalCount;
    } else {
      playerPhotosHasMore = photos.length === playerPhotosTake;
    }
 
    // Re-observe sentinel for next scroll trigger
    setupPlayerPhotoObserver();
  } else {
    if (!append) {
      photosGrid.innerHTML = '';
      if (photosEmpty) { photosEmpty.textContent = 'Error loading photos.'; photosEmpty.style.display = 'block'; }
    }
    playerPhotosHasMore = false;
  }
}


async function showRoomByName(roomName) {
  if (!roomName) return;
  addLog(`Looking up room "${roomName}"...`, 'info', 'server');
  // A search, not a lookup: results come back in the active sort (most
  // cheered first on Vanilla), and any room whose description mentions the
  // name matches too. So take a page and prefer the room actually called that.
  const res = await window.radium?.fetchRooms({ query: roomName, take: 24 });
  const results = res?.success ? (res.data?.Results || []) : [];
  if (results.length > 0) {
    const wanted = roomName.trim().toLowerCase();
    const room = results.find(r => String(r.Name || r.name || '').trim().toLowerCase() === wanted)
      || results[0];
    switchTab('rooms');
    showRoomDetails(room);
  } else {
    toast(`Could not find room "${roomName}"`, 'error');
  }
}

async function showPhotoDetails(photo, backToView) {
  if (!photo) return;
  const photoId = photo.Id || photo.id;
  if (!photoId) return;

  currentBackToView = backToView;
  const seq = ++photoDetailSeq;
  
  // Switch to photo detail panel
  document.querySelectorAll('.tab-panel').forEach(p => p.classList.remove('active'));
  $('tab-photo-detail')?.classList.add('active');
  
  // Render initial photo fields we have
  const imgEl = $('photoDetailImage');
  if (imgEl) {
    imgEl.classList.add('image-loading-placeholder');
    imgEl.onload = () => imgEl.classList.remove('image-loading-placeholder');
    imgEl.onerror = () => { imgEl.src = './images.png'; imgEl.classList.remove('image-loading-placeholder'); imgEl.onerror = null; };
    imgEl.src = photoImageUrl(photo, 1000);
  }

  // The page shows the photo scaled to fit; the lightbox is how you actually
  // look at it. Assigned rather than added, so reopening the panel doesn't
  // stack a listener per visit.
  const imgWrapEl = document.querySelector('.photo-detail-img-wrap');
  if (imgWrapEl) {
    // Through the thumbnail proxy like every other image, at a width that
    // leaves the picture full-screen sharp (1920 CSS px, scaled by the
    // display's pixel ratio and then capped at the proxy's own 2048). This
    // used to go straight to the origin, which meant the one URL in the app
    // that reached an arbitrary host was the one the user clicked — and it
    // was the reason `img-src` had to allow the whole of `https:`. It also
    // re-downloaded the photo on every open; now it is cached like the rest.
    imgWrapEl.onclick = () => showLightbox(photoImageUrl(photo, 1920), {
      title: 'PHOTO',
      alt: photo.caption || 'Photo',
      // The already-loaded page copy, so a failed full-size fetch still shows
      // the picture rather than an empty frame.
      fallbackSrc: $('photoDetailImage')?.src
    });
  }

  const captionEl = $('photoDetailCaption');
  if (captionEl) {
    captionEl.textContent = photo.Description || photo.description || 'No description.';
  }

  const cheersEl = $('photoDetailCheers');
  if (cheersEl) cheersEl.textContent = photo.CheerCount || photo.cheerCount || '0';
  setupPhotoSocial(photo);

  // Comments, only where the network has them. Vanilla reports no count at
  // all, and a hardcoded "0 COMMENTS" would read as "nobody commented" rather
  // than "this network has no comments" — the same rule the feed card follows.
  const commentCount = photo.CommentCount ?? photo.commentCount;
  const commentsStatEl = $('photoDetailCommentsStat');
  if (commentsStatEl) {
    commentsStatEl.hidden = commentCount == null;
    const commentsEl = $('photoDetailComments');
    if (commentsEl) commentsEl.textContent = Number(commentCount) || 0;
  }

  // Everyone else in the shot, matching the feed card. Names are attached as
  // elements rather than interpolated markup so a display name containing
  // quotes can't break out of a string context.
  const taggedBoxEl = $('photoDetailTaggedBox');
  const taggedEl = $('photoDetailTagged');
  if (taggedBoxEl && taggedEl) {
    const tagged = (photo.TaggedPlayers || [])
      .filter(p => p.userName && p.userName !== photo.CreatorUsername);
    taggedEl.innerHTML = '';
    taggedBoxEl.hidden = tagged.length === 0;
    tagged.forEach(p => {
      const link = document.createElement('span');
      link.className = 'photo-detail-tagged-name';
      link.textContent = p.displayName || p.userName;
      link.addEventListener('click', () => showCreatorProfile(p.userName));
      taggedEl.appendChild(link);
    });
  }

  const createdEl = $('photoDetailCreatedAt');
  if (createdEl) {
    createdEl.textContent = '—';
    const createdAt = photo.CreatedAt || photo.createdAt;
    if (createdAt) {
      try {
        createdEl.textContent = new Date(createdAt).toLocaleString();
      } catch (e) {
        createdEl.textContent = createdAt;
      }
    }
  }
  
  // Reset scraped elements
  const creatorNameEl = $('photoDetailCreatorName');
  if (creatorNameEl) creatorNameEl.textContent = 'Loading...';
  const creatorHandleEl = $('photoDetailCreatorHandle');
  if (creatorHandleEl) creatorHandleEl.textContent = '@...';
  const creatorAvatarEl = $('photoDetailCreatorAvatar');
  if (creatorAvatarEl) {
    creatorAvatarEl.classList.add('image-loading-placeholder');
    // Re-armed on every visit: the shared handler consumes `data-fallback` the
    // first time an image fails, so a panel opened twice would otherwise have
    // no fallback left the second time.
    creatorAvatarEl.dataset.fallback = placeholderAvatar();
    creatorAvatarEl.src = defaultAvatarUrl(32);
  }
  const roomLinkEl = $('photoDetailRoomLink');
  if (roomLinkEl) roomLinkEl.hidden = true;
  const noRoomEl = $('photoDetailNoRoom');
  if (noRoomEl) noRoomEl.hidden = false;
  const creatorLinkEl = $('photoDetailCreatorLink');
  if (creatorLinkEl) creatorLinkEl.onclick = null;
  
  // Fetch scraped details from photo webpage
  const res = await getPhotoWebDetails(photoId, photo);
  if (seq !== photoDetailSeq) return;
  if (res && res.success) {
    const creatorUsername = res.creatorUsername || '';
    const roomName = res.roomName || '';
    
    if (creatorUsername) {
      if (creatorNameEl) creatorNameEl.textContent = creatorUsername;
      if (creatorHandleEl) creatorHandleEl.textContent = `@${creatorUsername}`;
      if (creatorLinkEl) {
        creatorLinkEl.onclick = (e) => {
          e.stopPropagation();
          showCreatorProfile(creatorUsername);
        };
      }
      // fetch avatar of user in background
      const avatarSrc = res.creatorAvatar || (await getUserWebDetails(creatorUsername))?.avatar;
      if (seq !== photoDetailSeq) return;
      if (avatarSrc && creatorAvatarEl) {
        creatorAvatarEl.classList.add('image-loading-placeholder');
        creatorAvatarEl.dataset.fallback = placeholderAvatar();
        creatorAvatarEl.src = thumbSrc(avatarSrc, avatarWidth(32));
      } else if (creatorAvatarEl) {
        creatorAvatarEl.classList.remove('image-loading-placeholder');
      }
    } else {
      if (creatorNameEl) creatorNameEl.textContent = 'Unknown Creator';
      if (creatorAvatarEl) creatorAvatarEl.classList.remove('image-loading-placeholder');
    }
    
    if (roomName && roomName.toLowerCase() !== 'none') {
      const roomNameEl = $('photoDetailRoomName');
      if (roomNameEl) roomNameEl.textContent = roomName;
      if (roomLinkEl) {
        roomLinkEl.hidden = false;
        roomLinkEl.onclick = (e) => {
          e.stopPropagation();
          showRoomByName(roomName);
        };
      }
      if (noRoomEl) noRoomEl.hidden = true;
    }
  } else {
    if (creatorNameEl) creatorNameEl.textContent = 'Unknown';
    if (creatorAvatarEl) creatorAvatarEl.classList.remove('image-loading-placeholder');
  }
}

/// Where the photo detail's Back button returns to, keyed by the view that
/// opened the photo. Every grid passes its own key to `buildPhotoCard()`.
///
/// A map rather than a chain of `if`s because the chain had no entry for the
/// FEED tab, so backing out of a feed photo dropped the user on Home. A key
/// that isn't listed here still falls back to Home — but adding a photo grid
/// now means adding a line here, in one obvious place.
const PHOTO_BACK_TARGETS = {
  'feed':          { tab: 'feed' },
  'rooms-detail':  { tab: 'rooms',  list: 'roomsListView',  detail: 'roomsDetailView' },
  'people-detail': { tab: 'people', list: 'peopleListView', detail: 'peopleDetailView' }
};

$('btnPhotoDetailBack')?.addEventListener('click', () => {
  socialPhoto = null;
  photoDetailSeq++;
  $('tab-photo-detail').classList.remove('active');

  const target = PHOTO_BACK_TARGETS[currentBackToView];
  if (!target) {
    switchTab('home');
    return;
  }

  switchTab(target.tab);

  // Tabs that show a list and a detail pane need the detail put back, since
  // switching tabs alone would land on the list the user had already left.
  const list = target.list && $(target.list);
  const detail = target.detail && $(target.detail);
  if (list && detail) {
    list.classList.add('hidden');
    detail.classList.remove('hidden');
  }
});



/// The "● 7" corner badge on a room card: players in the room right now, in
/// public and private copies of it together (the split is in its accessible
/// name; no hover tooltips). Only Stella's rows carry the counts, and an empty
/// room gets no badge.
function liveBadgeHtml(room) {
  const pub = Number(room?.LivePlayers) || 0;
  const priv = Number(room?.PrivatePlayers) || 0;
  const n = pub + priv;
  if (n <= 0) return '';
  const label = priv > 0
    ? `${n} in this room now: ${pub} in public, ${priv} in private`
    : `${n} ${n === 1 ? 'player' : 'players'} in this room now`;
  return `<span class="room-live-badge" role="img" aria-label="${label}"><span class="room-live-dot" aria-hidden="true"></span>${n}</span>`;
}

/// "7 playing now · 3 in private" under the room's name. `players` (public
/// copies) null hides it; `priv` null or 0 leaves the private part off.
function setRoomLive(players, priv = null) {
  const el = $('roomsDetailLive');
  if (!el) return;
  const known = Number.isFinite(players) && players >= 0;
  el.hidden = !known;
  if (!known) return;
  const p = Number.isFinite(priv) ? priv : 0;
  el.classList.toggle('is-empty', players + p === 0);
  let text;
  if (players + p === 0) text = 'Nobody here right now';
  else if (p === 0) text = `${players} playing now`;
  else if (players === 0) text = `${p} playing now · all in private`;
  else text = `${players} playing now · ${p} in private`;
  el.lastChild.textContent = text;
}

async function showRoomDetails(room) {
  const list = $('roomsListView');
  const detail = $('roomsDetailView');
  if (!list || !detail) return;
  const seq = ++roomDetailSeq;

  // What the list row knew, then a fresh count: the page may have sat open.
  setRoomLive(activeNetwork === 'stella' ? (room.LivePlayers ?? null) : null, room.PrivatePlayers ?? null);
  if (activeNetwork === 'stella' && (room.RoomId || room.roomId)) {
    window.radium?.stellaRoomPlayers(room.RoomId || room.roomId)
      .then(r => { if (seq === roomDetailSeq && r?.players != null) setRoomLive(r.players, r.private); })
      .catch(() => {});
  }
  
  const thumbUrl = roomThumbUrl(room, 720);
  
  const imgEl = $('roomsDetailImage');
  if (imgEl) {
    imgEl.classList.add('image-loading-placeholder');
    imgEl.src = thumbUrl;
    imgEl.onerror = () => { imgEl.src = './images.png'; imgEl.classList.remove('image-loading-placeholder'); imgEl.onerror = null; };
  }
  // The header's blurred backdrop is the same picture (one fetch, cached).
  const backdropEl = $('roomsDetailBackdrop');
  if (backdropEl) {
    backdropEl.src = thumbUrl;
    backdropEl.onerror = () => { backdropEl.src = './images.png'; backdropEl.onerror = null; };
  }
  
  const roomName = room.Name || room.name || 'Unknown Room';
  const nameEl = $('roomsDetailName');
  if (nameEl) nameEl.textContent = roomName;
  
  const creatorUsername = room.CreatorUsername || room.creatorUsername || 'Coach';
  const creatorNameEl = $('roomsDetailCreatorName');
  if (creatorNameEl) creatorNameEl.textContent = creatorUsername;
  
  const creatorLinkEl = $('roomsDetailCreatorLink');
  if (creatorLinkEl) {
    creatorLinkEl.onclick = async (e) => {
      e.stopPropagation();
      await showCreatorProfile(creatorUsername, {
        id: room.CreatorPlayerId ?? room.CreatorAccountId ?? null,
        displayName: creatorUsername,
        avatarUrl: room.CreatorAvatarUrl
      });
      hideRoomDetails();
    };
  }

  const creatorAvatarEl = $('roomsDetailCreatorAvatar');
  if (creatorAvatarEl) {
    creatorAvatarEl.classList.add('image-loading-placeholder');
    // Re-armed on every visit, as on the photo screen: the shared handler
    // uses the fallback up the first time a picture fails.
    creatorAvatarEl.dataset.fallback = placeholderAvatar();
    // A row that resolved its creator (Stella, Vanilla) carries the picture
    // already. Stella has no room page to look it up from below, so without
    // this it would stay on the placeholder.
    creatorAvatarEl.src = room.CreatorAvatarUrl
      ? thumbSrc(room.CreatorAvatarUrl, avatarWidth(32))
      : defaultAvatarUrl(32);
  }

  // The day only, in the strip with the other numbers: "Mar 14, 2025".
  const createdAt = room.CreatedAt || room.createdAt;
  const createdEl = $('roomsDetailCreatedAt');
  if (createdEl) {
    const when = createdAt ? new Date(createdAt) : null;
    createdEl.textContent = when && !isNaN(when)
      ? when.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' })
      : '—';
  }

  // Paint whatever the row already carried, so the stats are correct on the
  // first frame instead of showing "..." until a page scrape returns. The
  // lookup below still runs — it is the only source for the description and
  // the creator avatar — and refreshes these if it has fresher numbers.
  const cheersEl = $('roomsDetailCheers');
  const favsEl = $('roomsDetailFavorites');
  const visitsEl = $('roomsDetailVisits');
  const known = roomStatsFromRow(room);
  if (cheersEl) cheersEl.textContent = known ? known.cheers : '...';
  if (favsEl) favsEl.textContent = known?.favorites ?? '...';
  if (visitsEl) visitsEl.textContent = known ? known.visits : '...';
  const descEl = $('roomsDetailDescription');
  if (descEl) {
    descEl.textContent = room.Description || room.description || 'Loading details from web...';
  }
  
  const roomsPhotosGrid = $('roomsDetailPhotosGrid');
  const roomsPhotosEmpty = $('roomsDetailPhotosEmpty');
  if (roomsPhotosGrid) roomsPhotosGrid.innerHTML = '';
  if (roomsPhotosEmpty) roomsPhotosEmpty.style.display = 'none';

  list.classList.add('hidden');
  detail.classList.remove('hidden');
  setupRoomSocial(room);

  // Load scraped web details asynchronously (handle both PascalCase and camelCase APIs)
  const webDetails = await getRoomWebDetails(room.Name || room.name || '');
  if (seq !== roomDetailSeq) return;
  if (webDetails && webDetails.success) {
    if (cheersEl && webDetails.cheers) cheersEl.textContent = webDetails.cheers;
    if (favsEl && webDetails.favorites) favsEl.textContent = webDetails.favorites;
    if (visitsEl && webDetails.visits) visitsEl.textContent = webDetails.visits;
    if (descEl && webDetails.description) descEl.textContent = webDetails.description;
    if (webDetails.creatorAvatar && creatorAvatarEl) {
      creatorAvatarEl.classList.add('image-loading-placeholder');
      creatorAvatarEl.dataset.fallback = placeholderAvatar();
      creatorAvatarEl.src = thumbSrc(webDetails.creatorAvatar, avatarWidth(32));
    } else if (creatorAvatarEl) {
      creatorAvatarEl.classList.remove('image-loading-placeholder');
    }
  }

  // Anything the row didn't carry and the lookup didn't fill stays unknown
  // rather than sitting on the "..." placeholder forever.
  for (const el of [cheersEl, favsEl, visitsEl]) {
    if (el && el.textContent === '...') el.textContent = '—';
  }
  if (descEl && descEl.textContent === 'Loading details from web...') {
    descEl.textContent = 'No description available.';
  }
  if (creatorAvatarEl) creatorAvatarEl.classList.remove('image-loading-placeholder');

  // Load room photos
  loadRoomPhotos(room.RoomId || room.roomId);
}

function hideRoomDetails() {
  // A Play request already under way carries on; only the view is closed.
  socialRoom = null;
  roomDetailSeq++;
  const list = $('roomsListView');
  const detail = $('roomsDetailView');
  if (list && detail) {
    detail.classList.add('hidden');
    list.classList.remove('hidden');
  }
}

async function showPlayerDetails(person) {
  const list = $('peopleListView');
  const detail = $('peopleDetailView');
  if (!list || !detail) return;
  const seq = ++playerDetailSeq;
  
  const avatarUrl = personAvatarUrl(person, 112);
  
  const avatarEl = $('peopleDetailAvatar');
  if (avatarEl) {
    avatarEl.classList.add('image-loading-placeholder');
    avatarEl.onload = () => { avatarEl.classList.remove('image-loading-placeholder'); avatarEl.onload = null; };
    avatarEl.src = avatarUrl;
    avatarEl.onerror = () => { avatarEl.src = placeholderAvatar(); avatarEl.classList.remove('image-loading-placeholder'); avatarEl.onerror = null; };
    avatarEl.style.cursor = 'pointer';
    avatarEl.onclick = () => {
      // Proxied for the same reason as the photo lightbox above. 512 is well
      // over the box this is drawn in, so nothing visible is lost.
      showLightbox(thumbSrc(personAvatarFullUrl(person), 512), {
        title: 'PROFILE IMAGE',
        alt: `${person.displayName || person.userName} profile image`,
        fallbackSrc: avatarEl.src
      });
    };
  }
  
  const nameEl = $('peopleDetailDisplayName');
  if (nameEl) nameEl.textContent = person.displayName || person.userName || 'Unknown Player';
  
  const userEl = $('peopleDetailUsername');
  if (userEl) userEl.textContent = `@${person.userName || ''}`;

  renderPlayerRoles($('peopleDetailRoles'), person);

  // Level is on the row itself where the list had it, so it needs no lookup.
  // Stella's profile lookup carries it too, for a profile opened from
  // somewhere with no level (a friend, a room's creator), so there it waits
  // for that. Absent (Radium, or a record without one) hides the tile.
  const levelEl = $('peopleDetailLevel');
  setProfileStat(levelEl, person.level ?? (activeNetwork === 'stella' ? '...' : ''));

  const friendsEl = $('peopleDetailFriends');
  const subsEl = $('peopleDetailSubscribers');
  const visitsEl = $('peopleDetailVisits');
  // Only show a loading placeholder for stats this network actually publishes.
  // Vanilla has a subscriber count and nothing else — no friends, no visits —
  // so flashing those tiles as "..." only to hide them a moment later is the
  // jump the user sees on opening a profile. Radium's scraped profile has all
  // three. `webDetails` fills the real values (or '' to hide) once it lands.
  // Only Radium publishes friends and visit counts. Vanilla and Stella expose
  // a subscriber count and nothing else, so those tiles stay hidden rather than
  // flashing "..." and then vanishing.
  const hasFriendsStat = activeNetwork === 'radium' || activeNetwork === 'receos';
  const hasVisitsStat  = activeNetwork === 'radium';
  setProfileStat(friendsEl, hasFriendsStat ? '...' : '');
  setProfileStat(subsEl, '...');
  setProfileStat(visitsEl, hasVisitsStat ? '...' : '');
  // Only Stella says when an account was made; filled from the lookup.
  const joinedEl = $('peopleDetailJoined');
  setProfileStat(joinedEl, '');
  // Hidden until the lookup brings one, so a missing bio never flashes in.
  setProfileBio('');
  // Likewise the emoji, cheers and mutual friends.
  setProfileExtras(null);
  // Shown by applyStellaPresence() for a friend who is online.
  const joinBtn = $('peopleDetailJoinBtn');
  if (joinBtn) joinBtn.hidden = true;
  // And these by paintStellaFriendActions(), for a friend or a request.
  stellaProfileId = activeNetwork === 'stella' ? Number(person.id) || 0 : 0;
  for (const id of ['peopleDetailAcceptBtn', 'peopleDetailFavoriteBtn']) {
    const btn = $(id);
    if (btn) btn.hidden = true;
  }

  const dotEl = $('peopleDetailStatusDot');
  const labelEl = $('peopleDetailStatusLabel');
  const activityEl = $('peopleDetailActivityBadge');
  
  // `isOnline` is null on networks with no presence API (Vanilla), which is
  // distinct from a known-offline false.
  const isOnline = person.isOnline;
  const presenceUnknown = isOnline == null;
  if (dotEl) {
    dotEl.className = `status-dot ${presenceUnknown ? 'unknown' : (isOnline ? 'online' : 'offline')}`;
  }
  if (labelEl) {
    labelEl.textContent = presenceUnknown ? 'Status unknown' : (isOnline ? 'Online' : 'Offline');
  }
  if (activityEl) {
    activityEl.style.display = 'none';
    activityEl.textContent = '';
  }
  
  const bannerEl = $('peopleDetailBanner');
  if (bannerEl) {
    bannerEl.style.backgroundImage = defaultProfileBanner();
    bannerEl.style.backgroundSize = defaultProfileBannerSize();
    bannerEl.style.backgroundPosition = 'center';
    bannerEl.style.backgroundRepeat = 'no-repeat';
  }

  const peoplePhotosGrid = $('peopleDetailPhotosGrid');
  const peoplePhotosEmpty = $('peopleDetailPhotosEmpty');
  if (peoplePhotosGrid) peoplePhotosGrid.innerHTML = '';
  if (peoplePhotosEmpty) peoplePhotosEmpty.style.display = 'none';
  // The previous profile's lists stop being this view's now, not after the
  // lookup below: one of them landing meanwhile would fill this cleared grid.
  currentPlayerId = null;
  currentPlayerFeedsId = null;
  currentPlayerRoomsUserId = null;
  currentPlayerInventionsId = null;

  list.classList.add('hidden');
  detail.classList.remove('hidden');
  setupPersonSocial(person);

  // Load scraped web details asynchronously
  let webDetails = null;
  try {
    // An account id is a number; Rec EOS players' ids are their usernames.
    const accountId = person.id != null && person.id !== '' && Number.isFinite(Number(person.id)) ? Number(person.id) : null;
    webDetails = await getUserWebDetails(person.userName, accountId);
  } catch (err) {
    console.error("Error loading web details for user:", err);
  }
  // A newer profile was opened (or this one closed) while that ran. Its own
  // call sets up the tabs and loads its photos; this one must not.
  if (seq !== playerDetailSeq) return;
  if (webDetails && webDetails.success) {
    setProfileStat(friendsEl, webDetails.friends);
    setProfileStat(subsEl, webDetails.subscribers);
    setProfileStat(visitsEl, webDetails.visits);
    if (activeNetwork === 'stella') setProfileStat(levelEl, webDetails.level ?? person.level ?? '');
    setProfileStat(joinedEl, joinedLabel(webDetails.createdAt));
    setProfileBio(webDetails.bio);
    setProfileExtras(webDetails);
    // A profile opened from a room/photo row may have had no avatar on the row
    // (e.g. the row's creator lookup came back thin). The details carry the real
    // one, so fill it in now — through the thumbnail cache like every picture.
    if (webDetails.avatar && avatarEl) {
      const resolved = thumbSrc(webDetails.avatar, avatarWidth(112));
      if (resolved && resolved.startsWith(THUMB_BASE)) avatarEl.src = resolved;
    }
    // Through the thumbnail cache like every other remote picture. Set as the
    // bare scraped URL it could never load — the CSP's img-src doesn't allow
    // remote hosts — and whatever the page's markup held went into the style
    // unescaped. Only the proxied form is used: it is URL-encoded, so it holds
    // no quote to end the url("...") early.
    const banner = webDetails.banner ? thumbSrc(webDetails.banner, 1000) : '';
    // Layered over the default, which then shows if the picture fails.
    if (bannerEl && banner && banner.startsWith(THUMB_BASE)) {
      bannerEl.style.backgroundImage = `url("${banner}"), ${defaultProfileBanner()}`;
      bannerEl.style.backgroundSize = `cover, ${defaultProfileBannerSize()}`;
    }
    
    // Live update status if scrape has it
    if (webDetails.status) {
      const isOnlineScraped = webDetails.status !== 'OFFLINE';
      if (dotEl) dotEl.className = `status-dot ${isOnlineScraped ? 'online' : 'offline'}`;
      if (labelEl) labelEl.textContent = isOnlineScraped ? 'Online' : 'Offline';

      // Anything else is the room they're in ("^RECCENTER"), said the way
      // Stella's status reads: "In ^RECCENTER".
      if (webDetails.status !== 'ONLINE' && webDetails.status !== 'OFFLINE') {
        if (activityEl) {
          activityEl.style.display = 'inline-flex';
          activityEl.textContent = `In ${webDetails.status}`;
        }
      }
    } else {
      // If no status was scraped, fall back to person.isOnline (still tri-state).
      if (dotEl) dotEl.className = `status-dot ${presenceUnknown ? 'unknown' : (person.isOnline ? 'online' : 'offline')}`;
      if (labelEl) labelEl.textContent = presenceUnknown ? 'Status unknown' : (person.isOnline ? 'Online' : 'Offline');
    }
  } else {
    // The lookup failed, which is different from the stat not existing: the
    // number is real on this network, we just don't have it right now. A stat
    // the network doesn't have stays hidden rather than showing a dash.
    // (Presence on Stella is filled in separately, below.)
    setProfileStat(friendsEl, hasFriendsStat ? '—' : '');
    setProfileStat(subsEl, '—');
    setProfileStat(visitsEl, hasVisitsStat ? '—' : '');
    if (activeNetwork === 'stella') setProfileStat(levelEl, person.level ?? '—');
    setProfileBio(person.bio);
  }

  // Stella's presence comes from its live hub, not the profile lookup.
  if (activeNetwork === 'stella' && person.id != null) {
    applyStellaPresence(person.id, seq);
    paintStellaFriendActions(person.id, seq);
  }

  // Disconnect existing observers and reset pagination states
  if (playerPhotoObserver) playerPhotoObserver.disconnect();
  if (playerFeedsObserver) playerFeedsObserver.disconnect();
  if (playerRoomsObserver) playerRoomsObserver.disconnect();
  if (playerInventionsObserver) playerInventionsObserver.disconnect();

  playerPhotosSkip = 0;
  playerPhotosHasMore = false;
  playerPhotosLoading = false;

  playerFeedsSkip = 0;
  playerFeedsHasMore = false;
  playerFeedsLoading = false;

  playerRoomsSkip = 0;
  playerRoomsHasMore = false;
  playerRoomsLoading = false;

  playerInventionsSkip = 0;
  playerInventionsHasMore = false;
  playerInventionsLoading = false;

  // Tab switching logic
  const tabs = [
    { btn: $('tabBtnPeoplePhotos'), sec: $('peopleDetailPhotosSection') },
    { btn: $('tabBtnPeopleFeeds'), sec: $('peopleDetailFeedsSection') },
    { btn: $('tabBtnPeopleRooms'), sec: $('peopleDetailRoomsSection') },
    { btn: $('tabBtnPeopleInventions'), sec: $('peopleDetailInventionsSection') }
  ];
  
  // FEEDS only where the network publishes an activity feed (Radium); on the
  // others it could only ever be empty.
  if (tabs[1].btn) tabs[1].btn.hidden = networkInfo().hasFeed === false;
  // INVENTIONS only where the network publishes them (Stella).
  if (tabs[3].btn) tabs[3].btn.hidden = !networkInfo().hasInventions;

  tabs.forEach(t => {
    if (t.btn) {
      // The active tab's look is each skin's `.profile-tab-btn.active`; inline
      // colours set here used to override it with one green for every skin.
      t.btn.onkeydown = (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          t.btn.click();
        }
      };
      t.btn.onclick = () => {
        tabs.forEach(other => {
          if (other.btn) {
            other.btn.classList.remove('active');
            other.btn.setAttribute('aria-selected', 'false');
          }
          if (other.sec) other.sec.style.display = 'none';
        });
        t.btn.classList.add('active');
        t.btn.setAttribute('aria-selected', 'true');
        if (t.sec) t.sec.style.display = 'block';
        
        // Trigger observer layouts and lazy load data on tab switch
        if (t.btn.id === 'tabBtnPeoplePhotos') {
          if (currentPlayerId !== person.id) {
            loadPlayerPhotos(person.id);
          }
          setupPlayerPhotoObserver();
        } else if (t.btn.id === 'tabBtnPeopleFeeds') {
          if (currentPlayerFeedsId !== person.id) {
            loadPlayerFeeds(person.id);
          }
          setupPlayerFeedsObserver();
        } else if (t.btn.id === 'tabBtnPeopleRooms') {
          if (currentPlayerRoomsUserId !== person.id) {
            loadPlayerRooms(person.id);
          }
          setupPlayerRoomsObserver();
        } else if (t.btn.id === 'tabBtnPeopleInventions') {
          if (currentPlayerInventionsId !== person.id) {
            loadPlayerInventions(person.id);
          }
          setupPlayerInventionsObserver();
        }
      };
    }
  });
  
  // Reset active user trackers to guarantee correct lazy loading
  currentPlayerId = null;
  currentPlayerFeedsId = null;
  currentPlayerRoomsUserId = null;
  currentPlayerInventionsId = null;

  // Reset tabs to Photos active by default
  if (tabs[0].btn) tabs[0].btn.click();
}

/// Online / offline, and the room, on a Stella profile — what the game shows
/// for that player. Right after the hub connects a player not yet heard from
/// is still "checking", so it asks again a few times before settling.
/// The Stella profile on screen (0 for none, or another network's).
let stellaProfileId = 0;

/// FAVORITE for a friend's profile, ACCEPT REQUEST for someone who has sent
/// the player one. The friends list may not have loaded yet (or be set to
/// hidden), so it is asked for once.
async function paintStellaFriendActions(playerId, seq) {
  const id = Number(playerId);
  if (!id || id !== stellaProfileId || seq !== playerDetailSeq) return;
  if (!stellaFriendsRes?.success || !stellaFriendsRes.loaded) {
    try {
      const res = await window.radium.stellaFriends();
      if (res?.success && res.loaded) {
        stellaFriendsById = new Map((res.friends || []).map(f => [Number(f.id), f]));
        stellaRequestsById = new Map((res.requests || []).map(r => [Number(r.id), r]));
      }
    } catch (e) {}
    if (id !== stellaProfileId || seq !== playerDetailSeq) return;
  }
  const accept = $('peopleDetailAcceptBtn');
  const request = stellaRequestsById.get(id);
  if (accept) {
    accept.hidden = !request;
    accept.disabled = false;
    accept.textContent = 'ACCEPT REQUEST';
    if (request) {
      const name = request.displayName || request.userName || 'them';
      accept.setAttribute('aria-label', `Accept ${name}'s friend request`);
      accept.onclick = () => acceptStellaRequest(request, accept);
    }
  }
  paintProfileFavorite(id);
}

/// The profile's FAVORITE toggle, for a friend only.
function paintProfileFavorite(playerId) {
  const btn = $('peopleDetailFavoriteBtn');
  if (!btn || Number(playerId) !== stellaProfileId) return;
  const friend = stellaFriendsById.get(Number(playerId));
  btn.hidden = !friend;
  if (!friend) return;
  const on = !!friend.favorite;
  btn.setAttribute('aria-pressed', String(on));
  btn.querySelector('.profile-fav-label').textContent = on ? 'FAVORITED' : 'FAVORITE';
  btn.setAttribute('aria-label', `Favorite ${friend.displayName || friend.userName || 'this friend'}`);
  btn.onclick = () => setStellaFavorite(friend, !friend.favorite);
}

async function applyStellaPresence(playerId, seq, tries = 0) {
  let p;
  try {
    p = await window.radium.stellaPresence(playerId);
  } catch (e) {
    return;
  }
  if (seq !== playerDetailSeq) return;
  const dotEl = $('peopleDetailStatusDot');
  const labelEl = $('peopleDetailStatusLabel');
  const activityEl = $('peopleDetailActivityBadge');
  const setBadge = (text) => {
    if (!activityEl) return;
    activityEl.textContent = text;
    activityEl.style.display = text ? 'inline-flex' : 'none';
  };
  // JOIN, for a friend who is online — as from the friends card. The card
  // may not have loaded yet (or be set to hidden), so ask for the list once.
  const joinBtn = $('peopleDetailJoinBtn');
  if (joinBtn) joinBtn.hidden = true;
  if (p?.status === 'online' && joinBtn) {
    if (!stellaFriendsById.size) {
      try {
        const res = await window.radium.stellaFriends();
        if (res?.success && res.loaded) stellaFriendsById = new Map((res.friends || []).map(f => [Number(f.id), f]));
      } catch (e) {}
      if (seq !== playerDetailSeq) return;
    }
    const listed = stellaFriendsById.get(Number(playerId));
    // Where they are comes from this page's presence, fresher than the list's.
    const friend = listed && { ...listed, private: !!p.private, roomId: p.roomId, roomName: p.roomName };
    if (friend) {
      const name = friend.displayName || friend.userName || 'your friend';
      joinBtn.setAttribute('aria-label', friend.private && !stellaInvitedBy(friend.id) ? `Ask ${name} to let you into their private room` : `Start Stella and join ${name}`);
      joinBtn.onclick = () => joinStellaFriend(friend);
      joinBtn.hidden = false;
    }
  }
  if (p?.status === 'online') {
    if (dotEl) dotEl.className = 'status-dot online';
    if (labelEl) labelEl.textContent = 'Online';
    setBadge(p.roomName ? `In ${p.roomName}${p.private ? ' · private' : ''}` : (p.private ? 'In a private room' : ''));
  } else if (p?.status === 'offline') {
    if (dotEl) dotEl.className = 'status-dot offline';
    if (labelEl) labelEl.textContent = 'Offline';
    setBadge('');
  } else if (p?.status === 'checking') {
    if (dotEl) dotEl.className = 'status-dot unknown';
    if (labelEl) labelEl.textContent = 'Checking…';
    if (tries < 20) setTimeout(() => applyStellaPresence(playerId, seq, tries + 1), 5000);
  } else if (p?.status === 'paused') {
    // The game holds Stella's presence connection while it runs; see
    // stella_hub.rs. Asked again so it fills in once the game closes.
    if (dotEl) dotEl.className = 'status-dot unknown';
    if (labelEl) labelEl.textContent = 'Shown in game';
    setBadge('');
    setTimeout(() => applyStellaPresence(playerId, seq, 0), 10000);
  }
  // "unknown" (hub unreachable) leaves "Status unknown" as it is.
}

async function loadPlayerFeeds(userId, append = false) {
  const grid = $('peopleDetailFeedsGrid');
  const empty = $('peopleDetailFeedsEmpty');
  if (!grid) return;
  if (playerFeedsLoading) return;
  
  if (!userId) {
    grid.innerHTML = '';
    if (empty) empty.style.display = 'block';
    playerFeedsHasMore = false;
    return;
  }
  
  if (!append) {
    playerFeedsSkip = 0;
    playerFeedsHasMore = false;
    grid.innerHTML = '<div id="playerFeedsLoading" style="text-align: center; padding: 10px; font-size: 11px; color: var(--text-muted);">Loading feeds...</div>';
    if (empty) empty.style.display = 'none';
  } else {
    const loadingEl = document.createElement('div');
    loadingEl.id = 'playerFeedsLoading';
    loadingEl.style.cssText = 'text-align: center; padding: 10px; font-size: 11px; color: var(--text-muted); width: 100%;';
    loadingEl.textContent = 'Loading more...';
    grid.appendChild(loadingEl);
  }
  
  currentPlayerFeedsId = userId;
  playerFeedsLoading = true;
  
  const res = await window.radium?.fetchUserFeed({ userId, skip: playerFeedsSkip, take: playerFeedsTake });
  // Another profile took over meanwhile; see loadPlayerPhotos().
  if (currentPlayerFeedsId !== userId) return;
  const loadingEl = $('playerFeedsLoading');
  if (loadingEl) loadingEl.remove();
  playerFeedsLoading = false;
  
  if (res && res.success && res.data && res.data.Results) {
    const feeds = res.data.Results || [];
    
    if (!append) grid.innerHTML = '';
    
    feeds.forEach(photo => {
      grid.appendChild(buildPhotoCard(photo, 'people-detail'));
    });
    
    const totalInGrid = grid.querySelectorAll('.feed-post-card').length;
    if (totalInGrid === 0 && !append) {
      if (empty) empty.style.display = 'block';
    } else {
      if (empty) empty.style.display = 'none';
    }
    
    playerFeedsHasMore = feeds.length === playerFeedsTake;
    
    setupPlayerFeedsObserver();
  } else {
    if (!append) {
      grid.innerHTML = '';
      if (empty) { empty.textContent = 'Error loading feed.'; empty.style.display = 'block'; }
    }
    playerFeedsHasMore = false;
  }
}

async function loadPlayerRooms(userId, append = false) {
  const grid = $('peopleDetailRoomsGrid');
  const empty = $('peopleDetailRoomsEmpty');
  if (!grid) return;
  if (playerRoomsLoading) return;
  
  if (!userId) {
    grid.innerHTML = '';
    if (empty) empty.style.display = 'block';
    playerRoomsHasMore = false;
    return;
  }
  
  if (!append) {
    playerRoomsSkip = 0;
    playerRoomsHasMore = false;
    grid.innerHTML = '<div id="playerRoomsLoading" style="grid-column: 1 / -1; text-align: center; padding: 20px; font-size: 11px; color: var(--text-muted);">Loading rooms...</div>';
    if (empty) empty.style.display = 'none';
  } else {
    const loadingEl = document.createElement('div');
    loadingEl.id = 'playerRoomsLoading';
    loadingEl.style.cssText = 'grid-column: 1 / -1; text-align: center; padding: 10px; font-size: 11px; color: var(--text-muted);';
    loadingEl.textContent = 'Loading more...';
    grid.appendChild(loadingEl);
  }
  
  currentPlayerRoomsUserId = userId;
  playerRoomsLoading = true;
  
  const res = await window.radium?.fetchUserRooms({ userId, skip: playerRoomsSkip, take: playerRoomsTake });
  // Another profile took over meanwhile; see loadPlayerPhotos().
  if (currentPlayerRoomsUserId !== userId) return;
  const loadingEl = $('playerRoomsLoading');
  if (loadingEl) loadingEl.remove();
  playerRoomsLoading = false;
  
  if (res && res.success && res.data && res.data.Results) {
    const rooms = res.data.Results || [];
    
    if (!append) grid.innerHTML = '';
    
    // The cheers come off the row when the API sent them, which every
    // network does. Each card used to fetch the room's web page for them
    // instead — on Radium a full HTML page plus five regexes per card — so
    // that lookup is only a fallback for a row that genuinely has no counts.
    rooms.forEach(room => grid.appendChild(
      buildRoomCard(room, { width: 400, onOpen: openRoomFromElsewhere, scrapeMissing: true })));
    
    const totalInGrid = grid.querySelectorAll('.room-card').length;
    if (totalInGrid === 0 && !append) {
      if (empty) empty.style.display = 'block';
    } else {
      if (empty) empty.style.display = 'none';
    }
    
    playerRoomsHasMore = rooms.length === playerRoomsTake;
    
    setupPlayerRoomsObserver();
  } else {
    if (!append) {
      grid.innerHTML = '';
      if (empty) { empty.textContent = 'Error loading rooms.'; empty.style.display = 'block'; }
    }
    playerRoomsHasMore = false;
  }
}

function hidePlayerDetails() {
  socialPerson = null;
  playerDetailSeq++;
  const list = $('peopleListView');
  const detail = $('peopleDetailView');
  if (list && detail) {
    detail.classList.add('hidden');
    list.classList.remove('hidden');
  }
  if (playerPhotoObserver) playerPhotoObserver.disconnect();
  if (playerFeedsObserver) playerFeedsObserver.disconnect();
  if (playerRoomsObserver) playerRoomsObserver.disconnect();
  if (playerInventionsObserver) playerInventionsObserver.disconnect();
}

$('btnRoomsBack')?.addEventListener('click', hideRoomDetails);
$('btnPeopleBack')?.addEventListener('click', hidePlayerDetails);

// Image Lightbox Modal
const lightboxModal = $('lightboxModal');
const lightboxImage = $('lightboxImage');
const lightboxCloseBtn = $('lightboxCloseBtn');

/// Open the image preview.
///
/// `opts.title` names what is on screen — the box is shared between a player's
/// avatar and a full-size photo, and it used to be hard-labelled "PROFILE IMAGE
/// PREVIEW" for both, so opening a room photo announced it as somebody's
/// profile picture.
///
/// `opts.fallbackSrc` is the smaller copy to drop back to if the full-size URL
/// fails. That used to be hard-wired to the player-detail avatar, which is the
/// wrong picture entirely when the thing being previewed is a photo — and on
/// the photo screen it would swap in whichever profile happened to be loaded.
function showLightbox(src, opts = {}) {
  if (!lightboxModal || !lightboxImage) return;

  const titleEl = $('lightboxTitle');
  if (titleEl) titleEl.textContent = opts.title || 'IMAGE PREVIEW';
  lightboxImage.alt = opts.alt || opts.title || 'Full size preview';

  lightboxImage.classList.add('image-loading-placeholder');
  lightboxImage.onload = () => lightboxImage.classList.remove('image-loading-placeholder');
  lightboxImage.onerror = () => {
    lightboxImage.onerror = null;
    const fallback = opts.fallbackSrc;
    if (fallback && lightboxImage.src !== fallback) {
      lightboxImage.src = fallback;
    } else {
      lightboxImage.classList.remove('image-loading-placeholder');
    }
  };
  lightboxImage.src = src;
  showModal(lightboxModal);
  lightboxCloseBtn?.focus();
}

function hideLightbox() {
  if (!lightboxModal) return;
  // Drop the picture so a large one is not held in memory behind a closed
  // dialog, and so reopening never shows the previous image for a frame.
  // After the exit animation, so the picture does not vanish mid-fade.
  hideModal(lightboxModal, () => {
    if (lightboxImage) {
      lightboxImage.onload = null;
      lightboxImage.onerror = null;
      lightboxImage.src = 'data:,';
    }
  });
}

function lightboxIsOpen() {
  // A lightbox playing its exit animation is already closed as far as the
  // keyboard handlers are concerned.
  return !!lightboxModal && lightboxModal.style.display !== 'none'
    && !lightboxModal.classList.contains('is-closing');
}

lightboxCloseBtn?.addEventListener('click', hideLightbox);
lightboxModal?.addEventListener('click', (e) => {
  if (e.target === lightboxModal) {
    hideLightbox();
  }
});
// Esc closes it, the way every other dialog on the desktop does. Bound on the
// document because focus sits on the close button, not the overlay.
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && lightboxIsOpen()) {
    e.preventDefault();
    hideLightbox();
  }
});

// Clear image placeholders for images that loaded from cache (where the inline
// onload may not fire). A MutationObserver reacts only when nodes are actually
// added, instead of polling the whole DOM every 500ms forever. Kept outside
// setupBugReporter so it installs even if the bug-report UI is absent.
(function setupImagePlaceholderObserver() {
  const clearIfLoaded = (el) => {
    if (el.tagName === 'IMG' && el.complete && el.src && el.src !== 'data:,') {
      el.classList.remove('image-loading-placeholder');
    }
  };
  const imgPlaceholderObserver = new MutationObserver((mutations) => {
    for (const m of mutations) {
      for (const node of m.addedNodes) {
        if (node.nodeType !== 1) continue;
        if (node.classList?.contains('image-loading-placeholder')) clearIfLoaded(node);
        node.querySelectorAll?.('.image-loading-placeholder').forEach(clearIfLoaded);
      }
    }
  });
  imgPlaceholderObserver.observe(document.body, { childList: true, subtree: true });
})();

