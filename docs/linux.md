# Linux builds and Proton setup

The native Linux launcher targets **Ubuntu 22.04+ and Debian 12+ on x64**.
Packages are `.deb` and AppImage. **Linux gameplay validation is pending.**
All four networks must pass the desktop tests below before a release claims
full Linux support. Builds and synthetic desktop tests alone do not establish
game, Steam authentication, graphics, VR, or anti-cheat compatibility.

## Installation and Steam

Install native Linux Steam, open it, sign in, and install a stable Valve Proton
release from Steam's Tools library. Install the Steam Linux Runtime required by
that Proton release as well; the launcher's runtime check names a missing app ID.
Flatpak Steam, ARM64, and Steam Deck Gaming Mode are outside this first release.

Install GPU drivers meeting the selected Proton version's
[DXVK requirements](https://github.com/doitsujin/dxvk/wiki/Driver-support).
Runtime readiness confirms the Steam, Proton and runtime files; graphics and
game authentication still need their own tests.

Install the `.deb` with `sudo apt install ./Rec*.deb`. For an AppImage, make it
executable and run it. AppImage users need their distribution's WebKitGTK 4.1,
tray support, `xdg-utils`, and `libnotify-bin` (`notify-send`). On Wayland,
AppImage uses XWayland through Tauri's GTK hook; install `xwayland`. The `.deb`
also runs with GTK's native Wayland backend. Some systems also
need `libfuse2`, or can extract the image with `--appimage-extract`.

In **Settings → General → Linux / Proton**, leave fields empty for automatic
Steam discovery and the newest installed stable Valve Proton. Additional Steam
libraries are read from `libraryfolders.vdf`. You can choose the Steam directory
and a Proton directory manually, and choose a separate compatibility-data
directory for each network. This directory contains `pfx/`; select its parent,
not `pfx/` itself. Official Steam `steamapps/compatdata` directories and shared
network prefixes are rejected. Prefixes are retained when client files are
uninstalled. Runtime checks reject unwritable compatibility-data directories
and existing `pfx` directories before starting Proton. Close the game and finish
file operations before changing runtimes.

Keep compatibility-data directories outside all game client folders. Launch,
update and uninstall refuse a nested prefix, including a `pfx` symlink pointing
into a client. Move the whole prefix outside the client folder and update its
selection first. Cleanup also stops when it cannot inspect a prefix directory;
an unreadable symlink target must not be treated as a nonexistent path. These
cleanup guards do not depend on Steam or Proton still being installed.

- **Rec EOS:** downloads and signature/hash verification work as on Windows.
  Proton receives the `version=n,b` override for the bundled Doorstop loader.
- **Radium:** runs the detected Windows client or legacy batch launcher through
  Proton. Its required Steam app check reads Steam's Linux library manifests.
- **Stella:** Steam sign-in uses the native Linux Steam SDK. A bundled Windows
  helper runs inside the Stella prefix, verifies the accepted patch again,
  launches the game, and confirms patch loading. Failed injection closes the
  game. Request/result files live in a fresh private directory because Proton
  redirects Windows console streams. A complete result is published only after
  verification; an unpublishable result also closes the game. Steam tickets
  never go through this helper, these files, or its command line.
- **Vanilla:** GET VANILLA opens the verified official MSI under Proton.
  Finish its installer, then press PLAY to open the official launcher. Sign in,
  install/update the game, and press Play there. Linux does not run Windows UI
  automation or manufacture anti-cheat sessions. Change game options and manage
  installation in that launcher. Proton receives `winhttp=n,b` for Vanilla's
  bundled client loader. The community
  [Vanilla on Linux guide](https://drive.proton.me/urls/E9F4QR503C#7yZkVX6souQS)
  also calls for a Wine-compatible WebView2 runtime, rather than assuming the
  latest Evergreen release works. Its installer is pinned in
  [Bottles' dependency manifest](https://github.com/bottlesdevs/dependencies/blob/main/Essentials/webview2.yml).
  Run prerequisite installers in Vanilla's own prefix with its selected Proton
  and matching Steam runtime; installing WebView2 on Windows does not supply it
  to Proton. Review Microsoft's runtime license before installation. This
  launcher does not silently download or install that prerequisite.
  Vanilla's published PC platform is Windows;
  a Proton incompatibility blocks a full-support claim.

Steam readiness, installed-client, server, version, file integrity and signature
checks remain active. Optional warnings can be skipped; mandatory file/patch and
runtime validation cannot. Windows Defender and Smart App Control have no Linux
equivalent and their controls are hidden. VR remains experimental.

## Desktop behavior and files

Linux uses a native tray menu. Tray support depends on the desktop; GNOME may
need an AppIndicator extension. Tray mode and Start at login are opt-in. Without
a registered tray host, closing quits and a background startup shows the main window.
Starting the application again also restores its existing window.

Notifications use the desktop notification service. Windows desktop capture and
frost effects use a solid background on Linux. Launcher updates open GitHub
release downloads; install the new Linux package manually.

The `.deb` declares OpenSSL 3 and the GStreamer base/good plugins used by notification audio.
AppImage bundles matching multimedia plugins, following
[Tauri's multimedia packaging guidance](https://v2.tauri.app/distribute/appimage/).
External desktop tools and Proton run with host libraries rather than inheriting
AppImage's bundled GTK/GLib library paths.

Default locations (respecting XDG overrides):

| Content | Location |
| --- | --- |
| Configuration and client directories | `$XDG_DATA_HOME/com.receos.launcher/` (usually `~/.local/share/com.receos.launcher/`) |
| Per-network Proton data | `…/com.receos.launcher/proton/<network>/pfx/` |
| Thumbnail cache | `$XDG_CACHE_HOME/com.receos.launcher/thumbs/` |
| Startup entry | `$XDG_CONFIG_HOME/autostart/com.receos.launcher.desktop` |

Logs use Tauri's local application data directory. Vanilla's launcher settings
are resolved from the prefix's AppData registry entry. Vanilla's web session is
kept in memory only on Linux, so sign in again after restarting the launcher.

## Building from Windows 10 and Linux

On Windows, normal `npm run dev`, `npm run build`, and x64 commands retain their
Windows behavior. To prepare the Linux Stella helper:

```powershell
npm ci
npm run prepare:proton-helper
```

Copy `src-tauri/resources/receos-proton-helper.exe` to the Linux checkout at the
same path. This is generated output and is not committed. The Desktop builds
GitHub Actions workflow does this automatically using Windows and Ubuntu jobs;
it uploads artifacts without publishing a GitHub release or using signing keys.
Windows CI installers are unsigned; normal local Windows builds still require
the existing update-signing key.

On Ubuntu/Debian install the [Tauri system prerequisites](https://v2.tauri.app/start/prerequisites/),
Node.js 24 and Rust. Then:

```bash
npm ci
node scripts/prepare-linux.mjs
cd src-tauri && cargo test --locked --lib && cd ..
npm run dev:linux
npm run build:linux
```

Preparation copies Linux's Steam redistributable from the locked
`steamworks-sys` SDK into generated resources. `STEAM_SDK_LOCATION` can override
the SDK root. `RECEOS_PROTON_HELPER` can name a prebuilt helper to copy. Linux
packages must be built on Linux; Windows commands explain how to use CI.

## Release validation gate

Record the distro, desktop/display server, GPU/driver, Steam, Proton/runtime
versions, package format and result for each test. The initial container matrix
covers account-free checks. Follow-up signed-in Steam and Proton results are
recorded separately below; all four real gameplay gates remain open.

- [x] Production `.deb` installs and starts on Ubuntu 22.04, Ubuntu 24.04, and Debian 12 in containers.
- [x] Production AppImage starts on all three distros, with its Steam SDK and helper present.
- [x] Account-free startup, themes, audio, native tray protocol/menu actions,
      notifications, autostart entry controls and release-download updates pass
      under X11 and headless Wayland (AppImage uses XWayland).
- [ ] Repeat desktop smoke tests on physical Linux desktops with normal WebKit
      sandboxing, GPU drivers, panel implementations and a real login session.
- [ ] Signed-in accounts, downloads and game file verification work end to end
      from the installed Linux frontend on X11 and Wayland.
- [ ] Rec EOS installs, validates its mod, signs in, joins a room/friend, and exits.
- [ ] Radium installs, handles its Steam prerequisites, starts screen mode, and exits.
- [ ] Stella signs in through native Steam, loads its verified patch, joins, and exits.
- [x] A missing Stella patch blocks native Linux launch; the Windows helper
      refuses a modified patch before spawning a game.
- [x] Controlled Windows process tests confirm successful patch loading,
      fail-closed cleanup, rejection of another DLL with the same filename,
      and rejection of game exit during loading.
- [x] The same four controlled patch-loading cases pass inside Valve Proton
      11.0 and its required Steam Linux Runtime 4.0. These are private fixtures,
      not the real Stella game.
- [x] User-completed native Linux Steam sign-in produces a nonempty native
      Steam session ticket. Account identifiers and ticket contents are not logged.
- [x] The real Stella client loads the exact user-accepted patch under Proton
      before reporting success; actual process monitoring and Stop pass.
- [x] Stella closes a real Proton game after patch-loading failure and reports
      success only after the accepted patch loads.
- [ ] Vanilla installs its official launcher, signs in, updates, and starts a game
      through that launcher with its normal anti-cheat checks passing.
- [x] Synthetic mapped game processes exercise actual `/proc` discovery, launcher
      restart recovery, game-closed events and pidfd Stop while preserving
      unrelated prefix/client processes. No wrapper PID is required.
- [ ] Repeat lifecycle and wrapper-exit checks with real Proton games.
- [x] Rec EOS's real Proton process survives a normal launcher Quit/restart,
      is recovered by the restarted launcher, and is closed by Stop while Steam stays open.
- [x] Automated fixtures cover missing Steam/Proton/runtime, paths with spaces,
      case-sensitive paths, symlink containment and protected/shared prefixes.
      A missing tray host leaves the window reachable.
- [x] Reject an unwritable prefix before starting Proton, with an actionable
      error; a real launcher IPC test also leaves the original configuration intact.
- [x] An explicitly selected Proton symlink path containing spaces launches the
      real installed client and supports Stop.
- [x] Move the real Rec EOS client to a games library with spaces, launch and
      detect it there, Stop it, and restore its original files/configuration.
- [ ] Windows 10 startup, all four launch paths, tray, installer and signed updates
      retain their behavior.

Only mark these checks passed after observing the result. Keep the pending
label in the UI, artifacts and documentation until the game matrix passes.

### Automated validation recorded on 2026-11-10

- Windows 10 x64: 282 regular Rust tests passed. All 15 opt-in tests also passed,
  including per-user startup registry round trips, live release signatures and
  downloads, real HTTPS file repair, Vanilla public room/player/image parsing,
  the official Vanilla installer signature, and full pinned Rec EOS base archive
  hash/extraction. The release NSIS installer built, was signed with the existing
  update key, and passed the launcher's actual signature verifier.
- Windows compatibility helper: three request/patch validation tests and four
  native process-loading integration tests passed; the statically linked x64
  helper built successfully. The integration tests first reproduced two false
  success cases: an unrelated DLL sharing the patch filename, and process exit
  during loading. The shared loader now checks the loaded file's identity and
  module address before reporting success. Failed loading closes its owned child.
- Ubuntu 22.04 in Docker on this Windows machine: 267 regular Rust tests and all
  14 opt-in tests passed, including full Rec EOS archive extraction on Linux.
  Production x64 `.deb` and AppImage packages were built on glibc 2.35 and
  installed on Ubuntu 22.04, Ubuntu 24.04 and Debian 12.
- Native WebDriver integration exercises the packaged application's actual IPC,
  D-Bus tray menus, notification service, XDG startup files and HTTPS URI handler.
  Steam metadata and game processes are synthetic fixtures; no fixture Proton
  executable or actual game is run. The Steam SDK helper's missing-session
  failure is exercised with the packaged native library.
- All 12 package/display/distro combinations passed 15 desktop checks each.
  Missing-tray-host close and background-start regressions passed on all three
  distributions as well. The `.deb` used native GTK Wayland; AppImage's default
  X11 backend connected to Weston's XWayland server in Wayland-session cases.
- Desktop tests use UID 1000, Xvfb for X11, Weston for Wayland, software rendering,
  a silent PulseAudio sink, and a minimal StatusNotifier host. Notifications use
  a real Dunst service on a companion X server, including in Wayland cases.
  WebKit sandboxing is disabled only inside these disposable test containers.
  These results do not validate physical GPUs, every desktop panel, notification
  positioning on Wayland, or the normal sandbox on a physical Linux desktop.
- Validation found and fixed AppImage library contamination of desktop tools,
  missing multimedia bundling, a hidden window when no tray host exists, and
  the Windows build wrapper's signing working directory.
- After the patch-loader fix, both package formats were refreshed with the new
  Windows helper. Their native Linux executables were compared byte for byte
  with the builds above, including each format's existing bundle marker.
  All four Ubuntu package/display combinations passed their 15 desktop checks
  again, along with both missing-tray-host regressions.
- The GitHub Actions workflow now includes a three-distro desktop matrix after
  the Ubuntu release build. Equivalent local container jobs were run here;
  the remote workflow has not been dispatched or published.
- Follow-up with Windows Steam signed in: the packaged Steam-ticket helper
  obtained a nonempty ticket and exited after stdin closed. Ticket contents
  and account identifiers were not logged. The subsequent Linux check is
  recorded below; Windows Steam alone cannot establish Proton compatibility.
- Windows visual regression was attempted, but the computer-use window capture
  timed out twice. No visual or interactive game checks are marked passed from
  that attempt. The only local WSL distribution is Docker's; no separately
  booted Linux desktop or remote GPU host has been supplied.
- An interactive Ubuntu 22.04/XFCE desktop was prepared in Docker with native
  Steam, its normal dependency checks, nested user namespaces, and Mesa 23.2.1
  software rendering on the WSL2 6.6.87.2 kernel. The launcher started with
  normal WebKit sandboxing and XFCE's actual StatusNotifier host. Actual frontend
  IPC verified all 4,997 seeded Rec EOS client files with zero damage. Steam
  finished its initial update and reached its own sign-in screen.

### Signed-in follow-up on 2026-11-11

- The user completed native Linux Steam sign-in. The installed launcher's
  native Steam-ticket subprocess returned a nonempty ticket and exited when
  stdin closed. The first request was empty during session warm-up; retrying
  after Steam connected passed. No account identifiers or tickets were logged.
- Native Steam installed stable x64 Proton 11.0 (app 4628710). Its manifest
  requires Steam Linux Runtime 4.0 (app 4183110), which was also installed.
  Actual launcher IPC resolved both tools and the isolated network prefix.
- Real Proton testing exposed detached input and redirected output in the
  original pipe-based Stella helper protocol. Private bounded files now carry
  the request and an atomically published result. The helper closes its owned
  game if publishing fails, and the launcher rejects symlink/oversized results.
- All four controlled process-loading cases passed inside Proton 11.0 and
  Runtime 4.0, including paths with spaces. Windows additionally passed three
  helper unit tests and six integration tests, including both console/file
  protocols and failure to publish. The final Windows suite passed 284 tests.
- The installed frontend downloaded and verified Rec EOS mod build
  `202611092155`. Its Doorstop/BepInEx loader started under Proton. The default
  renderer exited at D3D11 factory creation in this Mesa software environment.
  A test with `PROTON_USE_WINED3D=1` kept the actual game process alive and
  reached BepInEx chainloader completion with RecEOS loaded. This does not
  establish successful sign-in or playable room rendering.
- Rec EOS stayed running for three minutes in that renderer test. Stop closed
  the actual process. A separate normal launcher Quit/restart recovered the
  still-running game, and Stop left the native Steam process running.
- Stella's full client downloaded and all 9,367 files verified with zero damage.
  The current upstream patch differs from the automatic trust pin. After the
  user approved SHA-256 `1eb9dad182094cbde5a68956d03429a5273d2012aa33bddf4ab1e544f6613316`,
  the existing Update flow accepted that exact build without changing the pin.
  The real client loaded it through the Proton helper, stayed running for two
  minutes, and was closed by Stop. Its startup screen remained black here.
  Native Steam account lookup returned no linked Stella account; successful
  Stella sign-in and gameplay remain untested.
- Vanilla's signature-verified official MSI 1.0.17 completed inside its isolated
  prefix. The first handoff reported missing WebView2. With the user's license
  approval, Microsoft's Authenticode-verified x64 Evergreen installer supplied
  and registered WebView2 157.0.4341.46. Subsequent official-launcher handoffs
  exited before a usable interface appeared, with both default rendering and
  the WineD3D test setting. This does not validate Vanilla sign-in or anti-cheat.
- Radium's Steam prerequisite, Codename Gordon (app 92), installed after transient
  Steam connection timeouts. The launcher's configured client archive returned
  HTTP 404. Its original [download page](https://recroom.baby/downloads/) also
  reported downloads unavailable. No alternate client source was substituted.
- A real AppImage-to-Proton test exposed a second packaging issue: the multimedia
  hook sets `PYTHONHOME` and `PYTHONPATH`, causing Proton's interpreter to fail
  importing `encodings`. Clearing those values for external host tools restored
  path translation. A native Python import regression test covers this case.
  The final Linux Rust suite passed 271 regular tests; Windows passed 284.
- The rebuilt AppImage also loaded the real, accepted Stella patch through
  Proton, detected the actual game process, and stopped it successfully.
- A legacy batch fixture reproduced a Proton quoting failure with spaces.
  Passing `call` and the path as separate `cmd.exe` arguments starts the real
  fixture child. Paths containing cmd expansion/metacharacters remain rejected.
- Final production packages passed all 12 package/display/distro combinations
  again (15 checks each), plus six missing-tray-host regressions. The Proton
  fixture suite passed four patch-loading cases and the legacy batch case.
  The rebuilt Windows NSIS installer was signed and verified with the launcher's
  actual update-signature verifier.
- The latest Wayland matrix uses Weston nested on an isolated Xvfb server with
  virtual input devices. The application still uses native GTK Wayland for the
  `.deb` and Weston's XWayland for AppImage. This avoids the old headless
  compositor's missing-seat GTK warnings and intermittent screenshot hangs;
  all screenshot exports completed in the final matrix. It does not validate
  a physical display/input stack. See the upstream
  [Weston headless seat fix](https://cgit.freedesktop.org/wayland/weston/commit/?id=0126a5b4fc03c3ac7f023d87303bf09f7d494ac1).
- The interactive desktop's restart initially failed on stale X display files
  left after Docker stopped. The test entry point now removes only owned stale
  display-99 lock/socket files after checking for a live X server. Restart
  restored the desktop and preserved all client/prefix data; Steam required
  another user sign-in.

### Additional validation recorded on 2026-11-11

- Hardware OpenGL works on this Windows/WSL test host through Mesa 23.2.1's
  D3D12 backend on the NVIDIA RTX 5070, including inside Steam Linux Runtime 4.
  The complete WSL driver tree is required. The local test adapter exposes that
  tree read-only inside a private runtime copy, retaining the real bubblewrap
  executable and its isolation options. No WSL-specific adapter ships in either
  launcher package. This is not physical Ubuntu/Debian desktop validation.
- Xvfb is forced to use software rendering even in this optional GPU test mode.
  A first probe hung in WSL graphics teardown; restarting the test VM and keeping
  the display server on software rendering restored clean probe exit. Steam was
  shut down cleanly first, and its signed-in session survived the restart.
- Rec EOS launched on the hardware-backed WineD3D path and was detected, but
  Unity reported failure to initialize graphics. Dismissing the error was
  detected as game closure. This is not a room/gameplay pass.
- The real Stella client again loaded the accepted patch in both the `.deb` and
  AppImage launch paths. Actual game discovery and Stop passed. A separate
  helper test used a deliberately invalid local DLL with the real client: the
  helper reported loading failure, closed the game, and left Steam running.
  The accepted upstream patch and its automatic trust pins were not changed.
- A real unwritable-prefix launch test exposed an early success report before
  Proton initialized its prefix. Runtime status and launch now check the nearest
  existing directory and the existing `pfx` permissions before starting Proton.
  Two Linux Rust regressions cover writable/missing paths and unprivileged
  failures, including when the suite itself runs as root. The installed package
  returns the permissions error without spawning a game. A selected Proton
  alias containing spaces also launches and stops the real client; a full
  Steam-library move remains untested. A separate test moved the actual Rec EOS
  installation to a games library containing spaces. Launch, process discovery
  and Stop passed, then its original directory/configuration were restored and
  its prefix was retained.
- With these fixes, Windows passed 284 regular Rust tests and Linux passed 273.
  Both Linux package formats were rebuilt and passed all 12 desktop matrix
  cases, plus six missing-tray-host checks. One Ubuntu 22.04 AppImage/X11
  screenshot export timed out during concurrent release builds; that case passed
  on retry after the builds finished. The rebuilt signed Windows installer
  passed the actual updater signature verifier.
- The user supplied a Vanilla-specific Linux guide. Its Microsoft-hosted
  WebView2 installer matches Bottles' checksum and has a valid Microsoft
  Authenticode signature (SHA-256
  `91d82975e73cf4c0b1315679282c9f67e6cdbfeefc0ac6eaf2e68e5be71e982e`).
  WebView2 146.0.3856.62 installed in a separate test prefix. The launcher now
  supplies Vanilla's required native `winhttp` override. With Proton 11, the
  official launcher still exits before a usable interface, including with
  WebView2 software rendering; diagnostics show an access violation in Wine's
  `ole32`. The original prefix and WebView2 157 installation were retained.
- Valve Proton 10.0-4b and its required Steam Linux Runtime 3.0 were then
  installed through Steam. In another fresh prefix with WebView2 146, the
  official launcher stays running but shows a black window on this host; the
  software-WebView diagnostic also crashes. The installed native launcher's
  real IPC correctly selects Proton 10's matching runtime and starts the
  official launcher in that prefix. This validates the handoff and per-network
  selection, not a usable sign-in screen, authentication, game updates or
  anti-cheat. These WSL results do not establish behavior on every native Linux GPU.
- Radium's original downloads page now says Ten Whole Years is inactive. The
  configured client still returns 404; no replacement source was invented.
- The final cleanup audit added a prefix-retention boundary. A compatibility
  directory inside a client, or a `pfx` symlink into game data, is rejected before
  launch, update or uninstall. Filesystem access errors and dangling symlinks
  are distinguished from directories that have not been created yet, so an
  unreadable prefix cannot hide its location from cleanup checks. The installed
  package refused update/uninstall in a disposable client with a save inside
  game data, including an unreadable prefix alias and a missing Steam location;
  the save and client files were retained and the original settings restored.
  Windows passed 284 regular Rust tests and Linux passed 276 after these changes.
  Both final Linux packages passed the expanded 16-check desktop suite in all
  12 package/display/distribution combinations, plus six missing-tray-host
  checks. The prefix-retention IPC regression is part of that account-free CI
  suite. Valid moved-client launching, an explicit Proton path with spaces,
  AppImage-to-Proton patch loading/Stop, and the native Vanilla handoff through
  Proton 10/Steam Linux Runtime 3 also passed again with these packages.

### Further graphics and hosted checks on 2026-11-11

- The tested source was pushed to `abod/linux-support` at `b581cdf`, leaving
  `main` unchanged. Its
  [first hosted Desktop builds run](https://github.com/abod124-sudo/RecEOS-Launcher/actions/runs/43033335459)
  passed the Windows suite (284 regular tests), the helper's three unit and six
  native process tests, and the Windows installer build. Linux dependency
  installation was stopped after repeated DNS failures for the hosted image's
  Azure package mirror. CI now uses the official Ubuntu archive with apt
  signature verification, bounded retries and step timeouts. Follow-up Linux
  jobs are tracked in the
  [branch's workflow runs](https://github.com/abod124-sudo/RecEOS-Launcher/actions/workflows/desktop.yml?query=branch%3Aabod%2Flinux-support);
  the Windows results alone do not certify Linux packages.
- The WSL test desktop exposes software Vulkan 1.3 through llvmpipe. Rec EOS's
  Windows build does not include Unity's OpenGL renderer, so `-force-glcore`
  cannot work around missing graphics support. Proton 10's bundled DXVK 2.6.2
  creates its Direct3D device, but crashes on Xvfb's reported 0 Hz refresh rate.
  A nested Weston/XWayland display reports about 60 Hz and removes that crash.
- With Proton 10, its matching Steam Linux Runtime 3, and that display, the
  installed native launcher verified and started the actual Rec EOS client,
  which rendered its account-selection and sign-in screens. The saved login
  failed; the tester does not have a Rec EOS account/password. Steam sign-in
  does not supply a Rec EOS account. Room and friend gameplay remain untested.
  Stop closed the game and the temporary runtime selection was restored.
- The same normal-refresh display and Proton 10 also render Stella's actual
  welcome screen after the exact accepted patch loads. Account sign-in and room
  gameplay remain unverified. The hosted Windows helper artifact (SHA-256
  `5daa7067cfe4ee903f3307a87f628da3bd45ac19779e01c5e5e1d2f210963a60`)
  separately passed all five real Proton fixture cases: valid patch, invalid
  patch, an unrelated DLL with the same basename, game exit during loading,
  and a legacy batch launcher containing spaces. These tests do not use a game
  account or accept a different upstream Stella patch.
- A subsequent cached Windows CI run exposed a timing-dependent test fixture:
  its DLL delay could finish before another thread requested process exit.
  The private fixture now terminates its own process inside DLL initialization,
  and both protocol tests assert that initialization was entered. The loader's
  verification checks were not relaxed. All nine native helper tests, 20 repeated
  file-protocol runs, and the five real Proton fixture cases then passed.
- Vanilla was also tested with the guide's WebView2 146 on the corrected
  display. Proton 10 still crashed in Wine's COM handling. Valve Proton
  Experimental (`experimental-11.0-20261105c`) and Steam Linux Runtime 4 were
  then tested in a fresh prefix: the approved, Microsoft-signed WebView2
  installer completed, but the official launcher again crashed in `ole32`.
  No WebView sandbox, official authentication or anti-cheat checks were disabled.

The remaining gameplay gates require successful network authentication and
room gameplay, with usable rendering for each client. The software-rendered
container results above do not establish gameplay on a supported physical
Linux GPU desktop. Keep Linux gameplay and VR labeled pending/experimental
until those gates are observed passing.

### Reproducing the account-free desktop checks

`tests/linux/Dockerfile` provides the Ubuntu 22.04 build/test environment.
`tests/linux/runtime.Dockerfile` provides the runtime test environment and accepts
`--build-arg DISTRO=ubuntu:24.04` or `debian:12`. Build the Windows helper first,
prepare Linux resources, run the Rust suite, and build production packages as
above. Install `tauri-driver` 2.1.0 with `cargo install tauri-driver --version
2.1.0 --locked`; place it at `/tools/bin/tauri-driver` in the test container.

In an isolated container with this checkout at its working directory:

```bash
# Root: install the .deb and extract AppImage as the UID 1000 test user.
bash tests/linux/install-packages.sh /artifacts/launcher.deb /artifacts/launcher.AppImage
user=$(getent passwd 1000 | cut -d: -f1)
runuser -u "$user" -- env HOME=/tmp/receos-test-home \
  bash tests/linux/run-desktop-matrix.sh /tmp/receos-desktop-results
```

Each package/display combination writes `results.json`, native service/driver
logs and frontend screenshots. The suite also runs missing-tray-host close and
background-start regressions. See `.github/workflows/desktop.yml` for the full
dependency list and artifact transfer steps. Local validation artifacts and logs
are ignored under `src-tauri/target/`; they are not release publications.

### Local interactive Steam test desktop

`tests/linux/playtest.Dockerfile` adds an XFCE desktop, native Steam from
[Valve's signed apt repository](https://repo.steampowered.com/steam/), 32-bit
graphics libraries, Mesa software Vulkan and a password-protected noVNC view.
This allows real Steam/Proton testing on this Windows development machine.
Xvfb and software rendering do not replace physical desktop/GPU validation.

Put the built `.deb` in `src-tauri/target/linux-validation/account-free/release-ubuntu22/`,
or set `RECEOS_PACKAGE_DIR` to its directory inside the container. From the checkout
in PowerShell:

```powershell
docker build -t receos-linux-build:local -f tests/linux/Dockerfile .
docker build -t receos-linux-playtest:local -f tests/linux/playtest.Dockerfile .
$workspaceMount = 'type=bind,source=' + (Get-Location).Path + ',target=/workspace,readonly'
docker run -d --init --name receos-linux-playtest --hostname receos-linux-playtest `
  --security-opt seccomp=unconfined --shm-size 2g -p 127.0.0.1:6080:6080 `
  --mount $workspaceMount `
  --mount 'type=volume,source=receos-linux-playtest-home,target=/home/validation' `
  receos-linux-playtest:local
docker exec receos-linux-playtest cat /home/validation/.vnc/session-password.txt
```

Open `http://127.0.0.1:6080/vnc.html?autoconnect=1&resize=remote` and enter the
generated VNC password. Complete Steam sign-in inside the native client, then
install a stable Valve Proton and its matching Steam Linux Runtime through
Steam. The test container allows nested user namespaces for Steam's runtime;
it does not set the WebKit sandbox-disable variables used by CI.

Steam data, installed clients and Proton prefixes persist in the named home
volume. Closing the desktop or removing its container does not delete that
volume. Keep it when testing restarts or client uninstall behavior. The optional
read-only `/opt/receos-seed` mount can reuse this suite's pinned Rec EOS archive
extraction; copying it stages a new client directory without overwriting an
existing client. Account sign-in and game results must still be observed and
recorded before marking any gameplay gate passed.

On an NVIDIA Windows/WSL host, the optional local GPU test mode adds these
arguments to `docker run` (use a separate container name and run only one
desktop against the home volume at a time):

```powershell
--gpus all -e RECEOS_WSL_GPU=1 `
  --mount 'type=bind,source=/usr/lib/wsl,target=/usr/lib/wsl,readonly'
```

Install Proton 11 and Steam Linux Runtime 4 before enabling this diagnostic
mode. It uses `tests/linux/wsl-gpu-bwrap.py` and a private runtime cache under
`~/.cache/receos-gpu-runtime-probe/`; the launcher packages do not use this
adapter. The complete WSL driver tree supplies files missing from Windows'
`lxss/lib` directory. See Microsoft's
[WSLg container guidance](https://github.com/microsoft/wslg/blob/main/samples/container/Containers.md).
The virtual display server and Steam UI keep software rendering while game
clients can use Mesa's D3D12 driver. This host exposed OpenGL 4.2; successful
context creation still did not establish game compatibility.

The real helper fixture checks can be repeated after running the Windows
compat-helper integration tests (which build `game.exe`, `module.dll`, and
`exit.dll` under `src-tauri/compat-helper/target/patch-loading-fixtures`). Save
the installed launcher's `get_linux_runtime_status` result as JSON, then run:

```bash
python3 tests/linux/proton-helper.py --runtime-status runtime-status.json \
  --fixtures /path/to/patch-loading-fixtures \
  --helper '/usr/lib/Rec EOS Launcher/receos-proton-helper.exe' \
  --output /path/to/test-results
```

Run as the signed-in desktop user with that session's display and D-Bus
environment. The test creates a separate prefix, retains its results, and uses
pidfds to stop only its own mapped fixture images in that prefix. It neither
changes network prefix selections nor establishes real gameplay compatibility.
