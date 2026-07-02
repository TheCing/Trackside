//! uma_bridge — feeds decrypted game responses (and plain requests) to companion overlays over
//! UDP 17229, so tools that would otherwise need a separate capture plugin work with Heaven directly.
//!
//! The older external plugin captured the response BEFORE decryption (`Convert.FromBase64String`) and
//! forwarded the ciphertext + the game's AES key/iv; the overlay decrypted. That broke on Global's
//! 2026-07-01 update because Global now lz4-compresses responses (like JP) and the overlay doesn't
//! decompress after decrypting → garbage. We sidestep it: Heaven captures the response at
//! `HttpHelper.DecompressResponse` (AFTER decrypt + lz4-decompress = plain msgpack, resolved by name
//! so it's update-proof), re-encrypts with our OWN AES-256-CBC key/iv, and speaks the same UDP framing
//! the overlays already expect. We also forward the plain request (from `HttpHelper.CompressRequest`)
//! as the type-3 packet, which is what makes the overlay consider itself "wired". All sending is on a
//! worker thread so the game frame never blocks.

#![allow(dead_code)]

use core::ffi::c_void;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::time::Duration;

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use retour::RawDetour;

use crate::htt_il2cpp as h;

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;

const UL_ADDR: &str = "127.0.0.1:17229";
/// Matches the overlay protocol's default `max_partial_message_size`.
const PARTIAL: usize = 30000;
const CHUNK_DELAY_MS: u64 = 50;

/// Our own AES-256-CBC key/iv, sent to the overlay with every response so it decrypts with them.
const KEY: [u8; 32] = *b"HeavenUmaBridge-AES256-Key--v1!!";
const IV: [u8; 16] = *b"HeavenBridgeIV01";

fn blog(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "[uma_bridge] {msg}");
    }
}

static ENABLED: AtomicBool = AtomicBool::new(true);
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

// Worker: 0 = response (encrypt + key/iv/response), 3 = request (plain type-3 packet).
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
    if !is_enabled() || plain.is_empty() || plain.len() > 50 * 1024 * 1024 {
        return;
    }
    let _ = tx().send((0, plain.to_vec()));
}

/// Feed the compressed request (CompressRequest's output = the buffer that goes into CryptoStream).
/// The overlay expects exactly these bytes as the type-3 packet; it's what "wires" it.
pub fn send_request(plain: &[u8]) {
    if !is_enabled() || plain.is_empty() || plain.len() > 65535 {
        return;
    }
    let _ = tx().send((3, plain.to_vec()));
}

/// type-3 request packet: `[3, len_hi, len_lo, ...plain]` (single, uncompressed/unencrypted).
fn send_request_frame(sock: &UdpSocket, plain: &[u8]) -> bool {
    let mut p = Vec::with_capacity(3 + plain.len());
    p.push(3);
    p.push((plain.len() / 256) as u8);
    p.push((plain.len() % 256) as u8);
    p.extend_from_slice(plain);
    sock.send_to(&p, UL_ADDR).is_ok()
}

/// AES-encrypt then send key/iv/response in the overlay's framing. Returns whether the sends succeeded.
fn forward(sock: &UdpSocket, plain: &[u8]) -> bool {
    let ct = Aes256CbcEnc::new(&KEY.into(), &IV.into()).encrypt_padded_vec_mut::<Pkcs7>(plain);
    let mut ok = true;

    let mut p = Vec::with_capacity(3 + 32);
    p.extend_from_slice(&[1, 0, 32]);
    p.extend_from_slice(&KEY);
    ok &= sock.send_to(&p, UL_ADDR).is_ok();

    p.clear();
    p.extend_from_slice(&[2, 0, 16]);
    p.extend_from_slice(&IV);
    ok &= sock.send_to(&p, UL_ADDR).is_ok();

    if ct.len() <= PARTIAL {
        p.clear();
        p.push(0);
        p.push((ct.len() / 256) as u8);
        p.push((ct.len() % 256) as u8);
        p.extend_from_slice(&ct);
        ok &= sock.send_to(&p, UL_ADDR).is_ok();
    } else {
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
    }
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

// ── request capture: hook HttpHelper.CompressRequest and forward its INPUT (plain request) ──
static REQ_INSTALLED: AtomicBool = AtomicBool::new(false);
static REQ_ORIG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static REQ_DETOUR: OnceLock<RawDetour> = OnceLock::new();

// static byte[] CompressRequest(byte[] requestData) → (arr, MethodInfo*) -> byte[]
type CompressFn = unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void;

unsafe extern "C" fn on_compress_request(arr: *mut c_void, method: *const c_void) -> *mut c_void {
    // Run the original first and capture its RETURN (the COMPRESSED request). This is byte-for-byte
    // what the external plugin forwards from CryptoStream.Write (the buffer fed into AES = CompressRequest's
    // output). Capturing the input instead would send uncompressed msgpack, which the overlay can't
    // decompress → it never wires. Return the original result unchanged.
    let t = REQ_ORIG.load(Ordering::Relaxed);
    let ret = if t != 0 {
        let f: CompressFn = std::mem::transmute(t);
        f(arr, method)
    } else {
        std::ptr::null_mut()
    };
    if !ret.is_null() {
        let len = h::array_len(ret as *mut h::RawObject);
        if len > 0 && len <= 65535 {
            let data = (ret as *mut u8).add(0x20);
            let slice = std::slice::from_raw_parts(data, len);
            send_request(slice);
        }
    }
    ret
}

/// Install the request-capture hook (CompressRequest). Response capture is fed from the existing
/// DecompressResponse hook in the full build/race_net. Run on an IL2CPP-attached thread (boot).
pub fn install() {
    if REQ_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        if !h::init() {
            return;
        }
        let image = crate::htt::find_game_image();
        if image.is_null() {
            return;
        }
        let ns = std::ffi::CString::new("Gallop").unwrap();
        let cn = std::ffi::CString::new("HttpHelper").unwrap();
        let klass = match h::CLASS_FROM_NAME {
            Some(f) => f(image, ns.as_ptr(), cn.as_ptr()),
            None => return,
        };
        if klass.is_null() {
            return;
        }
        let mname = std::ffi::CString::new("CompressRequest").unwrap();
        let method = match h::CLASS_GET_METHOD_FROM_NAME {
            Some(f) => f(klass, mname.as_ptr(), 1),
            None => return,
        };
        if method.is_null() {
            blog("CompressRequest method not found");
            return;
        }
        let fnptr = h::method_addr(method);
        if fnptr == 0 || crate::il2cpp::is_detoured(fnptr as *const c_void) {
            blog("CompressRequest not hookable");
            return;
        }
        if let Ok(d) = RawDetour::new(fnptr as *const (), on_compress_request as *const ()) {
            if d.enable().is_ok() {
                REQ_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = REQ_DETOUR.set(d);
                blog("request capture (CompressRequest) hooked");
            }
        }
    }
}
