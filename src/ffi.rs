//! C ABI surface — everything declared in include/settingskit.h.
//! Conventions (house standard):
//! - `*mut c_char` results are caller-owned; free with `sk_free_string`.
//! - Failures set a thread-local last-error (code + message) and
//!   return the documented sentinel (an SK_ERR_* code, NULL, or -1).
//! - Nothing unwinds across the FFI boundary (catch_unwind
//!   everywhere; release profile also sets panic = "abort").
//! - A `SkDoc*` handle is used from one thread at a time (the app
//!   thread that opened it) — same rule as the apps' own settings.

// The exported functions ARE the raw-pointer C ABI declared in
// include/settingskit.h; pointer validity is the documented
// contract there.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::catch_unwind;
use std::path::PathBuf;

use crate::{Doc, SkResult, ERR_INTERNAL, ERR_INVALID_ARG, SK_OK};

/// Open flag: keep the previous file as `<name>.bak` on save.
pub const SAVE_BACKUP: u32 = 0x1;

/// Migration step callback: mutate the doc through this same API,
/// return SK_OK or an SK_ERR_* code.
pub type SkMigrateFn = extern "C" fn(*mut SkDoc, *mut c_void) -> c_int;

/// One registered migration step (layout mirrors the header).
#[repr(C)]
#[derive(Clone)]
pub struct SkMigration {
    pub from_version: i64,
    pub migrate: Option<SkMigrateFn>,
    pub user: *mut c_void,
}

/// The opaque document handle (SkDoc in the header).
pub struct SkDoc {
    pub doc: Doc,
    pub path: Option<PathBuf>,
    pub current_schema: i64,
    pub migrations: Vec<SkMigration>,
    pub backup: bool,
}

thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> =
        RefCell::new((0, CString::new("").unwrap()));
}

pub fn set_error(code: i32, msg: &str) {
    let clean: String = msg.chars().filter(|&c| c != '\0').collect();
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = (
            code,
            CString::new(clean).unwrap_or_else(|_| CString::new("error").unwrap()),
        );
    });
}

fn clear_error() {
    set_error(SK_OK, "");
}

fn fail(code: i32, msg: &str) -> c_int {
    set_error(code, msg);
    code as c_int
}

fn panic_code() -> c_int {
    set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
    ERR_INTERNAL as c_int
}

fn guard<T, F: FnOnce() -> T>(f: F) -> std::thread::Result<T> {
    catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn cstr<'a>(ptr: *const c_char, what: &str) -> SkResult<&'a str> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, format!("{what} must not be null")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| (ERR_INVALID_ARG, format!("{what} must be valid UTF-8")))
}

fn handle<'a>(ptr: *mut SkDoc) -> SkResult<&'a mut SkDoc> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, "doc handle must not be null".to_string()));
    }
    Ok(unsafe { &mut *ptr })
}

fn owned_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => {
            set_error(
                ERR_INTERNAL,
                "value contains a NUL byte and cannot cross the C ABI",
            );
            std::ptr::null_mut()
        }
    }
}

/// Run `inner`, translating its result into the code+error-channel
/// convention. `T` is discarded (out-params carry the value).
fn code_of<T>(inner: SkResult<T>) -> c_int {
    match inner {
        Ok(_) => {
            clear_error();
            SK_OK as c_int
        }
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn sk_version() -> *const c_char {
    static VERSION_C: &[u8] = b"0.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn sk_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().1.as_ptr())
}

#[no_mangle]
pub extern "C" fn sk_last_error_code() -> c_int {
    LAST_ERROR.with(|e| e.borrow().0) as c_int
}

/// # Safety (caller contract): `p` is null or a string this library
/// returned and has not been freed yet. Documented in the header.
#[no_mangle]
pub extern "C" fn sk_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}

// ----------------------------------------------------------------
// Open / create / close / save
// ----------------------------------------------------------------

fn open_impl(
    path: &str,
    schema: i64,
    flags: u32,
    migrations: Vec<SkMigration>,
) -> SkResult<*mut SkDoc> {
    if schema < 1 {
        return Err((
            ERR_INVALID_ARG,
            "current schema version must be >= 1".to_string(),
        ));
    }
    for m in &migrations {
        if m.from_version < 1 {
            return Err((
                ERR_INVALID_ARG,
                "migration from_version must be >= 1".to_string(),
            ));
        }
        if m.migrate.is_none() {
            return Err((
                ERR_INVALID_ARG,
                "migration callback must not be null".to_string(),
            ));
        }
    }
    let path_buf = PathBuf::from(path);
    let text = crate::platform::read_file(&path_buf)?;
    let doc = match text {
        None => Doc::new(schema),
        Some(t) => Doc::from_json(&t)?,
    };
    let boxed = Box::new(SkDoc {
        doc,
        path: Some(path_buf),
        current_schema: schema,
        migrations,
        backup: flags & SAVE_BACKUP != 0,
    });
    let ptr = Box::into_raw(boxed);
    if let Err(e) = unsafe { crate::migrate(ptr, schema) } {
        unsafe { drop(Box::from_raw(ptr)) };
        return Err(e);
    }
    Ok(ptr)
}

#[no_mangle]
pub extern "C" fn sk_open_ex(
    path: *const c_char,
    current_schema: i64,
    flags: u32,
    migrations: *const SkMigration,
    migration_count: usize,
    out: *mut *mut SkDoc,
) -> c_int {
    match guard(|| {
        if out.is_null() {
            return fail(ERR_INVALID_ARG, "out must not be null");
        }
        unsafe { *out = std::ptr::null_mut() };
        let path = match cstr(path, "path") {
            Ok(p) => p,
            Err((c, m)) => return fail(c, &m),
        };
        let migs: Vec<SkMigration> = if migration_count == 0 {
            Vec::new()
        } else {
            if migrations.is_null() {
                return fail(
                    ERR_INVALID_ARG,
                    "migrations must not be null when count > 0",
                );
            }
            unsafe { std::slice::from_raw_parts(migrations, migration_count) }.to_vec()
        };
        match open_impl(path, current_schema, flags, migs) {
            Ok(ptr) => {
                unsafe { *out = ptr };
                clear_error();
                SK_OK as c_int
            }
            Err((c, m)) => fail(c, &m),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_open(
    path: *const c_char,
    current_schema: i64,
    flags: u32,
    out: *mut *mut SkDoc,
) -> c_int {
    sk_open_ex(path, current_schema, flags, std::ptr::null(), 0, out)
}

#[no_mangle]
pub extern "C" fn sk_create(current_schema: i64, flags: u32, out: *mut *mut SkDoc) -> c_int {
    match guard(|| {
        if out.is_null() {
            return fail(ERR_INVALID_ARG, "out must not be null");
        }
        unsafe { *out = std::ptr::null_mut() };
        if current_schema < 1 {
            return fail(ERR_INVALID_ARG, "current schema version must be >= 1");
        }
        let boxed = Box::new(SkDoc {
            doc: Doc::new(current_schema),
            path: None,
            current_schema,
            migrations: Vec::new(),
            backup: flags & SAVE_BACKUP != 0,
        });
        unsafe { *out = Box::into_raw(boxed) };
        clear_error();
        SK_OK as c_int
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_close(doc: *mut SkDoc) {
    if !doc.is_null() {
        let _ = guard(|| unsafe { drop(Box::from_raw(doc)) });
    }
}

fn save_impl(h: &mut SkDoc, path: &std::path::Path) -> SkResult<()> {
    let text = h.doc.to_json();
    crate::platform::atomic_write(path, text.as_bytes(), h.backup)?;
    h.doc.mark_clean();
    Ok(())
}

#[no_mangle]
pub extern "C" fn sk_save(doc: *mut SkDoc) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<()> {
            let h = handle(doc)?;
            let path = h.path.clone().ok_or((
                ERR_INVALID_ARG,
                "document has no path yet; use sk_save_as".to_string(),
            ))?;
            save_impl(h, &path)
        })();
        code_of(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_save_as(doc: *mut SkDoc, path: *const c_char) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<()> {
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            let path_buf = PathBuf::from(p);
            save_impl(h, &path_buf)?;
            h.path = Some(path_buf);
            Ok(())
        })();
        code_of(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_reload(doc: *mut SkDoc) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<()> {
            let h = handle(doc)?;
            let path = h.path.clone().ok_or((
                ERR_INVALID_ARG,
                "document has no path to reload from".to_string(),
            ))?;
            let text = crate::platform::read_file(&path)?;
            let fresh = match text {
                None => Doc::new(h.current_schema),
                Some(t) => Doc::from_json(&t)?,
            };
            // Build the replacement handle and migrate it BEFORE
            // swapping, so a failed reload leaves the live doc as-is.
            let boxed = Box::new(SkDoc {
                doc: fresh,
                path: Some(path),
                current_schema: h.current_schema,
                migrations: h.migrations.clone(),
                backup: h.backup,
            });
            let ptr = Box::into_raw(boxed);
            match unsafe { crate::migrate(ptr, h.current_schema) } {
                Ok(()) => {
                    let new = unsafe { *Box::from_raw(ptr) };
                    h.doc = new.doc;
                    Ok(())
                }
                Err(e) => {
                    unsafe { drop(Box::from_raw(ptr)) };
                    Err(e)
                }
            }
        })();
        code_of(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

// ----------------------------------------------------------------
// Introspection
// ----------------------------------------------------------------

#[no_mangle]
pub extern "C" fn sk_schema_version(doc: *mut SkDoc, out: *mut i64) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<()> {
            let h = handle(doc)?;
            if out.is_null() {
                return Err((ERR_INVALID_ARG, "out must not be null".to_string()));
            }
            let v = h.doc.schema()?;
            unsafe { *out = v };
            Ok(())
        })();
        code_of(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_is_dirty(doc: *mut SkDoc) -> c_int {
    match guard(|| match handle(doc) {
        Ok(h) => {
            clear_error();
            if h.doc.is_dirty() {
                1
            } else {
                0
            }
        }
        Err((c, m)) => {
            set_error(c, &m);
            -1
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_has(doc: *mut SkDoc, path: *const c_char) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<bool> {
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            h.doc.has(p)
        })();
        match inner {
            Ok(found) => {
                clear_error();
                if found {
                    1
                } else {
                    0
                }
            }
            Err((c, m)) => {
                set_error(c, &m);
                -1
            }
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

/// Serialize the whole document (pretty JSON). Caller-owned.
#[no_mangle]
pub extern "C" fn sk_to_json(doc: *mut SkDoc) -> *mut c_char {
    match guard(|| {
        let inner = (|| -> SkResult<String> {
            let h = handle(doc)?;
            Ok(h.doc.to_json())
        })();
        match inner {
            Ok(text) => {
                clear_error();
                owned_string(text.trim_end_matches('\n').to_string())
            }
            Err((c, m)) => {
                set_error(c, &m);
                std::ptr::null_mut()
            }
        }
    }) {
        Ok(p) => p,
        Err(_) => {
            panic_code();
            std::ptr::null_mut()
        }
    }
}

// ----------------------------------------------------------------
// Getters
// ----------------------------------------------------------------

fn set_found(found: *mut c_int, v: c_int) {
    if !found.is_null() {
        unsafe { *found = v };
    }
}

#[no_mangle]
pub extern "C" fn sk_get_i64(
    doc: *mut SkDoc,
    path: *const c_char,
    default_value: i64,
    out: *mut i64,
    found: *mut c_int,
) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<(i64, bool)> {
            let h = handle(doc)?;
            if out.is_null() {
                return Err((ERR_INVALID_ARG, "out must not be null".to_string()));
            }
            let p = cstr(path, "path")?;
            let present = h.doc.has(p)?;
            let v = h.doc.get_i64(p, default_value)?;
            Ok((v, present))
        })();
        match inner {
            Ok((v, present)) => {
                unsafe { *out = v };
                set_found(found, if present { 1 } else { 0 });
                clear_error();
                SK_OK as c_int
            }
            Err((c, m)) => {
                set_found(found, -1);
                fail(c, &m)
            }
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_get_f64(
    doc: *mut SkDoc,
    path: *const c_char,
    default_value: f64,
    out: *mut f64,
    found: *mut c_int,
) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<(f64, bool)> {
            let h = handle(doc)?;
            if out.is_null() {
                return Err((ERR_INVALID_ARG, "out must not be null".to_string()));
            }
            let p = cstr(path, "path")?;
            let present = h.doc.has(p)?;
            let v = h.doc.get_f64(p, default_value)?;
            Ok((v, present))
        })();
        match inner {
            Ok((v, present)) => {
                unsafe { *out = v };
                set_found(found, if present { 1 } else { 0 });
                clear_error();
                SK_OK as c_int
            }
            Err((c, m)) => {
                set_found(found, -1);
                fail(c, &m)
            }
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_get_bool(
    doc: *mut SkDoc,
    path: *const c_char,
    default_value: c_int,
    out: *mut c_int,
    found: *mut c_int,
) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<(bool, bool)> {
            let h = handle(doc)?;
            if out.is_null() {
                return Err((ERR_INVALID_ARG, "out must not be null".to_string()));
            }
            let p = cstr(path, "path")?;
            let present = h.doc.has(p)?;
            let v = h.doc.get_bool(p, default_value != 0)?;
            Ok((v, present))
        })();
        match inner {
            Ok((v, present)) => {
                unsafe { *out = if v { 1 } else { 0 } };
                set_found(found, if present { 1 } else { 0 });
                clear_error();
                SK_OK as c_int
            }
            Err((c, m)) => {
                set_found(found, -1);
                fail(c, &m)
            }
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

/// String getter. Returns a caller-owned copy of the value, or of
/// `default_value` when the key is absent (NULL default + absent key
/// returns NULL with found=0 — that means "no value", not error).
/// NULL return with found=-1 signals an error; check sk_last_error.
#[no_mangle]
pub extern "C" fn sk_get_string(
    doc: *mut SkDoc,
    path: *const c_char,
    default_value: *const c_char,
    found: *mut c_int,
) -> *mut c_char {
    match guard(|| {
        let inner = (|| -> SkResult<(Option<String>, bool)> {
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            let default = if default_value.is_null() {
                None
            } else {
                Some(cstr(default_value, "default_value")?)
            };
            let present = h.doc.has(p)?;
            if !present {
                return Ok((default.map(|d| d.to_string()), false));
            }
            let v = h.doc.get_string(p, "")?;
            Ok((Some(v), true))
        })();
        match inner {
            Ok((Some(s), present)) => {
                set_found(found, if present { 1 } else { 0 });
                clear_error();
                owned_string(s)
            }
            Ok((None, _)) => {
                set_found(found, 0);
                clear_error();
                std::ptr::null_mut()
            }
            Err((c, m)) => {
                set_found(found, -1);
                set_error(c, &m);
                std::ptr::null_mut()
            }
        }
    }) {
        Ok(p) => p,
        Err(_) => {
            panic_code();
            std::ptr::null_mut()
        }
    }
}

/// A value's compact JSON (covers arrays/objects, and any scalar).
/// NULL with found=0 = key absent; NULL with found=-1 = error.
#[no_mangle]
pub extern "C" fn sk_get_json(
    doc: *mut SkDoc,
    path: *const c_char,
    found: *mut c_int,
) -> *mut c_char {
    match guard(|| {
        let inner = (|| -> SkResult<Option<String>> {
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            match h.doc.get(p)? {
                None => Ok(None),
                Some(v) => Ok(Some(crate::json::to_string_compact(v))),
            }
        })();
        match inner {
            Ok(Some(s)) => {
                set_found(found, 1);
                clear_error();
                owned_string(s)
            }
            Ok(None) => {
                set_found(found, 0);
                clear_error();
                std::ptr::null_mut()
            }
            Err((c, m)) => {
                set_found(found, -1);
                set_error(c, &m);
                std::ptr::null_mut()
            }
        }
    }) {
        Ok(p) => p,
        Err(_) => {
            panic_code();
            std::ptr::null_mut()
        }
    }
}

// ----------------------------------------------------------------
// Setters / remove
// ----------------------------------------------------------------

#[no_mangle]
pub extern "C" fn sk_set_i64(doc: *mut SkDoc, path: *const c_char, value: i64) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<()> {
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            h.doc.set(p, crate::json::Value::I64(value))
        })();
        code_of(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_set_f64(doc: *mut SkDoc, path: *const c_char, value: f64) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<()> {
            if !value.is_finite() {
                return Err((
                    ERR_INVALID_ARG,
                    "value must be finite (JSON has no NaN/Infinity)".to_string(),
                ));
            }
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            h.doc.set(p, crate::json::Value::F64(value))
        })();
        code_of(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_set_bool(doc: *mut SkDoc, path: *const c_char, value: c_int) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<()> {
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            h.doc.set(p, crate::json::Value::Bool(value != 0))
        })();
        code_of(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn sk_set_string(
    doc: *mut SkDoc,
    path: *const c_char,
    value: *const c_char,
) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<()> {
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            let v = cstr(value, "value")?;
            h.doc.set(p, crate::json::Value::Str(v.to_string()))
        })();
        code_of(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

/// Set a value from its JSON text — the way to write arrays and
/// nested objects.
#[no_mangle]
pub extern "C" fn sk_set_json(
    doc: *mut SkDoc,
    path: *const c_char,
    json_text: *const c_char,
) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<()> {
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            let t = cstr(json_text, "json_text")?;
            let v = crate::json::parse(t)
                .map_err(|e| (crate::ERR_PARSE, format!("invalid JSON value: {e}")))?;
            h.doc.set(p, v)
        })();
        code_of(inner)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

/// Returns 1 when a key was removed, 0 when it was absent,
/// -1 on error (see sk_last_error).
#[no_mangle]
pub extern "C" fn sk_remove(doc: *mut SkDoc, path: *const c_char) -> c_int {
    match guard(|| {
        let inner = (|| -> SkResult<bool> {
            let h = handle(doc)?;
            let p = cstr(path, "path")?;
            h.doc.remove(p)
        })();
        match inner {
            Ok(removed) => {
                clear_error();
                if removed {
                    1
                } else {
                    0
                }
            }
            Err((c, m)) => {
                set_error(c, &m);
                -1
            }
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "settingskit-ffi-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    extern "C" fn step_rename(doc: *mut SkDoc, _user: *mut c_void) -> c_int {
        let mut out = String::new();
        let mut found: c_int = 0;
        let path = cs("old");
        let s = sk_get_string(doc, path.as_ptr(), std::ptr::null(), &mut found);
        if found != 1 {
            return 3;
        }
        let text = unsafe { CStr::from_ptr(s) }.to_str().unwrap().to_string();
        out.push_str(&text);
        sk_free_string(s);
        let newp = cs("new");
        let val = cs(&out);
        let rc = sk_set_string(doc, newp.as_ptr(), val.as_ptr());
        if rc != SK_OK as c_int {
            return rc;
        }
        // sk_remove reports 1 = removed; a migration must return
        // an SK_* code, so translate it.
        if sk_remove(doc, path.as_ptr()) < 0 {
            return crate::ERR_INTERNAL as c_int;
        }
        SK_OK as c_int
    }

    #[test]
    fn ffi_roundtrip_through_files() {
        let dir = temp_dir("roundtrip");
        let file = dir.join("app.json");
        let file_c = cs(file.to_str().unwrap());

        // Open fresh, set values, save.
        let mut h: *mut SkDoc = std::ptr::null_mut();
        assert_eq!(sk_open(file_c.as_ptr(), 2, 0, &mut h), SK_OK as c_int);
        assert!(!h.is_null());
        let preset = cs("downloads.preset");
        let v = cs("720p");
        assert_eq!(
            sk_set_string(h, preset.as_ptr(), v.as_ptr()),
            SK_OK as c_int
        );
        assert_eq!(sk_is_dirty(h), 1);
        assert_eq!(sk_save(h), SK_OK as c_int);
        assert_eq!(sk_is_dirty(h), 0);
        sk_close(h);

        // The file on disk: only what was set, plus $schema.
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"$schema\": 2"), "{text}");
        assert!(text.contains("\"preset\": \"720p\""), "{text}");
        assert!(!text.contains("theme"), "no defaults in file: {text}");

        // Reopen and read back.
        let mut h2: *mut SkDoc = std::ptr::null_mut();
        assert_eq!(sk_open(file_c.as_ptr(), 2, 0, &mut h2), SK_OK as c_int);
        let mut found: c_int = -9;
        let got = sk_get_string(h2, preset.as_ptr(), std::ptr::null(), &mut found);
        assert_eq!(found, 1);
        assert_eq!(unsafe { CStr::from_ptr(got) }.to_str().unwrap(), "720p");
        sk_free_string(got);
        // Missing key with a default returns the default, found=0.
        let theme = cs("ui.theme");
        let dflt = cs("dark");
        let got2 = sk_get_string(h2, theme.as_ptr(), dflt.as_ptr(), &mut found);
        assert_eq!(found, 0);
        assert_eq!(unsafe { CStr::from_ptr(got2) }.to_str().unwrap(), "dark");
        sk_free_string(got2);
        sk_close(h2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ffi_migration_via_open_ex() {
        let dir = temp_dir("migrate");
        let file = dir.join("old.json");
        std::fs::write(&file, "{\n  \"$schema\": 1,\n  \"old\": \"data\"\n}\n").unwrap();
        let file_c = cs(file.to_str().unwrap());
        let migs = [SkMigration {
            from_version: 1,
            migrate: Some(step_rename),
            user: std::ptr::null_mut(),
        }];
        let mut h: *mut SkDoc = std::ptr::null_mut();
        let rc = sk_open_ex(file_c.as_ptr(), 2, 0, migs.as_ptr(), 1, &mut h);
        assert_eq!(rc, SK_OK as c_int, "err: {}", unsafe {
            CStr::from_ptr(sk_last_error()).to_str().unwrap()
        });
        let mut ver: i64 = 0;
        assert_eq!(sk_schema_version(h, &mut ver), SK_OK as c_int);
        assert_eq!(ver, 2);
        let newp = cs("new");
        let mut found: c_int = 0;
        let got = sk_get_string(h, newp.as_ptr(), std::ptr::null(), &mut found);
        assert_eq!(found, 1);
        assert_eq!(unsafe { CStr::from_ptr(got) }.to_str().unwrap(), "data");
        sk_free_string(got);
        sk_close(h);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ffi_corrupt_and_newer_errors() {
        let dir = temp_dir("errors");
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        let bad_c = cs(bad.to_str().unwrap());
        let mut h: *mut SkDoc = std::ptr::null_mut();
        assert_eq!(
            sk_open(bad_c.as_ptr(), 1, 0, &mut h),
            crate::ERR_PARSE as c_int
        );
        assert!(h.is_null());
        // File untouched by the failed open.
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{not json");

        let newer = dir.join("newer.json");
        std::fs::write(&newer, "{\"$schema\": 7}").unwrap();
        let newer_c = cs(newer.to_str().unwrap());
        assert_eq!(
            sk_open(newer_c.as_ptr(), 2, 0, &mut h),
            crate::ERR_SCHEMA_NEWER as c_int
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
