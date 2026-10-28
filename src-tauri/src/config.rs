use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::Manager;

static MIGRATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One-shot guard for the cross-network install repair. `ensure_config` runs on
/// nearly every IPC command, and the repair has to scan a client folder to
/// decide anything — on a 5 GB install that is a full directory walk. The
/// repair is a one-time migration, so it runs once per process, not per command.
static INSTALL_REPAIR_DONE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The Liquid Glass effect and the two settings that shape it.
///
/// This used to be three fields inside the custom-theme palette editor, which
/// is gone. Glass survived it because it never depended on that palette: its
/// stylesheet redefines every colour token itself, so it always looked the same
/// whatever the editor's swatches said. As its own object it is what it always
/// behaved like — an effect layered over whichever skin is selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct GlassSettings {
    pub enabled: bool,
    /// Colour behind the frosted panels when no background image is set.
    pub tint: String,
    /// Optional backdrop, as an `https://` URL or a `data:` URI. Applies only
    /// while glass is on — it is the surface the frosted panels sit over.
    pub bg_image: String,
    /// Frost every panel rather than only the sidebar, cards and settings
    /// groups. On by default; turning it off is for older integrated GPUs,
    /// where each extra blur being redone on every nearby repaint is felt as
    /// stutter.
    pub full_effects: bool,
}

impl Default for GlassSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            tint: "#0b0c14".to_string(),
            bg_image: String::new(),
            full_effects: true,
        }
    }
}

/// The parts of the removed `customTheme` object still worth reading.
///
/// Deserialized from an existing config purely so a user who had Liquid Glass
/// switched on keeps it, along with the tint and backdrop they chose. Never
/// serialized, so the stale palette drops out of config.json on the first save
/// after the upgrade rather than sitting there being ignored forever.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct LegacyCustomTheme {
    pub glass_enabled: bool,
    pub glass_bg: String,
    pub bg_image: String,
}

/// Whether `s` is a CSS hex colour and nothing else.
///
/// These strings are interpolated into a generated stylesheet by the frontend,
/// so one carrying a `;` or a `}` would close the declaration and open a rule
/// of its own. The frontend validates them too; doing it here as well means a
/// hand-edited or shared config.json is repaired on load rather than carried
/// around until something renders it.
fn is_hex_color(s: &str) -> bool {
    let s = s.trim();
    let Some(body) = s.strip_prefix('#') else {
        return false;
    };
    matches!(body.len(), 3 | 4 | 6 | 8) && body.chars().all(|c| c.is_ascii_hexdigit())
}

impl GlassSettings {
    /// Repair anything that isn't safe to put in a stylesheet.
    ///
    /// Returns true if something changed, so the caller can persist the repair
    /// instead of redoing it on every load.
    pub(crate) fn sanitize(&mut self) -> bool {
        let mut changed = false;
        if !is_hex_color(&self.tint) {
            self.tint = GlassSettings::default().tint;
            changed = true;
        }
        // The backdrop is interpolated into a `url('...')`, so it is held to
        // the two shapes that can legitimately appear there and to characters
        // that cannot terminate the declaration.
        let img = self.bg_image.trim();
        let is_data = img.starts_with("data:image/");
        let shape_ok = img.is_empty() || img.starts_with("https://") || is_data;
        // A data: URI legitimately carries a `;` (the `;base64` marker), and
        // inside the quoted url() it cannot end the declaration — so it is
        // exempt from that one character, as in `safeBackdrop()` in app.js.
        // Rejecting it here used to wipe every picked local image on save.
        let chars_ok = !img.contains(['\'', '"', '(', ')', '{', '}', '\\'])
            && (is_data || !img.contains(';'))
            && !img.chars().any(|c| c.is_control());
        if !shape_ok || !chars_ok {
            self.bg_image = String::new();
            changed = true;
        }
        changed
    }
}

/// Home's sections that can be moved and switched off, by name: the status
/// strip, the friends list (Stella) and "Popular right now". The hero and the
/// download panel always lead.
pub const HOME_SECTIONS: [&str; 3] = ["status", "friends", "rooms"];

/// How Home is laid out, picked in Settings → Home. The first is the default:
/// the network's art across the top, everything else below it.
pub const HOME_STYLES: [&str; 4] = ["showcase", "spotlight", "compact", "dashboard"];

/// Largest banner picture kept, as an encoded `data:` URI. The page downscales
/// a picked file to fit (see `prepareBannerImage` in app.js); the hero is at
/// most ~1,700 CSS px wide and 248 tall, so this is plenty.
pub const HOME_BANNER_MAX_BYTES: usize = 1_000_000;

/// What Home shows, in what order, and a picture of the user's own behind
/// each network's brand. Owned by Settings → Home.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct HomeSettings {
    /// Home's sections from the top, by [`HOME_SECTIONS`] name.
    pub order: Vec<String>,
    /// Sections switched off. Friends is not among them: whether the friends
    /// list is on Home is `stella.friends_view`.
    pub hidden: Vec<String>,
    /// Each network's banner picture.
    pub banners: HomeBanners,
    /// The layout, by [`HOME_STYLES`] name.
    pub style: String,
    /// The sections Dashboard puts in its column beside the game; the rest go
    /// under the game. Each column keeps `order`'s order.
    pub side: Vec<String>,
}

impl Default for HomeSettings {
    fn default() -> Self {
        Self {
            order: HOME_SECTIONS.iter().map(|s| s.to_string()).collect(),
            hidden: Vec::new(),
            banners: HomeBanners::default(),
            style: HOME_STYLES[0].to_string(),
            side: vec!["status".to_string(), "friends".to_string()],
        }
    }
}

/// A banner picture per network, as a downscaled `data:image/` URI; empty
/// shows the network's own art. Written only by `cmd_set_home_banner`, never
/// by a whole-config save (see `preserve_backend_managed_fields`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct HomeBanners {
    pub radium: String,
    pub vanilla: String,
    pub stella: String,
    pub receos: String,
}

impl HomeBanners {
    pub fn for_network_mut(&mut self, network: Network) -> &mut String {
        match network {
            Network::Radium => &mut self.radium,
            Network::Vanilla => &mut self.vanilla,
            Network::Stella => &mut self.stella,
            Network::RecEos => &mut self.receos,
        }
    }
}

/// Whether `s` can be a banner: a stored picture the page can paint (the CSP
/// loads no remote image, so no addresses), small enough to keep, and with
/// nothing that could end the `url("...")` it is put into.
fn is_safe_banner(s: &str) -> bool {
    s.is_empty()
        || (s.starts_with("data:image/")
            && s.len() <= HOME_BANNER_MAX_BYTES
            && !s.contains(['\'', '"', '(', ')', '{', '}', '\\'])
            && !s.chars().any(|c| c.is_control()))
}

impl HomeSettings {
    /// Repair the order (known names, each once, every one present), the
    /// hidden list (known names, each once), the style (a known one, else the
    /// default) and any banner that isn't safe to paint. Returns true if
    /// something changed.
    pub(crate) fn sanitize(&mut self) -> bool {
        let before = self.clone();
        let known = |name: &String| HOME_SECTIONS.contains(&name.as_str());

        let mut order: Vec<String> = Vec::new();
        for name in self.order.iter().filter(|n| known(n)) {
            if !order.contains(name) {
                order.push(name.clone());
            }
        }
        // A section the saved order doesn't name (one added in a later
        // version, say) goes where the default order has it relative to the
        // ones before it — in practice, the end.
        for name in HOME_SECTIONS {
            if !order.iter().any(|o| o == name) {
                order.push(name.to_string());
            }
        }
        self.order = order;

        let mut hidden: Vec<String> = Vec::new();
        for name in self.hidden.iter().filter(|n| known(n)) {
            if !hidden.contains(name) {
                hidden.push(name.clone());
            }
        }
        self.hidden = hidden;

        let mut side: Vec<String> = Vec::new();
        for name in self.side.iter().filter(|n| known(n)) {
            if !side.contains(name) {
                side.push(name.clone());
            }
        }
        self.side = side;

        if !HOME_STYLES.contains(&self.style.as_str()) {
            self.style = HOME_STYLES[0].to_string();
        }

        for banner in [
            &mut self.banners.radium,
            &mut self.banners.vanilla,
            &mut self.banners.stella,
            &mut self.banners.receos,
        ] {
            if !is_safe_banner(banner) {
                banner.clear();
            }
        }
        *self != before
    }
}

/// Which revival network the launcher is currently pointed at.
///
/// The launcher speaks to one network at a time. Radium is the historical
/// default and owns the flat `Config` fields; Vanilla's and Stella's install
/// state live in [`VanillaState`] and [`StellaState`] so existing configs keep
/// working with no migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Radium,
    Vanilla,
    Stella,
    /// Rec EOS: the stock 20220211 client with the Rec EOS mod (`crate::receos`).
    RecEos,
}

impl Network {
    /// Parse a network name from config/IPC. Anything unrecognised (including
    /// `None`) falls back to Radium, so a corrupt value can never strand the
    /// user on a network they can't leave.
    pub fn parse(name: Option<&str>) -> Self {
        match name.unwrap_or("") {
            "vanilla" => Network::Vanilla,
            "stella" => Network::Stella,
            "receos" => Network::RecEos,
            _ => Network::Radium,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Network::Radium => "radium",
            Network::Vanilla => "vanilla",
            Network::Stella => "stella",
            Network::RecEos => "receos",
        }
    }

    /// Every network, Radium first. The order matters where one has to give
    /// way to another — see [`dedupe_install_dirs_at`].
    pub const ALL: [Network; 4] = [Network::Radium, Network::Vanilla, Network::Stella, Network::RecEos];
}

/// Vanilla-specific install state.
///
/// Mirrors the flat Radium fields on [`Config`] so the two clients can be
/// installed side by side without either one's paths, version or ETag
/// standing in for the other's.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct VanillaState {
    pub install_dir: String,
    pub game_exe_path: String,
    /// Where to download the Vanilla client zip from. Empty until Vanilla
    /// actually ships a build - vanillarec.net currently lists every platform
    /// as "coming soon" with no download link, so the UI falls back to opening
    /// their download page instead.
    pub client_url: String,
    pub client_version: String,
    pub client_etag: String,
    pub client_build: String,
    pub defender_excluded: bool,
    /// See [`Config::defender_dirs`].
    pub defender_dirs: Vec<String>,
}

/// Stella-specific install state.
///
/// Stella ships a stock 2024 client plus a small patch DLL that the launcher
/// injects at launch to point the game at Stella's servers (see
/// `crate::stella`). The patch is tracked on its own: it changes far more
/// often than the multi-gigabyte client, and which patch file is trusted is
/// decided by its hash, never by where it was downloaded from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct StellaState {
    pub install_dir: String,
    pub game_exe_path: String,
    pub client_version: String,
    pub client_etag: String,
    pub client_build: String,
    pub defender_excluded: bool,
    /// See [`Config::defender_dirs`].
    pub defender_dirs: Vec<String>,
    /// Lowercase hex SHA-256 of the patch DLL the user has accepted: a pinned
    /// build, or one they installed by pressing UPDATE. Launch injects the
    /// patch on disk only if it still hashes to this.
    pub patch_sha256: String,
    /// Where the friends list shows: "home" (a card on Home, also what empty
    /// means), "tab" (its own sidebar tab) or "hidden". Owned by Settings.
    pub friends_view: String,
}

/// Rec EOS-specific install state. The mod's own files are not tracked here:
/// what is installed is read from the client folder (see `crate::receos`),
/// since the mod also updates itself in-game.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct RecEosState {
    pub install_dir: String,
    pub game_exe_path: String,
    pub client_version: String,
    pub client_etag: String,
    pub client_build: String,
    pub defender_excluded: bool,
    /// See [`Config::defender_dirs`].
    pub defender_dirs: Vec<String>,
}

/// Application configuration for the launcher.
///
/// Fields are serialized as camelCase to match the existing config.json format
/// used by the Electron version of the launcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct Config {
    pub api_url: String,
    pub game_exe_path: String,
    pub play_mode: String,
    pub minimize_on_launch: bool,
    pub auto_update: bool,
    pub install_dir: String,
    /// Whether every folder Radium's exclusion has to cover (see
    /// [`exclusion_dirs_for`]) is excluded from Windows Defender. Worked out
    /// from [`Config::defender_dirs`] on every load and save rather than
    /// trusted from the page: see [`Config::refresh_defender_flags`].
    pub defender_excluded: bool,
    /// The folders the launcher added to Windows Defender's exclusions for
    /// Radium and has not removed since: exactly what a removal takes out.
    ///
    /// The exclusion used to be a bare flag, with the folders worked out
    /// again from the current install location whenever one was removed. A
    /// client moved to another folder kept the flag, so the new folder was
    /// taken as excluded when it wasn't, and the removal took out the new
    /// folder (which was never in the list) and left the old one excluded for
    /// good. Uninstalling cleared the flag and left the exclusion behind.
    pub defender_dirs: Vec<String>,
    /// Whether the user has opted out of the launch-time third-party antivirus
    /// warning ("Don't warn me again"). Distinct from `defender_excluded`, which
    /// tracks a real Windows Defender folder exclusion — a third-party AV can't
    /// be auto-excluded, so this is a pure "stop warning me" acknowledgement.
    pub third_party_av_acknowledged: bool,
    pub theme: String,
    pub baseline_theme: String,
    pub close_on_launch: bool,
    pub disable_warnings: bool,
    /// Vanilla notification pop-ups (in their own window in the corner of the
    /// screen), and whether they play a sound.
    pub notif_popups: bool,
    pub notif_sound: bool,
    /// Closing the window hides it to the tray instead of quitting.
    pub run_in_background: bool,
    /// Whether "start with Windows" has had its on-by-default applied. The
    /// setting itself lives in the registry; this only stops the default from
    /// being applied again after the user turns it off.
    pub autostart_initialized: bool,
    /// The Liquid Glass effect, layered over whichever skin is selected.
    pub glass: GlassSettings,
    /// Home's layout and banner pictures.
    pub home: HomeSettings,
    /// Read from an older config so glass survives the removal of the custom
    /// theme editor; never written back. See [`LegacyCustomTheme`].
    #[serde(rename = "customTheme", skip_serializing)]
    pub legacy_custom_theme: Option<LegacyCustomTheme>,
    /// Build id of the currently-installed client (see download::REQUIRED_CLIENT_BUILD).
    pub client_build: String,
    /// Real version string of the installed client (e.g. "0.9.2"), as published
    /// on the recroom.baby downloads page. Used to detect live client updates,
    /// separately from `client_build`'s launcher-compatibility marker.
    pub client_version: String,
    /// CDN ETag of the downloaded client zip, captured at download time. Lets
    /// update checks catch a rebuilt zip even when the version number on the
    /// download page hasn't changed.
    pub client_etag: String,
    /// Whether the one-time "your client predates version tracking, please
    /// sync" nudge has already been shown. Without this, a client with no
    /// recorded version would be flagged as needing an update on every single
    /// check forever, since there's no version to compare against.
    pub client_version_sync_prompted: bool,
    /// Active network name: "radium" | "vanilla". See [`Network`].
    pub network: String,
    /// A client folder the launcher renamed out of the way because it held a
    /// client that did not belong to the network installing there (see
    /// [`recover_misplaced_radium_install`]). Kept so the UI can tell the user
    /// where their files went; cleared once the folder is gone.
    pub orphaned_client_dir: String,
    /// Install state for the Vanilla network. The flat fields above stay Radium's.
    pub vanilla: VanillaState,
    /// Install state for the Stella network.
    pub stella: StellaState,
    /// Install state for the Rec EOS network.
    pub receos: RecEosState,
}

impl Config {
    /// Overwrite the backend-managed fields on `self` with the authoritative
    /// values from `current` (the config currently on disk).
    ///
    /// These fields are written by backend commands — the client build id,
    /// version and ETag are stamped in by a download; the version-sync flag by
    /// the live update check; the exe path by download/uninstall — *after* the
    /// settings UI loaded its in-memory copy of the config. That UI writes the
    /// whole config back on every autosave, so without this a stale save would
    /// silently revert these to the values it holds (typically the empty
    /// defaults from startup). That is what made a freshly-downloaded client
    /// read as "outdated" on the next check, triggering an endless re-download
    /// loop. The Defender exclusions are the defender commands' too: the
    /// folders they recorded are kept, and the flag is worked out from them
    /// when the config is saved (see [`Config::refresh_defender_flags`]).
    pub fn preserve_backend_managed_fields(&mut self, current: &Config) {
        self.client_build = current.client_build.clone();
        self.client_version = current.client_version.clone();
        self.client_etag = current.client_etag.clone();
        self.client_version_sync_prompted = current.client_version_sync_prompted;
        self.game_exe_path = current.game_exe_path.clone();
        self.orphaned_client_dir = current.orphaned_client_dir.clone();
        self.autostart_initialized = current.autostart_initialized;
        // Vanilla's install state is written by the same backend commands and is
        // just as absent from a stale frontend copy, so it needs the identical
        // treatment. `vanilla.client_url` and `vanilla.defender_excluded` are
        // deliberately NOT preserved - the settings and AV-exclude UIs own those.
        self.vanilla.client_build = current.vanilla.client_build.clone();
        self.vanilla.client_version = current.vanilla.client_version.clone();
        self.vanilla.client_etag = current.vanilla.client_etag.clone();
        self.vanilla.game_exe_path = current.vanilla.game_exe_path.clone();
        // Stella's likewise. The accepted patch hash is written only by the
        // install and UPDATE commands.
        self.stella.client_build = current.stella.client_build.clone();
        self.stella.client_version = current.stella.client_version.clone();
        self.stella.client_etag = current.stella.client_etag.clone();
        self.stella.game_exe_path = current.stella.game_exe_path.clone();
        self.stella.patch_sha256 = current.stella.patch_sha256.clone();
        self.receos.client_build = current.receos.client_build.clone();
        self.receos.client_version = current.receos.client_version.clone();
        self.receos.client_etag = current.receos.client_etag.clone();
        self.receos.game_exe_path = current.receos.game_exe_path.clone();
        // The glass backdrop has its own command (`cmd_set_glass_backdrop`).
        // It can be a 1.4 MB data URI, so the frontend leaves it out of every
        // whole-config save rather than shipping it over IPC on each autosave;
        // what arrives here is therefore always blank and must not win.
        self.glass.bg_image = current.glass.bg_image.clone();
        // Home's banner pictures likewise (`cmd_set_home_banner`): up to a
        // megabyte each, and never part of a whole-config save.
        self.home.banners = current.home.banners.clone();
        // What Windows Defender actually has excluded, which only the
        // defender commands know.
        for network in Network::ALL {
            self.set_defender_dirs(network, current.defender_dirs_for(network).to_vec());
        }
    }

    /// The currently selected network.
    pub fn network(&self) -> Network {
        Network::parse(Some(self.network.as_str()))
    }

    /// User-chosen install directory for `network`, or "" to use the default.
    pub fn install_dir_for(&self, network: Network) -> &str {
        match network {
            Network::Radium => &self.install_dir,
            Network::Vanilla => &self.vanilla.install_dir,
            Network::Stella => &self.stella.install_dir,
            Network::RecEos => &self.receos.install_dir,
        }
    }

    /// Mutable install-dir override for `network`.
    pub fn install_dir_mut(&mut self, network: Network) -> &mut String {
        match network {
            Network::Radium => &mut self.install_dir,
            Network::Vanilla => &mut self.vanilla.install_dir,
            Network::Stella => &mut self.stella.install_dir,
            Network::RecEos => &mut self.receos.install_dir,
        }
    }

    /// Whether `network`'s client folder was excluded from Windows Defender.
    pub fn defender_excluded_for(&self, network: Network) -> bool {
        match network {
            Network::Radium => self.defender_excluded,
            Network::Vanilla => self.vanilla.defender_excluded,
            Network::Stella => self.stella.defender_excluded,
            Network::RecEos => self.receos.defender_excluded,
        }
    }

    pub fn set_defender_excluded(&mut self, network: Network, value: bool) {
        match network {
            Network::Radium => self.defender_excluded = value,
            Network::Vanilla => self.vanilla.defender_excluded = value,
            Network::Stella => self.stella.defender_excluded = value,
            Network::RecEos => self.receos.defender_excluded = value,
        }
    }

    /// The folders the launcher has excluded from Windows Defender for
    /// `network`. See [`Config::defender_dirs`].
    pub fn defender_dirs_for(&self, network: Network) -> &[String] {
        match network {
            Network::Radium => &self.defender_dirs,
            Network::Vanilla => &self.vanilla.defender_dirs,
            Network::Stella => &self.stella.defender_dirs,
            Network::RecEos => &self.receos.defender_dirs,
        }
    }

    pub fn set_defender_dirs(&mut self, network: Network, dirs: Vec<String>) {
        match network {
            Network::Radium => self.defender_dirs = dirs,
            Network::Vanilla => self.vanilla.defender_dirs = dirs,
            Network::Stella => self.stella.defender_dirs = dirs,
            Network::RecEos => self.receos.defender_dirs = dirs,
        }
    }

    /// Work out each network's `defender_excluded` from what is recorded as
    /// excluded: true when every folder its exclusion has to cover now (see
    /// [`exclusion_dirs_for`]) is one of those, or inside one. Returns true if
    /// a flag changed.
    ///
    /// A client moved to another folder therefore stops counting as excluded
    /// the moment it moves, and the launch check offers to exclude the new
    /// folder, while the old one stays recorded for that exclusion to take out.
    pub fn refresh_defender_flags(&mut self, app_data_dir: &std::path::Path) -> bool {
        let mut changed = false;
        for network in Network::ALL {
            let recorded = self.defender_dirs_for(network);
            let excluded = !recorded.is_empty()
                && exclusion_dirs_for(self, network, app_data_dir)
                    .iter()
                    .all(|dir| recorded.iter().any(|r| is_inside(dir, r)));
            if excluded != self.defender_excluded_for(network) {
                self.set_defender_excluded(network, excluded);
                changed = true;
            }
        }
        changed
    }

    /// Give a flag-only exclusion from an older launcher the folders it was
    /// made for. Returns true if anything changed.
    ///
    /// Before the folders were recorded, a set flag meant the current folders
    /// had been excluded, so those are the best record there is. Without this
    /// the flag, worked out from an empty record, would turn off on upgrade and
    /// the launch check would ask to exclude a folder that already is.
    pub fn adopt_legacy_defender_flags(&mut self, app_data_dir: &std::path::Path) -> bool {
        let mut changed = false;
        for network in Network::ALL {
            if self.defender_excluded_for(network) && self.defender_dirs_for(network).is_empty() {
                let dirs = exclusion_dirs_for(self, network, app_data_dir);
                self.set_defender_dirs(network, dirs);
                changed = true;
            }
        }
        changed
    }

    /// Recorded game executable path for `network`.
    pub fn game_exe_for(&self, network: Network) -> &str {
        match network {
            Network::Radium => &self.game_exe_path,
            Network::Vanilla => &self.vanilla.game_exe_path,
            Network::Stella => &self.stella.game_exe_path,
            Network::RecEos => &self.receos.game_exe_path,
        }
    }

    /// Build id recorded for `network`'s installed client.
    pub fn client_build_for(&self, network: Network) -> &str {
        match network {
            Network::Radium => &self.client_build,
            Network::Vanilla => &self.vanilla.client_build,
            Network::Stella => &self.stella.client_build,
            Network::RecEos => &self.receos.client_build,
        }
    }

    /// Installed client version for `network`.
    pub fn client_version_for(&self, network: Network) -> &str {
        match network {
            Network::Radium => &self.client_version,
            Network::Vanilla => &self.vanilla.client_version,
            Network::Stella => &self.stella.client_version,
            Network::RecEos => &self.receos.client_version,
        }
    }

    /// ETag of the zip `network`'s installed client came from.
    pub fn client_etag_for(&self, network: Network) -> &str {
        match network {
            Network::Radium => &self.client_etag,
            Network::Vanilla => &self.vanilla.client_etag,
            Network::Stella => &self.stella.client_etag,
            Network::RecEos => &self.receos.client_etag,
        }
    }

    /// Record the results of a successful download for `network`.
    pub fn set_client_install(
        &mut self,
        network: Network,
        exe_path: String,
        build: String,
        version: String,
        etag: String,
    ) {
        match network {
            Network::Radium => {
                self.game_exe_path = exe_path;
                self.client_build = build;
                self.client_version = version;
                self.client_etag = etag;
                self.client_version_sync_prompted = false;
            }
            Network::Vanilla => {
                self.vanilla.game_exe_path = exe_path;
                self.vanilla.client_build = build;
                self.vanilla.client_version = version;
                self.vanilla.client_etag = etag;
            }
            Network::Stella => {
                self.stella.game_exe_path = exe_path;
                self.stella.client_build = build;
                self.stella.client_version = version;
                self.stella.client_etag = etag;
            }
            Network::RecEos => {
                self.receos.game_exe_path = exe_path;
                self.receos.client_build = build;
                self.receos.client_version = version;
                self.receos.client_etag = etag;
            }
        }
    }

    /// Clear the recorded install for `network` (used by uninstall).
    pub fn clear_client_install(&mut self, network: Network) {
        match network {
            Network::Radium => {
                self.game_exe_path = String::new();
                self.client_build = String::new();
                self.client_version = String::new();
                self.client_etag = String::new();
            }
            Network::Vanilla => {
                self.vanilla.game_exe_path = String::new();
                self.vanilla.client_build = String::new();
                self.vanilla.client_version = String::new();
                self.vanilla.client_etag = String::new();
            }
            Network::Stella => {
                self.stella.game_exe_path = String::new();
                self.stella.client_build = String::new();
                self.stella.client_version = String::new();
                self.stella.client_etag = String::new();
                self.stella.patch_sha256 = String::new();
            }
            Network::RecEos => {
                self.receos.game_exe_path = String::new();
                self.receos.client_build = String::new();
                self.receos.client_version = String::new();
                self.receos.client_etag = String::new();
            }
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_url: "https://api.radie.app/".to_string(),
            game_exe_path: String::new(),
            play_mode: "screen".to_string(),
            // Off by default: a launcher that hides itself the moment you press
            // PLAY is a surprise the first time, and the download/settings it
            // was showing are what you want back if the game fails to start.
            minimize_on_launch: false,
            auto_update: true,
            install_dir: String::new(),
            defender_excluded: false,
            defender_dirs: Vec::new(),
            third_party_av_acknowledged: false,
            theme: DEFAULT_THEME.to_string(),
            baseline_theme: DEFAULT_THEME.to_string(),
            close_on_launch: false,
            disable_warnings: false,
            notif_popups: true,
            run_in_background: true,
            autostart_initialized: false,
            notif_sound: true,
            glass: GlassSettings::default(),
            home: HomeSettings::default(),
            legacy_custom_theme: None,
            client_build: String::new(),
            client_version: String::new(),
            client_etag: String::new(),
            client_version_sync_prompted: false,
            // Rec EOS is the main network. A config saved before it existed
            // names the network it was on, and stays there.
            network: "receos".to_string(),
            orphaned_client_dir: String::new(),
            vanilla: VanillaState::default(),
            stella: StellaState::default(),
            receos: RecEosState::default(),
        }
    }
}

/// The skin a fresh install starts on.
pub const DEFAULT_THEME: &str = "blackandwhite";

/// Every skin that ships, and the only values `theme` / `baselineTheme` may
/// hold. Kept in step with the `<option>` list in index.html — a name here that
/// the markup doesn't offer leaves the Active Skin dropdown with nothing
/// selected, and a name in the markup that isn't here gets migrated away the
/// moment it is saved.
pub const AVAILABLE_THEMES: [&str; 15] = [
    "steam-green",
    "steam2010",
    "win98",
    "win95",
    "winxp",
    "royalenoir",
    "winvista",
    "win7",
    "macosclassic",
    "macosaqua",
    "moderndark",
    "modernlight",
    "moderngreen",
    "blackandwhite",
    "blackandwhite-inverted",
];

/// Skins that used to ship and no longer do. A config still naming one is
/// rewritten by `load_config`; see the migration there.
///
/// `"custom"` is the user-built theme, removed along with its palette editor.
/// Leaving it in place would stamp a `theme-custom` class that matches no rules
/// at all, so the window would paint in the base palette with an Active Skin
/// dropdown showing nothing selected.
const REMOVED_THEMES: [&str; 2] = ["recroom", "custom"];

/// Returns the path to config.json inside the app data directory.
pub fn get_config_path(app_handle: &tauri::AppHandle) -> PathBuf {
    let app_data_dir = app_handle.path().app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    app_data_dir.join("config.json")
}

/// Move `src` to `dst`, merging into a `dst` folder that already exists.
///
/// A link inside the tree — a symlink, or a junction, which is what Windows
/// tools make — is moved as the link. Followed, it copied the contents of
/// whatever folder it pointed at, anywhere on the disk, and then deleted the
/// originals there. One that can't be moved as it is (another volume) is left
/// in place, and the folder around it with it.
fn move_dir_recursive(src: std::path::PathBuf, dst: std::path::PathBuf) -> std::io::Result<()> {
    if fs::symlink_metadata(&src)?.file_type().is_symlink() {
        return fs::rename(&src, &dst);
    }
    if src.is_dir() {
        std::fs::create_dir_all(&dst)?;
        for entry in std::fs::read_dir(src.clone())? {
            let entry = entry?;
            move_dir_recursive(entry.path(), dst.join(entry.file_name()))?;
        }
        std::fs::remove_dir(src)?;
    } else {
        std::fs::copy(src.clone(), dst)?;
        std::fs::remove_file(src)?;
    }
    Ok(())
}

/// Migrates data from the legacy Electron `%APPDATA%\radium-launcher` folder
/// to the new Tauri `%APPDATA%\com.radium.launcher` folder.
fn migrate_legacy_data(app_handle: &tauri::AppHandle) {
    if MIGRATED.swap(true, std::sync::atomic::Ordering::SeqCst) { return; }
    if let Ok(data_dir) = app_handle.path().data_dir() {
        let legacy_dir = data_dir.join("radium-launcher");
        if let Ok(new_dir) = app_handle.path().app_data_dir() {
            // Check if legacy dir exists to identify if migration is needed
            if legacy_dir.exists() {
                if !new_dir.exists() {
                    let _ = std::fs::create_dir_all(&new_dir);
                }
                
                // Move config.json first
                let target_config = new_dir.join("config.json");
                let legacy_config = legacy_dir.join("config.json");
                if legacy_config.exists() && !target_config.exists() {
                    let _ = std::fs::rename(&legacy_config, &target_config);
                }

                // Move client folder (containing rec room files)
                let target_client = new_dir.join("client");
                let legacy_client = legacy_dir.join("client");
                
                let legacy_has_client = legacy_client.exists() && (
                    legacy_client.join("RecRoom.exe").exists() ||
                    legacy_client.join("RecRoom_ScreenMode.bat").exists()
                );
                
                let target_has_client = target_client.exists() && (
                    target_client.join("RecRoom.exe").exists() ||
                    target_client.join("RecRoom_ScreenMode.bat").exists()
                );

                if legacy_has_client && !target_has_client {
                    // No delete first: the target holds no client, but it may
                    // hold something else. rename() refuses an existing target,
                    // and the recursive move then merges into it.
                    if std::fs::rename(&legacy_client, &target_client).is_err() {
                        let _ = move_dir_recursive(legacy_client, target_client);
                    }
                } else if legacy_client.exists() && !target_client.exists() {
                    // Fallback standard move
                    if std::fs::rename(&legacy_client, &target_client).is_err() {
                        let _ = move_dir_recursive(legacy_client, target_client);
                    }
                }
                
                // Try to clean up the legacy directory, but only if migrating
                // its contents emptied it. remove_dir (not remove_dir_all)
                // fails on a non-empty directory, so anything the Electron
                // launcher kept there that this migration does not understand
                // is left for the user rather than deleted on their behalf.
                let _ = std::fs::remove_dir(&legacy_dir);
            }
        }
    }
}

/// Move `theme` / `baselineTheme` onto skins that actually ship.
///
/// A name that no longer exists leaves the frontend stamping a class matching
/// no rule — so the window paints in the base palette — and an Active Skin
/// dropdown with nothing selected, since the `<option>` is gone too. Rewriting
/// on load means the stale name isn't re-read on every launch.
///
/// When `theme` was `"custom"`, the baseline underneath it is preferred: that
/// is the skin the user actually picked, and it is very likely the one they
/// want back now the custom layer is gone.
///
/// Returns true if `config` changed.
pub fn migrate_themes(config: &mut Config) -> bool {
    let known = |name: &str| AVAILABLE_THEMES.contains(&name);
    let mut changed = false;

    if REMOVED_THEMES.contains(&config.theme.as_str()) {
        config.theme = if known(&config.baseline_theme) {
            config.baseline_theme.clone()
        } else {
            DEFAULT_THEME.to_string()
        };
        changed = true;
    }
    if !known(&config.baseline_theme) {
        config.baseline_theme = config.theme.clone();
        changed = true;
    }
    // Anything still unrecognised — a hand-edited name, a skin from a future
    // build — falls back rather than painting an unstyled window.
    if !known(&config.theme) {
        config.theme = DEFAULT_THEME.to_string();
        config.baseline_theme = DEFAULT_THEME.to_string();
        changed = true;
    }
    changed
}

/// Canonical form for comparing install paths: forward slashes, lowercase, no
/// trailing separator. Not for display or filesystem use.
fn norm_dir(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// "Is this the same folder?" — separator- and case-insensitive. Install paths
/// reach us from three sources (config.json, the folder picker and
/// `app_data_dir()`) that do not agree on separators or casing, so a plain
/// string compare misses real collisions. Empty is never equal to anything,
/// including another empty.
pub(crate) fn same_dir(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && norm_dir(a) == norm_dir(b)
}

/// True when `path` is `dir` or sits inside it.
fn is_inside(path: &str, dir: &str) -> bool {
    if path.is_empty() || dir.is_empty() {
        return false;
    }
    let (path, dir) = (norm_dir(path), norm_dir(dir));
    path == dir || path.starts_with(&format!("{}/", dir))
}

/// Whether an install dir names the Electron launcher's client folder
/// (`%APPDATA%\radium-launcher\client`) or somewhere inside it — this
/// machine's, given as `legacy_dir`, or the same folder under another profile,
/// as a config.json copied between machines carries.
fn points_at_legacy_client(install_dir: &str, legacy_dir: &str) -> bool {
    if install_dir.is_empty() {
        return false;
    }
    let dir = norm_dir(install_dir);
    is_inside(install_dir, legacy_dir)
        || dir.ends_with("/radium-launcher/client")
        || dir.contains("/radium-launcher/client/")
}

/// Whether `path` resolves to a location inside `dir`.
///
/// Prefers a canonicalized comparison so slash style, drive-letter casing and
/// symlinks can't disguise an escape; falls back to a normalized, separator-
/// aware prefix test when either side can't be canonicalized (a path that does
/// not exist yet, a permission error). The fallback appends a trailing
/// separator so `C:\client2\x` is not treated as inside `C:\client`.
///
/// This is what keeps a recorded — or frontend-supplied — exe path from
/// pointing the launcher at an executable outside the install folder.
pub fn path_is_inside_dir(path: &str, dir: &str) -> bool {
    if path.is_empty() || dir.is_empty() {
        return false;
    }
    match (
        std::fs::canonicalize(std::path::Path::new(path)),
        std::fs::canonicalize(std::path::Path::new(dir)),
    ) {
        (Ok(path_canon), Ok(dir_canon)) => path_canon.starts_with(&dir_canon),
        _ => {
            let mut dir = norm_dir(dir);
            dir.push('/');
            norm_dir(path).starts_with(&dir)
        }
    }
}

/// Whether `dir` contains a game client. Uses the same lookup as launching, so
/// a folder that the launcher would happily run counts as occupied here.
fn dir_holds_client(dir: &std::path::Path) -> bool {
    crate::game::find_game_exe(&dir.to_string_lossy()).is_some()
}

/// Recovers from the cross-network install bug: a Radium client sitting in the
/// Vanilla client folder.
///
/// Before the install directory became per-network, an autosave taken while
/// Vanilla was the active network copied Vanilla's resolved folder into
/// Radium's flat `installDir`. Radium then downloaded into `client-vanilla`,
/// and Vanilla — which has never shipped a build — reported that client as its
/// own install.
///
/// The trigger is a client in Vanilla's *default* folder that Vanilla has no
/// record of (no custom dir, no build, no version, no exe path). Nothing but a
/// Vanilla download writes those fields, so a client there without them was
/// not put there by Vanilla. What happens next depends on how much is known:
///
/// * Radium's config still points into that folder — its install dir is pinned
///   there, or its recorded exe lives there — so the client is provably
///   Radium's and is moved back into Radium's own folder.
/// * Otherwise the folder is only renamed aside, never deleted and never
///   claimed for Radium: the launcher is confident the client is not Vanilla's,
///   not that it is Radium's.
///
/// Returns true if `config` changed.
fn recover_misplaced_radium_install(config: &mut Config, app_data_dir: &std::path::Path) -> bool {
    let vanilla_default = app_data_dir.join(default_client_folder(Network::Vanilla));
    let radium_default = app_data_dir.join(default_client_folder(Network::Radium));
    let vanilla_default_str = vanilla_default.to_string_lossy().to_string();

    if !dir_holds_client(&vanilla_default) {
        return false;
    }
    let v = &config.vanilla;
    let vanilla_recorded_nothing = v.install_dir.is_empty()
        && v.client_build.is_empty()
        && v.client_version.is_empty()
        && v.game_exe_path.is_empty();
    if !vanilla_recorded_nothing {
        return false;
    }

    // Is the client in there provably Radium's, or merely not Vanilla's?
    let radium_points_there = same_dir(&config.install_dir, &vanilla_default_str)
        || is_inside(&config.game_exe_path, &vanilla_default_str);

    if radium_points_there && !dir_holds_client(&radium_default) {
        // Radium's own folder is free, so the client goes home. Nothing here
        // deletes: an absent target is a plain rename, and an existing one is
        // merged into, because whatever else is sitting in that folder is not
        // this function's to remove.
        let moved = if radium_default.exists() {
            move_dir_recursive(vanilla_default.clone(), radium_default.clone()).is_ok()
        } else {
            fs::rename(&vanilla_default, &radium_default).is_ok()
                || move_dir_recursive(vanilla_default.clone(), radium_default.clone()).is_ok()
        };
        if moved {
            config.install_dir = String::new();
            config.game_exe_path =
                crate::game::find_game_exe(&radium_default.to_string_lossy()).unwrap_or_default();
            return true;
        }
    }

    // Not provably Radium's, or Radium's folder is already taken. Move the
    // folder out of the way so Vanilla stops reporting a client it never had,
    // and remember where it went so the UI can point the user at it.
    let Some(aside) = set_aside_path(&vanilla_default) else {
        return false;
    };
    if fs::rename(&vanilla_default, &aside).is_err() {
        return false;
    }
    config.orphaned_client_dir = aside.to_string_lossy().to_string();

    if same_dir(&config.install_dir, &vanilla_default_str) {
        config.install_dir = String::new();
    }
    if is_inside(&config.game_exe_path, &vanilla_default_str) {
        // The recorded client is somewhere nothing points at any more. Drop the
        // stale bookkeeping so the launcher re-derives it from disk instead of
        // reporting a build for a folder it does not read.
        config.game_exe_path = String::new();
        config.client_build = String::new();
        config.client_version = String::new();
        config.client_etag = String::new();
    }
    true
}

/// First free `<dir>.orphan`, `<dir>.orphan2`, … next to `dir`. `None` if the
/// name is somehow taken every time, in which case the caller leaves `dir` be
/// rather than picking a surprising location.
fn set_aside_path(dir: &std::path::Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_string_lossy().to_string();
    let parent = dir.parent()?;
    for n in 1..100 {
        let suffix = if n == 1 {
            ".orphan".to_string()
        } else {
            format!(".orphan{}", n)
        };
        let candidate = parent.join(format!("{}{}", name, suffix));
        if !candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Put an install dir that isn't a full path back to its network's default.
/// Returns true if `config` changed.
///
/// A relative path resolves against the working directory, which changes with
/// how the launcher was started — System32 for the startup entry — so the
/// client would be looked for, downloaded to and deleted from somewhere
/// different each time. The folder picker never produces one; a hand-edited
/// config.json can.
pub fn drop_relative_install_dirs(config: &mut Config) -> bool {
    let relative = |dir: &str| !dir.is_empty() && !std::path::Path::new(dir).is_absolute();
    let mut changed = false;
    for network in Network::ALL {
        let dir = config.install_dir_mut(network);
        if relative(dir) {
            dir.clear();
            changed = true;
        }
    }
    changed
}

/// Enforces that no two networks resolve to the same client folder, or to one
/// inside another. Returns true if `config` was changed.
///
/// They install different games from different sources, so a shared folder
/// means each network's download overwrites the other's install and each then
/// reports the other's client as its own. An explicit dir that collides is
/// cleared, which returns that network to its own default folder. Of a
/// colliding pair, the earlier one in [`Network::ALL`] is cleared first: Radium
/// before the others, because its flat `installDir` is the field the old
/// settings autosave overwrote.
///
/// A folder inside the other's is the same collision a level down: the
/// install check searches four levels deep, so Radium pointed at the app data
/// folder found `client-vanilla` in it, reported Vanilla's client as its own
/// and would have launched it.
pub fn dedupe_install_dirs(app_handle: &tauri::AppHandle, config: &mut Config) -> bool {
    dedupe_install_dirs_at(config, &app_data_dir(app_handle))
}

/// Whether two install folders are the same folder or one holds the other.
pub fn install_dirs_overlap(a: &str, b: &str) -> bool {
    is_inside(a, b) || is_inside(b, a)
}

pub fn dedupe_install_dirs_at(config: &mut Config, app_data_dir: &std::path::Path) -> bool {
    let first_collision = |config: &Config| {
        let dirs = Network::ALL.map(|n| client_dir_for(config, n, app_data_dir));
        (0..dirs.len())
            .flat_map(|i| (i + 1..dirs.len()).map(move |j| (i, j)))
            .find(|&(i, j)| install_dirs_overlap(&dirs[i], &dirs[j]))
            .map(|(i, j)| (Network::ALL[i], Network::ALL[j]))
    };

    let mut changed = false;
    // Each pass clears one override, so this settles within one pass per
    // network. Clearing one can still leave a collision (the other side may
    // in turn point at the cleared network's default folder), hence the loop.
    for _ in 0..Network::ALL.len() {
        let Some((a, b)) = first_collision(config) else { break };
        let Some(network) = [a, b]
            .into_iter()
            .find(|&n| !config.install_dir_for(n).is_empty())
        else {
            // Both are already on their defaults, which are different folders
            // by construction, so there is nothing left to clear.
            break;
        };
        config.install_dir_mut(network).clear();
        changed = true;
    }
    changed
}

/// The config as last read or written, so a command doesn't pay for a disk read
/// and a full JSON parse just to look at one field.
///
/// `ensure_config` is called by roughly fifteen commands — `check_install`,
/// `launch_game`, `download_client`, `open_client_folder`, the Defender pair,
/// `submit_bug_report`, and `cmd_save_config` twice over — so every toggle
/// click, every game-state refresh and every install check re-read and re-parsed
/// the whole file. That is cheap for a few kilobytes and emphatically not cheap
/// once `customTheme.bgImage` holds a base64 background image, which is
/// megabytes parsed per command.
///
/// The launcher enforces a single instance and is the only writer, so the cache
/// can only go stale if something outside edits config.json mid-session; that
/// is picked up on the next restart, which is the same guarantee as before for
/// anyone doing it.
///
/// Held behind an `Arc` so a read shares it rather than copying it: the glass
/// backdrop alone can be a 1.4 MB data URI, and cloning the whole config for
/// every command that wanted one flag was a megabyte-sized copy each time.
static CACHED: std::sync::RwLock<Option<std::sync::Arc<Config>>> = std::sync::RwLock::new(None);

/// Held across every read-modify-write of the config.
///
/// Each updater reads the config, changes its own fields and writes the whole
/// thing back. Two of them at once — a settings autosave landing just as a
/// download stamps in its build id — each read the old copy, and the second
/// write silently undid the first. Taking this for the whole sequence makes
/// them queue. Never hold it across an `.await`.
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the config write lock. See [`WRITE_LOCK`].
pub fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The current config, shared rather than copied. For reading.
///
/// Reads config.json from the app data directory the first time, creating it
/// with defaults if it doesn't exist and running the one-time migrations, and
/// is served from [`CACHED`] after that. While config.json exists but can't be
/// read, this is a fresh install's defaults: fine to look at, never to save —
/// code that changes the config goes through [`ensure_config`] or [`update`],
/// which refuse in that state.
pub fn current(app_handle: &tauri::AppHandle) -> std::sync::Arc<Config> {
    current_checked(app_handle).unwrap_or_else(|_| std::sync::Arc::new(Config::default()))
}

/// [`current`], or an error while config.json exists but can't be read.
pub fn current_checked(app_handle: &tauri::AppHandle) -> Result<std::sync::Arc<Config>, String> {
    if let Some(cfg) = cached() {
        return Ok(cfg);
    }
    // One loader at a time. At startup the page fires a burst of commands at
    // once, and each used to find the cache empty and run the whole load —
    // migrations, the install repair and a save — side by side, the later save
    // able to land over the earlier one's repair. Separate from WRITE_LOCK,
    // which callers may already hold when they get here.
    let _loading = LOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cfg) = cached() {
        return Ok(cfg);
    }
    let (cfg, readable) = load_config(app_handle);
    // A file that exists but couldn't be read is not cached, so the next
    // command tries again rather than serving the stand-in defaults all session.
    if !readable {
        return Err(UNREADABLE.into());
    }
    let cfg = std::sync::Arc::new(cfg);
    *CACHED.write().unwrap_or_else(|e| e.into_inner()) = Some(cfg.clone());
    Ok(cfg)
}

/// Why a change to the config was refused. See [`current_checked`].
const UNREADABLE: &str = "The settings file couldn't be read just now (another program may have it \
     open), so nothing was changed. Try again in a moment.";

fn cached() -> Option<std::sync::Arc<Config>> {
    CACHED.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Serializes the first, uncached load. See [`current`].
static LOAD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read config.json's bytes, giving a briefly locked file a moment to come
/// free.
///
/// Antivirus scanners open a file that was just written, and on Windows a read
/// in that window fails with a sharing violation rather than waiting.
///
/// Bytes rather than a `String`: `read_to_string` fails on anything that is
/// not UTF-8 with the same kind of error a lock gives, so a file saved as
/// UTF-16 — Notepad's "Unicode", or PowerShell 5.1's `>` — was retried, called
/// unreadable, and then refused every settings change for good. What the bytes
/// say is [`decode_config_text`]'s business.
fn read_config_bytes(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let mut attempt = 0;
    loop {
        match fs::read(path) {
            Ok(bytes) => return Ok(bytes),
            Err(e) if attempt >= 4 || e.kind() == std::io::ErrorKind::NotFound => return Err(e),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// config.json's text, in whichever encoding a text editor left it: UTF-8 with
/// or without a byte-order mark, or UTF-16 with one. `None` for bytes that are
/// none of those, which the loader treats like any other corrupt file.
fn decode_config_text(bytes: &[u8]) -> Option<String> {
    let utf16 = |body: &[u8], from: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = body.as_chunks::<2>().0.iter().map(|p| from(*p)).collect();
        String::from_utf16(&units).ok()
    };
    match bytes {
        [0xFF, 0xFE, body @ ..] => utf16(body, u16::from_le_bytes),
        [0xFE, 0xFF, body @ ..] => utf16(body, u16::from_be_bytes),
        _ => std::str::from_utf8(bytes).ok().map(str::to_string),
    }
}

/// Parse config.json, accepting a UTF-8 byte-order mark.
///
/// Notepad's "UTF-8 with BOM", and PowerShell 5.1's `Out-File -Encoding utf8`,
/// both write one, and serde_json rejects it — which read as a corrupt file and
/// reset every setting to the defaults.
fn parse_config(text: &str) -> serde_json::Result<Config> {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
}

/// An owned copy of the current config, for changing and then passing to
/// [`save_config`]. Take [`write_lock`] first. Code that only reads should use
/// [`current`], which doesn't copy.
///
/// An error while config.json exists but can't be read (an antivirus scan
/// holding it, say). The stand-in defaults [`current`] serves then must not be
/// changed and saved: every setting the user had would be replaced with a
/// fresh install's. The load itself already refused to do that; each writer
/// that took a copy of those defaults, changed one field and saved them did it
/// anyway.
pub fn ensure_config(app_handle: &tauri::AppHandle) -> Result<Config, String> {
    current_checked(app_handle).map(|cfg| (*cfg).clone())
}

/// Change the config and save it, as one step under [`write_lock`].
///
/// Refuses, changing nothing, while config.json can't be read — see
/// [`ensure_config`].
pub fn update<R>(app_handle: &tauri::AppHandle, change: impl FnOnce(&mut Config) -> R) -> Result<R, String> {
    let _lock = write_lock();
    let mut cfg = ensure_config(app_handle)?;
    let out = change(&mut cfg);
    save_config(app_handle, &cfg)?;
    Ok(out)
}

/// The uncached read, including the one-time migrations and repairs.
///
/// The flag is false when config.json exists but could not be read, in which
/// case the defaults returned are a stand-in and must not be cached or saved.
fn load_config(app_handle: &tauri::AppHandle) -> (Config, bool) {
    // Run data migration from legacy Electron folder if needed
    migrate_legacy_data(app_handle);

    let config_path = get_config_path(app_handle);

    // Tracks whether anything below modified the config, so we only write back
    // to disk when there is an actual change (avoids redundant I/O on every read).
    let mut changed = false;

    let mut config = if config_path.exists() {
        match read_config_bytes(&config_path) {
            Ok(bytes) => match decode_config_text(&bytes).and_then(|text| parse_config(&text).ok()) {
                Some(cfg) => cfg,
                None => {
                    let backup_path = config_path.with_extension("json.bak");
                    let _ = fs::copy(&config_path, &backup_path);
                    changed = true;
                    Config::default()
                }
            },
            Err(_) => {
                // Still unreadable after the retries in `read_config_text`. Run
                // on the defaults, but neither write them back nor let them be
                // cached: that replaced every setting the user had with a fresh
                // install's over what was usually a moment's lock. The next
                // command reads the file again.
                return (Config::default(), false);
            }
        }
    } else {
        // No config file yet — create one with defaults.
        changed = true;
        Config::default()
    };

    // Migrate old API URL to the current one.
    if config.api_url == "https://ns.radie.app" || config.api_url == "https://ns.radie.app/" {
        config.api_url = "https://api.radie.app/".to_string();
        changed = true;
    }

    // Carry Liquid Glass across the removal of the custom theme editor, so
    // anyone who had it switched on still has it — with their tint and their
    // backdrop — rather than silently losing it on upgrade. Only applied when
    // the new object is still untouched, so it can never overwrite a real
    // choice made since.
    if let Some(legacy) = config.legacy_custom_theme.take() {
        let fresh = GlassSettings::default();
        let untouched = !config.glass.enabled
            && config.glass.tint == fresh.tint
            && config.glass.bg_image.is_empty();
        if untouched && (legacy.glass_enabled || !legacy.glass_bg.is_empty() || !legacy.bg_image.is_empty()) {
            config.glass = GlassSettings {
                enabled: legacy.glass_enabled,
                tint: if legacy.glass_bg.is_empty() { fresh.tint } else { legacy.glass_bg },
                bg_image: legacy.bg_image,
                full_effects: fresh.full_effects,
            };
        }
        // Taken above, so the save below drops `customTheme` from the file.
        changed = true;
    }

    if migrate_themes(&mut config) {
        changed = true;
    }

    // An install dir carried over from the Electron launcher that still names
    // its old client folder goes back to the default one, which is where
    // `migrate_legacy_data` moved that client.
    //
    // Only then. This also used to fire whenever the old folder still held a
    // client — which it does for good when the migration found a client at
    // the new default too and left both alone — and so cleared a custom
    // install folder the user had picked since, on every single start.
    if let Ok(data_dir) = app_handle.path().data_dir() {
        let legacy_client_dir = data_dir.join("radium-launcher").join("client");
        if points_at_legacy_client(&config.install_dir, &legacy_client_dir.to_string_lossy()) {
            config.install_dir = String::new();
            changed = true;
        }
    }

    // Repair the glass tint and backdrop before they can reach the frontend,
    // which interpolates both into a generated stylesheet.
    if config.glass.sanitize() {
        changed = true;
    }
    // Home's order and banners likewise: the banners go into CSS too.
    if config.home.sanitize() {
        changed = true;
    }
    if drop_relative_install_dirs(&mut config) {
        changed = true;
    }

    // Undo a Radium client that the pre-fix settings autosave dropped into the
    // Vanilla folder, then guarantee the two networks resolve to separate
    // folders regardless of how the config got into its current state.
    let data_dir = app_data_dir(app_handle);
    if !INSTALL_REPAIR_DONE.swap(true, std::sync::atomic::Ordering::SeqCst)
        && recover_misplaced_radium_install(&mut config, &data_dir)
    {
        changed = true;
    }
    // Stop pointing at a set-aside folder the user has since deleted.
    if !config.orphaned_client_dir.is_empty()
        && !std::path::Path::new(&config.orphaned_client_dir).exists()
    {
        config.orphaned_client_dir = String::new();
        changed = true;
    }
    if dedupe_install_dirs_at(&mut config, &data_dir) {
        changed = true;
    }
    // Last, with the install folders settled: whether each network's are
    // excluded from Defender depends on where they are.
    if config.adopt_legacy_defender_flags(&data_dir) {
        changed = true;
    }
    if config.refresh_defender_flags(&data_dir) {
        changed = true;
    }

    // Persist only when something actually changed, to avoid rewriting
    // config.json on every command that reads the config.
    if changed {
        let _ = save_config(app_handle, &config);
    }

    (config, true)
}

/// Serializes and writes the config to config.json in the app data directory.
///
/// Updates [`CACHED`] on success, so the memoized copy and the file never
/// disagree.
///
/// Each network's `defender_excluded` is worked out here, from the folders on
/// record and where the client is now (see [`Config::refresh_defender_flags`]),
/// whatever the caller set it to: a settings save after the install folder
/// moved still carries the page's old `true`.
pub fn save_config(app_handle: &tauri::AppHandle, config: &Config) -> Result<(), String> {
    let config_path = get_config_path(app_handle);

    // Ensure the parent directory exists.
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let mut config = config.clone();
    config.refresh_defender_flags(&app_data_dir(app_handle));

    let json =
        serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;

    let temp_path = config_path.with_extension("json.tmp");
    write_synced(&temp_path, json.as_bytes()).map_err(|e| e.to_string())?;
    replace_file(&temp_path, &config_path).map_err(|e| {
        let _ = fs::remove_file(&temp_path);
        e.to_string()
    })?;

    // Only after the rename succeeded: a failed write must leave the cache
    // holding what is actually on disk, not what we hoped to put there.
    *CACHED.write().unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(config));

    Ok(())
}

/// Write `bytes` to `path` and flush them to the disk before returning.
///
/// The rename that follows is what makes a save atomic, but only for the
/// names: without the flush, a power cut shortly after can leave config.json
/// renamed into place with nothing written in it yet, which loads as a corrupt
/// file and puts every setting back to its default.
fn write_synced(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Move `from` over `to`, giving a locked target a moment to come free.
///
/// The same antivirus scan that [`read_config_bytes`] waits out holds the file
/// just written, and replacing it then fails with a sharing violation — which
/// reported "Save failed" for a settings change nothing was wrong with.
fn replace_file(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt >= 4 || e.kind() == std::io::ErrorKind::NotFound => return Err(e),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// Returns the game client directory for `network`.
///
/// If that network's configured install dir is non-empty, the client goes in
/// a subfolder of it named for the network (see [`custom_client_dir`]);
/// otherwise the default location is returned. The two networks default to
/// separate folders (`client` and `client-vanilla`) so both clients can be
/// installed at once without one uninstall wiping the other.
pub fn get_client_dir_for(
    app_handle: &tauri::AppHandle,
    config: &Config,
    network: Network,
) -> String {
    client_dir_for(config, network, &app_data_dir(app_handle))
}

/// Folder each network installs into by default, relative to the app data dir.
/// The two must never be equal — see [`dedupe_install_dirs_at`].
pub fn default_client_folder(network: Network) -> &'static str {
    match network {
        Network::Radium => "client",
        Network::Vanilla => "client-vanilla",
        Network::Stella => "client-stella",
        Network::RecEos => "client-receos",
    }
}

/// The subfolder a custom install location gets for each network.
pub fn network_subfolder(network: Network) -> &'static str {
    match network {
        Network::Radium => "Radium",
        Network::Vanilla => "Vanilla",
        Network::Stella => "Stella",
        Network::RecEos => "Rec EOS",
    }
}

/// Where `network`'s client goes when the user has picked `picked` as its
/// install location: a subfolder named for the network, the way Steam keeps
/// each game in its own folder under a library. The extraction writes
/// thousands of files by name, so straight into a shared folder like
/// `D:\Games` it overwrote anything of the same name already there.
///
/// Applied here, where the folder is resolved, rather than when it is picked,
/// so every custom location gets it, including ones saved before. The one
/// exception is a pick already named for the network, so `D:\Games\Stella`
/// does not become `D:\Games\Stella\Stella`.
pub fn custom_client_dir(picked: &str, network: Network) -> String {
    let name = network_subfolder(network);
    let path = std::path::Path::new(picked);
    if path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
    {
        return picked.to_string();
    }
    path.join(name).to_string_lossy().to_string()
}

pub fn app_data_dir(app_handle: &tauri::AppHandle) -> PathBuf {
    app_handle
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// [`get_client_dir_for`] without the app handle, so the resolution rule has a
/// single definition that tests can exercise directly.
pub fn client_dir_for(config: &Config, network: Network, app_data_dir: &std::path::Path) -> String {
    resolve_install_dir(config.install_dir_for(network), network, app_data_dir)
}

/// The folder Stella's patch DLL is kept in: in app data, outside every client
/// folder (see `stella::patch_path`).
pub fn stella_patch_dir(app_data_dir: &std::path::Path) -> PathBuf {
    app_data_dir.join("stella")
}

/// Every folder `network`'s Windows Defender exclusion has to cover: its
/// client folder, and for Stella also the folder its patch is kept in, which
/// is injected into the game — a quarantined patch is as broken as a
/// quarantined client.
pub fn exclusion_dirs_for(config: &Config, network: Network, app_data_dir: &std::path::Path) -> Vec<String> {
    // Vanilla's game is in Vanilla's launcher's game folder, not a Rec EOS one.
    if network == Network::Vanilla {
        if let Some(dir) = crate::game::vanilla_launcher_game_dir() {
            return vec![dir];
        }
    }
    let mut dirs = vec![client_dir_for(config, network, app_data_dir)];
    if network == Network::Stella {
        dirs.push(stella_patch_dir(app_data_dir).to_string_lossy().to_string());
    }
    dirs
}

/// Where `network`'s client is for an install location of `configured`: its
/// default folder when nothing is set, else its subfolder of what is (see
/// [`custom_client_dir`]).
///
/// A location that *is* the network's default folder counts as unset. The old
/// settings autosave wrote the default path it was showing into the field, so
/// configs carry `%APPDATA%\com.radium.launcher\client` for Radium with the
/// client right there. Given a subfolder like any other pick, that install
/// went missing behind `client\Radium`, and the next download put a second
/// copy in there.
pub fn resolve_install_dir(configured: &str, network: Network, app_data_dir: &std::path::Path) -> String {
    let default = app_data_dir.join(default_client_folder(network)).to_string_lossy().to_string();
    if configured.trim().is_empty() || same_dir(configured, &default) {
        return default;
    }
    custom_client_dir(configured, network)
}

#[cfg(test)]
// These tests start from `Config::default()` and then set the one or two fields
// the case is actually about. Clippy would rather see a struct-update
// expression, but `Config` has twenty-odd fields and the point of each test is
// which ones it touches — spelling that as `..Default::default()` buries the
// subject among the defaults. Scoped to the test module so `-D warnings` still
// means something everywhere else.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::download::REQUIRED_CLIENT_BUILD;

    /// Rebuild a Config the way `cmd_save_config` does: JSON from the frontend
    /// is deserialized straight into a `Config`, so any field the frontend
    /// omits (or holds a stale value for) lands as the serde default.
    fn config_from_frontend_json(json: serde_json::Value) -> Config {
        serde_json::from_value::<Config>(json).expect("frontend config should deserialize")
    }

    #[test]
    fn stale_autosave_does_not_clobber_downloaded_build() {
        // On disk after a successful download: build id + version + ETag stamped in.
        let mut on_disk = Config::default();
        on_disk.client_build = REQUIRED_CLIENT_BUILD.to_string();
        on_disk.client_version = "0.9.2".to_string();
        on_disk.client_etag = "\"etag-xyz\"".to_string();
        on_disk.client_version_sync_prompted = true;
        on_disk.game_exe_path = "C:/client/Recroom_Release.exe".to_string();

        // What the settings UI actually sends on autosave: it was loaded at
        // startup (before the download) so it carries no client fields, plus a
        // genuine settings change the user just made.
        let mut incoming = config_from_frontend_json(serde_json::json!({
            "theme": "steam-green",
            "minimizeOnLaunch": false,
            "closeOnLaunch": true
        }));
        // Sanity: the stale copy really is missing the build id.
        assert_eq!(incoming.client_build, "", "frontend copy should be stale/empty");

        incoming.preserve_backend_managed_fields(&on_disk);

        // The download's fields survive the save untouched...
        assert_eq!(incoming.client_build, REQUIRED_CLIENT_BUILD);
        assert_eq!(incoming.client_version, "0.9.2");
        assert_eq!(incoming.client_etag, "\"etag-xyz\"");
        assert!(incoming.client_version_sync_prompted);
        assert_eq!(incoming.game_exe_path, "C:/client/Recroom_Release.exe");

        // ...and the user's real settings change is still applied.
        assert!(!incoming.minimize_on_launch);
        assert!(incoming.close_on_launch);
    }

    #[test]
    fn preserved_build_is_not_flagged_outdated() {
        // The exact regression: check_install computes
        // `client_outdated = client_build != REQUIRED_CLIENT_BUILD`.
        let mut on_disk = Config::default();
        on_disk.client_build = REQUIRED_CLIENT_BUILD.to_string();

        let mut incoming = Config::default(); // stale: client_build == ""
        let outdated_before = incoming.client_build != REQUIRED_CLIENT_BUILD;
        assert!(outdated_before, "stale save alone would report outdated (the bug)");

        incoming.preserve_backend_managed_fields(&on_disk);

        let outdated_after = incoming.client_build != REQUIRED_CLIENT_BUILD;
        assert!(!outdated_after, "after preserving, client is correctly up to date");
    }

    #[test]
    fn stale_autosave_does_not_clobber_vanilla_install() {
        // Same hazard as the Radium case: a Vanilla download stamps its exe
        // path, build and ETag into the nested `vanilla` object, and a settings
        // autosave that was loaded beforehand would otherwise revert them.
        let mut on_disk = Config::default();
        on_disk.vanilla.game_exe_path = "C:/client-vanilla/RecRoom.exe".to_string();
        on_disk.vanilla.client_build = REQUIRED_CLIENT_BUILD.to_string();
        on_disk.vanilla.client_version = "1.2.3".to_string();
        on_disk.vanilla.client_etag = "\"vanilla-etag\"".to_string();
        on_disk.vanilla.client_url = "https://example.invalid/old.zip".to_string();

        // The settings UI sends the whole config, including a *new* client URL
        // the user just typed, but a stale (empty) copy of everything the
        // backend owns.
        let mut incoming = config_from_frontend_json(serde_json::json!({
            "network": "vanilla",
            "vanilla": { "clientUrl": "https://example.invalid/new.zip" }
        }));
        assert_eq!(incoming.vanilla.client_build, "", "frontend copy should be stale");

        incoming.preserve_backend_managed_fields(&on_disk);

        // Backend-owned Vanilla fields survive...
        assert_eq!(incoming.vanilla.game_exe_path, "C:/client-vanilla/RecRoom.exe");
        assert_eq!(incoming.vanilla.client_build, REQUIRED_CLIENT_BUILD);
        assert_eq!(incoming.vanilla.client_version, "1.2.3");
        assert_eq!(incoming.vanilla.client_etag, "\"vanilla-etag\"");
        // ...while the URL the settings UI owns is the one the user just set.
        assert_eq!(incoming.vanilla.client_url, "https://example.invalid/new.zip");
        assert_eq!(incoming.network(), Network::Vanilla);
    }

    #[test]
    fn networks_keep_separate_install_state() {
        // The whole point of the nested object: uninstalling one network must
        // not disturb the other's recorded install.
        let mut cfg = Config::default();
        cfg.set_client_install(
            Network::Radium,
            "C:/client/Recroom_Release.exe".into(),
            REQUIRED_CLIENT_BUILD.into(),
            "0.9.2".into(),
            "r-etag".into(),
        );
        cfg.set_client_install(
            Network::Vanilla,
            "C:/client-vanilla/RecRoom.exe".into(),
            REQUIRED_CLIENT_BUILD.into(),
            "1.2.3".into(),
            "v-etag".into(),
        );

        cfg.clear_client_install(Network::Vanilla);

        assert_eq!(cfg.game_exe_for(Network::Radium), "C:/client/Recroom_Release.exe");
        assert_eq!(cfg.client_version_for(Network::Radium), "0.9.2");
        assert_eq!(cfg.game_exe_for(Network::Vanilla), "");
        assert_eq!(cfg.client_version_for(Network::Vanilla), "");
    }

    #[test]
    fn a_glass_tint_that_is_not_a_colour_is_repaired() {
        // The tint is interpolated into a generated stylesheet by the frontend,
        // and config.json is a hand-editable file people swap between
        // themselves. A value carrying a `;` or a `}` would close the
        // declaration and open a rule of its own.
        let mut glass = GlassSettings::default();
        glass.tint = "#fff; } body { background: url(https://example.invalid/x) } .z {".into();
        assert!(glass.sanitize());
        assert_eq!(glass.tint, GlassSettings::default().tint);

        // A valid CSS colour that isn't the hex form we accept still goes.
        glass.tint = "red".into();
        assert!(glass.sanitize());
        assert_eq!(glass.tint, GlassSettings::default().tint);

        // 8-digit hex is legitimate and must survive.
        glass.tint = "#00ff00aa".into();
        assert!(!glass.sanitize());
        assert_eq!(glass.tint, "#00ff00aa");
    }

    #[test]
    fn a_glass_backdrop_is_held_to_a_url_it_is_safe_to_interpolate() {
        let mut glass = GlassSettings::default();

        // The two shapes the picker and the URL field actually produce.
        for good in [
            "https://example.invalid/wallpaper.jpg",
            // What the file picker stores. Its `;` is fine inside the quoted
            // url(); this used to be dropped, losing every local image on save.
            "data:image/jpeg;base64,/9j/4AAQSkZJRg+/=",
            "",
        ] {
            glass.bg_image = good.into();
            assert!(!glass.sanitize(), "{good:?} should not be reported as repaired");
            assert_eq!(glass.bg_image, good, "{good:?} should survive");
        }

        // Anything that could terminate the declaration, or reach a scheme the
        // backdrop has no business using.
        for bad in [
            "https://x/a.jpg') no-repeat; } body { color: red } .z {",
            "https://x/a.jpg;color:red",
            "data:image/png;base64,AAAA') } body { color: red } .z {",
            "javascript:alert(1)",
            "http://example.invalid/x.jpg",
            "file:///C:/Windows/win.ini",
            "https://x/a(b).jpg",
        ] {
            glass.bg_image = bad.into();
            assert!(glass.sanitize(), "{bad:?} should be reported as repaired");
            assert_eq!(glass.bg_image, "", "{bad:?} should be dropped");
        }
    }

    #[test]
    fn clean_glass_settings_are_left_exactly_as_they_are() {
        // Sanitizing must not report a change for settings that are already
        // fine — `ensure_config` persists on any change, so a false positive
        // would rewrite config.json on every single launch.
        let mut glass = GlassSettings {
            enabled: true,
            tint: "#ABC".into(),
            bg_image: "https://example.invalid/bg.png".into(),
            full_effects: true,
        };
        let before = glass.clone();
        assert!(!glass.sanitize(), "nothing here needs repairing");
        assert_eq!(glass.tint, before.tint);
        assert_eq!(glass.bg_image, before.bg_image);
        assert!(glass.enabled);
    }

    #[test]
    fn hex_colours_are_recognised_by_shape_not_by_first_character() {
        for good in ["#fff", "#FFFF", "#a1b2c3", "#A1B2C3D4", "  #fff  "] {
            assert!(is_hex_color(good), "{good:?} should be accepted");
        }
        for bad in [
            "",
            "fff",
            "#",
            "#ff",
            "#fffff",
            "#gggggg",
            "#fff;",
            "#fff }",
            "red",
            "rgb(1,2,3)",
            "url(https://example.invalid)",
        ] {
            assert!(!is_hex_color(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn unknown_network_name_falls_back_to_radium() {
        // A corrupt or future value must never strand the user on a network the
        // launcher can't talk to.
        assert_eq!(Network::parse(None), Network::Radium);
        assert_eq!(Network::parse(Some("")), Network::Radium);
        assert_eq!(Network::parse(Some("nonsense")), Network::Radium);
        assert_eq!(Network::parse(Some("vanilla")), Network::Vanilla);
    }

    #[test]
    fn a_save_without_the_backdrop_keeps_the_stored_one() {
        // The frontend strips `glass.bgImage` from whole-config saves; it is
        // written by its own command. A save must not blank it.
        let mut on_disk = Config::default();
        on_disk.glass.bg_image = "data:image/jpeg;base64,/9j/AAAA".to_string();

        let mut incoming = config_from_frontend_json(serde_json::json!({
            "glass": { "enabled": true, "tint": "#123456" }
        }));
        assert_eq!(incoming.glass.bg_image, "");

        incoming.preserve_backend_managed_fields(&on_disk);

        assert_eq!(incoming.glass.bg_image, "data:image/jpeg;base64,/9j/AAAA");
        // The rest of the glass object is still the frontend's to change.
        assert!(incoming.glass.enabled);
        assert_eq!(incoming.glass.tint, "#123456");
    }

    #[test]
    fn a_save_without_the_home_banners_keeps_the_stored_ones() {
        // Whole-config saves leave the banners out; `cmd_set_home_banner`
        // writes them. The layout itself is the frontend's to change.
        let mut on_disk = Config::default();
        on_disk.home.banners.stella = "data:image/jpeg;base64,/9j/BBBB".to_string();

        let mut incoming = config_from_frontend_json(serde_json::json!({
            "home": { "order": ["friends", "status", "rooms"], "hidden": ["rooms"] }
        }));
        assert_eq!(incoming.home.banners.stella, "");

        incoming.preserve_backend_managed_fields(&on_disk);

        assert_eq!(incoming.home.banners.stella, "data:image/jpeg;base64,/9j/BBBB");
        assert_eq!(incoming.home.order, ["friends", "status", "rooms"]);
        assert_eq!(incoming.home.hidden, ["rooms"]);
    }

    #[test]
    fn a_config_without_home_gets_the_default_layout() {
        // Configs from before the Home settings existed.
        let cfg = config_from_frontend_json(serde_json::json!({ "theme": "win98" }));
        assert_eq!(cfg.home.order, ["status", "friends", "rooms"]);
        assert!(cfg.home.hidden.is_empty());
        assert_eq!(cfg.home.style, "showcase");
        assert_eq!(cfg.home.side, ["status", "friends"]);
    }

    #[test]
    fn dashboard_side_column_keeps_known_sections_each_once() {
        let mut home = HomeSettings { side: vec!["rooms".into(), "x".into(), "rooms".into()], ..HomeSettings::default() };
        assert!(home.sanitize());
        assert_eq!(home.side, ["rooms"]);
        // Empty is a choice too: everything under the game.
        let cfg = config_from_frontend_json(serde_json::json!({ "home": { "side": [] } }));
        assert!(cfg.home.side.is_empty());
    }

    #[test]
    fn home_style_is_kept_when_known_and_reset_otherwise() {
        let cfg = config_from_frontend_json(serde_json::json!({ "home": { "style": "dashboard" } }));
        assert_eq!(cfg.home.style, "dashboard");

        let mut home = HomeSettings { style: "dashboard".into(), ..HomeSettings::default() };
        assert!(!home.sanitize());
        for bad in ["", "Dashboard", "x\"); color: red", "spotlight "] {
            home.style = bad.into();
            assert!(home.sanitize(), "{bad:?} should be reset");
            assert_eq!(home.style, "showcase");
        }
    }

    #[test]
    fn home_order_is_repaired_to_known_sections_each_once() {
        let mut home = HomeSettings {
            order: vec!["rooms".into(), "bogus".into(), "rooms".into(), "status".into()],
            hidden: vec!["rooms".into(), "rooms".into(), "<script>".into()],
            ..HomeSettings::default()
        };
        assert!(home.sanitize());
        // Known names in the saved order, then whatever was missing.
        assert_eq!(home.order, ["rooms", "status", "friends"]);
        assert_eq!(home.hidden, ["rooms"]);
        // Already clean: nothing to report.
        assert!(!home.sanitize());
    }

    #[test]
    fn only_paintable_banners_are_kept() {
        let good = "data:image/jpeg;base64,/9j/AAAA";
        let mut home = HomeSettings::default();
        home.banners.radium = good.into();
        assert!(!home.sanitize());
        assert_eq!(home.banners.radium, good);

        for bad in [
            "https://example.invalid/banner.png",
            "javascript:alert(1)",
            "data:image/png;base64,AAAA\"); background: red",
            "data:text/html;base64,AAAA",
        ] {
            home.banners.vanilla = bad.into();
            assert!(home.sanitize(), "{bad:?} should be dropped");
            assert_eq!(home.banners.vanilla, "");
        }

        home.banners.stella = format!("data:image/jpeg;base64,{}", "A".repeat(HOME_BANNER_MAX_BYTES));
        assert!(home.sanitize(), "an oversized banner should be dropped");
        assert_eq!(home.banners.stella, "");
    }

    // ── Windows Defender exclusions ─────────────────────────────────────────

    fn defender_data_dir() -> PathBuf {
        PathBuf::from("C:/data/com.radium.launcher")
    }

    #[test]
    fn the_folders_defender_excluded_survive_a_stale_settings_save() {
        let data = defender_data_dir();
        let mut on_disk = Config::default();
        on_disk.set_defender_dirs(Network::Radium, exclusion_dirs_for(&on_disk, Network::Radium, &data));

        // The page loaded before the exclusion was added: no folders, flag off.
        let mut incoming = Config::default();
        incoming.preserve_backend_managed_fields(&on_disk);
        incoming.refresh_defender_flags(&data);

        assert_eq!(incoming.defender_dirs_for(Network::Radium), on_disk.defender_dirs_for(Network::Radium));
        assert!(incoming.defender_excluded, "the folder on record is the client's, so it is excluded");
    }

    #[test]
    fn the_flag_comes_from_the_record_not_from_the_page() {
        let data = defender_data_dir();
        // The page says excluded, with nothing on record: nothing is.
        let mut cfg = Config::default();
        cfg.defender_excluded = true;
        assert!(cfg.refresh_defender_flags(&data));
        assert!(!cfg.defender_excluded);
    }

    #[test]
    fn a_client_moved_to_another_folder_is_no_longer_excluded() {
        let data = defender_data_dir();
        let mut cfg = Config::default();
        cfg.set_defender_dirs(Network::Radium, exclusion_dirs_for(&cfg, Network::Radium, &data));
        cfg.refresh_defender_flags(&data);
        assert!(cfg.defender_excluded);

        cfg.install_dir = "D:/Games".into();
        assert!(cfg.refresh_defender_flags(&data));
        assert!(!cfg.defender_excluded, "D:/Games/Radium was never excluded");
        // The old folder stays on record, for the next exclusion to take out.
        assert_eq!(cfg.defender_dirs_for(Network::Radium).len(), 1);
    }

    #[test]
    fn a_folder_inside_an_excluded_one_counts_as_excluded() {
        // An exclusion made when a custom location held the client directly
        // still covers the network's subfolder of it.
        let data = defender_data_dir();
        let mut cfg = Config::default();
        cfg.install_dir = "D:/Games".into();
        cfg.set_defender_dirs(Network::Radium, vec!["D:\\Games".into()]);
        cfg.refresh_defender_flags(&data);
        assert!(cfg.defender_excluded);
    }

    #[test]
    fn stella_is_excluded_only_with_its_patch_folder() {
        let data = defender_data_dir();
        let mut cfg = Config::default();
        let client_only = vec![client_dir_for(&cfg, Network::Stella, &data)];
        cfg.set_defender_dirs(Network::Stella, client_only);
        cfg.refresh_defender_flags(&data);
        assert!(!cfg.stella.defender_excluded, "the patch folder isn't excluded");

        cfg.set_defender_dirs(Network::Stella, exclusion_dirs_for(&cfg, Network::Stella, &data));
        cfg.refresh_defender_flags(&data);
        assert!(cfg.stella.defender_excluded);
        assert!(!cfg.defender_excluded && !cfg.vanilla.defender_excluded, "per network");
    }

    #[test]
    fn an_older_launchers_flag_is_given_the_folders_it_was_set_for() {
        let data = defender_data_dir();
        let mut cfg = Config::default();
        cfg.vanilla.defender_excluded = true;

        assert!(cfg.adopt_legacy_defender_flags(&data));
        assert_eq!(cfg.vanilla.defender_dirs, exclusion_dirs_for(&cfg, Network::Vanilla, &data));
        cfg.refresh_defender_flags(&data);
        assert!(cfg.vanilla.defender_excluded, "an upgrade must not ask to exclude it again");
        // Done once: nothing left to adopt.
        assert!(!cfg.adopt_legacy_defender_flags(&data));
        assert!(cfg.defender_dirs.is_empty(), "Radium's flag was off");
    }

    // ── Per-network install directories ─────────────────────────────────────
    //
    // Regression cover for the cross-network install bug: the settings autosave
    // copied the *active* network's resolved folder into Radium's flat
    // `installDir`, so after a visit to Vanilla, Radium pointed at
    // `client-vanilla`. Radium's download then extracted there and Vanilla —
    // which has never shipped a build — reported that client as installed.

    /// Scratch app-data dir for the tests that touch the filesystem.
    fn temp_app_data(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radium-cfg-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp app data dir");
        dir
    }

    fn write_client(dir: &std::path::Path) {
        fs::create_dir_all(dir).expect("client dir");
        fs::write(dir.join("RecRoom.exe"), b"stub").expect("client exe");
    }

    #[test]
    fn networks_default_to_separate_client_folders() {
        let cfg = Config::default();
        let data = PathBuf::from("C:/data");
        assert_ne!(
            client_dir_for(&cfg, Network::Radium, &data),
            client_dir_for(&cfg, Network::Vanilla, &data)
        );
    }

    #[test]
    fn radium_pointed_at_the_vanilla_folder_is_reset_to_its_own() {
        let data = PathBuf::from("C:/data");
        let mut cfg = Config::default();
        cfg.install_dir = "C:/data/client-vanilla".to_string();

        assert!(dedupe_install_dirs_at(&mut cfg, &data));
        assert_eq!(cfg.install_dir, "", "the colliding pin is dropped");
        assert_ne!(
            client_dir_for(&cfg, Network::Radium, &data),
            client_dir_for(&cfg, Network::Vanilla, &data)
        );
    }

    /// Separators and casing differ between config.json, the folder picker and
    /// `app_data_dir()`, so the collision check cannot be a string compare.
    #[test]
    fn collision_is_detected_across_separators_and_casing() {
        let data = PathBuf::from("C:/data");
        let mut cfg = Config::default();
        cfg.install_dir = "C:\\Data\\Client-Vanilla\\".to_string();

        assert!(dedupe_install_dirs_at(&mut cfg, &data));
        assert_eq!(cfg.install_dir, "");
    }

    #[test]
    fn vanilla_pointed_at_the_radium_folder_is_reset_to_its_own() {
        let data = PathBuf::from("C:/data");
        let mut cfg = Config::default();
        cfg.vanilla.install_dir = "C:/data/client".to_string();

        assert!(dedupe_install_dirs_at(&mut cfg, &data));
        assert_eq!(cfg.vanilla.install_dir, "");
        assert_ne!(
            client_dir_for(&cfg, Network::Radium, &data),
            client_dir_for(&cfg, Network::Vanilla, &data)
        );
    }

    /// Two networks given one custom folder each get their own subfolder in
    /// it, so nothing needs splitting apart.
    #[test]
    fn a_shared_custom_folder_gives_each_network_a_subfolder() {
        let data = PathBuf::from("C:/data");
        let mut cfg = Config::default();
        cfg.install_dir = "D:/Games/RecRoom".to_string();
        cfg.vanilla.install_dir = "D:/Games/RecRoom".to_string();

        assert!(!dedupe_install_dirs_at(&mut cfg, &data));
        assert!(norm_dir(&client_dir_for(&cfg, Network::Radium, &data)).ends_with("d:/games/recroom/radium"));
        assert!(norm_dir(&client_dir_for(&cfg, Network::Vanilla, &data)).ends_with("d:/games/recroom/vanilla"));
    }

    /// A custom location always gets the network's subfolder, empty or not,
    /// unless it is already named for that network.
    #[test]
    fn a_custom_location_resolves_to_the_network_subfolder() {
        let data = PathBuf::from("C:/data");
        let mut cfg = Config::default();
        cfg.stella.install_dir = r"C:\Users\me\Downloads\games".into();
        assert_eq!(
            norm_dir(&client_dir_for(&cfg, Network::Stella, &data)),
            "c:/users/me/downloads/games/stella"
        );
        cfg.stella.install_dir = r"D:\Games\stella".into();
        assert_eq!(client_dir_for(&cfg, Network::Stella, &data), r"D:\Games\stella");
        assert_eq!(norm_dir(&custom_client_dir(r"D:\Games\stella", Network::Radium)), "d:/games/stella/radium");
    }

    /// A saved location that is the network's own default folder (what the
    /// old autosave wrote) is that folder, not a subfolder of it.
    #[test]
    fn a_saved_default_folder_is_the_default_folder() {
        let data = PathBuf::from(r"C:\Users\me\AppData\Roaming\com.radium.launcher");
        let mut cfg = Config::default();
        cfg.install_dir = r"C:\Users\me\AppData\Roaming\com.radium.launcher\client".into();
        assert_eq!(client_dir_for(&cfg, Network::Radium, &data), data.join("client").to_string_lossy());
        // Spelled with other slashes and case, as the sources disagree.
        cfg.install_dir = "c:/users/me/appdata/roaming/com.radium.launcher/CLIENT/".into();
        assert_eq!(client_dir_for(&cfg, Network::Radium, &data), data.join("client").to_string_lossy());
        cfg.stella.install_dir = data.join("client-stella").to_string_lossy().to_string();
        assert_eq!(client_dir_for(&cfg, Network::Stella, &data), data.join("client-stella").to_string_lossy());
        // Another network's default is still a pick like any other.
        cfg.vanilla.install_dir = data.join("client").to_string_lossy().to_string();
        assert!(norm_dir(&client_dir_for(&cfg, Network::Vanilla, &data)).ends_with("/client/vanilla"));
        assert!(dedupe_install_dirs_at(&mut cfg, &data));
    }

    /// Stella is held to the same rule against both of the others.
    #[test]
    fn stella_never_shares_a_folder_with_another_network() {
        let data = PathBuf::from("C:/data");
        let all_apart = |cfg: &Config| {
            let dirs = Network::ALL.map(|n| client_dir_for(cfg, n, &data));
            !install_dirs_overlap(&dirs[0], &dirs[1])
                && !install_dirs_overlap(&dirs[0], &dirs[2])
                && !install_dirs_overlap(&dirs[1], &dirs[2])
        };

        let cfg = Config::default();
        assert!(all_apart(&cfg));
        assert_eq!(client_dir_for(&cfg, Network::Stella, &data), data.join("client-stella").to_string_lossy());

        // Stella pointed at Vanilla's default folder.
        let mut cfg = Config::default();
        cfg.stella.install_dir = "C:/data/client-vanilla".into();
        assert!(dedupe_install_dirs_at(&mut cfg, &data));
        assert_eq!(cfg.stella.install_dir, "");

        // All three pointed at one folder: each has its own subfolder in it.
        let mut cfg = Config::default();
        cfg.install_dir = "D:/Games".into();
        cfg.vanilla.install_dir = "D:/Games".into();
        cfg.stella.install_dir = "D:/Games".into();
        assert!(!dedupe_install_dirs_at(&mut cfg, &data));
        assert!(all_apart(&cfg));

        // Radium's subfolder is the folder Stella was pointed at, so one
        // would hold the other: Radium gives way.
        let mut cfg = Config::default();
        cfg.install_dir = "D:/Games".into();
        cfg.stella.install_dir = "D:/Games/Radium".into();
        assert!(dedupe_install_dirs_at(&mut cfg, &data));
        assert!(all_apart(&cfg));
        assert_eq!(cfg.install_dir, "");

        // A relative Stella folder is dropped like the others.
        let mut cfg = Config::default();
        cfg.stella.install_dir = "games\\stella".into();
        assert!(drop_relative_install_dirs(&mut cfg));
        assert_eq!(cfg.stella.install_dir, "");
    }

    /// Stella's accepted patch is written by backend commands only, so a stale
    /// settings save must not revert it — that would refuse the next launch.
    #[test]
    fn a_stale_save_keeps_the_accepted_stella_patch() {
        let mut on_disk = Config::default();
        on_disk.stella.patch_sha256 = "abc".into();
        on_disk.stella.game_exe_path = "C:/data/client-stella/RecRoom.exe".into();
        let mut incoming = Config::default();
        incoming.preserve_backend_managed_fields(&on_disk);
        assert_eq!(incoming.stella.patch_sha256, "abc");
        assert_eq!(incoming.stella.game_exe_path, "C:/data/client-stella/RecRoom.exe");

        incoming.clear_client_install(Network::Stella);
        assert_eq!(incoming.stella.patch_sha256, "");
        assert_eq!(Network::parse(Some("stella")), Network::Stella);
        assert_eq!(Network::Stella.as_str(), "stella");
    }

    /// One network's folder inside the other's is the same collision a level
    /// down: the install check searches beneath the folder it is given.
    #[test]
    fn a_folder_inside_the_other_networks_is_split_apart() {
        let data = PathBuf::from("C:/data");

        // Radium pointed at Vanilla's default folder: its subfolder would sit
        // inside Vanilla's client.
        let mut cfg = Config::default();
        cfg.install_dir = "C:\\Data\\client-vanilla\\".to_string();
        assert!(dedupe_install_dirs_at(&mut cfg, &data));
        assert_eq!(cfg.install_dir, "");

        // The folder holding the defaults is fine now: Radium gets
        // `C:\Data\Radium`, beside them rather than around them.
        let mut cfg = Config::default();
        cfg.install_dir = "C:\\Data\\".to_string();
        assert!(!dedupe_install_dirs_at(&mut cfg, &data));

        // Vanilla pointed inside Radium's default.
        let mut cfg = Config::default();
        cfg.vanilla.install_dir = "C:/data/client/vanilla".to_string();
        assert!(dedupe_install_dirs_at(&mut cfg, &data));
        assert_eq!(cfg.vanilla.install_dir, "");

        // Two custom folders, one inside the other.
        let mut cfg = Config::default();
        cfg.install_dir = "D:/Games/Radium/Vanilla".into();
        cfg.vanilla.install_dir = "D:/Games/Radium".into();
        assert!(dedupe_install_dirs_at(&mut cfg, &data));
        assert!(!install_dirs_overlap(
            &client_dir_for(&cfg, Network::Radium, &data),
            &client_dir_for(&cfg, Network::Vanilla, &data)
        ));

        // Neighbours that share a prefix are not nested.
        let mut cfg = Config::default();
        cfg.install_dir = "D:/Games/Radium".into();
        cfg.vanilla.install_dir = "D:/Games/Radium-Vanilla".into();
        assert!(!dedupe_install_dirs_at(&mut cfg, &data));
    }

    #[test]
    fn distinct_custom_dirs_are_left_alone() {
        let data = PathBuf::from("C:/data");
        let mut cfg = Config::default();
        cfg.install_dir = "D:/Games/Radium".to_string();
        cfg.vanilla.install_dir = "D:/Games/Vanilla".to_string();

        assert!(!dedupe_install_dirs_at(&mut cfg, &data));
        assert_eq!(cfg.install_dir, "D:/Games/Radium");
        assert_eq!(cfg.vanilla.install_dir, "D:/Games/Vanilla");
    }

    /// The repair for users who already hit the bug: their Radium client sits
    /// in `client-vanilla` and, since Radium's own folder is free, comes home.
    #[test]
    fn a_radium_client_left_in_the_vanilla_folder_is_moved_back() {
        let data = temp_app_data("recover");
        write_client(&data.join("client-vanilla"));

        let mut cfg = Config::default();
        cfg.install_dir = data.join("client-vanilla").to_string_lossy().to_string();
        cfg.game_exe_path = data
            .join("client-vanilla")
            .join("RecRoom.exe")
            .to_string_lossy()
            .to_string();
        cfg.client_build = "recroom-baby-2016".to_string();

        assert!(recover_misplaced_radium_install(&mut cfg, &data));
        assert!(data.join("client").join("RecRoom.exe").exists());
        assert!(
            !data.join("client-vanilla").exists(),
            "Vanilla must stop claiming a client it never had"
        );
        assert_eq!(cfg.install_dir, "", "Radium tracks its own default again");
        assert!(is_inside(
            &cfg.game_exe_path,
            &data.join("client").to_string_lossy()
        ));
        assert_eq!(cfg.client_build, "recroom-baby-2016", "same client, just moved");
        assert_eq!(cfg.orphaned_client_dir, "", "nothing was left behind");

        let _ = fs::remove_dir_all(&data);
    }

    /// A real Vanilla install must never be touched, even while Radium's
    /// install dir is (wrongly) pinned to the same folder.
    #[test]
    fn a_real_vanilla_install_is_never_moved() {
        let data = temp_app_data("keep-vanilla");
        write_client(&data.join("client-vanilla"));

        let mut cfg = Config::default();
        cfg.install_dir = data.join("client-vanilla").to_string_lossy().to_string();
        cfg.vanilla.client_build = "vanilla-build".to_string();

        assert!(!recover_misplaced_radium_install(&mut cfg, &data));
        assert!(data.join("client-vanilla").join("RecRoom.exe").exists());
        assert!(!data.join("client").exists());

        // The pin is still wrong, so the collision guard is what unwinds it.
        assert!(dedupe_install_dirs_at(&mut cfg, &data));
        assert_eq!(cfg.install_dir, "");

        let _ = fs::remove_dir_all(&data);
    }

    /// Radium's own folder is already occupied, so the duplicate cannot go
    /// home. It is renamed aside rather than deleted, and the stale bookkeeping
    /// for the abandoned folder is dropped — otherwise the launcher would
    /// report a build for a folder it no longer reads.
    #[test]
    fn a_duplicate_is_set_aside_when_it_cannot_be_moved_back() {
        let data = temp_app_data("occupied");
        write_client(&data.join("client-vanilla"));
        write_client(&data.join("client"));

        let mut cfg = Config::default();
        cfg.install_dir = data.join("client-vanilla").to_string_lossy().to_string();
        cfg.game_exe_path = data
            .join("client-vanilla")
            .join("RecRoom.exe")
            .to_string_lossy()
            .to_string();
        cfg.client_build = "recroom-baby-2016".to_string();
        cfg.client_version = "0.9.2".to_string();

        assert!(recover_misplaced_radium_install(&mut cfg, &data));
        assert!(!data.join("client-vanilla").exists());
        assert!(
            data.join("client-vanilla.orphan").join("RecRoom.exe").exists(),
            "set aside, never deleted"
        );
        assert!(data.join("client").join("RecRoom.exe").exists(), "Radium's own install is untouched");
        assert_eq!(
            cfg.orphaned_client_dir,
            data.join("client-vanilla.orphan").to_string_lossy().to_string()
        );
        assert_eq!(cfg.install_dir, "");
        assert_eq!(cfg.game_exe_path, "");
        assert_eq!(cfg.client_build, "");
        assert_eq!(cfg.client_version, "");

        let _ = fs::remove_dir_all(&data);
    }

    /// The shape this bug leaves behind once the config has been reset: neither
    /// network points at the stray folder any more, so nothing proves it is
    /// Radium's. It is not claimed for Radium — only moved out of the way, so
    /// Vanilla stops reporting an install it never had.
    #[test]
    fn an_unclaimed_client_in_the_vanilla_folder_is_set_aside_not_adopted() {
        let data = temp_app_data("unclaimed");
        write_client(&data.join("client-vanilla"));

        let mut cfg = Config::default();

        assert!(recover_misplaced_radium_install(&mut cfg, &data));
        assert!(!data.join("client").exists(), "an unproven client is not adopted");
        assert!(data.join("client-vanilla.orphan").join("RecRoom.exe").exists());
        assert_eq!(
            cfg.orphaned_client_dir,
            data.join("client-vanilla.orphan").to_string_lossy().to_string()
        );

        let _ = fs::remove_dir_all(&data);
    }

    /// A second run must not clobber the folder set aside by the first.
    #[test]
    fn a_second_set_aside_folder_gets_its_own_name() {
        let data = temp_app_data("twice");
        write_client(&data.join("client-vanilla"));
        write_client(&data.join("client-vanilla.orphan"));

        let mut cfg = Config::default();
        assert!(recover_misplaced_radium_install(&mut cfg, &data));
        assert!(data.join("client-vanilla.orphan").join("RecRoom.exe").exists());
        assert!(data.join("client-vanilla.orphan2").join("RecRoom.exe").exists());

        let _ = fs::remove_dir_all(&data);
    }

    /// An empty Vanilla folder is the normal case and must be left alone.
    #[test]
    fn an_empty_vanilla_folder_is_not_touched() {
        let data = temp_app_data("empty");
        fs::create_dir_all(data.join("client-vanilla")).expect("dir");

        let mut cfg = Config::default();
        assert!(!recover_misplaced_radium_install(&mut cfg, &data));
        assert!(data.join("client-vanilla").exists());

        let _ = fs::remove_dir_all(&data);
    }

    /// The set-aside path is backend-managed: a stale settings autosave must
    /// not wipe the only record of where the user's files went.
    #[test]
    fn stale_autosave_does_not_clobber_the_set_aside_path() {
        let mut on_disk = Config::default();
        on_disk.orphaned_client_dir = "C:/data/client-vanilla.orphan".to_string();

        let mut incoming = config_from_frontend_json(serde_json::json!({ "theme": "moderngreen" }));
        assert_eq!(incoming.orphaned_client_dir, "");

        incoming.preserve_backend_managed_fields(&on_disk);
        assert_eq!(incoming.orphaned_client_dir, "C:/data/client-vanilla.orphan");
    }

    /// Nothing in the repair path deletes. When Radium's folder already exists
    /// but holds no client — a cancelled download, or files the user put there
    /// — the client is merged in alongside them, not dropped on top of a
    /// wiped folder.
    #[test]
    fn moving_a_client_home_never_deletes_what_is_already_there() {
        let data = temp_app_data("merge");
        write_client(&data.join("client-vanilla"));
        fs::create_dir_all(data.join("client")).expect("dir");
        fs::write(data.join("client").join("notes.txt"), b"keep me").expect("stray file");

        let mut cfg = Config::default();
        cfg.install_dir = data.join("client-vanilla").to_string_lossy().to_string();

        assert!(recover_misplaced_radium_install(&mut cfg, &data));
        assert!(data.join("client").join("RecRoom.exe").exists(), "client moved home");
        assert!(
            data.join("client").join("notes.txt").exists(),
            "an unrelated file in the target folder survives"
        );
        assert_eq!(
            fs::read_to_string(data.join("client").join("notes.txt")).expect("read"),
            "keep me"
        );

        let _ = fs::remove_dir_all(&data);
    }

    // ── Executable containment ──────────────────────────────────────────────

    /// The launcher spawns whatever `gameExePath` names, and for Radium that
    /// value arrives from the frontend. It must never resolve outside the
    /// client directory.
    #[test]
    fn an_exe_outside_the_client_dir_is_not_contained() {
        let data = temp_app_data("contain");
        let client = data.join("client");
        write_client(&client);
        fs::write(data.join("evil.exe"), b"stub").expect("stub");

        let client_str = client.to_string_lossy().to_string();
        assert!(path_is_inside_dir(
            &client.join("RecRoom.exe").to_string_lossy(),
            &client_str
        ));
        assert!(!path_is_inside_dir(
            &data.join("evil.exe").to_string_lossy(),
            &client_str
        ));
        assert!(
            !path_is_inside_dir(r"C:\Windows\System32\cmd.exe", &client_str),
            "an absolute path elsewhere on disk is never inside the client dir"
        );

        let _ = fs::remove_dir_all(&data);
    }

    /// Traversal has to be resolved, not string-matched: the prefix of this
    /// path looks like the client dir.
    #[test]
    fn traversal_out_of_the_client_dir_is_not_contained() {
        let data = temp_app_data("traversal");
        let client = data.join("client");
        write_client(&client);
        fs::write(data.join("evil.exe"), b"stub").expect("stub");

        let escape = client.join("..").join("evil.exe").to_string_lossy().to_string();
        assert!(!path_is_inside_dir(&escape, &client.to_string_lossy()));

        let _ = fs::remove_dir_all(&data);
    }

    /// A sibling folder whose name starts with the client dir's name is a
    /// different folder, not a child of it.
    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_contained() {
        assert!(!path_is_inside_dir(
            r"C:\data\client2\RecRoom.exe",
            r"C:\data\client"
        ));
        assert!(path_is_inside_dir(
            r"C:/data/client/RecRoom.exe",
            r"C:\data\client"
        ));
    }

    /// A config saved while a since-removed skin existed must not keep naming
    /// it: the class it stamps matches nothing now, so the window would paint
    /// in the base palette with an empty Active Skin dropdown.
    #[test]
    fn a_config_naming_a_removed_skin_falls_back_to_the_default() {
        let mut cfg: Config = serde_json::from_str(
            r#"{"theme":"recroom","baselineTheme":"recroom","minimizeOnLaunch":true}"#,
        )
        .expect("it should still load");

        assert!(migrate_themes(&mut cfg));
        assert_eq!(cfg.theme, DEFAULT_THEME);
        assert_eq!(cfg.baseline_theme, DEFAULT_THEME);
        assert!(cfg.minimize_on_launch, "the rest of the config survives");
    }

    /// The custom theme is gone. Someone using one had also picked a baseline
    /// skin underneath it — that is a real choice, and it is the one to land on
    /// rather than resetting them to the stock default.
    #[test]
    fn a_custom_theme_falls_back_to_the_skin_it_was_built_on() {
        let mut cfg: Config = serde_json::from_str(
            r#"{"theme":"custom","baselineTheme":"win7"}"#,
        )
        .expect("it should still load");

        assert!(migrate_themes(&mut cfg));
        assert_eq!(cfg.theme, "win7", "the baseline they chose is kept");
        assert_eq!(cfg.baseline_theme, "win7");
    }

    /// A custom theme whose baseline is itself unusable has nothing to fall
    /// back to but the default.
    #[test]
    fn a_custom_theme_with_no_usable_baseline_lands_on_the_default() {
        for json in [
            r#"{"theme":"custom","baselineTheme":"recroom"}"#,
            r#"{"theme":"custom","baselineTheme":"custom"}"#,
            r#"{"theme":"custom"}"#,
        ] {
            let mut cfg: Config = serde_json::from_str(json).expect("it should load");
            assert!(migrate_themes(&mut cfg), "{json} should be migrated");
            assert_eq!(cfg.theme, DEFAULT_THEME, "{json}");
            assert_eq!(cfg.baseline_theme, DEFAULT_THEME, "{json}");
        }
    }

    #[test]
    fn a_skin_that_still_ships_is_left_alone() {
        // Migration runs on every load, so a false positive would rewrite
        // config.json every launch.
        for name in AVAILABLE_THEMES {
            let mut cfg = Config::default();
            cfg.theme = name.to_string();
            cfg.baseline_theme = name.to_string();
            assert!(!migrate_themes(&mut cfg), "{name} should need no migration");
            assert_eq!(cfg.theme, name);
        }
    }

    /// The two lists have to agree or a theme is unreachable from the UI.
    #[test]
    fn the_default_skin_is_one_that_ships_and_the_markup_offers() {
        assert!(AVAILABLE_THEMES.contains(&DEFAULT_THEME));

        let html = include_str!("../../src/index.html");
        let select = html
            .split_once(r#"id="cfgTheme""#)
            .and_then(|(_, rest)| rest.split_once("</select>"))
            .map(|(body, _)| body)
            .expect("index.html has an Active Skin dropdown");

        for name in AVAILABLE_THEMES {
            assert!(
                select.contains(&format!(r#"value="{}""#, name)),
                "{name} is in AVAILABLE_THEMES but the dropdown does not offer it"
            );
        }
        let offered = select.matches("<option").count();
        assert_eq!(
            offered,
            AVAILABLE_THEMES.len(),
            "the dropdown offers {offered} skins but AVAILABLE_THEMES lists {}",
            AVAILABLE_THEMES.len()
        );
    }

    /// Glass predates this change and has to survive it: someone who had it on
    /// should still have it on, with the tint and backdrop they chose.
    #[test]
    fn liquid_glass_survives_the_removal_of_the_custom_theme() {
        // `r##"..."##`: the hex colours below contain `"#`, which would close
        // an `r#"..."#` string mid-literal.
        let mut cfg: Config = serde_json::from_str(
            r##"{"theme":"custom","baselineTheme":"moderndark","customTheme":{
                 "bgDark":"#111111","green":"#00ff00","styleBase":"modern",
                 "glassEnabled":true,"glassBg":"#1a0b2e",
                 "bgImage":"https://example.invalid/bg.png"}}"##,
        )
        .expect("an old config should still load");

        // The same two steps load_config performs, in the same order.
        let legacy = cfg.legacy_custom_theme.take().expect("the old object parses");
        cfg.glass = GlassSettings {
            enabled: legacy.glass_enabled,
            tint: legacy.glass_bg,
            bg_image: legacy.bg_image,
            full_effects: true,
        };
        migrate_themes(&mut cfg);

        assert!(cfg.glass.enabled, "glass stays on");
        assert_eq!(cfg.glass.tint, "#1a0b2e", "their tint is kept");
        assert_eq!(cfg.glass.bg_image, "https://example.invalid/bg.png");
        assert_eq!(cfg.theme, "moderndark", "and they land on their baseline skin");

        // The dropped palette must not come back when the config is written.
        let written = serde_json::to_string(&cfg).expect("serializes");
        assert!(!written.contains("customTheme"), "the stale object is not re-saved");
        assert!(!written.contains("bgDark"), "the palette is gone");
        assert!(written.contains("\"glass\""), "glass is stored in its own object");
    }

    /// A config saved by Notepad as "UTF-8 with BOM" (or by PowerShell 5.1's
    /// `Out-File -Encoding utf8`) must load, not be treated as corrupt and
    /// replaced with the defaults.
    #[test]
    fn a_config_with_a_byte_order_mark_still_loads() {
        let cfg = parse_config("\u{feff}{\"theme\":\"win98\",\"minimizeOnLaunch\":true}")
            .expect("a BOM is not corruption");
        assert_eq!(cfg.theme, "win98");
        assert!(cfg.minimize_on_launch);
        assert!(parse_config("{\"theme\":\"win7\"}").is_ok(), "no BOM still loads");
        assert!(parse_config("{ not json").is_err(), "real corruption is still caught");
    }

    #[cfg(windows)]
    #[test]
    fn an_install_dir_that_is_not_a_full_path_goes_back_to_the_default() {
        let mut cfg = Config::default();
        cfg.install_dir = "client".into();
        cfg.vanilla.install_dir = r"D:\Games\Vanilla".into();
        assert!(drop_relative_install_dirs(&mut cfg));
        assert_eq!(cfg.install_dir, "", "relative: reset");
        assert_eq!(cfg.vanilla.install_dir, r"D:\Games\Vanilla", "absolute: kept");

        for full in [r"C:\x", "D:/Games", r"\\server\share\client"] {
            cfg.install_dir = full.into();
            assert!(!drop_relative_install_dirs(&mut cfg), "{full} is a full path");
        }
        for partial in [r"\client", "C:client", r"..\client"] {
            cfg.install_dir = partial.into();
            assert!(drop_relative_install_dirs(&mut cfg), "{partial} depends on where the launcher started");
        }
    }

    /// A config.json saved as UTF-16 — Notepad's "Unicode", PowerShell 5.1's
    /// `>` — used to fail the read itself, which counted as "locked", so every
    /// settings change was refused for good. It loads now.
    #[test]
    fn a_config_saved_as_utf16_still_loads() {
        let json = "{\"theme\":\"win95\"}";
        let le: Vec<u8> = [0xFF, 0xFE].into_iter().chain(json.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain(json.encode_utf16().flat_map(u16::to_be_bytes)).collect();
        for bytes in [le, be] {
            let text = decode_config_text(&bytes).expect("UTF-16 with a BOM decodes");
            assert_eq!(parse_config(&text).expect("and parses").theme, "win95");
        }
        let bom8 = decode_config_text(b"\xEF\xBB\xBF{\"theme\":\"win7\"}").expect("UTF-8 with a BOM");
        assert_eq!(parse_config(&bom8).expect("parses").theme, "win7");

        // Bytes that are no text at all are corrupt, not unreadable: the
        // loader keeps a backup and starts over rather than refusing forever.
        assert!(decode_config_text(&[0xC3, 0x28, 0xA0]).is_none());
        assert!(decode_config_text(&[0xFF, 0xFE, 0x00, 0xD8]).is_none(), "a lone surrogate");
    }

    /// A junction inside a folder being moved is moved as the junction. It
    /// used to be followed, which copied out — and then deleted — whatever
    /// folder it pointed at, wherever that was.
    #[cfg(windows)]
    #[test]
    fn moving_a_folder_does_not_follow_a_junction_out_of_it() {
        let root = temp_app_data("junction");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(outside.join("precious.txt"), b"keep").expect("outside file");
        let src = root.join("src");
        fs::create_dir_all(&src).expect("src");
        fs::write(src.join("RecRoom.exe"), b"stub").expect("client file");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(src.join("link"))
            .arg(&outside)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skipped: couldn't create a junction here");
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let dst = root.join("dst");
        fs::create_dir_all(&dst).expect("dst exists, so the move merges");
        move_dir_recursive(src.clone(), dst.clone()).expect("move");

        assert!(outside.join("precious.txt").exists(), "the junction's target is untouched");
        assert!(dst.join("RecRoom.exe").exists());
        assert!(
            fs::symlink_metadata(dst.join("link")).expect("link moved").file_type().is_symlink(),
            "moved as a link, not copied as a folder"
        );
        let _ = fs::remove_dir(dst.join("link"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_fresh_install_starts_on_black_and_white_and_does_not_minimize() {
        let cfg = Config::default();
        assert_eq!(cfg.theme, "blackandwhite");
        assert_eq!(cfg.baseline_theme, "blackandwhite");
        assert!(!cfg.minimize_on_launch);
        assert!(!cfg.glass.enabled, "glass is opt-in");
        assert!(cfg.glass.full_effects, "full effects default to on for whenever glass is");
    }

    /// Every config.json in the wild still carries the `launchOptions` and
    /// `font` keys that Settings used to write. Those settings are gone, but
    /// the keys stay on disk until the next save rewrites the file, so the
    /// loader has to skip them rather than fail the whole parse and reset
    /// every other setting the user has.
    #[test]
    fn a_config_written_when_launch_options_and_fonts_existed_still_loads() {
        let old = r#"{"launchOptions":"-fullscreen","font":"minecraft","theme":"win98",
                      "vanilla":{"installDir":"C:/v","launchOptions":"-windowed"}}"#;
        let cfg: Config = serde_json::from_str(old).expect("an older config should still load");

        assert_eq!(cfg.theme, "win98");
        assert_eq!(cfg.vanilla.install_dir, "C:/v");
    }

    /// Only an install dir naming the Electron launcher's old client folder is
    /// reset. A folder picked since is the user's, whatever the old folder
    /// still holds.
    #[test]
    fn only_the_electron_client_folder_counts_as_legacy() {
        let legacy = r"C:\Users\Jane\AppData\Roaming\radium-launcher\client";
        for old in [
            legacy,
            r"c:/users/jane/appdata/roaming/RADIUM-LAUNCHER/client/",
            r"C:\Users\Jane\AppData\Roaming\radium-launcher\client\nested",
            // The same folder in a config.json brought over from another PC.
            r"C:\Users\Someone Else\AppData\Roaming\radium-launcher\client",
        ] {
            assert!(points_at_legacy_client(old, legacy), "{old:?} is the old folder");
        }
        for mine in [
            "",
            r"D:\Games\Radium",
            r"C:\Users\Jane\AppData\Roaming\com.radium.launcher\client",
            r"C:\Users\Jane\AppData\Roaming\radium-launcher\client-backup",
            r"C:\Users\Jane\AppData\Roaming\radium-launcher",
        ] {
            assert!(!points_at_legacy_client(mine, legacy), "{mine:?} is not the old folder");
        }
    }
}
