//! clipboard — wire ImGui's copy/paste to the real Windows clipboard, so Ctrl+C / Ctrl+V / Ctrl+X
//! work in every overlay text field. hudhook doesn't set a clipboard backend by default, so without
//! this, paste is a no-op. Registered once in `overlay::initialize` via `ctx.set_clipboard_backend`.

use core::ffi::c_void;

use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

const CF_UNICODETEXT: u32 = 13;

pub struct WinClipboard;

impl hudhook::imgui::ClipboardBackend for WinClipboard {
    fn get(&mut self) -> Option<String> {
        // Debounce: ImGui triggers paste on the Ctrl+V key auto-repeat, so HOLDING it pastes the
        // text many times in a fraction of a second. Suppress repeats within 250 ms → one paste per
        // press. (Intentional repeated pastes >250 ms apart still work.)
        {
            use std::sync::Mutex;
            use std::time::Instant;
            use std::sync::OnceLock;
            static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
            let m = LAST.get_or_init(|| Mutex::new(None));
            if let Ok(mut g) = m.lock() {
                let now = Instant::now();
                if let Some(prev) = *g {
                    if now.duration_since(prev).as_millis() < 250 {
                        return None;
                    }
                }
                *g = Some(now);
            }
        }
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let h = GetClipboardData(CF_UNICODETEXT);
            if h.is_null() {
                CloseClipboard();
                return None;
            }
            let p = GlobalLock(h) as *const u16;
            if p.is_null() {
                CloseClipboard();
                return None;
            }
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            GlobalUnlock(h);
            CloseClipboard();
            Some(s)
        }
    }

    fn set(&mut self, value: &str) {
        unsafe {
            let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = wide.len() * 2;
            let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if hmem.is_null() {
                return;
            }
            let dst = GlobalLock(hmem) as *mut u16;
            if dst.is_null() {
                return;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            GlobalUnlock(hmem);
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                EmptyClipboard();
                // ownership of hmem transfers to the clipboard on success
                if SetClipboardData(CF_UNICODETEXT, hmem as *mut c_void).is_null() {
                    // failed — we still own hmem, but leaking one small block is acceptable here
                }
                CloseClipboard();
            }
        }
    }
}
