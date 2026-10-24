# SettingsKit

## TL;DR

Settings files, done once, correctly: a tiny zero-dependency DLL that loads and saves a JSON settings file **atomically**, carries a **schema version** in the file and runs **your migrations** when it's out of date, and **never writes defaults** — so changing a default in your next release can't silently change the meaning of settings files already on your users' disks. You talk to it through a small C ABI (or the Rust crate directly).

```c
SkDoc *doc;
sk_open_ex("settings.json", 3, 0, migrations, 2, &doc);   // migrates 1->2->3 if needed
int64_t w;  sk_get_i64(doc, "ui.width", 1280, &w, NULL);  // default lives here, not in the file
sk_set_string(doc, "downloads.preset", "720p");
sk_save(doc);                                             // atomic replace
sk_close(doc);
```

## Why this exists

Every Windows app hand-rolls settings, and the same three bugs keep showing up:

1. **The defaults trap.** The app writes its defaults into the file on first run. Later, a developer changes a default — and now old files silently mean something different from what their owners' apps wrote, and golden-file tests break for invisible reasons (this exact failure cost a real project a debugging session; it's WinDevKit pain point #13). SettingsKit's fix is a rule, not a setting: **a key reaches the file only when the app explicitly sets it.** Reads with defaults are pure lookups.
2. **The torn file.** A crash or power cut mid-save leaves a half-written JSON file, and the app loses every setting. SettingsKit writes to a temp file in the same directory, flushes it to disk, then replaces the target in a single operation (`MoveFileEx` with `REPLACE_EXISTING | WRITE_THROUGH` on Windows). Old file or new file — never half of each. With `SK_SAVE_BACKUP`, the previous file also survives as `<name>.bak`.
3. **The unversioned file.** The format grows, old files meet new code, and nobody knows what shape anything is in. SettingsKit files carry `"$schema": N` at the root. Opening an older file runs your registered migration for each step, in order. Opening a **newer** file fails with `SK_ERR_SCHEMA_NEWER` instead of silently mangling it. A **missing step** fails with `SK_ERR_NO_MIGRATION` instead of being skipped.

And a deliberate non-feature: a **corrupt file fails to open** with `SK_ERR_PARSE` and is left byte-for-byte untouched. Whether to start fresh, restore the `.bak`, or show the user an error is your app's decision — the library never quietly discards someone's settings.

## The API in one screen

Keys are addressed by dotted paths: `"downloads.preset"`, `"ui.width"`. Setting a path creates intermediate objects. Getting a present key of the wrong JSON type fails with `SK_ERR_TYPE` — no silent coercions.

| Area | Calls |
|---|---|
| Lifecycle | `sk_open`, `sk_open_ex` (with migrations), `sk_create`, `sk_close` |
| Persistence | `sk_save`, `sk_save_as`, `sk_reload` (re-read + re-migrate; a failed reload changes nothing) |
| Scalars | `sk_get_i64/f64/bool/string`, `sk_set_i64/f64/bool/string` — every getter takes a default and an optional `found` out-flag |
| Structured | `sk_get_json` (a value as compact JSON) / `sk_set_json` (arrays, nested objects) |
| Housekeeping | `sk_has`, `sk_remove`, `sk_is_dirty`, `sk_schema_version`, `sk_to_json` |
| Errors | Integer `SK_*` codes + thread-local `sk_last_error()` message; returned strings are caller-owned (`sk_free_string`) |

Migrations are plain C callbacks:

```c
static int to_v2(SkDoc *doc, void *user) {
    char *old = sk_get_string(doc, "path", NULL, NULL);
    if (old) { sk_set_string(doc, "download_dir", old); sk_free_string(old); }
    sk_remove(doc, "path");   /* returns 1/0 — NOT an SK_* code */
    return SK_OK;             /* migrations must return SK_OK or SK_ERR_* */
}
static const SkMigration MIGRATIONS[] = { { 1, to_v2, NULL } };
```

One migration etiquette note, learned while testing this library: `sk_remove` returns 1 for "removed" by design — don't return it from a migration as if it were a status code.

## File format

Plain pretty-printed JSON, key order preserved from whatever wrote the file, `"$schema"` at the root. Numbers that are integral stay integers. Duplicate keys inside one object are rejected at load (two values for one key is ambiguous, and silently picking one is how settings bugs hide). Anything you can read with a JSON parser, you can inspect by hand — and your users can too.

## Honest caveats

- **One thread per handle.** A `SkDoc*` is used from the thread that opened it (or properly synchronized) — the same rule your app's own settings code already lives by. Different handles are independent.
- **Migration steps can partially run.** If step 3 of 5 fails, steps 1–2 already changed the in-memory document (the file on disk is untouched until you save). Write steps to be re-runnable; the next open starts the chain from the file's schema again.
- **Strings containing U+0000** can be stored in a file but can't be returned through the C ABI (C strings end there); `sk_get_string` on such a value fails with `SK_ERR_INTERNAL` and says why.
- **No file locking.** Two processes writing the same settings file is last-writer-wins. Single-instance apps (the norm for settings owners) are unaffected.

## Verification status

21 unit tests cover the JSON engine (round-trips, surrogate pairs, duplicate-key rejection, depth cap), dotted paths, the defaults-never-written rule (a dedicated regression test named for the incident that motivated it), migration chains (order, missing step, newer-file refusal), atomic write + backup, and full FFI round-trips through real files, including a migration driven through `sk_open_ex`. Windows CI runs the full WinDevKit gate — format, clippy, the whole test suite, and a real MSVC build and link (which is the only place Win32 link errors can surface). The atomic-replace path uses `MoveFileExW` on Windows and is exercised by CI there; the test suite runs cross-platform against the same core logic.

Building: `cargo build --release` → `settingskit.dll` / `libsettingskit.so`; `cargo test --locked`.

License: MIT. Part of the WinDevKit family — see `AGENTS.md` for build discipline and `ROADMAP.md` for what's next.
