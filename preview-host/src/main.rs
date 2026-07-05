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

#![windows_subsystem = "console"]

use core::mem::size_of;

use windows::core::{w, PCWSTR, Result};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
    ID3D11Texture2D, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
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
        let _device = device.expect("no device");
        let context = context.expect("no context");

        let back: ID3D11Texture2D = swapchain.GetBuffer(0)?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        _device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
        let rtv = rtv.expect("no rtv");

        // --- load the overlay into this process -----------------------------
        // hudhook's DllMain hooks Present/WndProc on attach, so from here the
        // overlay draws over our cleared frames and receives our input.
        let wide: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        match LoadLibraryW(PCWSTR(wide.as_ptr())) {
            Ok(_) => println!("Loaded overlay: {dll}"),
            Err(e) => eprintln!("LoadLibrary('{dll}') failed: {e}  (build it first)"),
        }
        println!("Preview running. Close the window to quit; press Insert for the menu.");

        // --- present loop ---------------------------------------------------
        let clear = [0.04f32, 0.03, 0.07, 1.0]; // dim plum, so the panel reads clearly
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
            context.ClearRenderTargetView(&rtv, &clear);
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
