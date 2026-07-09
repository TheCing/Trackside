# Trackside

In-game **QoL overlay** for **Umamusume Pretty Derby (Steam / Global)** - a single native
DLL that loads with the game and renders inside it (D3D11 + imgui). No external window, no
Python, no extra process. Open the game and press **Insert** for the menu.

**Trackside is a fork of [Heaven](https://github.com/Nighty3333/Heaven-Internal-Public-Version-)
by Night DC (nighty3333)** - fully open source under MIT, with every feature in the open.

> **Disclaimer.** Trackside is an unofficial third-party tool that runs inside the game. Like
> any mod that touches the game - Hachimi included - using it is against the game's Terms of
> Service and carries a small but real risk of an account ban. Use it at your own risk; you
> alone are responsible for how you use it.

---

## Install

1. Close the game.
2. Download the right zip from the [Releases](https://github.com/TheCing/Trackside/releases)
   page:
   - **`Trackside.zip`** — the standard build (what most people want).
   - **`Trackside+Hachimi.zip`** — only if you also run **Hachimi** (see below).
3. Extract it into the game folder, next to `UmamusumePrettyDerby.exe`
   (usually `.\steamapps\common\UmamusumePrettyDerby\`). `Trackside.zip` contains three
   files - **all three must be in the folder**:
   - `version.dll` — the Trackside loader proxy (source in [`proxy/`](proxy/)).
   - `trackside.dll` — the overlay itself.
   - `trackside_version.dll` — a copy of Windows' own `version.dll`, which the proxy
     forwards the version APIs to. Without it the game won't start.
4. Launch the game. Trackside loads itself - press **Insert** to show/hide the menu.
   Use **Windowed / Borderless** so the overlay is visible (not exclusive fullscreen).

> **Running Hachimi too?** Use **`Trackside+Hachimi.zip`** (its `trackside.dll` is the
> Hachimi-compatible build). It ships `version.dll` + `trackside.dll`; instead of a plain
> `trackside_version.dll`, put **Hachimi's own proxy DLL there** (rename Hachimi's
> `version.dll` to `trackside_version.dll`) and keep its `hachimi\` folder. Trackside's
> proxy then forwards into Hachimi, so both boot in the right order. The included
> `Toggle-TracksideStack.ps1` can enable/disable the whole stack at once.

> **Upgrading from Heaven?** Extract the zip over your install (it brings the three files
> above, including `trackside_version.dll`) and delete the old `heaven_overlay.dll`. Your
> settings, saved teams, pins and hunt targets are migrated automatically to the new
> `trackside-*` file names on first launch.

> **Antivirus note:** `version.dll` is a *proxy loader* (a normal technique for in-game
> overlays). Windows Defender or some antivirus may flag it as a **false positive**
> because it loads a DLL into the game. It is not malware - the loader's full source is
> in this repository ([`proxy/`](proxy/)); build it yourself if you prefer.

To uninstall: delete the files.

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

### Skill Optimizer  (end of career)
Maximizes your final **rating** when buying skills at the end of a career — a native port of
UmaLauncher's skill recommender, upgraded with live game data.

Open the end-of-career **Learn Skills** screen, press **Insert → Gameplay → Optimizer**, and a
dedicated window shows:

- **Current rating, live** — a big animated readout with your rank emblem and an
  **animated progress bar to the next rank**. It reacts in real time: tap **+ / −** on skills
  in the game and watch the rating, rank progress and SP bars glide to match.
- **The optimal buy set** — a knapsack optimizer picks the skill purchases that add the most
  rating for your SP, aware of **upgrade chains** (○→◎ costs), **hint discounts**,
  **Fast Learner** (auto-detected), and your **aptitudes** (a mile skill is worth less to a
  sprinter). The list mirrors the whole shop in the game's own order — bright rows are
  recommended buys, dim rows were considered and skipped.
- **Filters** — restrict to a distance, running style, or a **Champions Meeting preset**
  (only skills that can trigger under that race's conditions). Changing a filter recomputes
  instantly.
- **APPLY OPTIMAL** — one click selects every recommended skill on the game's own list
  (upgrade chains get their double-press automatically), then **you** press the game's
  **Decide** to confirm. Nothing is ever bought without your confirmation.

The optimizer reads the game's **actual offered list** live from the screen — inherited
skills from your parents, event-only skills and unhinted upgrades are all included, and the
skill costs match the shop to the point. After each career it checks its own math against
the game's official rating and reports any drift.

### Race Director (races)
A broadcast-style overlay for watching and casting races — a free camera plus a live, TV-grade
telemetry suite. Read-only: it never changes the race.

**Free camera** — a 3rd-person chase camera you move during a race (orbit, zoom, raise / lower,
and switch which Uma it follows), with up to 4 named angle presets **per circuit**. **Every
control is rebindable to any key** in *Race Director → Key bindings*, and the camera can be
turned on or off mid-race.

**Live telemetry HUD** — a broadcast layout that works **independently of the camera** (use
either one on its own):

- **Timing tower** — the whole field, leader-first: live order, time gaps, running-style
  colours, and a green / red flash on the position number when an Uma gains or loses a place.
  Click a row to follow that Uma.
- **Win probability** — a live win chance for every runner that swings with the race.
- **Race phase + final-furlong** markers, distance and progress.
- **Followed-Uma panel** — stamina, speed, active skills *with their effect* (e.g. `+0.35 m/s`),
  a whole-race **pace graph** (hover for max / average / minimum), and a side-by-side comparison
  with the rival ahead.
- **Trainer names** in lobby races (Team Trials, Champions Meeting, Room Match), with each
  trainer's Umas grouped by colour.

Toggle the whole HUD or any individual panel under *Race Director → Telemetry*. All windows are
**resizable** — drag a corner to scale; sizes and positions are remembered.

Full step-by-step guide (controls, presets, reading the HUD): **[race-director.md](race-director.md)**.

### Performance
- **Low Resources mode** — "potato" mode for very weak PCs: lowest 3D quality, no
  shadows / AA, low textures & LOD, and lighter cloth physics. One toggle.
- **Frame rate** — master **Cap FPS** toggle, a **1–300** slider, and **Unlimited**
  (renders as fast as possible, vSync forced off). Shows the **real measured FPS** (a true
  frames-per-second counter, not an estimate).
- **V-Sync** — cycle **Off / On / Half** under *Performance → Frame rate*. **On** removes
  screen tearing by syncing to your monitor's refresh (and overrides the FPS cap — the two
  are mutually exclusive by nature); **Half** targets half refresh for cooler/quieter play.
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
writes it to the Heaven dashboard's data folder, so it keeps working with the existing
analysis app: **https://github.com/Nighty3333/Heaven**

1. Enable **Team Trials** under `Capture` (it shows `N saved`).
2. Open your Team Trials results in-game — each one you view is saved automatically.
3. Browse/analyze them in the dashboard app.

This overlay only does the *capture*; the analysis lives in the dashboard.

### Team Trials — deck profiles & opponent finder
A dedicated **Team Trials** menu section with two tools for competitive play.

**Deck profiles** — save your 15-Uma team as a profile and swap your whole lineup with one click.

1. Open the Team Trials team-edit screen in-game.
2. In Trackside's **Team Trials** section, type a name and press **Save current** to store your
   team (keep up to 5 named profiles — e.g. a main team and a padding team).
3. To switch later, open the team-edit screen and press **Apply** on a profile. Trackside fills
   the grid for you; press the game's own **Confirm** to save.

A profile pins each exact Uma, so it keeps working even after your inventory order changes. If an Uma
in a profile no longer exists, **Apply** tells you instead of saving a broken team.

**Opponent finder** — auto-refresh the opponent list until a specific trainer appears.

1. Open the Team Trials **Select Opponent** screen.
2. Under **Opponent hunter**, enter the trainer's **name and/or ID** and press **Start hunt**.
3. Trackside refreshes for you at a relaxed, human pace. When the target appears it stops and
   alerts you — an on-screen banner, a **Windows notification**, and a flashing taskbar — so you
   can leave it running and come back. It stops on its own after a while if the target never shows.

The opponent pool is random, so a given trainer may take many refreshes — or not appear at all.

### Room Match finder
Auto-refresh the **Room Match** room list until a room matches your filters — track, distance,
surface, season, weather, minimum open slots, career-rank restriction and Uma bans — then alert
you or **auto-join**: Trackside opens the room, loads a **saved My Runners team** of your choice
and can **auto-confirm** the entry, so you beat other players into contested rooms. Found under
**Gameplay → Room finder**.

### Follower pruner
When you're near the **1000-follower cap**, prune the oldest-inactive followers (longest since
last login) down to a target you set. Always shows a **dry-run preview** of the exact list first —
nothing is removed until you press Start — and individual trainers can be **pinned** so they're
never touched. Removals are paced like a human tapping the button. Found under
**Gameplay → Followers**. (The upstream one-by-one *auto-unfollow* click tool is also included.)

### Affinity display
On the **Legacy Select** screen — where you pick your inherited parents at the start of a career —
Trackside shows the **exact succession affinity** the game itself uses: the pair total plus each
parent's value. The numbers appear as on-screen badges you can **drag anywhere and resize**; your
placement and size are remembered. Turn it on under **Interface → Affinity numbers**. It only shows
on the Legacy Select screen.

### Companion plugins
Built-in, native stand-ins for the popular companion tools — so you get their functionality
without loading any external DLLs. All toggles live under **Gameplay → Companion plugins**.

- **Export races (horseACT)** — save each race you run to a JSON file (under a `trackside-races`
  folder next to the game), grouped by race type, for web race viewers/analysis. Captures both
  the races you watch in 3D **and** the ones you simulate/skip.
- **Export veterans (Hakuraku)** — export your trained Umamusume — your "veterans" — to a local
  file that the [Hakuraku](https://hakuraku.moe/veterans) site reads. The next time your
  trained-uma roster loads in-game, Trackside writes `trackside_umas/veterans.json` next to the
  game; upload that file to Hakuraku.
- **Companion feed (CarrotBlender)** — serves the game's decrypted responses to companion
  overlays (such as UmaOverlay-lite) over a local connection, so those overlays work without a
  separate plugin.
  - **Works with UmaLauncher (Global)** too — no CarrotBlender.dll or Hachimi needed: in
    UmaLauncher's settings set **CarrotBlender Port** to **17229** (Trackside's feed port)
    and its training analytics, event helper and race logging run straight off Trackside.

These replicate the race/veterans dump previously provided by the **horseACT** plugin and the
response feed provided by **CarrotBlender**, natively in-process. Included in the original
project with the kind permission of **ayaliz** ([horseACT](https://github.com/ayaliz/horseACT))
and **qwcan** ([CarrotBlender](https://github.com/qwcan/CarrotBlender)) — thank you both.

---

## Game icons  *(optional but pretty)*

The race HUD and the Skill Optimizer show real in-game art — skill icons, character
portraits and rating-rank emblems — when a **`trackside-icons/`** folder sits next to the
DLL (included in release zips). Without it everything still works with clean text
fallbacks.

Rebuilding or extending the set (from source):

```
python fetch_icons.py          # skill icons + portraits (downloads from GameTora by id)
```

Rank emblems and any art not hosted publicly can be ripped from your own game: in-game,
press **Insert → About → Diagnostics → Dump loaded icon textures** while the art is on
screen (e.g. the career-complete screen for rank emblems). Raw textures land in
`trackside-icons/_dump/`; `curate_rank_icons.py` cuts the rank atlas into the final files.

---

## Custom intro  *(optional)*

Play your own video as the game's startup intro. It draws over the splash screens, plays
your audio track, and shows a **START GAME** button (bottom-right) to skip into the game.

Two files in the game folder drive it, read at runtime (no reinstall to change them):

| File | What it is |
|------|------------|
| `intro_full.bin` | the video (a stream of frames + a small header) |
| `intro_song.ogg` | the audio track |

Both go next to `trackside.dll`. If either is missing, that part is simply skipped.

**Build them from any video** with the included `pack_intro.py` (needs Python 3.8+ and
ffmpeg, on PATH or `pip install imageio-ffmpeg`):

```
python pack_intro.py my_video.mp4
```

Copy the two output files next to `trackside.dll` and launch. Resolution and fps are
configurable:

```
python pack_intro.py my_video.mp4 --res 1920x1080 --fps 30
```

Full guide: **[custom-intro.md](custom-intro.md)**. Delete the two files to restore the
normal startup.

> You supply your own video; nothing copyrighted is included with Trackside.

---

## The menu (press **Insert**)

A sidebar with sections: **Gameplay**, **Team Trials**, **Race Director**, **Visuals**,
**Performance**, **Interface**, **About**. Every setting is remembered across sessions. The
open/close key (default **Insert**) and the window layout are configurable in **Interface → Layout**.

**About** includes a **Reset game** button that reloads the game to the title screen without
closing it (with a two-click confirm to avoid accidental use).

Prefer something simpler? Toggle **Classic menu** in **Interface → Layout** to switch to the
original compact menu in-game — it carries the full feature set grouped into collapsible
categories, just a plainer style.

---

## Compatibility

Trackside runs alongside Hachimi — when both are installed, Trackside takes over the shared UI
tweaks (game speed, etc.) automatically, with no config changes needed. It is also compatible
with [SparkCollectPlugin](https://github.com/xialight/SparkCollectPlugin), so you can run both
at the same time.

---

## Updating

Trackside updates itself from within the game. On startup it checks for the latest release, and
if a newer version is out it shows a prompt with the full changelog of everything since your
version. Click **Download** and Trackside fetches the new build and restarts the game for you —
no manual file swapping.

Don't want a particular version? Tick **don't ask again** and it won't nag you for that one;
a newer release will still prompt. You can also check any time from the menu under **About**.

Need a different build? Under **About** you can list every available version and switch to any
of them — including rolling back to an earlier one — and Trackside downloads it and restarts.

Prefer to update by hand? The **Releases** page has every version as a zip:
**https://github.com/TheCing/Trackside/releases**

---

## Troubleshooting

### Where the logs are

Trackside writes everything to a **`trackside-logs`** folder next to the game — the same
folder as `UmamusumePrettyDerby.exe` and `trackside.dll`
(usually `.\steamapps\common\UmamusumePrettyDerby\trackside-logs\`).

| File | What it's for |
|------|---------------|
| `trackside-crash.log` | **The first thing to check after a crash.** On a crash it records the fault type, which module it happened in (Trackside vs the game vs another mod), and the last thing Trackside was doing. If it only contains "crash detector armed" lines with no `=== CRASH ===` block, the crash was outside Trackside (another mod, a driver, or the game itself). |
| `trackside-native.log` | The startup log — which features loaded and any that failed to resolve after a game update. |
| `trackside-diag.txt` | A full one-shot report (version, build, what's loaded, detected other mods, current settings). **Generate it from the menu: About → Diagnostics → Save diagnostic report**, then send that file. |
| `trackside-proxy.log` | The loader's log — useful if Trackside doesn't start at all (nothing else appears). |

**Reporting a crash?** Send `trackside-crash.log` and `trackside-diag.txt`. Together they
usually pinpoint the cause.

### Common problems

- **Game crashes on launch / boot-loops.** This is almost always a conflict with **another
  mod that also hooks the graphics layer** — most often **ReShade** (`dxgi.dll`) or another
  overlay. Temporarily remove/disable the other tool and launch again: if it starts, that
  tool and Trackside are colliding during graphics startup. Fixes: update the other tool to
  its latest version, update your GPU driver, or pick one. (For anti-aliasing you don't need
  ReShade — Trackside has built-in MSAA under **Performance → Graphics**.)
- **The overlay doesn't show up.** Use **Windowed** or **Borderless** mode, not exclusive
  fullscreen — the overlay can't draw over exclusive fullscreen. Also confirm the menu key
  (default **Insert**; check **Interface → Layout**).
- **A feature stopped working after a game update.** Nothing is hard-coded to game addresses,
  so an update never crashes the build — a renamed game method just makes that one feature
  show a status-line error instead. Grab a diagnostic report (above) and open an issue.
- **Windows Defender / antivirus flags `version.dll`.** Expected false positive for an
  in-game overlay loader — see the note in **Install**. The full source is in
  [`proxy/`](proxy/) if you'd rather build it yourself.

Still stuck? Open an issue with your `trackside-crash.log` + `trackside-diag.txt`:
**https://github.com/TheCing/Trackside/issues**

---

## Build from source

The full source lives in this repository: the overlay in [`native/`](native/), the loader
proxy in [`proxy/`](proxy/). Build with Rust (stable, MSVC toolchain) on Windows:

```
cd native
cargo build --release        # -> native/target/release/trackside.dll

cd ../proxy
cargo build --release        # -> proxy/target/release/version.dll
```

The custom-intro media (`intro_full.bin` / `intro_song.ogg`) is not part of the build —
supply your own (see the Custom intro section above).

**Skill data maintenance:** the optimizer's skill tables (`data/*.json`) are baked into the
DLL and regenerated straight from your local game database. After a game update adds skills:

```
python refresh_skill_data.py   # re-exports skill data/chains/roles/ranks from master.mdb
cd native && cargo build --release
```

**UI iteration without the game:** `Preview-Trackside.ps1` opens the overlay in a
standalone window (menu, themes, the optimizer with mock data via `TRACKSIDE_SKOPT_MOCK=1`).

---

## Credits & license

Trackside is maintained by **TheCing**.

It is a fork of **Heaven** by **Night DC (nighty3333)**, who built the original overlay and
the vast majority of what's documented above — full credit to him for that work.

Thanks also to **ayaliz** ([horseACT](https://github.com/ayaliz/horseACT)) and **qwcan**
([CarrotBlender](https://github.com/qwcan/CarrotBlender)) for kindly allowing their tools'
race, veterans and companion-feed functionality to be included natively.

Licensed under the **MIT License** — see [LICENSE](LICENSE). The full source is in this
repository: you're free to read, build, and modify it.
