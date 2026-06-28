# Race Director — guide

Race Director turns any race into a broadcast: a free chase camera plus a live, TV-grade telemetry
overlay. It is **read-only** — it never changes the race or its result.

Everything here lives in the Heaven menu (press **Insert**) under the **Race Director** section.

---

## 1. The free camera

A 3rd-person camera that chases an Uma during a race — orbit around her, zoom, raise or lower the
shot, and switch which Uma it follows.

**Turn it on:** *Race Director → Freecam → enable*. You can toggle it on or off at any point during a
race. With it off, the game's normal race camera plays.

**Move it:**

- **Mouse drag** (hold left button) — orbit / look around the Uma.
- **Mouse wheel** — zoom in and out.
- Or use the **keys you bind** for each action (see below).

**Rebind every control** in *Race Director → Key bindings*. The actions are:

| Action | What it does |
|--------|--------------|
| Orbit left / right | Swing the camera around the Uma |
| Zoom in / out | Move closer / further |
| Raise / Lower height | Lift or drop the shot |
| Previous / Next Uma | Switch which Uma the camera follows |
| Cycle preset | Step through your saved angle presets |
| Save preset | Save the current angle into the selected preset |

A key shown in red is bound to two actions — pick another.

### Angle presets

You can store up to **4 named angle presets per circuit**. Frame a shot you like, then use **Save
preset** to store it; **Cycle preset** steps between them. Presets are remembered per circuit, so each
track keeps its own set of angles.

---

## 2. The telemetry HUD

A broadcast-style overlay that works **independently of the camera** — you can run the HUD with the
game's own camera, or the free camera with no HUD. Turn the whole HUD or any individual panel on/off
under *Race Director → Telemetry*.

- **Timing tower** — the whole field, leader first: live order, time gaps, and running-style colours.
  The position number flashes **green** when an Uma gains a place and **red** when it loses one.
  **Click a row** to make the camera follow that Uma.
- **Win probability** — a live win chance for each runner that moves with the race.
- **Race phase + final furlong** — markers for the stage of the race, plus distance and progress.
- **Followed-Uma panel** — the Uma the camera is on: stamina and speed, the skills she has activated
  *with their effect* (for example `+0.35 m/s`), a **pace graph** of her whole race (hover it for
  max / average / minimum speed), and a side-by-side comparison with the rival just ahead.
- **Head marker** — a marker over the followed Uma's head (needs the free camera).
- **Trainer names** — in lobby races (Team Trials, Champions Meeting, Room Match), each trainer's name
  and their Umas, grouped by colour.

**Presets:** two one-click layouts at the bottom of *Telemetry* — **Broadcast** (a clean, minimal set
for recording) and **Full** (everything on). **HUD scale** resizes the whole overlay.

Every window is **resizable** (drag a corner) and movable; sizes and positions are remembered.

---

## 3. Tips

- For clean footage: enable the free camera, pick the **Broadcast** telemetry preset, and frame your
  shot with a preset per circuit so you can recall it instantly.
- The HUD reacts in real time — win probability swings on skill procs and stamina collapses, and the
  timing tower flashes on every position change.
- Win probability is a live estimate from the race state (pace, stamina, distance); it reacts to what
  happens but does not predict future skill activations.
