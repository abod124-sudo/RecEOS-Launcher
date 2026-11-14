# Linux

The Linux launcher runs on **Ubuntu 22.04+ and Debian 12+ (x64)** as a `.deb`
or AppImage. Games run through Valve's Proton. **Linux support is new:**
gameplay on real Linux PCs has not been confirmed for every revival yet (see
[Status](#status)), and VR is not supported.

## Install

1. Install native Linux Steam (not Flatpak), open it and sign in.
2. In Steam, go to **Library → Tools** and install a stable **Proton** (for
   example Proton 11.0). Steam installs the Steam Linux Runtime it needs.
3. Install your graphics driver with **Vulkan 1.3** support (see
   [DXVK's driver list](https://github.com/doitsujin/dxvk/wiki/Driver-support)).
4. Install the launcher:
   - `.deb`: `sudo apt install ./Rec*.deb`
   - AppImage: `chmod +x Rec*.AppImage` and run it. It needs WebKitGTK 4.1,
     `xdg-utils`, `libnotify-bin`, and on Wayland `xwayland`. Some systems
     also need `libfuse2`.

The tray icon needs a desktop with tray support (GNOME needs an AppIndicator
extension). Without one, closing the window quits the launcher.

Linux tray icons always open a menu when clicked, so the panel that Windows
shows on a right-click (PLAY, Screen/VR, friends, pages, revival switch) is
the menu's first item, **Rec EOS Launcher…**. It opens at the pointer, next
to the icon (top-right of the screen on Wayland), and closes when you click
elsewhere.

## Settings › Proton

Everything is automatic by default. The page shows, per revival, whether it
is ready to play and why not.

| Setting | What it does |
| --- | --- |
| Steam folder | Found automatically (`~/.local/share/Steam`, `~/.steam/steam`). Change it only for an unusual install. |
| Proton version | Automatic picks the newest stable Proton. Any installed version (including Experimental or GE) can be chosen, or another folder. |
| OpenGL mode | Runs the game's graphics through OpenGL (WineD3D) instead of Vulkan (DXVK). For PCs without a working Vulkan driver. Slower. |
| Windows files (Proton prefix) | Each revival keeps its own, by default in `~/.local/share/com.receos.launcher/proton/<revival>`. It is kept when you uninstall a game and must not be inside a game folder or Steam's `compatdata`. |

## Revivals

- **Rec EOS:** download, verification and the mod work as on Windows.
- **Radium:** runs the client (or its legacy `.bat`) through Proton and checks
  Steam's Linux library for the required app.
- **Stella:** signs in with native Steam. A small Windows helper inside the
  Proton prefix loads the same verified `Stella2024.dll` patch as on Windows;
  the game is closed if the patch doesn't load.
- **Vanilla:** GET VANILLA installs Vanilla's official launcher under Proton,
  and PLAY opens it. Sign in, update and play from there. Vanilla's launcher
  may also need a Wine-compatible WebView2 runtime in its prefix; the
  [community guide](https://drive.proton.me/urls/E9F4QR503C#7yZkVX6souQS)
  covers it.

Antivirus and Smart App Control checks don't exist on Linux and are hidden.

## Troubleshooting

**The game closes a few seconds after pressing PLAY.** The launcher now says
why when it can. The usual cause is no Vulkan 1.3 driver: the game's
`Player.log` then shows `d3d11: failed to create factory`. Install your GPU's
Vulkan driver, or turn on **OpenGL mode** in Settings › Proton.

**"Not ready" in Settings › Proton.** The message names what is missing
(Steam, a Proton version, or the Steam Linux Runtime). Install it in Steam and
press **Check again**.

## Files

| Content | Location |
| --- | --- |
| Config and game clients | `~/.local/share/com.receos.launcher/` |
| Proton prefixes | `~/.local/share/com.receos.launcher/proton/<revival>/` |
| Thumbnail cache | `~/.cache/com.receos.launcher/thumbs/` |
| Start at login entry | `~/.config/autostart/com.receos.launcher.desktop` |

XDG variables (`XDG_DATA_HOME` and so on) are respected. Launcher updates on
Linux open the GitHub release page; install the new package by hand.

## Building

Linux packages are built on Linux (or by the **Desktop builds** GitHub
workflow, which also builds the Windows installer, unsigned). The Stella
helper is a Windows program and is built on Windows first:

```powershell
npm ci
npm run prepare:proton-helper
```

Copy `src-tauri/resources/receos-proton-helper.exe` to the same path in the
Linux checkout, then on Ubuntu/Debian with the
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/), Node.js 24
and Rust:

```bash
npm ci
node scripts/prepare-linux.mjs
(cd src-tauri && cargo test --locked --lib)
npm run build:linux
```

`tests/linux/` has the Docker images used for testing: `Dockerfile` (build),
`runtime.Dockerfile` (Ubuntu 24.04 / Debian 12 checks), and
`playtest.Dockerfile`, a desktop with Steam and a noVNC view at
`http://127.0.0.1:6080` for trying real games without a Linux PC. The
playtest container has no GPU, so games need OpenGL mode there.

## Status

Confirmed:

- Packages install and start on Ubuntu 22.04, Ubuntu 24.04 and Debian 12
  (X11 and Wayland), with tray, notifications, themes and start at login.
- The tray panel opens from the tray menu on XFCE (X11), follows the skin,
  switches revival, and closes on a click elsewhere.
- Steam and Proton detection, separate prefixes, and the folder safety checks.
- Stella's patch loads under Proton and a bad patch closes the game.
- Rec EOS launches from the launcher under Proton 11.0, reaches its welcome
  screen (OpenGL mode, no GPU), is detected while running, and Stop closes it.

Not yet confirmed (needs a Linux PC with a GPU):

- [ ] Rec EOS: sign in and join a room.
- [ ] Radium: install and play.
- [ ] Stella: sign in and join a room.
- [ ] Vanilla: official launcher installs, signs in and starts the game.
- [ ] Vulkan (DXVK) graphics on real GPUs.

Keep the "new / not confirmed" wording in the app and release notes until
these pass.
