# Rec EOS Launcher

_Formerly Radium Launcher._

An **unofficial** launcher for Rec Room revival servers.

| Server | Site |
| :--- | :--- |
| **Radium** | [radie.app](https://www.radie.app/) |
| **Vanilla** | [vanillarec.net](https://vanillarec.net/) |

## Showcase

### Steam 2003 Green Theme
| Home Tab | Rooms Tab | People Tab |
| :---: | :---: | :---: |
| ![Home Tab (Steam Green)](https://i.imgur.com/6rAgFPd.png) | ![Rooms Tab (Steam Green)](https://i.imgur.com/UX5nNZT.png) | ![People Tab (Steam Green)](https://i.imgur.com/4wjGRxC.png) |

### Windows 7 Aero Theme
| Home Tab | Rooms Tab | People Tab |
| :---: | :---: | :---: |
| ![Home Tab (Windows 7)](https://i.imgur.com/rgGrBw4.png) | ![Rooms Tab (Windows 7)](https://i.imgur.com/scr44Ma.png) | ![People Tab (Windows 7)](https://i.imgur.com/eae8aEU.png) |

---

## Features

- **Switch servers** — Swap between Radium and Vanilla from the sidebar. Each keeps
  its own client install, so both can be installed at once.
- **Browse** — Rooms with search, tag filters and sorting; player profiles with photos
  and rooms; and a live feed of what players are doing right now.
- **Client management** — Downloads and extracts the game client in-app with live speed
  and ETA, resumes interrupted downloads, detects outdated installs, and launches the
  game.
- **15 skins** — Steam 2003 Green, Windows 95 / 98 / XP / Vista / 7, Mac OS Classic and
  Aqua, Modern Dark and more — plus a **Liquid Glass** effect with your own tint or
  background picture.
- **Diagnostics** — A searchable, filterable log that is kept on disk for each session,
  a Report a Problem dialog that shows exactly what it sends (and can attach the session
  that crashed), server status on startup, and third-party antivirus detection.
- **Stays current** — Checks GitHub for launcher updates on startup and while it sits in
  the tray, and saves every setting the moment you change it.

---

## Download

**Update 4.0.0 is coming soon.(Probably on Vanilla or Radium release)** The link below goes live the moment it's published —
until then, grab the current build from the [Releases page](https://github.com/abod124-sudo/Radium-Launcher/releases/latest).

| File | Description |
|------|-------------|
| [Radium.Launcher_4.0.0_x64-setup.exe](https://github.com/abod124-sudo/Radium-Launcher/releases/download/v4.0.0/Radium.Launcher_4.0.0_x64-setup.exe) | NSIS installer for Windows x64 |



## File Locations

```
%APPDATA%\com.radium.launcher\
  ├── config.json         - saved settings
  ├── client\             - Radium game files
  └── client-vanilla\     - Vanilla game files
```

Uninstalling one server never touches the other.

---

## Building from source

Requires the [Rust toolchain](https://rustup.rs/) and Node.js.

```bash
npm install
npm run dev     # run the launcher
npm run build   # produce the installer
cd src-tauri && cargo test
```

On an ARM64 PC (a Surface Pro, say), use `npm run dev:x64` and `npm run build:x64`
instead. The Steamworks SDK ships its Windows library for x64 only, so an ARM64
build fails to link; the x64 build runs under Windows' emulation. The target
needs adding once: `rustup target add x86_64-pc-windows-msvc`.

---

## Note

Parts of this launcher's codebase were written/co-authored with the help of AI coding assistants.
