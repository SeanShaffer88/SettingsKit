//! SettingsKit — atomic settings files with schema versioning and
//! migrations, behind a small C ABI.
//!
//! The problem this solves (WinDevKit PAIN_POINTS #13): every app
//! hand-rolls settings load/save; defaults live in code AND get
//! serialized into the file, so flipping a default in code silently
//! changes what existing files mean — and golden-file tests break
//! for reasons nobody can see. SettingsKit's rules:
//!
//! - **Defaults are never written.** A key only reaches the file
//!   when the app explicitly sets it. Reading with a default is a
//!   pure lookup; it never materializes the key.
//! - **Files carry a schema version** (`"$schema"` at the root).
//!   Opening an older file runs the app's registered migration for
//!   each step, in order. Opening a newer file is an error, not a
//!   silent downgrade. A missing step is an error, not a skip.
//! - **Saves are atomic.** New content goes to a temp file in the
//!   same directory, is flushed to disk, then replaces the target
//!   in one rename (MoveFileEx with REPLACE_EXISTING on Windows) —
//!   a crash mid-save leaves the old file or the new one, never a
//!   torn half-file. Optionally the previous file is kept as
//!   `<name>.bak`.
//! - **A corrupt file is data, not a disaster to hide.** Loading
//!   fails with SK_ERR_PARSE and the file is left byte-for-byte
//!   untouched; the app decides whether to start fresh.
//!
//! Pure logic lives here and in `json`; platform access lives in
//! `platform`; the C ABI lives in `ffi`.

pub mod ffi;
pub mod json;
pub mod platform;

pub const VERSION: &str = "0.1.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
pub const VERSION_PATCH: u32 = 0;

// Error codes (mirror include/settingskit.h).
pub const SK_OK: i32 = 0;
pub const ERR_INVALID_ARG: i32 = 1;
pub const ERR_NOT_FOUND: i32 = 2;
pub const ERR_TYPE: i32 = 3;
pub const ERR_PARSE: i32 = 4;
pub const ERR_IO: i32 = 5;
pub const ERR_SCHEMA_NEWER: i32 = 6;
pub const ERR_NO_MIGRATION: i32 = 7;
pub const ERR_INTERNAL: i32 = 8;

/// Reserved root key holding the file's schema version (integer).
pub const SCHEMA_KEY: &str = "$schema";

/// Core error type: an SK_* code plus a human-readable message.
pub type SkResult<T> = Result<T, (i32, String)>;

fn err<T>(code: i32, msg: impl Into<String>) -> SkResult<T> {
    Err((code, msg.into()))
}

/// Split a dotted path ("downloads.preset") into segments.
/// Empty paths and empty segments are invalid.
pub fn split_path(path: &str) -> SkResult<Vec<&str>> {
    if path.is_empty() {
        return err(ERR_INVALID_ARG, "path must not be empty");
    }
    let parts: Vec<&str> = path.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return err(
            ERR_INVALID_ARG,
            format!("path '{path}' has an empty segment"),
        );
    }
    Ok(parts)
}

/// A settings document: a JSON object root. The file's schema
/// version lives inside the root under `$schema`.
#[derive(Clone, Debug)]
pub struct Doc {
    root: json::Value,
    dirty: bool,
}

impl Doc {
    /// A fresh document at `schema` — root holds only `$schema`.
    pub fn new(schema: i64) -> Doc {
        let mut root = json::Value::Object(Vec::new());
        root.set(SCHEMA_KEY, json::Value::I64(schema));
        Doc { root, dirty: false }
    }

    /// Parse a document from JSON text. The root must be an object.
    /// A missing `$schema` means version 1 (files written before
    /// versioning existed); a non-integer `$schema` is a parse error.
    pub fn from_json(text: &str) -> SkResult<Doc> {
        let value = json::parse(text).map_err(|e| (ERR_PARSE, format!("invalid JSON: {e}")))?;
        if !matches!(value, json::Value::Object(_)) {
            return err(ERR_PARSE, "settings root must be a JSON object");
        }
        let doc = Doc {
            root: value,
            dirty: false,
        };
        // Validate the schema key's shape up front.
        let _ = doc.schema()?;
        Ok(doc)
    }

    /// Pretty-printed JSON with a trailing newline, ready to write
    /// to a file.
    pub fn to_json(&self) -> String {
        let mut out = json::to_string_pretty(&self.root);
        out.push('\n');
        out
    }

    /// The document's schema version. Absent `$schema` reads as 1.
    pub fn schema(&self) -> SkResult<i64> {
        match self.root.get(SCHEMA_KEY) {
            None => Ok(1),
            Some(v) => v
                .as_i64()
                .ok_or((ERR_PARSE, format!("{SCHEMA_KEY} must be an integer"))),
        }
    }

    pub fn set_schema(&mut self, schema: i64) {
        self.root.set(SCHEMA_KEY, json::Value::I64(schema));
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clear the dirty flag (the FFI layer calls this after a
    /// successful save).
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// Look up a dotted path. `None` = absent (a missing or
    /// non-object intermediate reads as absent for getters).
    pub fn get(&self, path: &str) -> SkResult<Option<&json::Value>> {
        let parts = split_path(path)?;
        let mut cur = &self.root;
        for part in parts {
            match cur.get(part) {
                Some(v) => cur = v,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    pub fn has(&self, path: &str) -> SkResult<bool> {
        Ok(self.get(path)?.is_some())
    }

    /// Set a dotted path, creating intermediate objects as needed.
    /// Fails with ERR_TYPE if an intermediate exists and is not an
    /// object. Marks the document dirty only when the value changes.
    pub fn set(&mut self, path: &str, value: json::Value) -> SkResult<()> {
        if path == SCHEMA_KEY {
            return err(
                ERR_INVALID_ARG,
                format!("{SCHEMA_KEY} is managed by SettingsKit, not by direct set"),
            );
        }
        let parts = split_path(path)?;
        let (last, parents) = parts.split_last().unwrap();
        let mut cur = &mut self.root;
        for part in parents {
            // Inspect first (borrow ends), then navigate or create.
            let shape = cur
                .get(part)
                .map(|v| (matches!(v, json::Value::Object(_)), v.type_name()));
            match shape {
                Some((true, _)) => cur = cur.get_mut(part).unwrap(),
                Some((false, kind)) => {
                    return err(
                        ERR_TYPE,
                        format!("cannot set '{path}': '{part}' holds a {kind}, not an object"),
                    )
                }
                None => {
                    cur.set(part, json::Value::Object(Vec::new()));
                    cur = cur.get_mut(part).unwrap();
                }
            }
        }
        if cur.get(last) != Some(&value) {
            cur.set(last, value);
            self.dirty = true;
        }
        Ok(())
    }

    /// Remove a dotted path. Returns true when something was removed.
    pub fn remove(&mut self, path: &str) -> SkResult<bool> {
        let parts = split_path(path)?;
        let (last, parents) = parts.split_last().unwrap();
        let mut cur = &mut self.root;
        for part in parents {
            match cur.get_mut(part) {
                Some(v @ json::Value::Object(_)) => cur = v,
                _ => return Ok(false),
            }
        }
        let removed = cur.remove(last).is_some();
        if removed {
            self.dirty = true;
        }
        Ok(removed)
    }

    // ---- typed getters (default-on-missing conveniences) ----

    pub fn get_i64(&self, path: &str, default: i64) -> SkResult<i64> {
        match self.get(path)? {
            None => Ok(default),
            Some(v) => v.as_i64().ok_or((
                ERR_TYPE,
                format!("'{path}' holds a {}, not an integer", v.type_name()),
            )),
        }
    }

    pub fn get_f64(&self, path: &str, default: f64) -> SkResult<f64> {
        match self.get(path)? {
            None => Ok(default),
            Some(v) => v.as_f64().ok_or((
                ERR_TYPE,
                format!("'{path}' holds a {}, not a number", v.type_name()),
            )),
        }
    }

    pub fn get_bool(&self, path: &str, default: bool) -> SkResult<bool> {
        match self.get(path)? {
            None => Ok(default),
            Some(v) => v.as_bool().ok_or((
                ERR_TYPE,
                format!("'{path}' holds a {}, not a bool", v.type_name()),
            )),
        }
    }

    pub fn get_string(&self, path: &str, default: &str) -> SkResult<String> {
        match self.get(path)? {
            None => Ok(default.to_string()),
            Some(v) => v.as_str().map(|s| s.to_string()).ok_or((
                ERR_TYPE,
                format!("'{path}' holds a {}, not a string", v.type_name()),
            )),
        }
    }
}

/// Run the migration chain on the doc inside `handle` (the FFI
/// handle pointer, passed verbatim to each step so callbacks can
/// use the public sk_* API) up to `current`.
///
/// On success the doc's `$schema` is `current`. On any failure the
/// schema is unchanged — though a step may have partially run, so
/// callbacks should be written to be re-runnable. The file on disk
/// is untouched until the app saves.
///
/// # Safety
/// `handle` must be a live SkDoc handle (or null).
pub unsafe fn migrate(handle: *mut ffi::SkDoc, current: i64) -> SkResult<()> {
    let h = handle
        .as_mut()
        .ok_or((ERR_INVALID_ARG, "handle must not be null".to_string()))?;
    let from = h.doc.schema()?;
    if from > current {
        return err(
            ERR_SCHEMA_NEWER,
            format!(
                "file schema {from} is newer than this app's schema {current}; refusing to open"
            ),
        );
    }
    let mut v = from;
    while v < current {
        let step = h
            .migrations
            .iter()
            .find(|m| m.from_version == v && m.migrate.is_some())
            .ok_or((
                ERR_NO_MIGRATION,
                format!("no migration registered from schema {v} to {}", v + 1),
            ))?;
        let code = (step.migrate.unwrap())(handle, step.user);
        if code != SK_OK {
            return err(
                code,
                format!("migration from schema {v} failed with code {code}"),
            );
        }
        v += 1;
        h.doc.set_schema(v);
    }
    Ok(())
}

/// Escape a string for inclusion inside JSON double quotes.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    extern "C" fn rename_old_key(raw: *mut ffi::SkDoc, _user: *mut c_void) -> i32 {
        // Written the way a consumer writes it: through the handle.
        let doc = unsafe { &mut (*raw).doc };
        let old = match doc.get_string("old_name", "") {
            Ok(s) => s,
            Err((code, _)) => return code,
        };
        if doc.set("new_name", json::Value::Str(old)).is_err() {
            return ERR_INTERNAL;
        }
        match doc.remove("old_name") {
            Ok(_) => SK_OK,
            Err((code, _)) => code,
        }
    }

    extern "C" fn bump_counter(raw: *mut ffi::SkDoc, _user: *mut c_void) -> i32 {
        let doc = unsafe { &mut (*raw).doc };
        let n = match doc.get_i64("counter", 0) {
            Ok(n) => n,
            Err((code, _)) => return code,
        };
        match doc.set("counter", json::Value::I64(n + 1)) {
            Ok(()) => SK_OK,
            Err((code, _)) => code,
        }
    }

    fn mig(from: i64, f: ffi::SkMigrateFn) -> ffi::SkMigration {
        ffi::SkMigration {
            from_version: from,
            migrate: Some(f),
            user: std::ptr::null_mut(),
        }
    }

    #[test]
    fn dotted_paths() {
        let mut doc = Doc::new(1);
        doc.set("downloads.preset", json::Value::Str("720p".into()))
            .unwrap();
        doc.set("downloads.limit_kbps", json::Value::I64(0))
            .unwrap();
        assert_eq!(doc.get_string("downloads.preset", "best").unwrap(), "720p");
        assert!(doc.has("downloads").unwrap());
        assert!(doc.remove("downloads.preset").unwrap());
        assert!(!doc.has("downloads.preset").unwrap());
        assert!(!doc.remove("downloads.preset").unwrap());
    }

    #[test]
    fn defaults_are_never_written() {
        // THE golden-defaults regression test (InstallScope PR #11):
        // reading with a default must not materialize the key, so a
        // future default flip cannot change a saved file's meaning.
        let mut doc = Doc::new(1);
        assert_eq!(doc.get_i64("ui.width", 1280).unwrap(), 1280);
        assert_eq!(doc.get_string("ui.theme", "dark").unwrap(), "dark");
        let text = doc.to_json();
        assert!(!text.contains("width"), "default leaked into file: {text}");
        assert!(!text.contains("theme"), "default leaked into file: {text}");
        assert!(!doc.is_dirty());
        // Setting the same value twice: dirty only on real change.
        doc.set("ui.width", json::Value::I64(1024)).unwrap();
        assert!(doc.is_dirty());
    }

    #[test]
    fn type_errors_are_explicit() {
        let mut doc = Doc::new(1);
        doc.set("a", json::Value::Str("x".into())).unwrap();
        assert_eq!(doc.get_i64("a", 0).unwrap_err().0, ERR_TYPE);
        assert_eq!(doc.set("a.b", json::Value::I64(1)).unwrap_err().0, ERR_TYPE);
        assert_eq!(
            doc.set("", json::Value::Null).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            doc.set("a..b", json::Value::Null).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            doc.set(SCHEMA_KEY, json::Value::I64(9)).unwrap_err().0,
            ERR_INVALID_ARG
        );
    }

    #[test]
    fn schema_rules() {
        // Absent $schema reads as version 1.
        let doc = Doc::from_json(r#"{"a": 1}"#).unwrap();
        assert_eq!(doc.schema().unwrap(), 1);
        // Non-integer $schema is a parse error, never a guess.
        assert_eq!(
            Doc::from_json(r#"{"$schema": "two"}"#).unwrap_err().0,
            ERR_PARSE
        );
        // Non-object root is a parse error.
        assert_eq!(Doc::from_json("[1,2]").unwrap_err().0, ERR_PARSE);
    }

    #[test]
    fn migration_chain_runs_in_order() {
        let doc = Doc::from_json(r#"{"$schema": 1, "old_name": "x", "counter": 10}"#).unwrap();
        let mut handle = ffi::SkDoc {
            doc,
            path: None,
            current_schema: 3,
            migrations: vec![mig(1, rename_old_key), mig(2, bump_counter)],
            backup: false,
        };
        unsafe { migrate(&mut handle as *mut ffi::SkDoc, 3) }.unwrap();
        let d = &handle.doc;
        assert_eq!(d.schema().unwrap(), 3);
        assert_eq!(d.get_string("new_name", "").unwrap(), "x");
        assert!(!d.has("old_name").unwrap());
        assert_eq!(d.get_i64("counter", 0).unwrap(), 11);
    }

    #[test]
    fn migration_missing_step_and_newer() {
        let mut doc = Doc::from_json(r#"{"$schema": 1}"#).unwrap();
        let mut handle = ffi::SkDoc {
            doc,
            path: None,
            current_schema: 2,
            migrations: Vec::new(),
            backup: false,
        };
        assert_eq!(
            unsafe { migrate(&mut handle as *mut ffi::SkDoc, 2) }
                .unwrap_err()
                .0,
            ERR_NO_MIGRATION
        );
        doc = Doc::from_json(r#"{"$schema": 9}"#).unwrap();
        let mut handle2 = ffi::SkDoc {
            doc,
            path: None,
            current_schema: 3,
            migrations: Vec::new(),
            backup: false,
        };
        assert_eq!(
            unsafe { migrate(&mut handle2 as *mut ffi::SkDoc, 3) }
                .unwrap_err()
                .0,
            ERR_SCHEMA_NEWER
        );
    }

    #[test]
    fn escapes_json() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
