# Heaven Internal — Public Version

In-game **QoL overlay** for **Umamusume Pretty Derby (Steam / Global)** — a single native
DLL that loads with the game and renders inside it (D3D11 + imgui). No external window, no
Python, no extra process. Open the game and press **Insert** for the menu.

**Made by Night DC : nighty3333**

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/nighty33) [![Discord](https://img.shields.io/badge/Discord-Join%20the%20server-5865F2?logo=discord&logoColor=white)](https://discord.com/invite/SpCGcXMeFt)

> **Disclaimer.** Heaven is an unofficial third-party tool that runs inside the game. Like
> any mod that touches the game — Hachimi included — using it is against the game's Terms of
> Service and carries a small but real risk of an account ban. Use it at your own risk; you
> alone are responsible for how you use it.

---

## Install

1. Close the game.
2. Copy these files into the game folder, next to `UmamusumePrettyDerby.exe`
   (usually `…\steamapps\common\UmamusumePrettyDerby\`):
   - `version.dll`
   - `heaven_overlay.dll`
3. Add **`heaven_version.dll`** — pick one of the two ways:
   - **Default (our way):** just copy the included `heaven_version.dll` into the
     same folder. Done — works out of the box.
   - **Make it yourself (optional):** copy your own
     `C:\Windows\System32\version.dll` into the game folder and **rename it to
     `heaven_version.dll`**.
4. Launch the game. Heaven loads itself — press **Insert** to show/hide the menu.
   Use **Windowed / Borderless** so the overlay is visible (not exclusive fullscreen).

> **Antivirus note:** `version.dll` is a *proxy loader* (a normal technique for in-game
> overlays). Windows Defender or some antivirus may flag it as a **false positive**
> because it loads a DLL into the game. It is not malware — if your AV quarantines it,
> allow-list the game folder. (This build deliberately does **not** use a commercial
> packer like Themida/VMProtect — those trip both antivirus and anti-cheat.)

To uninstall: delete the 3 files.

---

## Features

### Skip
- **SuperSkip** — *Events / Training / Races / Shop / Rival intro*, each toggleable. Calls the
  game's own skip routines and auto-advances the post-race result screens. Training skip also
  skips the friendship training cut-in (the "FRIENDSHIP TRAINING!" splash).
  - **Races only auto-advances when you WON** (finished 1st). If you lost — or the
    placement isn't known yet — it stops so you can handle it manually (e.g. a retry).
  - **Races never runs during Team Trials** — it's a career (story-mode) feature only.
  - **Shop** skips the item-shop animations: both when you **buy** an item and when you
    **use** one (the "Use …" effect card). Works from the shop and from the item list.
  - **Rival intro** skips the "RIVAL &lt;name&gt;" entry card shown before a rival race.
  - Defaults: Events **ON**, Training **ON**, Races **ON**, Shop **ON**, Rival intro **ON**.
- **Game speed** — speeds up the game's UI / story animations (menu opens, transitions,
  event text). Slider **1x–10x**.

### Free camera (races)
A free 3rd-person chase camera you can move around during a race. **Mouse** or **← →** to
orbit, **↑ ↓** to zoom, **I / K** to raise and lower, **`[` `]`** to switch which Uma it
follows. Save up to 4 named angle presets **per circuit** (**P** to save the current angle,
**O** to cycle them). View-only — it never changes the race itself.

### Performance
- **Low Resources mode** — "potato" mode for very weak PCs: lowest 3D quality, no
  shadows / AA, low textures & LOD, and lighter cloth physics. One toggle.
- **Frame rate** — master **Cap FPS** toggle, a **1–300** slider, and **Unlimited**
  (renders as fast as possible, vSync forced off). Shows the **real measured FPS** (a true
  frames-per-second counter, not an estimate).
- **Cloth physics** — uncap the character's hair / cloth physics so they stay smooth at
  high frame rates instead of the default frame-skipping.
- **Graphics** — force the **max 3D model quality** beyond the in-game cap, plus enhanced
  textures (anisotropic filtering), LOD and shadow detail.
- **Display & Window** — **always-on-top**, **block-minimize**, and **screen mode**
  (borderless / exclusive / windowed).

> **⚠ Frame rate — note:** this **unlocks / caps the frames the game already produces**
> (removes the 30/60 lock + vSync override) and measures them exactly. It does **not**
> synthesise extra "real" frames; true high-refresh rendering is a separate WIP.

### Team Trials capture  (`Capture` → ON)
Captures your **Team Trials** results as you view them — it reads each trial's result and
writes it to Heaven's data folder. This works together with the main **Heaven** app, which
reads and analyzes the captured data.

1. Enable **Team Trials** under `Capture` (it shows `N saved`).
2. Open your Team Trials results in-game — each one you view is saved automatically.
3. Browse/analyze them in the main Heaven app:
   **https://github.com/Nighty3333/Heaven**

This public build only does the *capture*; the analysis lives in Heaven.

### Race export
Save each race you run to a JSON file (under a `heaven-races` folder next to the game), grouped
by race type, for web race viewers/analysis. Enable **Export races** under **Gameplay → Race
export**.

### Veterans export (Hakuraku)
Export your trained Umamusume — your "veterans" — to a local file that the
[Hakuraku](https://hakuraku.moe/veterans) site reads. Enable **Export veterans (Hakuraku)**
under **Gameplay → Race export**; the next time your trained-uma roster loads in-game, Heaven
writes `heaven_umas/veterans.json` next to the game. Upload that file to Hakuraku.

This brings the veterans export — previously provided by the **horseACT** plugin — natively
into Heaven. Doing it inside Heaven avoids the compatibility issues that came from running both
mods at once, integrates the feature with the rest of Heaven, and lets you keep using Hakuraku
without needing a second tool. Included with the kind permission of **ayaliz**, the author of
horseACT — thank you: **https://github.com/ayaliz/horseACT**

---

## Custom intro  *(optional)*

Play your own video as the game's startup intro. It draws over the splash screens, plays
your audio track, and shows a **START GAME** button (bottom-right) to skip into the game.

Two files in the game folder drive it, read at runtime (no reinstall to change them):

| File | What it is |
|------|------------|
| `intro_full.bin` | the video (a stream of frames + a small header) |
| `intro_song.ogg` | the audio track |

Both go next to `heaven_overlay.dll`. If either is missing, that part is simply skipped.

**Build them from any video** with the included `pack_intro.py` (needs Python 3.8+ and
ffmpeg, on PATH or `pip install imageio-ffmpeg`):

```
python pack_intro.py my_video.mp4
```

Copy the two output files next to `heaven_overlay.dll` and launch. Resolution and fps are
configurable:

```
python pack_intro.py my_video.mp4 --res 1920x1080 --fps 30
```

Full guide: **[custom-intro.md](custom-intro.md)**. Delete the two files to restore the
normal startup.

> You supply your own video; nothing copyrighted is included with Heaven.

---

## The menu (press **Insert**)

A sidebar with sections: **Gameplay**, **Camera**, **Visuals**, **Performance**,
**Interface**, **About**. Every setting is remembered across sessions. The open/close key
(default **Insert**) and the window layout are configurable in **Interface → Layout**.

Prefer something simpler? Toggle **Classic menu** in **Interface → Layout** to switch to the
original compact menu in-game — it carries the full feature set grouped into collapsible
categories, just a plainer style.

---

## Compatibility

Heaven runs alongside Hachimi — when both are installed, Heaven takes over the shared UI
tweaks (game speed, etc.) automatically, with no config changes needed. It is also compatible
with [SparkCollectPlugin](https://github.com/xialight/SparkCollectPlugin), so you can run both
at the same time.

---

## Updating

Heaven **checks for updates but never installs them.** On startup it looks once at the
latest release; if a newer version is out it just tells you in the menu under **Updates**
(e.g. *"Update vX.Y.Z available"*). It never downloads or changes anything on its own.

To update, do it manually:

1. Open the **Releases** page:
   **https://github.com/Nighty3333/Heaven-Internal-Public-Version-/releases**
2. Download the newest zip (`Heaven.zip`, or `Heaven+Hachimi.zip` if you run Hachimi).
3. Close the game, replace the DLLs with the new ones, relaunch.

---

## Build from source

The full source for the overlay DLL lives in [`native/`](native/). Build it with Rust
(stable, MSVC toolchain) on Windows:

```
cd native
cargo build --release
```

The DLL is produced at `native/target/release/heaven_overlay.dll`. The custom-intro media
(`intro_full.bin` / `intro_song.ogg`) is not part of the build — supply your own (see the
Custom intro section above).

---

## Credits & support

Made by **Night DC : nighty3333**.

Thanks to **ayaliz** ([horseACT](https://github.com/ayaliz/horseACT)) for kindly allowing the
veterans export to be included in Heaven, so users can keep using Hakuraku without conflicts.

If Heaven saves you time, a coffee is appreciated:
[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/nighty33) [![Discord](https://img.shields.io/badge/Discord-Join%20the%20server-5865F2?logo=discord&logoColor=white)](https://discord.com/invite/SpCGcXMeFt)

Licensed under the **MIT License** — see [LICENSE](LICENSE). The full source is in this
repository: you're free to read, build, and modify it.
