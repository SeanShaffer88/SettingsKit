# SettingsKit — agent notes

- Zero dependencies; the JSON engine (src/json.rs) is hand-rolled on purpose. Duplicate keys in one object are a PARSE ERROR by design — do not "fix" that into last-wins.
- The defaults-never-written rule is the product. Getters must stay pure lookups; any change that materializes a key on read reintroduces the InstallScope PR #11 golden-defaults bug. The regression test is `defaults_are_never_written` in src/lib.rs — keep it.
- Migrations run through the FFI handle (crate::migrate takes *mut ffi::SkDoc) so callbacks see exactly what a C consumer sees. Steps must return SK_* codes; sk_remove's 1/0 return is NOT a status code (this bit once, in ffi tests).
- Atomic save = temp file in the same directory + flush + single replace. On Windows that is MoveFileExW(REPLACE_EXISTING|WRITE_THROUGH) in src/platform.rs — std::fs::rename refuses to overwrite there. Do not simplify it back to fs::rename on the Windows path.
- Gate: WinDevKit tools/gate.sh (fmt + clippy both targets + full tests). Check/clippy do not link — CI's real build is the link gate.
