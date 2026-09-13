//! Trackside overlay preview host.
//!
//! A throwaway D3D11 window whose only job is to present frames so the Trackside
//! overlay can draw inside it WITHOUT the game. The overlay DLL is loaded into
//! this process with `LoadLibrary`; hudhook then hooks this window's swapchain
//! `Present`/`WndProc` exactly as it would in the game. The overlay's engine
//! thread idles (no GameAssembly.dll), so game-backed panels just show empty
//! states, but every visual — the sidebar menu, fonts, textures, animations —
//! renders identically to in-game.
//!
//! Usage:
//!   trackside-preview-host [path\to\trackside.dll]
//! Defaults to `trackside.dll` in the current directory. Press Insert in the
//! window to open the menu (same hotkey as in-game).
//!
//! Backdrop: set `TRACKSIDE_PREVIEW_BG` to a raw image file — 8-byte header
//! (`u32 width, u32 height`, little-endian) then RGBA8 pixels — and the host
//! paints it under the overlay every frame, letterboxed to the window, so a
//! panel can be judged over a real game screen. `Capture-Trackside.ps1
//! -Background <png|jpg>` writes that file from an ordinary screenshot.

#![windows_subsystem = "console"]

use core::mem::size_of;

use windows::core::{w, PCWSTR, Result};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
    ID3D11Texture2D, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGISwapChain, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, LoadCursorW, PeekMessageW, PostQuitMessage,
    RegisterClassExW, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HMENU, IDC_ARROW,
    MSG, PM_REMOVE, WM_DESTROY, WM_QUIT, WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    WINDOW_EX_STYLE,
};

/// The clear colour, also the letterbox bars behind a backdrop: dim plum, so panels read clearly.
const CLEAR: [f32; 4] = [0.04, 0.03, 0.07, 1.0];

/// Read `TRACKSIDE_PREVIEW_BG` (header + RGBA8) and resample it into a `w`×`h` RGBA8 frame,
/// letterboxed on the clear colour. Bilinear is plenty for a backdrop.
fn load_backdrop(w: u32, h: u32) -> Option<Vec<u8>> {
    let path = std::env::var_os("TRACKSIDE_PREVIEW_BG")?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("backdrop: cannot read {}: {e}", path.to_string_lossy());
            return None;
        }
    };
    if bytes.len() < 8 {
        eprintln!("backdrop: file too short");
        return None;
    }
    let sw = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let sh = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let px = &bytes[8..];
    if sw == 0 || sh == 0 || px.len() < sw * sh * 4 {
        eprintln!("backdrop: header says {sw}x{sh} but the file holds {} bytes of pixels", px.len());
        return None;
    }
    let (w, h) = (w as usize, h as usize);
    // Fit the image inside the window, keeping its aspect; bars take the clear colour.
    let scale = (w as f32 / sw as f32).min(h as f32 / sh as f32);
    let dw = ((sw as f32 * scale).round() as usize).max(1);
    let dh = ((sh as f32 * scale).round() as usize).max(1);
    let ox = (w - dw) / 2;
    let oy = (h - dh) / 2;
    let bar = [
        (CLEAR[0] * 255.0) as u8,
        (CLEAR[1] * 255.0) as u8,
        (CLEAR[2] * 255.0) as u8,
        255,
    ];
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 4;
            if x < ox || y < oy || x >= ox + dw || y >= oy + dh {
                out[o..o + 4].copy_from_slice(&bar);
                continue;
            }
            // Source coordinate of this destination pixel's centre.
            let fx = ((x - ox) as f32 + 0.5) / scale - 0.5;
            let fy = ((y - oy) as f32 + 0.5) / scale - 0.5;
            let x0 = fx.floor().clamp(0.0, (sw - 1) as f32) as usize;
            let y0 = fy.floor().clamp(0.0, (sh - 1) as f32) as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let y1 = (y0 + 1).min(sh - 1);
            let tx = (fx - x0 as f32).clamp(0.0, 1.0);
            let ty = (fy - y0 as f32).clamp(0.0, 1.0);
            for c in 0..3 {
                let p = |xx: usize, yy: usize| px[(yy * sw + xx) * 4 + c] as f32;
                let top = p(x0, y0) * (1.0 - tx) + p(x1, y0) * tx;
                let bot = p(x0, y1) * (1.0 - tx) + p(x1, y1) * tx;
                out[o + c] = (top * (1.0 - ty) + bot * ty).round() as u8;
            }
            out[o + 3] = 255;
        }
    }
    println!("Backdrop: {} ({sw}x{sh} -> {dw}x{dh} in {w}x{h})", path.to_string_lossy());
    Some(out)
}

fn main() -> Result<()> {
    let dll = std::env::args().nth(1).unwrap_or_else(|| "trackside.dll".to_string());

    unsafe {
        let hmodule = GetModuleHandleW(None)?;
        let hinstance = HINSTANCE(hmodule.0);
        let class_name = w!("TracksidePreviewHost");

        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            eprintln!("RegisterClassExW failed");
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("Trackside Overlay Preview  -  press Insert for the menu"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            800,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        );
        if hwnd.0 == 0 {
            eprintln!("CreateWindowExW failed");
            return Ok(());
        }

        // --- D3D11 device + swapchain on the window --------------------------
        let sd = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: 0,
                Height: 0,
                RefreshRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            ..Default::default()
        };

        let mut swapchain: Option<IDXGISwapChain> = None;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&sd),
            Some(&mut swapchain),
            Some(&mut device),
            Some(&mut D3D_FEATURE_LEVEL::default()),
            Some(&mut context),
        )?;
        let swapchain = swapchain.expect("no swapchain");
        let device = device.expect("no device");
        let context = context.expect("no context");

        let back: ID3D11Texture2D = swapchain.GetBuffer(0)?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
        let rtv = rtv.expect("no rtv");

        // --- optional backdrop: a same-size, same-format texture copied under every frame ---
        // No shaders: CopyResource needs identical size and format, and the back buffer's desc
        // gives both, so the image is resampled on the CPU once and blitted with one call.
        let mut bb_desc = D3D11_TEXTURE2D_DESC::default();
        back.GetDesc(&mut bb_desc);
        let backdrop: Option<ID3D11Texture2D> = load_backdrop(bb_desc.Width, bb_desc.Height).and_then(|pixels| {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: bb_desc.Width,
                Height: bb_desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: bb_desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: 0,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: pixels.as_ptr() as *const _,
                SysMemPitch: bb_desc.Width * 4,
                SysMemSlicePitch: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            match device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) {
                Ok(_) => tex,
                Err(e) => {
                    eprintln!("backdrop: CreateTexture2D failed: {e}");
                    None
                }
            }
        });

        // --- load the overlay into this process -----------------------------
        // hudhook's DllMain hooks Present/WndProc on attach, so from here the
        // overlay draws over our frames and receives our input.
        let wide: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        match LoadLibraryW(PCWSTR(wide.as_ptr())) {
            Ok(_) => println!("Loaded overlay: {dll}"),
            Err(e) => eprintln!("LoadLibrary('{dll}') failed: {e}  (build it first)"),
        }
        println!("Preview running. Close the window to quit; press Insert for the menu.");

        // --- present loop ---------------------------------------------------
        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            match &backdrop {
                Some(tex) => context.CopyResource(&back, tex),
                None => context.ClearRenderTargetView(&rtv, &CLEAR),
            }
            let _ = swapchain.Present(1, 0);
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
