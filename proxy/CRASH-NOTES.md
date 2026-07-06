# Trackside proxy — crash investigation notes

> Working log for the "game crashes / overlay doesn't show" investigation.
> **Read this first before re-deriving anything.** Append findings; don't delete
> them. Facts here are things we've *confirmed by test or disassembly*, not
> guesses. Guesses go under "Open hypotheses".

## The symptom

- Game faults on launch in `GameAssembly.dll` at RVA `0x3bdae3` (0x80000003)
  or `0x3d3279` (0xc0000005). Both addresses sit in a **zero-filled region of
  the file** = IL2CPP runtime data (pointers filled at runtime), not real code.
- When it crashes, `trackside-native.log` stops at `step2: exports resolved`.
  The overlay's boot thread always reaches step2, then the game faults.

## HACHIMI RULED OUT + NEW LEADING HYPOTHESIS (2026-07-05 19:50): load too EARLY

Test result: our proxy with forwarders → heaven_version.dll (Hachimi restored),
synchronous DllMain overlay load = STILL crashes, same offset. So Hachimi is NOT
the fix. Confirmed by the upstream README too: `heaven_version.dll` is just "your
own C:\Windows\System32\version.dll renamed" — Hachimi is explicitly optional.
So the ONLY real variable left between config 3 (works) and ours (crash) is the
PROXY BINARY, specifically WHEN it loads the overlay.

KEY EVIDENCE (native/src/boot.rs, line ~79-83 comment, verbatim):
  "With the proxy loader we reach this point during early init; attaching into a
   freshly-created domain races the GC. A short settle window makes the proxy
   path behave like the (working) late-injection path."
So the overlay has a KNOWN-GOOD "late-injection path" (originally injected AFTER
the game was up). Our proxy loads the overlay EARLY:
  - synchronous in DllMain = during UnityPlayer's dependency load (earliest)
  - worker thread = right after loader lock releases (still very early)
Both install hudhook's D3D vtable hook before the game's device/anti-tamper
settle → int3 (0x80000003) in GameAssembly. The overlay's OWN boot thread waits
for IL2CPP, but hudhook's hook install happens at DLL-load time regardless — so
WHEN we load the overlay DLL is what matters.

NEXT TEST: worker thread that WAITS for GameAssembly.dll to be present, then a
settle delay, THEN loads the overlay — replicating late injection. Forwarders
back to trackside_version (Hachimi not needed). If WORKS → timing was it, tune
the wait. If CRASH → load moment isn't it either; escalate.

## (obsolete) earlier hypothesis: Hachimi is required (anti-tamper shim)

Synchronous DllMain load ALSO crashed (step2 → 0x3bdae3, int3). So it's neither
the version-API path NOR load timing. Re-examining ALL configs, the only one that
EVER worked (config 3) is also the only one with **Hachimi (`heaven_version.dll`)
present and loaded early**:

| # | proxy            | Hachimi | overlay load        | result |
|---|------------------|---------|---------------------|--------|
| 1 | ours (old,buggy) | eager   | DllMain (+eager Hac)| crash  |
| 2 | ours (old)       | none    | DllMain             | crash  |
| 3 | **Night's**      | **yes** | DllMain (Night)     | **WORKS** |
| 4 | ours (fwd)       | none    | worker thread       | crash  |
| 5 | ours (fwd)       | none    | DllMain sync        | crash  |

The fault is `int3` (0x80000003) at a FIXED GameAssembly.dll offset while our
overlay is merely WAITING (step2, before it installs ANY IL2CPP hook). That's the
game's OWN code trapping — signature of an integrity/anti-tamper check. Umamusume
Global (Steam) has anti-cheat; Hachimi is the standard mod that coexists with it.
Night statically forwards version APIs → `heaven_version.dll` (Hachimi), so
Hachimi loads super-early and (hypothesis) neutralizes the check before it fires.

NEXT TEST: our proxy, forwarders → `heaven_version.dll`, Hachimi restored, keep
synchronous overlay load. If WORKS → Hachimi (or an early-loaded shim) is required;
revisit the "Hachimi is redundant" decision with the user. If CRASH → it's
something specific to Night's proxy binary; dig there.

CONFOUND STILL OPEN: config 3 changed BOTH proxy AND Hachimi-presence vs our
configs. This test fixes proxy=ours and toggles Hachimi=yes to break the confound.

## BUILD VARIANTS (clarified 2026-07-05 20:10, user + code)

Upstream ships `Heaven.zip` and `Heaven+Hachimi.zip`. Checked the code: the
`hachimi` cargo feature ONLY adds (a) the `arbiter` module = bookkeeping/reporting
of which hooks Heaven owns vs cedes, and (b) self-update asset name `_hh`. The
actual "yield if already detoured" coexistence logic is in BOTH builds
(il2cpp.rs ~347). So the two OVERLAY binaries are ~functionally identical here.
The real with/without-Hachimi difference is the LOADER CHAIN, not the overlay:
`version.dll` (same in both) always forwards to `heaven_version.dll`, which is
either a renamed System32 version.dll (standalone) or Hachimi's proxy (+hachimi/).

DESIGN LOCKED: our proxy forwards to `trackside_version.dll` (one branded name).
Variant = what that file is: renamed System32 version.dll (standalone) OR Hachimi
renamed (with-Hachimi). One proxy binary, both variants. User wants BOTH shipped.
User is UNSURE if a no-Hachimi chain ever worked on this game build — so a working
baseline must be established empirically; with-Hachimi is the higher-prior bet.

TEST NOW: our late-injection proxy + trackside_version.dll = Hachimi + hachimi/
restored + our overlay. Diffs from known-good config 3: proxy binary (Night→ours)
and load timing (Night sync→ours late-inject). If WORKS → with-Hachimi variant
done; then standalone = swap trackside_version.dll to a System32 copy + drop
hachimi/. If CRASH → escalate to a byte-level proxy diff.

## ★ BREAKTHROUGH — LATE INJECTION IS THE FIX (2026-07-05 20:16) ★

Our proxy + Hachimi + our overlay, with the proxy loading the overlay from a
worker thread that WAITS for GameAssembly.dll + a 3s settle window = **WORKS**.
Game reaches lobby, Insert opens the overlay. So the root cause was: loading the
overlay too EARLY (in DllMain / right after loader lock) installs hudhook's D3D
vtable hook before the game's runtime/anti-tamper settles → int3 in
GameAssembly.dll. Deferring the LoadLibrary until the runtime is up fixes it.
This is now the proxy's committed design (proxy/src/lib.rs loader_thread).

REMAINING ISSUES:
1. Standalone (no-Hachimi) variant NOT yet verified — TEST NEXT (swap
   heaven_version.dll for a System32 copy, drop hachimi/, forward to
   trackside_version). If it works too, Hachimi is optional and issue #2 only
   affects users who deliberately run Hachimi.
2. With Hachimi present, Hachimi's own ImGui (its update popup) can't be clicked
   — input contention: two overlays both hook the window proc; whichever is
   outermost eats mouse input. Only matters for the with-Hachimi variant.

## GROUNDED (docs, not memory)

- **Thread created in DllMain does NOT run until DllMain returns / loader lock is
  released.** When a thread starts, the OS must deliver `DLL_THREAD_ATTACH` to all
  DLLs, which needs the loader lock — held by the thread inside our DllMain. So
  the new thread blocks until we return. This is exactly what our worker-thread
  loader relies on. Source: Raymond Chen, "Does creating a thread from DllMain
  deadlock or doesn't it?" (devblogs.microsoft.com/oldnewthing/20070904-00) +
  MS Learn "Dynamic-Link Library Best Practices".
- **The deadlock everyone warns about only happens if DllMain WAITS on the new
  thread** (event/join). Ours does not: it `CreateThread` → `CloseHandle` →
  return. No synchronization back to the loader-lock thread. So it's the safe form
  of an otherwise-discouraged pattern.
- **`LoadLibrary` inside DllMain "can cause a deadlock or crash"** (MS Learn,
  DllMain entry point page). Night's proxy does it anyway and gets away with it;
  our worker-thread deferral is the more conservative, better-documented path.
- **.def `name = other.Name` emits a true forwarder** — verified EMPIRICALLY on
  our built DLL (pefile shows `-> trackside_version.*`), not just from docs.
- **System32 version.dll is mostly a real DLL here** (15 local exports, 2 apiset
  forwarders) — verified empirically by dumping its export table.

## CORRECTION (2026-07-05 19:37) — fact #1 below was WRONG

The static-forwarder proxy STILL crashed at the same offsets (0x3bdae3/0x3d3279,
step2 → fault). That proves the version-API path was NOT the cause — good, ruled
out. BUT it also exposed that old "fact #1" (timing doesn't matter) was based on
CONFOUNDED tests: every "DllMain load" variant I'd tried *also* eager-loaded
Hachimi under the lock, so a CLEAN synchronous DllMain load was never actually
tested. The known-GOOD config (Night's proxy) loads the overlay SYNCHRONOUSLY in
DllMain (confirmed by disassembly: attach handler → LoadLibraryW(overlay)). My
worker-thread version defers the load until AFTER the loader lock releases — i.e.
after the game's IL2CPP/anti-tamper init has begun. That timing gap is the real
untested variable. NOW TESTING: synchronous load_once() in DllMain, matching
Night. Everything else (static forwarders, no Hachimi) unchanged.

## ~~CONFIRMED FACTS~~ (fact #1 retracted — see correction above)

1. ~~**Load *timing* is NOT the cause.**~~ RETRACTED. Was based on confounded
   tests (Hachimi eager-load present in every "DllMain" variant). Night loads
   synchronously in DllMain and works; our deferred worker-thread load crashes.
   Timing is now the LEADING suspect.
2. **Nothing imports `version.dll!UnityMain`.** `UnityPlayer.dll` imports only
   three functions from version.dll: `VerQueryValueA`,
   `GetFileVersionInfoSizeA`, `GetFileVersionInfoA`. That's what pulls our proxy
   in. UnityPlayer exports its *own* UnityMain; the EXE binds to that. So a load
   hung off `version.dll!UnityMain` never fires. (The EXE's import table isn't
   parseable by pefile — packed/encrypted — but UnityPlayer's is.)
3. **Night's proxy loads the overlay synchronously in DllMain, under the loader
   lock, and it works.** Disassembly: CRT DllMain dispatch (0x1e098) calls the
   user loader fn (0x1010) on reason==DLL_PROCESS_ATTACH, which does
   `LoadLibraryW(overlay)` at 0x132e. So "loading under the loader lock faults
   GameAssembly" is FALSE as a general claim.
4. **Night's proxy uses STATIC export forwarders** for all the version APIs
   (`GetFileVersionInfoA -> heaven_version.GetFileVersionInfoA`, etc.). Our code
   never runs for those calls — the Windows loader resolves them. Our proxy
   instead forwards at *runtime* via a `forward!` macro that can trigger a
   `LoadLibrary` on first call. This is the main behavioral difference from the
   known-good proxy and is the current prime suspect.
5. **Known-GOOD config (tested, launched + worked):** Night's `version.dll`
   (176,640 bytes) + our overlay renamed `heaven_overlay.dll`. Overlay code is
   fine with Night's proxy. (paths.rs recognizes both `trackside.dll` and
   `heaven_overlay.dll`.)
6. **Known-BAD configs (tested, crashed):** our `version_proxy` build in every
   variant tried so far (eager heaven_version chain in DllMain; UnityMain-
   deferred; worker-thread). All crash at the same GameAssembly offsets.

## File-size fingerprints (to tell builds apart fast)

- Night `version.dll`: **176,640** bytes
- Our `version.dll` (version_proxy): **129,536** bytes
- Night `heaven_version.dll` (Hachimi's version proxy): 55,192 bytes
- Overlay (`trackside.dll` == our build): 9,393,152 bytes

## Backups on disk

`G:\SteamLibrary\steamapps\common\UmamusumePrettyDerby\_heaven_backup\`
contains Night's original `version.dll`, `heaven_version.dll`,
`heaven_overlay.dll`, and the `hachimi/` dir. Use these to restore a
known-good chain.

## RESOLVED: the proxy is the culprit (confirmed 2026-07-05 19:26)

Ran the decisive test — Night `version.dll` + our overlay as
`heaven_overlay.dll` — and it booted CLEANLY through `step5` / `ready`, all
modules armed, reached Home, no fault. Since the overlay binary is identical to
the one that crashes under our `version_proxy`, the overlay is proven good and
the crash is **entirely the proxy**. H-A confirmed: the difference that matters
is Night's **static export forwarders** vs our **runtime `forward!` macro** on
the version APIs (called by UnityPlayer very early). 

→ FIX: give our proxy static export forwarders so our code never runs on the
version-API path (mirror Night). Do NOT keep tuning overlay load timing (fact #1).

## Superseded hypotheses

- H-B (lazy `real_dll()` double-mapping): subsumed — static forwarders remove
  the whole runtime path.

## FIX IMPLEMENTED (2026-07-05 ~20:05) — awaiting confirm launch

Rewrote the proxy so our code never runs on the version-API path:

- `proxy/version.def` — 16 version APIs as STATIC forwarders to
  `trackside_version.dll` (a copy of the genuine `C:\Windows\System32\version.dll`,
  deployed beside us). Verified in the built DLL: all 16 show
  `-> trackside_version.*` forwarders.
- `UnityMain` / `UnityMain2` — could NOT be `.def` forwarders (link.exe rejects
  forwarding to `UnityPlayer.*` here: LNK2001). Kept as tiny runtime-forwarders in
  `lib.rs` that resolve UnityPlayer.dll and jump. Safe: not on the early version
  path, nothing normally imports them from version.dll. Listed as plain (local)
  exports in the .def because a .def EXPORTS section is authoritative — Rust's
  auto-exports are dropped otherwise (learned this: first build exported 0 of our
  own fns until we named them in the .def).
- `GetFileVersionInfoByHandle` intentionally OMITTED — UnityPlayer doesn't import
  it and link.exe won't accept it as a forwarder here. Matches Night's set.
- `proxy/build.rs` — `cargo:rustc-cdylib-link-arg=/DEF:version.def`.
- `lib.rs` — stripped the `forward!` macro / `real_dll` / `real_fn` / `REAL`
  machinery entirely. DllMain now only captures OWN_DIR + spawns the worker thread
  that does staged-update + plugin early-load + `LoadLibrary(trackside.dll)`.
- Dropped the `heaven_version.dll` (Hachimi) chain — pure Trackside per decision.

Deploy layout in game folder:
- `version.dll` = our proxy (124,416 bytes)
- `trackside_version.dll` = genuine version.dll (forwarder target)
- `trackside.dll` = overlay
- `heaven_version.dll` / `heaven_overlay.dll` = REMOVED
- `deploy-on-exit.ps1` updated to seed `trackside_version.dll` from System32.

If this launch crashes at the same GameAssembly offsets: the version path was not
the (whole) cause — revisit. If it works: DONE, this is the branded proxy.

## Decisions locked in

- Hachimi is redundant for this user (Global/Steam client); not required for the
  overlay to function. OK to drop the `heaven_version` chain in the branded proxy.

## CONFIRMED: How Hachimi actually loads (2026-07-05)

Hachimi does NOT load via `version.dll`, the `hachimi/` folder, `dxgi.dll`, or
`trackside.dll`. It ships **disguised as `cri_mana_vpx.dll` in the game ROOT**:

- Root `cri_mana_vpx.dll` (~17.6 MB) version info = ProductName "Hachimi",
  FileDescription "Game enhancement and translation mod", FileVersion 0.24.0.
  Contains strings `hachimi`, `umatl`, `First Time Setup` (the popup).
- The GENUINE CRI codec (~2.2 MB) lives at
  `UmamusumePrettyDerby_Data\Plugins\x86_64\cri_mana_vpx.dll` and is untouched.
- Neither GameAssembly.dll nor UnityPlayer.dll import cri_mana_vpx statically;
  the root copy shadows the real one at load time — that's Hachimi's injection.

To run standalone (no Hachimi): move/rename the ROOT `cri_mana_vpx.dll` aside.
Unity falls back to the real codec in `_Data\Plugins\x86_64\`. Renaming the
`hachimi/` folder or swapping `version.dll` does nothing to disable Hachimi.

## Reverse-engineering scripts (in this folder)

- `_exp.py` — dump/compare exports of deployed vs Night proxy.
- `_re.py` / `_re2.py` / `_re3.py` / `_re4.py` — disassemble Night proxy
  (exports, DllMain dispatch, loader fn, string/IAT annotation).
- `_who.py` — who imports version.dll / UnityMain (exe + UnityPlayer).
- `_ga.py` — disassemble the GameAssembly.dll fault sites.
- `_str.py` / `_imports.py` — string + import dumps.
