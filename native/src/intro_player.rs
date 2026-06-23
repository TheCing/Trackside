//! Heaven — native streaming intro video player (D3D11).
//!
//! The N-textures overlay (one GPU texture per frame, all resident) is VRAM-bound
//! and can only hold ~15 s of video. To play the WHOLE clip we need a real player:
//! a single dynamic texture whose pixels we rewrite every frame from a background
//! JPEG-decode thread, drawn as a fullscreen quad over the game.
//!
//! hudhook never exposes the D3D11 device, and its `add_image` only draws textures
//! from its own heap (which we can't update). So we:
//!   1) capture the game's `ID3D11Device` + immediate context once, via a transient
//!      hook on `ID3D11DeviceContext::OMSetRenderTargets` (a function hudhook does
//!      NOT hook → no detour conflict);
//!   2) build our own quad pipeline (VS/PS/IL/VB/sampler/blend/raster) + a DYNAMIC
//!      texture we Map-upload each frame;
//!   3) draw inside an imgui `RawCallback` (runs while hudhook has the back-buffer
//!      RTV + viewport bound), then emit `ResetRenderState` so hudhook re-binds its
//!      own pipeline and the control panels render undisturbed. hudhook already
//!      backs up / restores the GAME's device state around the whole imgui pass, so
//!      the game itself is never corrupted by our draw.
//!
//! Local-only (Cygames IP): the packed frame file lives next to the DLL and is
//! never bundled or committed. Private build only (`banner`).

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;
use windows::core::{s, Interface};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_NULL, D3D_FEATURE_LEVEL_11_0, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};

use hudhook::imgui::sys;

fn log(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

// ── captured game device/context (only ever touched on the render thread) ───────
static DEVICE: AtomicUsize = AtomicUsize::new(0); // *mut ID3D11Device (owned ref)
static CONTEXT: AtomicUsize = AtomicUsize::new(0); // *mut ID3D11DeviceContext (owned ref)
static CAPTURED: AtomicBool = AtomicBool::new(false);

// ── device capture hook ─────────────────────────────────────────────────────────
//
// We hook `ID3D11Device::CreateRenderTargetView` (not OMSetRenderTargets). Both run
// every frame and let us grab the game's device, BUT OMSetRenderTargets is ALSO
// called during the game's D3D11 teardown (unbinding render targets at quit) — an
// inline hook left on it deadlocks the shutdown path → the game hangs on close.
// CreateRenderTargetView is only called to BUILD targets (hudhook makes one per
// frame); creation stops well before teardown, so a permanent hook on it is never
// hit during the dangerous window. `this` is the device directly.
type CreateRtvFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const c_void,
    *mut *mut c_void,
) -> windows::core::HRESULT;
static CAP_TRAMP: AtomicUsize = AtomicUsize::new(0);

struct SendDetour(RawDetour);
unsafe impl Send for SendDetour {}
unsafe impl Sync for SendDetour {}
static CAP_DETOUR: OnceLock<SendDetour> = OnceLock::new();

unsafe extern "system" fn rtv_hook(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const c_void,
    out: *mut *mut c_void,
) -> windows::core::HRESULT {
    if !CAPTURED.load(Ordering::Acquire) && !this.is_null() {
        CAPTURED.store(true, Ordering::Release);
        // `this` is the device. Take owned refs to it + its immediate context so they
        // outlive transient borrows (the game keeps them alive for the process anyway).
        if let Some(dev) = ID3D11Device::from_raw_borrowed(&this) {
            let dev_owned = dev.clone();
            match dev.GetImmediateContext() {
                Ok(c) => {
                    DEVICE.store(dev_owned.into_raw() as usize, Ordering::Release);
                    CONTEXT.store(c.into_raw() as usize, Ordering::Release);
                    log("[vid] device captured (CreateRenderTargetView)");
                }
                Err(e) => log(&format!("[vid] capture: GetImmediateContext failed: {e}")),
            }
        }
    }
    let t = CAP_TRAMP.load(Ordering::Relaxed);
    let f: CreateRtvFn = std::mem::transmute(t);
    f(this, resource, desc, out)
}

/// Spawn the device capture early (off the IL2CPP boot path). A short delay lets the
/// game's D3D11 graphics come up first; capture then lands on hudhook's next rendered frame.
pub fn spawn_capture() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(800));
        install_capture();
    });
}

/// Install the device-capture hook. Safe to call once the runtime is up; hudhook
/// creates a back-buffer RTV every frame on the game device → capture within a frame.
pub fn install_capture() {
    if CAP_DETOUR.get().is_some() {
        return;
    }
    unsafe {
        // Dummy device just to read the (shared) vtable address of CreateRenderTargetView —
        // the same trick hudhook uses for Present. All d3d11.dll devices share it.
        let mut dev: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        let hr = D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_NULL,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        );
        if hr.is_err() || dev.is_none() {
            log(&format!("[vid] capture: dummy device failed: {hr:?}"));
            return;
        }
        let dev = dev.unwrap();
        let target: *const () = std::mem::transmute(dev.vtable().CreateRenderTargetView);
        if crate::il2cpp::is_detoured(target as *const std::ffi::c_void) {
            log("[vid] capture target already detoured, skipping");
            return;
        }
        match RawDetour::new(target, rtv_hook as *const ()) {
            Ok(detour) => {
                // Store the trampoline BEFORE enabling so the very first call (which can
                // race enable) never sees a null trampoline.
                CAP_TRAMP.store(detour.trampoline() as *const () as usize, Ordering::Relaxed);
                match detour.enable() {
                    Ok(()) => {
                        let _ = CAP_DETOUR.set(SendDetour(detour));
                        log("[vid] CreateRenderTargetView capture hook armed");
                    }
                    Err(e) => log(&format!("[vid] capture enable failed: {e}")),
                }
            }
            Err(e) => log(&format!("[vid] capture detour failed: {e}")),
        }
    }
}

pub fn device() -> Option<ID3D11Device> {
    let p = DEVICE.load(Ordering::Acquire);
    if p == 0 {
        return None;
    }
    unsafe { ID3D11Device::from_raw_borrowed(&(p as *mut c_void)).cloned() }
}
pub fn context() -> Option<ID3D11DeviceContext> {
    let p = CONTEXT.load(Ordering::Acquire);
    if p == 0 {
        return None;
    }
    unsafe { ID3D11DeviceContext::from_raw_borrowed(&(p as *mut c_void)).cloned() }
}

pub fn is_captured() -> bool {
    DEVICE.load(Ordering::Acquire) != 0
}

// ── quad pipeline (render thread only) ──────────────────────────────────────────
struct Pipe {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    il: ID3D11InputLayout,
    vb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    tw: u32,
    th: u32,
}

// Render-thread-only: built and used exclusively inside the imgui RawCallback, which
// always runs on the game's render thread. Never touched from any other thread.
static mut PIPE: Option<Pipe> = None;
static PIPE_FAILED: AtomicBool = AtomicBool::new(false);

const VS_SRC: &str = r"
struct VSI { float2 pos: POSITION; float2 uv: TEXCOORD0; };
struct PSI { float4 pos: SV_POSITION; float2 uv: TEXCOORD0; };
PSI main(VSI i) { PSI o; o.pos = float4(i.pos, 0.0, 1.0); o.uv = i.uv; return o; }
";
const PS_SRC: &str = r"
struct PSI { float4 pos: SV_POSITION; float2 uv: TEXCOORD0; };
Texture2D tex0: register(t0);
SamplerState s0: register(s0);
float4 main(PSI i): SV_Target { return tex0.Sample(s0, i.uv); }
";

#[repr(C)]
#[derive(Clone, Copy)]
struct Vtx {
    x: f32,
    y: f32,
    u: f32,
    v: f32,
}

unsafe fn compile(src: &str, entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Option<Vec<u8>> {
    let mut blob: Option<windows::Win32::Graphics::Direct3D::ID3DBlob> = None;
    let mut err: Option<windows::Win32::Graphics::Direct3D::ID3DBlob> = None;
    let hr = D3DCompile(
        src.as_ptr() as *const c_void,
        src.len(),
        None,
        None,
        None,
        entry,
        target,
        0,
        0,
        &mut blob,
        Some(&mut err),
    );
    if hr.is_err() {
        log(&format!("[vid] shader compile failed: {hr:?}"));
        return None;
    }
    let blob = blob?;
    let ptr = blob.GetBufferPointer() as *const u8;
    let len = blob.GetBufferSize();
    Some(std::slice::from_raw_parts(ptr, len).to_vec())
}

unsafe fn build_pipe(tw: u32, th: u32) -> Option<Pipe> {
    let dev = device()?;

    let vs_bc = compile(VS_SRC, s!("main"), s!("vs_5_0"))?;
    let ps_bc = compile(PS_SRC, s!("main"), s!("ps_5_0"))?;

    let mut vs: Option<ID3D11VertexShader> = None;
    dev.CreateVertexShader(&vs_bc, None, Some(&mut vs)).ok()?;
    let mut ps: Option<ID3D11PixelShader> = None;
    dev.CreatePixelShader(&ps_bc, None, Some(&mut ps)).ok()?;

    let layout = [
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: s!("POSITION"),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: s!("TEXCOORD"),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 8,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
    ];
    let mut il: Option<ID3D11InputLayout> = None;
    dev.CreateInputLayout(&layout, &vs_bc, Some(&mut il)).ok()?;

    // Fullscreen triangle strip (NDC). v flipped so the image is upright.
    let verts = [
        Vtx { x: -1.0, y: 1.0, u: 0.0, v: 0.0 },
        Vtx { x: 1.0, y: 1.0, u: 1.0, v: 0.0 },
        Vtx { x: -1.0, y: -1.0, u: 0.0, v: 1.0 },
        Vtx { x: 1.0, y: -1.0, u: 1.0, v: 1.0 },
    ];
    let vb_desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of_val(&verts) as u32,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
        ..Default::default()
    };
    let vb_data = D3D11_SUBRESOURCE_DATA {
        pSysMem: verts.as_ptr() as *const c_void,
        ..Default::default()
    };
    let mut vb: Option<ID3D11Buffer> = None;
    dev.CreateBuffer(&vb_desc, Some(&vb_data), Some(&mut vb)).ok()?;

    let samp_desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        ComparisonFunc: D3D11_COMPARISON_ALWAYS,
        MaxLOD: D3D11_FLOAT32_MAX,
        ..Default::default()
    };
    let mut sampler: Option<ID3D11SamplerState> = None;
    dev.CreateSamplerState(&samp_desc, Some(&mut sampler)).ok()?;

    // Opaque blend (the video covers the game fully).
    let mut blend_desc = D3D11_BLEND_DESC::default();
    blend_desc.RenderTarget[0].BlendEnable = false.into();
    blend_desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    let mut blend: Option<ID3D11BlendState> = None;
    dev.CreateBlendState(&blend_desc, Some(&mut blend)).ok()?;

    let rast_desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        DepthClipEnable: true.into(),
        ScissorEnable: false.into(),
        ..Default::default()
    };
    let mut raster: Option<ID3D11RasterizerState> = None;
    dev.CreateRasterizerState(&rast_desc, Some(&mut raster)).ok()?;

    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: tw,
        Height: th,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    dev.CreateTexture2D(&tex_desc, None, Some(&mut tex)).ok()?;
    let tex = tex?;
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    dev.CreateShaderResourceView(&tex, None, Some(&mut srv)).ok()?;

    log(&format!("[vid] pipeline built ({tw}x{th})"));
    Some(Pipe {
        vs: vs?,
        ps: ps?,
        il: il?,
        vb: vb?,
        sampler: sampler?,
        blend: blend?,
        raster: raster?,
        tex,
        srv: srv?,
        tw,
        th,
    })
}

/// Upload an RGBA frame (tw*th*4 bytes) into the dynamic texture.
unsafe fn upload(ctx: &ID3D11DeviceContext, pipe: &Pipe, rgba: &[u8]) {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if ctx
        .Map(&pipe.tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .is_err()
    {
        return;
    }
    let row = (pipe.tw * 4) as usize;
    let dst = mapped.pData as *mut u8;
    let pitch = mapped.RowPitch as usize;
    for y in 0..pipe.th as usize {
        let s = y * row;
        if s + row <= rgba.len() {
            std::ptr::copy_nonoverlapping(rgba.as_ptr().add(s), dst.add(y * pitch), row);
        }
    }
    ctx.Unmap(&pipe.tex, 0);
}

// ── streaming source: whole-video packed file, decoded by a pool of worker threads ──
//
// File layout (`intro_full.bin`, next to the DLL):
//   [u32 magic 'HVID'][u32 ver][u32 w][u32 h][u32 fps][u32 count]
//   then `count` × ([u32 jpeg_len][jpeg bytes])
//
// At 1440p60 a single decode thread can't keep up (and the whole file would be ~1.5 GB
// in RAM), so we keep ONLY the frame offset table in memory and stream each JPEG from
// disk on demand (positional `seek_read`, so one shared handle serves many threads). A
// pool of decode workers runs AHEAD of the playhead filling a small ring buffer of
// decoded RGBA frames; the render callback grabs the frame for the current time index.
// If decode can't sustain the fps the workers skip past stale frames → playback stays in
// sync (it's time-driven), it just drops frames gracefully.

use std::fs::File;
use std::io::Cursor;
use std::os::windows::fs::FileExt;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const RING_CAP: u64 = 24; // decoded frames buffered ahead (~0.4 s at 60 fps)
const DECODE_WORKERS: usize = 4; // parallel JPEG decoders

struct Meta {
    w: u32,
    h: u32,
    fps: u32,
    count: u32,
}
static META: OnceLock<Meta> = OnceLock::new();
static OFFSETS: OnceLock<Vec<(u64, usize)>> = OnceLock::new(); // (file offset, jpeg len)
static FILEH: OnceLock<Arc<File>> = OnceLock::new();
static LOADED: AtomicBool = AtomicBool::new(false);
static LOAD_FAILED: AtomicBool = AtomicBool::new(false);

static PLAYING: AtomicBool = AtomicBool::new(false);
static START: Mutex<Option<Instant>> = Mutex::new(None);
// While waiting for the audio to become audible we HOLD the video on frame 0. If the audio device
// is broken/absent and never reports a start, fall back to the local clock after this long so the
// intro can't freeze on frame 0 forever.
const AUDIO_WAIT_TIMEOUT_MS: u128 = 5000;

// Playback is time-driven: the render thread publishes the current absolute frame index
// (`PLAYHEAD`); workers claim the next index to decode (`DECODE_NEXT`) and stay within
// RING_CAP of the playhead. `EPOCH` is bumped on every start() so in-flight workers drop
// stale frames after a restart.
static PLAYHEAD: AtomicU64 = AtomicU64::new(0);
static DECODE_NEXT: AtomicU64 = AtomicU64::new(0);
static EPOCH: AtomicU64 = AtomicU64::new(0);

struct RingSlot {
    tag: i64, // absolute frame index this slot holds, -1 = empty
    buf: Vec<u8>,
}
static RING: OnceLock<Vec<Mutex<RingSlot>>> = OnceLock::new();
static LAST_UPLOADED: AtomicI64 = AtomicI64::new(-1); // render-thread-local (last shown abs)

fn rd_u32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

/// Index the packed file (header + offsets only — frame bytes stay on disk), open a
/// shared handle, build the ring, and spawn the decode workers. Idempotent.
fn load() {
    if LOADED.load(Ordering::Acquire) || LOAD_FAILED.load(Ordering::Relaxed) {
        return;
    }
    let path = crate::paths::local_file("intro_full.bin");
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            log(&format!("[vid] intro_full.bin open failed: {e}"));
            LOAD_FAILED.store(true, Ordering::Relaxed);
            return;
        }
    };
    let len_at = |off: u64| -> Option<u32> {
        let mut b = [0u8; 4];
        let mut got = 0usize;
        while got < 4 {
            match file.seek_read(&mut b[got..], off + got as u64) {
                Ok(0) | Err(_) => return None,
                Ok(n) => got += n,
            }
        }
        Some(u32::from_le_bytes(b))
    };
    // Header: magic, ver, w, h, fps, count (6 × u32). Frame bytes are NEVER read here —
    // we only walk the length prefixes (seek past each JPEG) to build the offset table.
    let (mut hdr, mut got) = ([0u8; 24], 0usize);
    while got < 24 {
        match file.seek_read(&mut hdr[got..], got as u64) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    if got < 24 || rd_u32(&hdr, 0) != 0x4856_4944 {
        log("[vid] intro_full.bin bad/short header");
        LOAD_FAILED.store(true, Ordering::Relaxed);
        return;
    }
    let w = rd_u32(&hdr, 8);
    let h = rd_u32(&hdr, 12);
    let fps = rd_u32(&hdr, 16);
    let count = rd_u32(&hdr, 20);
    let mut offs = Vec::with_capacity(count as usize);
    let mut pos = 24u64;
    for _ in 0..count {
        let Some(l) = len_at(pos) else { break };
        pos += 4;
        offs.push((pos, l as usize));
        pos += l as u64;
    }
    let file = Arc::new(file);
    let ring: Vec<Mutex<RingSlot>> = (0..RING_CAP)
        .map(|_| Mutex::new(RingSlot { tag: -1, buf: Vec::new() }))
        .collect();
    log(&format!(
        "[vid] loaded intro_full.bin: {} frames {w}x{h}@{fps} (streaming, {DECODE_WORKERS} workers)",
        offs.len()
    ));
    let _ = META.set(Meta { w, h, fps, count: offs.len() as u32 });
    let _ = OFFSETS.set(offs);
    let _ = FILEH.set(file);
    let _ = RING.set(ring);
    LOADED.store(true, Ordering::Release);
    for _ in 0..DECODE_WORKERS {
        spawn_worker();
    }
}

fn decode_jpeg(bytes: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    let px = dec.decode().ok()?;
    let info = dec.info()?;
    let n = (w * h) as usize;
    let mut out = vec![0u8; n * 4];
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 if px.len() >= n * 3 => {
            for i in 0..n {
                out[i * 4] = px[i * 3];
                out[i * 4 + 1] = px[i * 3 + 1];
                out[i * 4 + 2] = px[i * 3 + 2];
                out[i * 4 + 3] = 255;
            }
        }
        jpeg_decoder::PixelFormat::L8 if px.len() >= n => {
            for i in 0..n {
                out[i * 4] = px[i];
                out[i * 4 + 1] = px[i];
                out[i * 4 + 2] = px[i];
                out[i * 4 + 3] = 255;
            }
        }
        _ => return None,
    }
    Some(out)
}

fn spawn_worker() {
    std::thread::spawn(|| {
        let (Some(meta), Some(offs), Some(file), Some(ring)) =
            (META.get(), OFFSETS.get(), FILEH.get(), RING.get())
        else {
            return;
        };
        let count = meta.count as u64;
        if count == 0 {
            return;
        }
        let mut jpeg: Vec<u8> = Vec::new();
        loop {
            if !PLAYING.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(8));
                continue;
            }
            let ep = EPOCH.load(Ordering::Acquire);
            let ph = PLAYHEAD.load(Ordering::Acquire);
            // Don't run more than RING_CAP frames ahead of what's being shown.
            if DECODE_NEXT.load(Ordering::Acquire) >= ph + RING_CAP {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            let i = DECODE_NEXT.fetch_add(1, Ordering::AcqRel);
            // Far behind the playhead (decode can't keep up) → skip without decoding so the
            // counter catches up to the live playhead. Playback stays in sync, drops frames.
            if i + 2 < ph {
                continue;
            }
            if EPOCH.load(Ordering::Acquire) != ep {
                continue; // restarted — drop this claim
            }
            let (off, len) = offs[(i % count) as usize];
            if jpeg.len() != len {
                jpeg.resize(len, 0);
            }
            // Positional read (thread-safe on a shared handle). Loop to fill fully.
            let mut got = 0usize;
            let mut ok = true;
            while got < len {
                match file.seek_read(&mut jpeg[got..], off + got as u64) {
                    Ok(0) => {
                        ok = false;
                        break;
                    }
                    Ok(n) => got += n,
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            let Some(rgba) = decode_jpeg(&jpeg, meta.w, meta.h) else {
                continue;
            };
            if EPOCH.load(Ordering::Acquire) != ep {
                continue; // restarted during decode — drop
            }
            let Ok(mut slot) = ring[(i % RING_CAP) as usize].lock() else { continue; };
            slot.tag = i as i64;
            slot.buf = rgba;
        }
    });
}

/// Begin playback from the start (resets the frame clock + ring). Loads lazily on first call.
pub fn start() {
    load();
    if LOAD_FAILED.load(Ordering::Relaxed) {
        return;
    }
    // Bump epoch first so any in-flight worker drops its stale frame, then reset counters
    // and clear the ring so old frames can't be mistaken for the new playhead.
    EPOCH.fetch_add(1, Ordering::AcqRel);
    PLAYHEAD.store(0, Ordering::Release);
    DECODE_NEXT.store(0, Ordering::Release);
    LAST_UPLOADED.store(-1, Ordering::Release);
    if let Some(ring) = RING.get() {
        for s in ring {
            if let Ok(mut g) = s.lock() { g.tag = -1; }
        }
    }
    if let Ok(mut g) = START.lock() { *g = Some(Instant::now()); }
    PLAYING.store(true, Ordering::Release);
}
pub fn stop() {
    PLAYING.store(false, Ordering::Release);
}
pub fn is_playing() -> bool {
    PLAYING.load(Ordering::Acquire)
}

/// True only when a custom intro is actually present and indexed (intro_full.bin loaded ok).
/// Used to gate the whole intro path: with no media we don't mute the title BGM, draw, or
/// show the START button — the game's title plays normally.
pub fn has_video() -> bool {
    load();
    LOADED.load(Ordering::Acquire) && !LOAD_FAILED.load(Ordering::Relaxed)
}

/// Draw the quad. Called ONLY from the imgui RawCallback (render thread, RTV bound).
// PIPE is render-thread-only (see its declaration), so the &/&mut references here are sound.
#[allow(static_mut_refs)]
unsafe fn draw_now() {
    let Some(ctx) = context() else { return };
    if PIPE_FAILED.load(Ordering::Relaxed) {
        return;
    }
    let Some(meta) = META.get() else { return };
    if PIPE.is_none() {
        match build_pipe(meta.w, meta.h) {
            Some(p) => PIPE = Some(p),
            None => {
                PIPE_FAILED.store(true, Ordering::Relaxed);
                log("[vid] pipeline build FAILED — disabling");
                return;
            }
        }
    }
    let pipe = PIPE.as_ref().unwrap();
    let Some(ring) = RING.get() else { return };

    // Current absolute frame. The clock origin is the AUDIO's real start instant (set once the
    // device is open and the first sample is queued), so video and song stay locked — the video
    // simply holds frame 0 until audio actually starts, then both advance from the same t=0.
    // Fall back to the local start clock if the audio module never reports a start (shouldn't
    // happen — it marks one even when there's no song file).
    let cur = match crate::audio::playback_start() {
        Some(st) => (st.elapsed().as_secs_f64() * meta.fps as f64) as u64,
        // Audio is queued but not audible yet (device buffer still filling — up to a few seconds on
        // slow machines). HOLD on frame 0 so the video can't race ahead of the song; both then
        // advance from the same origin once audio truly starts. Safety: if audio never reports a
        // start within AUDIO_WAIT_TIMEOUT_MS (broken/absent device), fall back to the local clock so
        // the intro can't freeze on frame 0.
        None => match START.lock().ok().and_then(|g| *g) {
            Some(st) if st.elapsed().as_millis() >= AUDIO_WAIT_TIMEOUT_MS => {
                (st.elapsed().as_secs_f64() * meta.fps as f64) as u64
            }
            _ => 0,
        },
    };
    PLAYHEAD.store(cur, Ordering::Release);

    // Upload the frame for `cur` if a worker has it ready and we haven't shown it already.
    if LAST_UPLOADED.load(Ordering::Relaxed) != cur as i64 {
        if let Ok(slot) = ring[(cur % RING_CAP) as usize].lock() {
            if slot.tag == cur as i64 && slot.buf.len() == (pipe.tw * pipe.th * 4) as usize {
                upload(&ctx, pipe, &slot.buf);
                LAST_UPLOADED.store(cur as i64, Ordering::Relaxed);
            }
        }
    }
    // Nothing decoded yet → don't draw an uninitialized texture (black/garbage flash).
    if LAST_UPLOADED.load(Ordering::Relaxed) < 0 {
        return;
    }

    // Bind our pipeline (RTV + viewport are already hudhook's = back buffer, full).
    ctx.IASetInputLayout(&pipe.il);
    let stride = std::mem::size_of::<Vtx>() as u32;
    let offset = 0u32;
    ctx.IASetVertexBuffers(0, 1, Some(&Some(pipe.vb.clone())), Some(&stride), Some(&offset));
    ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    ctx.VSSetShader(&pipe.vs, Some(&[]));
    ctx.PSSetShader(&pipe.ps, Some(&[]));
    ctx.PSSetSamplers(0, Some(&[Some(pipe.sampler.clone())]));
    ctx.PSSetShaderResources(0, Some(&[Some(pipe.srv.clone())]));
    ctx.OMSetBlendState(&pipe.blend, Some(&[0.0; 4]), 0xffffffff);
    ctx.RSSetState(&pipe.raster);
    ctx.Draw(4, 0);
}

// imgui RawCallback trampoline.
unsafe extern "C" fn draw_trampoline(_list: *const sys::ImDrawList, _cmd: *const sys::ImDrawCmd) {
    draw_now();
}

/// Enqueue the video draw into imgui's background draw list, followed by a
/// ResetRenderState command so hudhook re-binds its own pipeline for the panels.
/// Call from `render()` while the intro is active.
pub fn enqueue_draw() {
    if !is_captured() || !is_playing() {
        return;
    }
    unsafe {
        let dl = sys::igGetBackgroundDrawList();
        if dl.is_null() {
            return;
        }
        sys::ImDrawList_AddCallback(dl, Some(draw_trampoline), std::ptr::null_mut());
        // -1 sentinel == ImDrawCallback_ResetRenderState → hudhook re-setups state.
        let reset: sys::ImDrawCallback = Some(std::mem::transmute(-1isize));
        sys::ImDrawList_AddCallback(dl, reset, std::ptr::null_mut());
    }
}
