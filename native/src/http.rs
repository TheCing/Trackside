//! Minimal HTTPS GET over WinHTTP — no third-party crates, native Windows TLS.
//! Used by the self-updater to hit the GitHub releases API and download the new DLL.
//! Blocking; always call from a background thread (the render loop must never block).

#![allow(dead_code)]

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};

const HTTPS_PORT: u16 = 443;
const UA: &str = "Trackside-Updater (github.com/TheCing)";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Parse `https://host/path` → (host, path). Only HTTPS is supported.
fn split_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://")?;
    let slash = rest.find('/').unwrap_or(rest.len());
    let host = rest[..slash].to_string();
    let path = if slash < rest.len() { rest[slash..].to_string() } else { "/".to_string() };
    Some((host, path))
}

/// RAII wrapper so every WinHTTP handle is closed on any early return.
struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// HTTPS GET → response body bytes. Follows HTTPS→HTTPS redirects (WinHTTP default,
/// which is how a GitHub release-download URL reaches objects.githubusercontent.com)
/// and sends a User-Agent (the GitHub API rejects requests without one). Err on any failure.
pub fn get(url: &str) -> Result<Vec<u8>, String> {
    let (host, path) = split_url(url).ok_or_else(|| "bad url (need https://)".to_string())?;
    let ua = wide(UA);
    let host_w = wide(&host);
    let path_w = wide(&path);
    let verb = wide("GET");

    unsafe {
        let session = Handle(WinHttpOpen(
            ua.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            ptr::null(),
            ptr::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("WinHttpOpen failed".into());
        }

        let connect = Handle(WinHttpConnect(session.0, host_w.as_ptr(), HTTPS_PORT, 0));
        if connect.0.is_null() {
            return Err("WinHttpConnect failed".into());
        }

        let request = Handle(WinHttpOpenRequest(
            connect.0,
            verb.as_ptr(),
            path_w.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err("WinHttpOpenRequest failed".into());
        }

        if WinHttpSendRequest(request.0, ptr::null(), 0, ptr::null(), 0, 0, 0) == 0 {
            return Err("WinHttpSendRequest failed (no network?)".into());
        }
        if WinHttpReceiveResponse(request.0, ptr::null_mut()) == 0 {
            return Err("WinHttpReceiveResponse failed".into());
        }

        // HTTP status code (as a number).
        let mut status: u32 = 0;
        let mut len: u32 = 4;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            ptr::null(),
            &mut status as *mut u32 as *mut c_void,
            &mut len,
            ptr::null_mut(),
        );
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }

        // Read the whole body.
        let mut out = Vec::new();
        loop {
            let mut avail: u32 = 0;
            if WinHttpQueryDataAvailable(request.0, &mut avail) == 0 {
                return Err("WinHttpQueryDataAvailable failed".into());
            }
            if avail == 0 {
                break;
            }
            let mut buf = vec![0u8; avail as usize];
            let mut read: u32 = 0;
            if WinHttpReadData(request.0, buf.as_mut_ptr() as *mut c_void, avail, &mut read) == 0 {
                return Err("WinHttpReadData failed".into());
            }
            if read == 0 {
                break;
            }
            buf.truncate(read as usize);
            out.extend_from_slice(&buf);
        }
        Ok(out)
    }
}

/// HTTPS GET → UTF-8 string (for JSON APIs).
pub fn get_string(url: &str) -> Result<String, String> {
    let bytes = get(url)?;
    String::from_utf8(bytes).map_err(|_| "response not valid utf-8".to_string())
}

/// HTTPS POST with a body (e.g. JSON) → response body bytes. Blocking; background thread only.
pub fn post(url: &str, body: &[u8], content_type: &str) -> Result<Vec<u8>, String> {
    let (host, path) = split_url(url).ok_or_else(|| "bad url (need https://)".to_string())?;
    let ua = wide(UA);
    let host_w = wide(&host);
    let path_w = wide(&path);
    let verb = wide("POST");
    let headers = wide(&format!("Content-Type: {content_type}\r\n"));

    unsafe {
        let session = Handle(WinHttpOpen(ua.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, ptr::null(), ptr::null(), 0));
        if session.0.is_null() {
            return Err("WinHttpOpen failed".into());
        }
        let connect = Handle(WinHttpConnect(session.0, host_w.as_ptr(), HTTPS_PORT, 0));
        if connect.0.is_null() {
            return Err("WinHttpConnect failed".into());
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0, verb.as_ptr(), path_w.as_ptr(), ptr::null(), ptr::null(), ptr::null(), WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err("WinHttpOpenRequest failed".into());
        }
        // Send with the Content-Type header + the body in one call.
        if WinHttpSendRequest(
            request.0,
            headers.as_ptr(),
            (headers.len() - 1) as u32, // wide chars, minus the NUL
            body.as_ptr() as *const c_void,
            body.len() as u32,
            body.len() as u32,
            0,
        ) == 0
        {
            return Err("WinHttpSendRequest(POST) failed (no network?)".into());
        }
        if WinHttpReceiveResponse(request.0, ptr::null_mut()) == 0 {
            return Err("WinHttpReceiveResponse failed".into());
        }
        let mut status: u32 = 0;
        let mut len: u32 = 4;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            ptr::null(),
            &mut status as *mut u32 as *mut c_void,
            &mut len,
            ptr::null_mut(),
        );
        // Read the whole body regardless of status (the Worker returns JSON with 200 or 403).
        let mut out = Vec::new();
        loop {
            let mut avail: u32 = 0;
            if WinHttpQueryDataAvailable(request.0, &mut avail) == 0 || avail == 0 {
                break;
            }
            let mut buf = vec![0u8; avail as usize];
            let mut read: u32 = 0;
            if WinHttpReadData(request.0, buf.as_mut_ptr() as *mut c_void, avail, &mut read) == 0 || read == 0 {
                break;
            }
            buf.truncate(read as usize);
            out.extend_from_slice(&buf);
        }
        Ok(out)
    }
}
