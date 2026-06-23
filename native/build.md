# Building the Heaven internal overlay (`heaven_overlay.dll`)

This DLL is the **internal** renderer — it draws the HUD inside the game's
D3D11 frame (like the reference screenshot). It is **optional**: Heaven runs
fully today in **external** mode without it. Build this when you want the HUD
rendered inside the swapchain instead of an overlay window.

## 1. Install the toolchain (one time)

1. **Rust (MSVC toolchain):** https://rustup.rs → run `rustup-init.exe`, accept
   defaults (`stable-x86_64-pc-windows-msvc`).
2. **MSVC Build Tools** (the C++ linker hudhook needs):
   - Download "Build Tools for Visual Studio" →
     https://visualstudio.microsoft.com/visual-cpp-build-tools/
   - In the installer tick **"Desktop development with C++"** (gives `link.exe`
     + Windows SDK). ~2–4 GB.
3. Restart the shell so `cargo` and `link.exe` are on PATH. Verify:
   ```
   cargo --version
   rustc --version
   ```

## 2. Build

```
cd native
cargo build --release
```

Output: `native/target/release/heaven_overlay.dll`.

> If `cargo` complains the `hudhook` API doesn't match, pin it:
> in `Cargo.toml` set `hudhook = "=0.6.0"` (or the latest 0.6.x) and re-run.
> The `ImguiRenderLoop` trait and `hudhook!` macro signatures occasionally move
> between minor versions — `overlay.rs` / `lib.rs` target the 0.6 line.

## 3. Switch Heaven to internal mode

In `../config.json`:

```json
"render_mode": "internal"
```

Then launch as usual:

```
python ../heaven.py
```

The Frida core will `Module.load` the DLL into the game; the Python host streams
`GameState` to it over `127.0.0.1:47800`; the DLL renders it in-game. **INSERT**
toggles the overlay (same key as external mode).

## Graphics API note

Umamusume (Unity) runs on **D3D11** by default — `lib.rs` uses `ImguiDx11Hooks`.
If a future build uses D3D12 or Vulkan, swap the hook type:

```rust
use hudhook::hooks::dx12::ImguiDx12Hooks;   // then: hudhook!(ImguiDx12Hooks, ...)
```

## How it connects to our data (no rewrite of hooks)

The native DLL does **not** re-implement any IL2CPP hooks. All game reading stays
in the Frida core (`../core`). The DLL is purely a renderer + a TCP sink:

```
Frida core (JS, IL2CPP) → Python host (GameState) → TCP :47800 → this DLL → imgui
```

`src/data.rs` mirrors `heaven_app/gamestate.py`. If you add a field to the Python
`GameState`, add it (snake_case, `#[serde(default)]`) to `data.rs` and use it in
`overlay.rs`.
