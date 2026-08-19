//! fts5-jieba: a SQLite FTS5 loadable extension registering a custom tokenizer.
//!
//! M1 goal: prove the registration link. A loadable extension's entry point
//! gets a `*sqlite3_api_routines`; we run `SELECT fts5(?1)` + `bind_pointer`
//! to fetch the `fts5_api`, then `xCreateTokenizer("jieba", ...)`.
//!
//! Two entry-point symbols are exported so any host binding can load us:
//!   - `sqlite3_extension_init`     (default; bindings that load by filename)
//!   - `sqlite3_fts5jieba_init`     (explicit name; `SELECT load_extension(f,'sqlite3_fts5jieba_init')`)

mod ffi;
mod tokenizer;
pub use ffi::Fts5Api;

use core::{ffi::CStr, ptr::null_mut};
use ffi::*;
use libc::{c_int, c_uchar, c_void};

/// Tokenizer name used in `CREATE VIRTUAL TABLE ... tokenize='jieba'`.
const TOKENIZER_NAME: &CStr = c"jieba";

/// Destroy callback for the tokenizer module. We register no per-module state,
/// so this is a no-op.
extern "C" fn x_destroy_module(_module: *mut c_void) {}

/// Shared registration logic. `p_api` is the `*sqlite3_api_routines` handed to
/// the extension entry point.
fn register(db: *mut Sqlite3, p_api: *const c_void) -> Result<(), c_int> {
    let api = unsafe { (p_api as *const Sqlite3ApiRoutines).as_ref() }.ok_or(SQLITE_INTERNAL)?;

    // FTS5's fts5_api_ptr binding trick needs SQLite >= 3.20.0.
    if (api.libversion_number)() < 3_020_000 {
        return Err(SQLITE_MISUSE);
    }

    // Fetch the fts5_api pointer: prepare `SELECT fts5(?1)`, bind a pointer
    // parameter tagged "fts5_api_ptr", step once; SQLite writes the pointer.
    let mut stmt = null_mut::<Sqlite3Stmt>();
    let rc = (api.prepare)(
        db,
        c"SELECT fts5(?1)".as_ptr().cast(),
        -1,
        &mut stmt,
        null_mut(),
    );
    if rc != SQLITE_OK {
        return Err(rc);
    }

    let mut p_fts5_api = null_mut::<Fts5Api>();
    let rc = (api.bind_pointer)(
        stmt,
        1,
        &mut p_fts5_api,
        c"fts5_api_ptr".as_ptr().cast(),
        null_mut(),
    );
    if rc != SQLITE_OK {
        (api.finalize)(stmt);
        return Err(rc);
    }

    // step returns SQLITE_ROW; treat anything else as harmless and proceed to
    // finalize, which surfaces real errors.
    let _ = (api.step)(stmt);
    let rc = (api.finalize)(stmt);
    if rc != SQLITE_OK {
        return Err(rc);
    }

    let fts5_api = unsafe { p_fts5_api.as_ref() }.ok_or(SQLITE_INTERNAL)?;
    if fts5_api.i_version < FTS5_API_VERSION {
        return Err(SQLITE_MISUSE);
    }

    let mut tokenizer_api = Fts5TokenizerApi {
        x_create: tokenizer::x_create,
        x_delete: tokenizer::x_delete,
        x_tokenize: tokenizer::x_tokenize,
    };

    let rc = (fts5_api.x_create_tokenizer)(
        fts5_api,
        TOKENIZER_NAME.as_ptr().cast(),
        null_mut(),
        &mut tokenizer_api,
        x_destroy_module,
    );
    if rc != SQLITE_OK {
        return Err(rc);
    }

    Ok(())
}

/// Register the tokenizer with an in-process SQLite connection.
///
/// The standalone extension entry point above is used by external SQLite
/// hosts. The Rust TraceDB binary instead links the tokenizer as a normal Rust
/// library and obtains the FTS5 API pointer itself; this avoids a fragile
/// runtime search for a platform-specific `.dylib`/`.so` next to the binary.
/// The caller must pass a pointer obtained from `SELECT fts5(?1)` and keep the
/// SQLite connection alive for the lifetime of the registration.
///
/// # Safety
///
/// `fts5_api` must be a valid pointer owned by a live SQLite connection. The
/// pointed-to API and its callbacks must remain valid for every tokenizer use.
pub unsafe fn register_with_fts5_api(fts5_api: *mut Fts5Api) -> c_int {
    if fts5_api.is_null() {
        return SQLITE_INTERNAL;
    }
    let mut tokenizer_api = Fts5TokenizerApi {
        x_create: tokenizer::x_create,
        x_delete: tokenizer::x_delete,
        x_tokenize: tokenizer::x_tokenize,
    };
    ((*fts5_api).x_create_tokenizer)(
        fts5_api,
        TOKENIZER_NAME.as_ptr().cast(),
        null_mut(),
        &mut tokenizer_api,
        x_destroy_module,
    )
}

/// Default extension entry point (loaded by filename).
///
/// # Safety
/// Called by SQLite's extension loader with a valid `db` and api routines table.
#[no_mangle]
pub extern "C" fn sqlite3_extension_init(
    db: *mut Sqlite3,
    _pz_err_msg: *mut *mut c_uchar,
    p_api: *const c_void,
) -> c_int {
    std::panic::catch_unwind(|| match register(db, p_api) {
        Ok(()) => SQLITE_OK,
        Err(code) => code,
    })
    .unwrap_or(SQLITE_INTERNAL)
}

/// Named entry point: `load_extension('libfts5jieba', 'sqlite3_fts5jieba_init')`.
///
/// # Safety
/// Same contract as [`sqlite3_extension_init`].
#[no_mangle]
pub extern "C" fn sqlite3_fts5jieba_init(
    db: *mut Sqlite3,
    pz_err_msg: *mut *mut c_uchar,
    p_api: *const c_void,
) -> c_int {
    sqlite3_extension_init(db, pz_err_msg, p_api)
}

pub mod segment;
