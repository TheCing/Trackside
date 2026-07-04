# Game methods reference — IL2CPP surface Heaven touches

Every game class/method Heaven resolves at runtime, how each one is invoked, and what it
does. Names were confirmed live on the **Global (Steam) build, 2026-07** unless marked
*(candidate)*. When a game update renames something, the affected feature degrades to a
status-line error naming the step that failed — nothing resolves at compile time, so a
rename never breaks the build, only that feature.

Related module docs live at the top of each source file; this is the cross-cutting index.

---

## 1. Ground rules

- **Main thread only.** Anything that calls managed code (getters, `Send`, view-controller
  methods) must run on the game's main thread. Heaven gets there two ways:
  1. inside one of our detours (the game called us on its thread), or
  2. from the per-frame pump tick — `DG.Tweening.Core.TweenManager.Update` is detoured in
     `hunter.rs` and calls `pump()`/`poll()` for every module that queues work
     (`padder`, `reset`, `affinity`, `pruner`, `roomfinder`). UI panels never touch
     IL2CPP; they flip atomic request flags the pump consumes.
- **Calling convention.** IL2CPP compiles methods as plain x64 functions:
  `fn(this, ...args, MethodInfo*)` for instance methods, `fn(...args, MethodInfo*)` for
  statics. The trailing `MethodInfo*` is the method handle itself.
- **Two ways to call** (see §2): direct pointer casts (fast, but a managed exception
  unwinds through native frames and crashes the game) vs `runtime_invoke` (boxed args,
  exceptions captured — the default for anything that can throw).
- **CodeStage Obscured types.** Player-facing values (`ObscuredLong`, `ObscuredInt`,
  `ObscuredString`, `ObscuredBool`) are anti-cheat wrappers. Never read their raw fields
  as plain ints — decode them (§2.4). Master-data rows and most view-controller fields
  are plain.
- **Never write a managed reference into managed memory** without the GC write barrier
  (`il2cpp::wbarrier_set`), or the GC can collect the value mid-operation.

---

## 2. Invocation primitives (`native/src/il2cpp.rs` + shared helpers)

### 2.1 Resolve a class and method

```rust
let k = il2cpp::class("Gallop.WorkDataManager");   // null if renamed/missing
let m = il2cpp::method(k, "get_TeamStadiumData", 0); // name + arg count; walks parent chain
```

`il2cpp::method` searches base classes too — that's why `Send` (declared on
``Gallop.RequestBase`1``) resolves from a concrete request class, and `get_Season`
*(candidate)* resolves from `RoomData` even though it lives on parent
`ExhibitionRaceDataBase`.

Nested classes (`WorkFriendData.FriendData`, `WorkRoomMatchData.RoomData`) can NOT be
found by name — resolve them from a live instance instead: `il2cpp::object_class(obj)`.

### 2.2 Direct pointer call — hot paths, known-safe methods

```rust
let p = il2cpp::method_pointer(m);
let f: extern "C" fn(*mut c_void, *const c_void) -> *mut c_void = std::mem::transmute(p);
let ret = f(this, m as *const c_void);   // instance getter: (this, MethodInfo*)
```

Use only when the method can't throw in practice (simple getters on live screens).
`hunter::call_getter_obj` wraps this pattern.

### 2.3 `runtime_invoke` — the safe default

```rust
let ret = il2cpp::runtime_invoke(m, this, &mut args); // args: &mut [*mut c_void]
```

- Managed exceptions are captured (returns null) instead of crashing.
- Value-type args are passed as **pointers to the value** (e.g. a bool flag is
  `&mut 0u8 as *mut u8 as *mut c_void`).
- Value-type RETURNS come back **boxed** — decode with §2.4.
- `pruner::bridge::invoke0(this, klass, "name")` is the shared 0-arg convenience wrapper
  (null-safe, resolves the method by name each call).

### 2.4 Decoding returns (`pruner::bridge`, shared with `roomfinder`)

```rust
unbox_i64(boxed)     // Int64/Int32/Boolean (boxed value @ +0x10) and ObscuredLong/ObscuredInt
                     //   (plain = hiddenValue ^ currentCryptoKey, offsets from metadata)
plain_string(obj)    // System.String directly; anything else (ObscuredString) via its
                     //   own ToString(), which returns the decrypted value
```

### 2.5 Raw field access

```rust
let off = il2cpp::field_offset(k, "_selectedRoomId");  // from metadata, not hardcoded
*((obj as usize + off) as *mut i32) = value;           // width must match the field type
```

Check the field's type first (`il2cpp::class_fields` gives name/offset/type) — a wrong
width silently corrupts the neighbouring field. Only plain (non-Obscured) value fields
are safe to touch raw.

### 2.6 Managed containers

```
List<T>:  _items @0x10 (T[]),  _size @0x18
T[]:      element data starts @0x20; reference elems are 8-byte pointers
String:   read via il2cpp::read_string (UTF-16 → UTF-8)
```

### 2.7 Hooks (retour `RawDetour`)

```rust
let d = RawDetour::new(target_ptr, my_hook as *const ())?;
d.enable()?;
ORIG.store(d.trampoline() as *const () as usize, ...);  // ALWAYS chain to this
```

- Check `il2cpp::is_detoured(p)` first and skip if another engine owns the method —
  EXCEPT `TweenManager.Update`, which deliberately **stacks** (installs even when
  Hachimi owns it) so the frame pump always runs; it always chains.
- Hooks are installed from `boot.rs` on an IL2CPP-attached thread, never lazily.

### 2.8 Discovery / scan tooling

```rust
il2cpp::find_classes("roommatch")   // every loaded class whose name contains the needle
il2cpp::class_fields(k)             // (name, offset, type) per field
il2cpp::class_parent(k)             // walk inheritance for inherited entry points
il2cpp::class_methods(k) / method_param_types(k, name)
```

`pruner::bridge::dump_class` formats one class (fields + methods + parent chain) into a
scan report. Both the Follower pruner and Room finder panels expose a **Scan (RE log)**
button that dumps keyword-matched classes to `heaven-logs/heaven-follower-scan.txt` /
`heaven-roommatch-scan.txt`. That workflow (ship candidates → run Scan → pin real names)
is how every table below was confirmed.

---

## 3. Shared game entry points

| Method | Args | Invoked via | Purpose |
|---|---|---|---|
| `Gallop.WorkDataManager.get_Instance` | 0 (static) | direct pointer | Root of all client-side "work data". Everything player-state flows from here. |
| `DG.Tweening.Core.TweenManager.Update` | 3 (static) | **hooked** (stacking) | Per-frame main-thread tick. Drives `hunter::frame_pump`, `padder::pump`, `reset::poll`, `affinity::poll`, `pruner::pump`, `roomfinder::pump`. Signature `(updateType: i32, dt: f32, idt: f32, MethodInfo*)`. |
| ``Gallop.RequestBase`1.Send`` | 7 | `runtime_invoke` | The game's own network send, inherited by every `Gallop.*Request`. Args: `(onSuccess: Action<Resp>, onError: Action<…>, 5 × bool)`. Pass null callbacks + all-false flags (as `&mut u8` pointers) for a silent fire-and-forget send. The flags drive UI side-effects (spinner / error dialog / caching). |

To build a request: `il2cpp::object_new(request_class)`, write the payload fields raw
(§2.5 — they're plain ints on `RequestCommon` subclasses; offsets ≥0x88 are the
request-specific fields), then invoke `Send`.

---

## 4. Team Trials opponent hunter (`hunter.rs`)

Target screen: **Team Trials → Select Opponent** (`Gallop.TeamStadiumOpponentSelectViewController`).

| Method / field | Args | Invoked via | Purpose |
|---|---|---|---|
| `TeamStadiumOpponentSelectViewController.OnOpponentInEnd` | 1 | **hooked** | Fires once per opponent card when a batch of 3 finishes loading. Captures the live VC (`this`), debounces 700 ms, then read → match → schedule next roll. |
| `TeamStadiumOpponentSelectViewController.PlayOut` | 2 | **hooked** | Screen exit transition → forget the VC, stop the hunt. |
| `TeamStadiumOpponentSelectViewController.<InitializeView>b__8_0` | 0 | direct pointer | The Reload BUTTON's own handler. Disables the button, builds the proper success callback (re-init view, re-enable buttons), then calls `SendApi`. Calling `SendApi(null)` directly leaves the button stuck grey — always go through the handler. |
| `WorkDataManager.get_TeamStadiumData` | 0 | direct pointer | Team Trials work-data blob (`Gallop.WorkTeamStadiumData`). |
| `WorkTeamStadiumData.get_OpponentDataList` | 0 | direct pointer | `List<OpponentData>` — the 3 currently offered opponents. |
| `OpponentData.GetTrainerName` | 1 | direct pointer | `(this, viewerId: i64, MethodInfo*) → String`. Resolved from the live instance (nested class). |
| `OpponentData.ServerData` @0x78 → `opponent_viewer_id` @0x18 | — | raw read | Target's viewer id (plain `i64`, no Obscured decode). |

## 5. Follower pruner (`pruner.rs`)

Data source: the follower list in friend work data (open the in-game follower list once
so it's populated).

| Method / field | Args | Invoked via | Purpose |
|---|---|---|---|
| `WorkDataManager.get_FriendData` | 0 | `invoke0` | Friend/follow work-data blob (`Gallop.WorkFriendData`). |
| `WorkFriendData.GetFollowerList` | 0 | `invoke0` | `List<WorkFriendData.FriendData>` — people following YOU. A METHOD, not a `get_` property. |
| `FriendData.get_ViewerId` | 0 | `invoke0` + `unbox_i64` | Follower's viewer id (`ObscuredLong`). |
| `FriendData.get_Name` | 0 | `invoke0` + `plain_string` | Trainer name (`ObscuredString`). |
| `FriendData.get_LastLoginUnixTime` | 0 | `invoke0` + `unbox_i64` | Last login (`ObscuredLong`, unix secs) → inactivity sort key. |
| `Gallop.FriendUnFollowerRequest` (class) | — | `object_new` | Removes one of YOUR followers. ⚠ `FriendUnFollowRequest` (no "-er") unfollows someone YOU follow — the wrong one. |
| `FriendUnFollowerRequest.friend_viewer_id` @0x88 | — | raw write (i64) | The TARGET's viewer id. ⚠ Parent `RequestCommon.viewer_id` @0x10 is the SENDER's own id — writing the target there corrupts the request. |
| `FriendUnFollowerRequest.Send` (inherited) | 7 | `runtime_invoke` | Fires the same wire call as the game's own remove button (see §3). |

## 6. Room Match finder (`roomfinder.rs`)

Target screen: **Room Match → Join Room** (guest room list,
`Gallop.RoomMatchGuestEntryViewController`).

| Method / field | Args | Invoked via | Purpose |
|---|---|---|---|
| `RoomMatchGuestEntryViewController.CreateRoomListUI` | 1 | **hooked** | Fires when the list UI is (re)built = fresh data in work data. Captures the VC, sets the read flag for `pump()`. |
| `RoomMatchGuestEntryViewController.PlayOutView` | 0 | **hooked** | Screen exit → forget VC, stop the hunt. |
| `RoomMatchGuestEntryViewController.OnClickRoomUpdateButton` | 0 | `runtime_invoke` | The reload BUTTON's own handler (cooldown `_reloadButtonCoolTimer` included) — the human refresh path. |
| `RoomMatchGuestEntryViewController._selectedRoomId` @0x30 | — | raw write (i32) | Select a room, same as tapping its list row. |
| `RoomMatchGuestEntryViewController.ChangeEntryScene` | 0 | `runtime_invoke` | The **Join Race** transition → runner-entry screen (`RoomMatchCharacterEntryViewController`, "Please select your runners"). Reads the selected RoomData. Used by auto-open. |
| `RoomMatchGuestEntryViewController.OpenSelectedRoomDetail` | 0 | `runtime_invoke` | The Details dialog. Fallback only. |
| `WorkDataManager.get_RoomMatchData` | 0 | `invoke0` | Room-match work data (`Gallop.WorkRoomMatchData`). |
| `WorkRoomMatchData.get_GuestEntryRoomList` | 0 | `invoke0` | `List<WorkRoomMatchData.RoomData>` — the browsable open rooms. |
| `RoomData.get_RoomId` | 0 | `invoke0` + `unbox_i64` | Room id (plain Int32, `_roomId` @0x88). |
| `RoomData.get_HostUser` → `UserData.get_Name` *(candidate)* | 0 | `invoke0` + `plain_string` | Host trainer name; falls back to `get_RoomName` (ObscuredString). |
| `RoomData.GetMasterRaceCourseSet` | 0 | `invoke0` | Master course row → track / distance / surface (below). |
| course row `get_RaceTrackId` / `get_Distance` / `get_Ground` *(candidates + raw fallback)* | 0 | `invoke0` / typed raw read | Racecourse id (10001…10101), metres, ground (1 turf / 2 dirt). Plain master data. |
| `RoomData.get_Season` / `get_Weather` *(candidates, on parent `ExhibitionRaceDataBase`)* | 0 | `invoke0` + `unbox_i64` | Race conditions; unresolved = "unknown" = an active filter won't match (fail-safe). |
| `RoomData.get_CurrentEntryNum` | 0 | `invoke0` + `unbox_i64` | Current entries (`ObscuredInt`). |
| `RoomData.GetRemainEntryNum` | 0 | `invoke0` + `unbox_i64` | Open slots, straight from the game's own math (preferred over capacity − members). |
| `RoomData.get_RankRestriction` / `get_RankRestrictionType` | 0 | `invoke0` + `unbox_i64` | Career-rank entry gate (`ObscuredInt`s, fields `_rankRestriction`/`_rankRestrictionType`). Both 0 = "Career Rank: None". |
| `RoomData.IsRestrictChara` | 0 | `invoke0` + `unbox_i64` (boxed `Boolean`) | True when the room bans specific Umas ("Restrictions: Yes"). |

Saved-team ("My Runners") loader on the runner-entry screen — replicates the preset dialog's
"Load List" button without opening the dialog:

| Method | Args | Invoked via | Purpose |
|---|---|---|---|
| `RoomMatchCharacterEntryViewController.PlayInView` / `PlayOutView` | 0 | **hooked** | Capture / release the live entry controller so the loader can target it. |
| `RoomMatchUtil.CreateDeckItemDataList` | 0 (static) | `runtime_invoke` (null `this`) | Builds the same `List<PartsExhibitionRaceDeckCarouselItem.ItemData>` the My Runners dialog shows from the saved-preset deck dict. ⚠ Distinct from the 2-arg `ExhibitionRaceUtil.CreateDeckItemDataList(dict,int)`. |
| `ItemData.get_PresetId` / `get_HasChara` / `get_CharaList` | 0 | `invoke0` | Slot id (match team 1–5), empty-slot guard, and the `List<ExhibitionRaceEntryCharaInfo>` runners. ItemData is nested under `PartsExhibitionRaceDeckCarouselItem`. |
| `List<ExhibitionRaceEntryCharaInfo>.ToArray` | 0 | `runtime_invoke` | Materialise the runner array set_TempEntryCharaArray expects. |
| `RoomMatchCharacterEntryViewController.set_TempEntryCharaArray` | 1 | `runtime_invoke` | Stage the runners (field `TempEntryCharaArray` @0x50) — the same field the game's own `OnCallChara` writes. |
| `RoomMatchCharacterEntryViewController.UpdateEntryList` | 0 | `runtime_invoke` | Refresh the on-screen entry slots from the staged array. |
| `RoomMatchCharacterEntryViewController.OnClickDecideButton` | 0 | `runtime_invoke` | The **Confirm** button → validates the entry, then opens the "Confirm Registration" dialog (`DialogRoomMatchConfirmCharaEntry`). Does NOT itself join. Used by auto-confirm. |
| `DialogRoomMatchConfirmCharaEntry.Initialize` / `PushDialog` | 2 | **hooked** | The "Confirm Registration" dialog build; capture its second arg — the OK (`onRight`) `System.Action` (`_onRight` @0x50). |
| `System.Action.Invoke` (on the captured `onRight`) | 0 | `runtime_invoke` | Fire the dialog's OK action = the real join (sends `SendRoomMatchEntryRoomAPI` + transitions). Exactly what tapping OK does. |
| `RoomMatchRaceGetPresetArrayRequest` (+ inherited `RequestBase.Send(7)`) | 7 | `object_new` + `runtime_invoke` | Prefetch presets so `WorkRoomMatchData` deck dict is populated before a room is found (the game otherwise only fetches on opening My Runners). Fire-and-forget; the game's response handler applies it. |

Full auto-join pipeline (found room → open → load team → confirm → accept) is a main-thread
state machine: `open_room` (ChangeEntryScene) → wait for the entry controller's `PlayInView`
capture → **poll `entry_ready` (buttons built) + retry `load_preset` until the presets arrive**
→ `OnClickDecideButton` → wait for the "Confirm Registration" dialog's captured `onRight` action
→ fire it (join). Every stage is readiness-gated: staging before `_charaEntryButtonList` (@0x38)
exists dereferences unbuilt buttons and hard-crashes the game, so we wait rather than guess a
delay. The two screen controllers (guest room-list vs. runner-entry) cross-clear each other on
capture so the loader UI can't stick after backing out of a room.

Deliberately NOT used: `RoomMatchCharacterEntryViewController.OnCallChara(list, Action)` — the
game's own load callback, but its second arg is a dialog-close delegate we'd have to
fabricate; writing `TempEntryCharaArray` + `UpdateEntryList` reaches the same state directly.
Also `Gallop.RoomMatchEntryRoomRequest` — a real entry needs
`entry_chara_array` (which trained Umas race), so unattended sending could be rejected or
corrupt entry state. The finder stops at the runner-entry screen; the user's Confirm
sends the validated request through `RoomMatchCharacterEntryViewController.SendRoomMatchEntryRoomAPI`.

## 7. Network capture bridge (`uma_bridge.rs` + `race_net.rs`)

Feeds companion tools (UmaLauncher) decrypted game traffic over UDP `127.0.0.1:17229`,
replacing the external CarrotBlender/CarrotJuicer plugin.

| Method | Args | Invoked via | Purpose |
|---|---|---|---|
| `Gallop.HttpHelper.CompressRequest` | 1 (static) | **hooked** | `(byte[] requestData, MethodInfo*) → byte[]`. The RETURN (compressed request, the exact buffer fed into AES) is forwarded as the type-3 packet — that's what makes the launcher consider itself "wired". |
| `Gallop.HttpHelper.DecompressResponse` | 1–2 (static/inst, probed) | **hooked** (`race_net.rs`) | Return = plain msgpack response AFTER decrypt + lz4-decompress — the update-proof capture point (survived the 2026-07-01 lz4 change that broke CarrotBlender). Feeds `uma_bridge::send_response` plus Heaven's own race/continue parsing. |

Forwarding re-encrypts with Heaven's OWN static AES-256-CBC key/iv and speaks the
launcher's existing UDP framing (types 1/2 = key/iv, 0 = response, 4/5 = multipart,
3 = plain request). All socket work happens on a worker thread — the game frame never
blocks. Managed `byte[]` payloads are read from offset `0x20` (see §2.6).

---

## 8. Adding a new feature against an un-RE'd screen (the proven loop)

1. Write the bridge with ordered **candidate name lists**, resolving everything by name
   at runtime; every failure message names the step that didn't resolve.
2. Add a **Scan** action: `il2cpp::find_classes(<keywords>)` + `dump_class` into a
   `heaven-logs/*.txt` report (fields, methods, parent chains — parents carry the
   inherited `Send`/view entry points).
3. Run once in-game on the target screen; pin the real names from the report and mark
   them confirmed here.
4. Prefer driving the game's OWN button handlers / view transitions over raw requests —
   they build the proper callbacks, respect cooldowns, and go through validated flows
   (hunter's `b__8_0` reload, room finder's `OnClickRoomUpdateButton` + `ChangeEntryScene`).
5. Keep everything through `runtime_invoke` + `unbox_i64`/`plain_string` unless a hot
   path forces a direct call, and keep all of it on the main thread via the tween pump.
