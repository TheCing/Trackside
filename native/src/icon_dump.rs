//! In-process icon ripper — the offline path is dead (Global encrypts both the meta index
//! and the UnityFS block data), but while the game is SHOWING art, the decoded textures
//! are sitting in VRAM with their asset names intact. So:
//!
//!   main thread (pump, via ui_tempo's TweenManager detour):
//!     UnityEngine.Resources.FindObjectsOfTypeAll(typeof(Texture2D)) → walk every loaded
//!     texture, read name/size/native pointer, write an inventory log, queue matches.
//!   render thread (render_pump, called from HeavenOverlay::render):
//!     for each queued texture: CopyResource → staging → Map → save raw pixels to
//!     `trackside-icons/_dump/<name>_<W>x<H>.rgba`. The immediate context is single-
//!     threaded and lives on the render thread, so the readback MUST happen there.
//!
//! Curation into `trackside-icons/rank/<id>.rgba` etc. happens offline afterwards (the
//! inventory names the assets; a Python pass converts/resizes/renames).
//!
//! Trigger from the screen that shows the art you want: the career-complete screen has the
//! rank emblem loaded; the skill-learn screen has every shop skill icon loaded.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use once_cell::sync::Lazy;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_BC1_TYPELESS,
    DXGI_FORMAT_BC1_UNORM, DXGI_FORMAT_BC1_UNORM_SRGB, DXGI_FORMAT_BC3_TYPELESS,
    DXGI_FORMAT_BC3_UNORM, DXGI_FORMAT_BC3_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
};

use crate::il2cpp;
use crate::tools::log_to;

const LOG: &str = "trackside-icon-dump.txt";

/// Name substrings worth pulling pixels for (broad on purpose — curation is offline).
/// Inventories so far: `tex_team_rank_icon_030`, `trained_chr_icon_*`, and the emblem
/// ATLASES `Rank_tex` / `StatusRank_tex` — so "rank"/"grade" match plain, both orders.
const WANT: [&str; 8] = [
    "rank",
    "grade",
    "class_icon",
    "ico_skill",
    "skill_icon",
    "ico_chara",
    "chr_icon",
    "utx_ico",
];

static DUMP_REQUESTED: AtomicBool = AtomicBool::new(false);
static QUEUE: Lazy<Mutex<Vec<(String, u32, u32, usize)>>> = Lazy::new(|| Mutex::new(Vec::new()));
static STATUS: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new(String::new()));

pub fn status() -> String {
    STATUS.lock().map(|s| s.clone()).unwrap_or_default()
}
fn set_status(s: impl Into<String>) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s.into();
    }
}

/// UI: harvest every loaded texture matching the icon patterns.
pub fn request_dump() {
    DUMP_REQUESTED.store(true, Ordering::Relaxed);
    set_status("Dumping\u{2026} (open the screen that shows the art you want first)");
}

/// Call a 0-arg instance method returning i32 (get_width / get_height).
unsafe fn call_i32(obj: *mut c_void, m: il2cpp::Method) -> i32 {
    if obj.is_null() || m.is_null() {
        return 0;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return 0;
    }
    let f: extern "C" fn(*mut c_void, *const c_void) -> i32 = std::mem::transmute(p);
    f(obj, m as *const c_void)
}

/// Call a 0-arg instance method returning a pointer-sized value (GetNativeTexturePtr).
unsafe fn call_ptr(obj: *mut c_void, m: il2cpp::Method) -> usize {
    if obj.is_null() || m.is_null() {
        return 0;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return 0;
    }
    let f: extern "C" fn(*mut c_void, *const c_void) -> usize = std::mem::transmute(p);
    f(obj, m as *const c_void)
}

/// Call UnityEngine.Object.get_name → String.
unsafe fn obj_name(obj: *mut c_void, m: il2cpp::Method) -> String {
    if obj.is_null() || m.is_null() {
        return String::new();
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return String::new();
    }
    let f: extern "C" fn(*mut c_void, *const c_void) -> *mut c_void = std::mem::transmute(p);
    il2cpp::read_string(f(obj, m as *const c_void))
}

/// Resolve a 1-arg enumerator for "all loaded objects of type": managed method if it
/// survived stripping, else Unity's internal-call implementation (icalls aren't stripped).
unsafe fn resolve_find_all() -> (il2cpp::Method, *const c_void) {
    for (class, name) in [
        ("UnityEngine.Resources", "FindObjectsOfTypeAll"),
        ("UnityEngine.ResourcesAPIInternal", "FindObjectsOfTypeAll"),
        ("UnityEngine.ResourcesAPI", "FindObjectsOfTypeAll"),
    ] {
        let k = il2cpp::class(class);
        if !k.is_null() {
            let m = il2cpp::method(k, name, 1);
            if !m.is_null() {
                return (m, std::ptr::null());
            }
        }
    }
    for sig in [
        "UnityEngine.ResourcesAPIInternal::FindObjectsOfTypeAll",
        "UnityEngine.Resources::FindObjectsOfTypeAll",
    ] {
        let ic = il2cpp::resolve_icall(sig);
        if !ic.is_null() {
            return (std::ptr::null(), ic);
        }
    }
    (std::ptr::null(), std::ptr::null())
}

/// Main-thread pump: enumerate textures, log the inventory, queue matches for readback.
pub fn pump() {
    if !DUMP_REQUESTED.swap(false, Ordering::Relaxed) {
        return;
    }
    unsafe {
        let tex_k = il2cpp::class("UnityEngine.Texture2D");
        let obj_k = il2cpp::class("UnityEngine.Object");
        if tex_k.is_null() || obj_k.is_null() {
            set_status("UnityEngine classes not found (dump aborted)");
            return;
        }
        let (find_all_m, find_all_ic) = resolve_find_all();
        let get_name = il2cpp::method(obj_k, "get_name", 0);
        let get_w = il2cpp::method(tex_k, "get_width", 0);
        let get_h = il2cpp::method(tex_k, "get_height", 0);
        // GetNativeTexturePtr lives on UnityEngine.Texture (base class); icall as fallback.
        let texture_k = il2cpp::class("UnityEngine.Texture");
        let get_native = il2cpp::method(texture_k, "GetNativeTexturePtr", 0);
        let native_ic = if get_native.is_null() {
            il2cpp::resolve_icall("UnityEngine.Texture::GetNativeTexturePtr")
        } else {
            std::ptr::null()
        };
        // Diagnose precisely instead of one vague status: name what's missing and dump the
        // candidate classes' real method lists to the log for the next wiring pass.
        let missing_find = find_all_m.is_null() && find_all_ic.is_null();
        let missing_native = get_native.is_null() && native_ic.is_null();
        if missing_find || get_name.is_null() || missing_native {
            let mut diag = format!(
                "RESOLUTION FAILED: find_all={} get_name={} native_ptr={}\n",
                !missing_find,
                !get_name.is_null(),
                !missing_native
            );
            for cname in ["UnityEngine.Resources", "UnityEngine.ResourcesAPIInternal", "UnityEngine.Texture", "UnityEngine.Object"] {
                let k = il2cpp::class(cname);
                diag.push_str(&format!("\n-- {cname} (found: {})\n", !k.is_null()));
                if !k.is_null() {
                    for m in il2cpp::class_methods(k) {
                        diag.push_str(&format!("   fn {m}\n"));
                    }
                }
            }
            log_to(LOG, &diag);
            set_status("Resolution failed — details in trackside-logs/trackside-icon-dump.txt");
            return;
        }
        let type_obj = il2cpp::type_object(tex_k);
        if type_obj.is_null() {
            set_status("typeof(Texture2D) unavailable (dump aborted)");
            return;
        }
        let arr = if !find_all_m.is_null() {
            il2cpp::runtime_invoke(find_all_m, std::ptr::null_mut(), &mut [type_obj])
        } else {
            let f: extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(find_all_ic);
            f(type_obj)
        };
        if arr.is_null() {
            set_status("FindObjectsOfTypeAll returned null");
            return;
        }
        // IL2CPP array: max_length @0x18, elements @0x20 (8-byte refs).
        let count = *((arr as usize + 0x18) as *const i32);
        let mut inv = format!("==== TEXTURE INVENTORY ({count} loaded) ====\n");
        let mut queued = 0;
        for i in 0..count.clamp(0, 65536) as usize {
            let tex = *((arr as usize + 0x20 + i * 8) as *const *mut c_void);
            if tex.is_null() {
                continue;
            }
            let name = obj_name(tex, get_name);
            if name.is_empty() {
                continue;
            }
            let w = call_i32(tex, get_w) as u32;
            let h = call_i32(tex, get_h) as u32;
            inv.push_str(&format!("{name}\t{w}x{h}\n"));
            let lname = name.to_lowercase();
            if WANT.iter().any(|p| lname.contains(p)) && w > 0 && h > 0 && w <= 2048 && h <= 2048 {
                let native = if !get_native.is_null() {
                    call_ptr(tex, get_native)
                } else {
                    // Instance icall ABI: plain native fn taking `this`.
                    let f: extern "C" fn(*mut c_void) -> usize = std::mem::transmute(native_ic);
                    f(tex)
                };
                if native != 0 {
                    if let Ok(mut q) = QUEUE.lock() {
                        q.push((name, w, h, native));
                        queued += 1;
                    }
                }
            }
        }
        log_to(LOG, &inv);
        set_status(format!("{queued} textures queued \u{2014} pixels land next frames in trackside-icons/_dump/"));
    }
}

// ── BC1 / BC3 block decompression (the game's UI art is DXT-compressed) ────────

/// Expand the two RGB565 endpoints of a BC1-style colour block into the 4-colour palette.
fn bc_palette(c0: u16, c1: u16, four_color: bool) -> [[u8; 4]; 4] {
    let e = |c: u16| {
        [
            (((c >> 11) & 31) as u32 * 255 / 31) as u8,
            (((c >> 5) & 63) as u32 * 255 / 63) as u8,
            ((c & 31) as u32 * 255 / 31) as u8,
            255,
        ]
    };
    let (p0, p1) = (e(c0), e(c1));
    let mix = |a: u8, b: u8, num: u32, den: u32| (((a as u32) * num + (b as u32) * (den - num)) / den) as u8;
    if four_color || c0 > c1 {
        [
            p0,
            p1,
            [mix(p0[0], p1[0], 2, 3), mix(p0[1], p1[1], 2, 3), mix(p0[2], p1[2], 2, 3), 255],
            [mix(p0[0], p1[0], 1, 3), mix(p0[1], p1[1], 1, 3), mix(p0[2], p1[2], 1, 3), 255],
        ]
    } else {
        [
            p0,
            p1,
            [mix(p0[0], p1[0], 1, 2), mix(p0[1], p1[1], 1, 2), mix(p0[2], p1[2], 1, 2), 255],
            [0, 0, 0, 0], // BC1 punch-through transparent
        ]
    }
}

/// Decode one 4x4 colour block (8 bytes, BC1 layout) into `px`.
fn decode_color_block(b: &[u8], four_color: bool, px: &mut [[u8; 4]; 16]) {
    let c0 = u16::from_le_bytes([b[0], b[1]]);
    let c1 = u16::from_le_bytes([b[2], b[3]]);
    let pal = bc_palette(c0, c1, four_color);
    let idx = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    for i in 0..16 {
        px[i] = pal[((idx >> (i * 2)) & 3) as usize];
    }
}

/// Decode one BC3 block (16 bytes): 8-byte interpolated alpha + BC1 colour (always 4-colour).
fn decode_bc3_block(b: &[u8], px: &mut [[u8; 4]; 16]) {
    decode_color_block(&b[8..16], true, px);
    let (a0, a1) = (b[0] as u32, b[1] as u32);
    // 16 3-bit alpha indices packed little-endian across 6 bytes.
    let bits = u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], 0, 0]);
    for i in 0..16 {
        let code = ((bits >> (i * 3)) & 7) as u32;
        let a = match code {
            0 => a0,
            1 => a1,
            c if a0 > a1 => ((8 - c) * a0 + (c - 1) * a1) / 7,
            6 => 0,
            7 => 255,
            c => ((6 - c) * a0 + (c - 1) * a1) / 5,
        };
        px[i][3] = a as u8;
    }
}

/// Decode a full BC1/BC3 mapped surface into a tightly-packed RGBA buffer.
fn decode_bc_surface(data: *const u8, row_pitch: u32, w: u32, h: u32, bc3: bool) -> Vec<u8> {
    let block_bytes = if bc3 { 16 } else { 8 };
    let blocks_w = (w as usize + 3) / 4;
    let mut out = vec![0u8; (w * h * 4) as usize];
    let mut px = [[0u8; 4]; 16];
    for by in 0..(h as usize + 3) / 4 {
        let row = unsafe { data.add(by * row_pitch as usize) };
        for bx in 0..blocks_w {
            let block = unsafe { std::slice::from_raw_parts(row.add(bx * block_bytes), block_bytes) };
            if bc3 {
                decode_bc3_block(block, &mut px);
            } else {
                decode_color_block(block, false, &mut px);
            }
            for i in 0..16 {
                let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
                if x < w as usize && y < h as usize {
                    let o = (y * w as usize + x) * 4;
                    out[o..o + 4].copy_from_slice(&px[i]);
                }
            }
        }
    }
    out
}

/// Render-thread pump: staging-copy queued textures and write raw pixels to disk.
/// A few per frame keeps the frame-time hit invisible.
pub fn render_pump() {
    let batch: Vec<(String, u32, u32, usize)> = {
        let Ok(mut q) = QUEUE.lock() else { return };
        if q.is_empty() {
            return;
        }
        let n = q.len().min(4);
        q.drain(..n).collect()
    };
    let (Some(device), Some(context)) = (crate::intro_player::device(), crate::intro_player::context()) else {
        set_status("No captured D3D11 device (banner build required)");
        if let Ok(mut q) = QUEUE.lock() {
            q.clear();
        }
        return;
    };
    let dir = crate::paths::local_dir_migrated("trackside-icons", "heaven-icons").join("_dump");
    let _ = std::fs::create_dir_all(&dir);
    for (name, w, h, native) in batch {
        unsafe {
            let src: ID3D11Texture2D = std::mem::transmute_copy(&native);
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut desc);
            // Plain 32-bit reads directly; BC1/BC3 (the game's usual UI compression) decode
            // on the CPU. Anything else (BC7 etc.) is logged so we know what to add.
            #[derive(Clone, Copy, PartialEq)]
            enum Fmt {
                Rgba,
                Bgra,
                Bc1,
                Bc3,
            }
            let fmt = match desc.Format {
                DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => Fmt::Rgba,
                DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => Fmt::Bgra,
                DXGI_FORMAT_BC1_UNORM | DXGI_FORMAT_BC1_UNORM_SRGB | DXGI_FORMAT_BC1_TYPELESS => Fmt::Bc1,
                DXGI_FORMAT_BC3_UNORM | DXGI_FORMAT_BC3_UNORM_SRGB | DXGI_FORMAT_BC3_TYPELESS => Fmt::Bc3,
                _ => {
                    log_to(LOG, &format!("SKIP {name} — format {:?}", desc.Format));
                    std::mem::forget(src);
                    continue;
                }
            };
            let mut sdesc = desc;
            sdesc.Usage = D3D11_USAGE_STAGING;
            sdesc.BindFlags = 0;
            sdesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            sdesc.MiscFlags = 0;
            sdesc.MipLevels = 1;
            sdesc.ArraySize = 1;
            let mut staging: Option<ID3D11Texture2D> = None;
            if device.CreateTexture2D(&sdesc, None, Some(&mut staging)).is_err() {
                std::mem::forget(src);
                continue;
            }
            let Some(staging) = staging else {
                std::mem::forget(src);
                continue;
            };
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &src, 0, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_ok() {
                let out = match fmt {
                    Fmt::Bc1 => decode_bc_surface(mapped.pData as *const u8, mapped.RowPitch, w, h, false),
                    Fmt::Bc3 => decode_bc_surface(mapped.pData as *const u8, mapped.RowPitch, w, h, true),
                    Fmt::Rgba | Fmt::Bgra => {
                        let mut out = Vec::with_capacity((w * h * 4) as usize);
                        for row in 0..h {
                            let rp = (mapped.pData as usize + (row * mapped.RowPitch) as usize) as *const u8;
                            let line = std::slice::from_raw_parts(rp, (w * 4) as usize);
                            if fmt == Fmt::Rgba {
                                out.extend_from_slice(line);
                            } else {
                                for px in line.chunks_exact(4) {
                                    out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                                }
                            }
                        }
                        out
                    }
                };
                context.Unmap(&staging, 0);
                let safe: String = name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
                let _ = std::fs::write(dir.join(format!("{safe}_{w}x{h}.rgba")), &out);
            }
            // `src` was conjured from a raw pointer the game owns — never release it.
            std::mem::forget(src);
        }
    }
    let remaining = QUEUE.lock().map(|q| q.len()).unwrap_or(0);
    if remaining == 0 {
        set_status("Dump complete \u{2014} see trackside-icons/_dump/ + trackside-logs/trackside-icon-dump.txt");
    }
}
