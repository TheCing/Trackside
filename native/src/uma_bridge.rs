//! uma_bridge — Heaven's native, in-process replacement for the CarrotBlender companion plugin.
//!
//! It feeds the game's decrypted responses (and the raw requests) to companion tools over UDP 17230,
//! byte-for-byte compatible with CarrotBlender, so those tools work with Heaven directly — no external
//! plugin (and no Hachimi) needed. It stays working after game updates that broke the standalone plugin
//! because we read the response at `Gallop.HttpHelper.DecompressResponse` (AFTER decrypt + lz4-decompress
//! = plain msgpack, resolved by name) and the request at `System.Security.Cryptography.CryptoStream.Write`
//! (the exact bytes fed into the AES stream), which is precisely where CarrotBlender captures them.
//!
//! Framing (identical to CarrotBlender): the plain response gets a 4-byte dummy header prepended, is
//! AES-256-CBC encrypted with a fixed key/iv, and sent as the response body first (type 0, or a type-4
//! multipart header + type-5 chunks when large), then the key (type 1) and iv (type 2). The request is
//! sent verbatim as the type-3 packet. All sending is on a worker thread so the game frame never blocks.

#![allow(dead_code)]

use core::ffi::c_void;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::time::Duration;

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use retour::RawDetour;

use crate::htt_il2cpp as h;

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;

const UL_ADDR: &str = "127.0.0.1:17230";
/// Matches CarrotBlender's default `max_partial_message_size`.
const PARTIAL: usize = 30000;
const CHUNK_DELAY_MS: u64 = 50;

/// The fixed AES-256-CBC key/iv CarrotBlender uses. We send them to the consumer with every response
/// so its existing decrypt step recovers the plain msgpack. Using CarrotBlender's exact key/iv keeps
/// Heaven a drop-in replacement.
const KEY: [u8; 32] = *b"CarrotBlender-Fixed-AES256-Key!!";
const IV: [u8; 16] = *b"CarrotBlenderIV0";

fn blog(msg: &str) {
    crate::tools::log(&format!("[uma_bridge] {msg}"));
}

static ENABLED: AtomicBool = AtomicBool::new(true);
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

// Set when an external SDK plugin (e.g. CarrotBlender) is loaded from heaven_plugins/. That plugin
// already feeds the same UDP channel, so the native feed steps aside to avoid a double-send that
// would corrupt the stream. Kept separate from the user toggle so it can't be re-enabled into a
// conflict; the request hook stays installed but every send short-circuits here.
static EXTERNAL_ACTIVE: AtomicBool = AtomicBool::new(false);
pub fn set_external_active(on: bool) {
    EXTERNAL_ACTIVE.store(on, Ordering::Relaxed);
}
/// The native feed is live only when the user enabled it AND no external plugin owns the channel.
fn active() -> bool {
    ENABLED.load(Ordering::Relaxed) && !EXTERNAL_ACTIVE.load(Ordering::Relaxed)
}

// Worker: 0 = response (prefix + encrypt + body/key/iv), 3 = request (verbatim type-3 packet).
static TX: OnceLock<Sender<(u8, Vec<u8>)>> = OnceLock::new();
fn tx() -> &'static Sender<(u8, Vec<u8>)> {
    TX.get_or_init(|| {
        let (tx, rx) = channel::<(u8, Vec<u8>)>();
        std::thread::spawn(move || {
            let sock = UdpSocket::bind("0.0.0.0:0").ok();
            blog(&format!("worker started, socket bound = {}", sock.is_some()));
            let mut n = 0u64;
            while let Ok((kind, data)) = rx.recv() {
                if let Some(ref s) = sock {
                    let ok = if kind == 3 { send_request_frame(s, &data) } else { forward(s, &data) };
                    n += 1;
                    if n <= 6 || n % 25 == 0 {
                        blog(&format!("sent #{n}: kind={kind} len={} ok={ok}", data.len()));
                    }
                }
            }
        });
        tx
    })
}

/// Feed a plain (decrypted + decompressed) msgpack response. Cheap + non-blocking.
pub fn send_response(plain: &[u8]) {
    if !active() || plain.is_empty() || plain.len() > 50 * 1024 * 1024 {
        return;
    }
    let _ = tx().send((0, plain.to_vec()));
}

/// Feed the raw request bytes (the buffer written into the game's AES CryptoStream). Non-blocking.
pub fn send_request(plain: &[u8]) {
    if !active() || plain.is_empty() || plain.len() > 65535 {
        return;
    }
    let _ = tx().send((3, plain.to_vec()));
}

/// type-3 request packet: `[3, len_hi, len_lo, ...plain]` — sent verbatim (CarrotBlender-identical).
fn send_request_frame(sock: &UdpSocket, plain: &[u8]) -> bool {
    let mut p = Vec::with_capacity(3 + plain.len());
    p.push(3);
    p.push((plain.len() / 256) as u8);
    p.push((plain.len() % 256) as u8);
    p.extend_from_slice(plain);
    sock.send_to(&p, UL_ADDR).is_ok()
}

/// Prepend the 4-byte dummy header, AES-encrypt, then send body first, then key (type 1), then iv
/// (type 2) — the exact order and framing CarrotBlender uses. Returns whether the sends succeeded.
fn forward(sock: &UdpSocket, plain: &[u8]) -> bool {
    // The consumer strips a 4-byte header after decrypting, so the plain msgpack must be padded with
    // one before encryption — otherwise it eats 4 bytes of real data (the "extra data" unpack error).
    let mut combined = Vec::with_capacity(4 + plain.len());
    combined.extend_from_slice(&[0, 0, 0, 0]);
    combined.extend_from_slice(plain);
    let ct = Aes256CbcEnc::new(&KEY.into(), &IV.into()).encrypt_padded_vec_mut::<Pkcs7>(&combined);
    let mut ok = true;
    let mut p = Vec::new();

    // response body FIRST
    if ct.len() > PARTIAL {
        let num = ct.len() / PARTIAL + 1;
        ok &= sock.send_to(&[4, num as u8], UL_ADDR).is_ok();
        for chunk in split(&ct, num) {
            p.clear();
            p.push(5);
            p.push((chunk.len() / 256) as u8);
            p.push((chunk.len() % 256) as u8);
            p.extend_from_slice(chunk);
            ok &= sock.send_to(&p, UL_ADDR).is_ok();
            std::thread::sleep(Duration::from_millis(CHUNK_DELAY_MS));
        }
    } else {
        p.push(0);
        p.push((ct.len() / 256) as u8);
        p.push((ct.len() % 256) as u8);
        p.extend_from_slice(&ct);
        ok &= sock.send_to(&p, UL_ADDR).is_ok();
    }

    // then key / iv (fixed, sent after the body so the consumer can decrypt it)
    p.clear();
    p.extend_from_slice(&[1, 0, 32]);
    p.extend_from_slice(&KEY);
    ok &= sock.send_to(&p, UL_ADDR).is_ok();

    p.clear();
    p.extend_from_slice(&[2, 0, 16]);
    p.extend_from_slice(&IV);
    ok &= sock.send_to(&p, UL_ADDR).is_ok();

    ok
}

fn split(slice: &[u8], n: usize) -> Vec<&[u8]> {
    if n == 0 {
        return vec![slice];
    }
    let base = slice.len() / n;
    let rem = slice.len() % n;
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    for k in 0..n {
        let sz = base + if k < rem { 1 } else { 0 };
        out.push(&slice[i..i + sz]);
        i += sz;
    }
    out
}

// ── request capture: hook CryptoStream.Write and forward the buffer being encrypted (the raw
//    request bytes), exactly where CarrotBlender captures it (so the type-3 payload is identical). ──
static REQ_INSTALLED: AtomicBool = AtomicBool::new(false);
static WRITE_ORIG: AtomicUsize = AtomicUsize::new(0);
static WRITE_DETOUR: OnceLock<RawDetour> = OnceLock::new();

// void CryptoStream.Write(this, byte[] buffer, int offset, int count)  [+ trailing MethodInfo*]
type WriteFn = unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32, *const c_void);

unsafe extern "C" fn on_write(
    this: *mut c_void,
    buffer: *mut c_void,
    offset: i32,
    count: i32,
    method: *const c_void,
) {
    // Forward the whole buffer (CarrotBlender ignores offset/count too). Capturing here means the
    // type-3 payload includes the game's 4-byte length header — the same bytes CarrotBlender sends.
    if !buffer.is_null() {
        let len = h::array_len(buffer as *mut h::RawObject);
        if len > 0 && len <= 65535 {
            let data = (buffer as *mut u8).add(0x20);
            let slice = std::slice::from_raw_parts(data, len);
            send_request(slice);
            // Independent/Idle Training Career setups (carry `training_policy_param_rate_set_id`).
            // Cheap content check; only writes on an actual idle start (rare).
            crate::jp_idle::note_request(slice);
        }
    }
    let t = WRITE_ORIG.load(Ordering::Relaxed);
    if t != 0 {
        let f: WriteFn = std::mem::transmute(t);
        f(this, buffer, offset, count, method);
    }
}

/// Install the request-capture hook (CryptoStream.Write). Response capture is fed from the existing
/// DecompressResponse hook in response_hook. Run on an IL2CPP-attached thread (boot).
pub fn install() {
    if REQ_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let klass = crate::il2cpp::class("System.Security.Cryptography.CryptoStream");
        if klass.is_null() {
            blog("CryptoStream class not found");
            return;
        }
        let method = crate::il2cpp::method(klass, "Write", 3);
        if method.is_null() {
            blog("CryptoStream.Write method not found");
            return;
        }
        let fnptr = crate::il2cpp::method_pointer(method);
        if fnptr.is_null() || crate::il2cpp::is_detoured(fnptr) {
            blog("CryptoStream.Write not hookable");
            return;
        }
        if let Ok(d) = RawDetour::new(fnptr as *const (), on_write as *const ()) {
            if d.enable().is_ok() {
                WRITE_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = WRITE_DETOUR.set(d);
                blog("request capture (CryptoStream.Write) hooked");
            }
        }
    }
}
