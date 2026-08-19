//! FTS5 tokenizer FFI glue. Bridges the C callback contract to the pure-Rust
//! [`crate::segment::Engine`]. The engine (jieba dict + stemmer) is built once
//! in `x_create` and reused for every `x_tokenize`.

use crate::ffi::*;
use crate::segment::{Engine, Reason};
use libc::{c_char, c_int, c_uchar, c_void};

// FTS5 tokenize flags (fts5.h). We only need to tell QUERY from DOCUMENT.
const FTS5_TOKENIZE_QUERY: c_int = 0x0001;
// Emitted back to FTS5 to mark a token at the same position as the previous one.
const FTS5_TOKEN_COLOCATED: c_int = 0x0001;

/// `xCreate`: parse args, build the engine, hand FTS5 an owning raw pointer.
///
/// Args come from `tokenize='jieba <arg>...'`. Recognized:
///   - `stem`   (default) enable English Porter stemming
///   - `nostem` disable stemming (Chinese-only + English folding)
pub extern "C" fn x_create(
    _p_context: *mut c_void,
    az_arg: *const *const c_uchar,
    n_arg: c_int,
    out: *mut *mut Fts5Tokenizer,
) -> c_int {
    let stem = match parse_stem_arg(az_arg, n_arg) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let tokenizer = Box::new(Fts5Tokenizer {
        engine: Engine::new(stem),
    });
    unsafe { *out = Box::into_raw(tokenizer) };
    SQLITE_OK
}

/// Read the nul-terminated tokenizer args; returns whether stemming is on.
fn parse_stem_arg(az_arg: *const *const c_uchar, n_arg: c_int) -> Result<bool, c_int> {
    let mut stem = true;
    if az_arg.is_null() {
        return Ok(stem);
    }
    let args = unsafe { core::slice::from_raw_parts(az_arg, n_arg as usize) };
    for &p in args {
        if p.is_null() {
            continue;
        }
        let s = unsafe { core::ffi::CStr::from_ptr(p as *const c_char) };
        match s.to_bytes() {
            b"stem" => stem = true,
            b"nostem" => stem = false,
            // Unknown arg: reject so misconfiguration is visible, not silent.
            _ => return Err(SQLITE_MISUSE),
        }
    }
    Ok(stem)
}

/// `xDelete`: reclaim the box `x_create` leaked.
pub extern "C" fn x_delete(tokenizer: *mut Fts5Tokenizer) {
    if !tokenizer.is_null() {
        drop(unsafe { Box::from_raw(tokenizer) });
    }
}

/// `xTokenize`: bridge C -> engine. Panics are caught at this boundary so a bug
/// can't unwind into C.
pub extern "C" fn x_tokenize(
    tokenizer: *mut Fts5Tokenizer,
    p_ctx: *mut c_void,
    flags: c_int,
    p_text: *const c_char,
    n_text: c_int,
    x_token: TokenFunction,
) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match tokenize_internal(tokenizer, p_ctx, flags, p_text, n_text, x_token) {
            Ok(()) => SQLITE_OK,
            Err(code) => code,
        }
    }))
    .unwrap_or(SQLITE_INTERNAL)
}

fn tokenize_internal(
    tokenizer: *mut Fts5Tokenizer,
    p_ctx: *mut c_void,
    flags: c_int,
    p_text: *const c_char,
    n_text: c_int,
    x_token: TokenFunction,
) -> Result<(), c_int> {
    if p_text.is_null() || n_text == 0 {
        return Ok(());
    }
    let engine = &unsafe { tokenizer.as_ref() }.ok_or(SQLITE_INTERNAL)?.engine;

    let slice = unsafe { core::slice::from_raw_parts(p_text as *const c_uchar, n_text as usize) };
    // Bad UTF-8 in a document must not make the DB inaccessible: swallow as OK.
    let input = core::str::from_utf8(slice).map_err(|_| SQLITE_OK)?;

    let reason = if flags & FTS5_TOKENIZE_QUERY != 0 {
        Reason::Query
    } else {
        Reason::Document
    };

    // Propagate a non-OK xToken return code out of the closure.
    let mut token_rc = SQLITE_OK;
    engine.segment(input, reason, |emit| {
        let tflags = if emit.colocated {
            FTS5_TOKEN_COLOCATED
        } else {
            0
        };
        let rc = x_token(
            p_ctx,
            tflags,
            emit.text.as_ptr() as *const c_char,
            emit.text.len() as c_int,
            emit.byte_start as c_int,
            emit.byte_end as c_int,
        );
        if rc != SQLITE_OK {
            token_rc = rc;
            return false; // abort segmentation
        }
        true
    });

    if token_rc != SQLITE_OK {
        return Err(token_rc);
    }
    Ok(())
}
