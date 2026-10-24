//! Platform layer: the only place OS APIs are touched.
//!
//! The one operation that matters here is the atomic file replace:
//! write the new content to a temp file in the SAME directory,
//! flush it to disk, then swap it over the target in a single
//! rename. On Windows `std::fs::rename` refuses to replace an
//! existing file, so the swap is MoveFileExW with
//! MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH (hand-declared,
//! kernel32). Everywhere else `fs::rename` already overwrites.
//!
//! Every extern block carries #[link(name = "...")] naming its
//! import library — without it the build fails at LINK time with
//! LNK2019, which check/clippy cannot catch; CI's real build can.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{SkResult, ERR_IO};

fn io_err<T>(what: &str, e: &std::io::Error) -> SkResult<T> {
    Err((ERR_IO, format!("{what}: {e}")))
}

/// Read a whole settings file as UTF-8 text.
/// Returns `Ok(None)` when the file does not exist (that is the
/// fresh-start case, not an error). Invalid UTF-8 is SK_ERR_PARSE
/// territory for the caller — reported here as an IO-class failure
/// with the reason, since the bytes could not be read as text.
pub fn read_file(path: &Path) -> SkResult<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Ok(Some(text)),
            Err(_) => Err((
                crate::ERR_PARSE,
                format!("{} is not valid UTF-8", path.display()),
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => io_err(&format!("reading {}", path.display()), &e),
    }
}

/// Write `bytes` to `path` atomically. If `keep_backup` and a file
/// already exists at `path`, the previous content is first copied
/// to `<path>.bak`. Parent directories are created as needed.
/// On any failure the target is left as it was (old content or
/// absent) and the temp file is removed best-effort.
pub fn atomic_write(path: &Path, bytes: &[u8], keep_backup: bool) -> SkResult<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| (ERR_IO, format!("creating {}: {e}", parent.display())))?;
        }
    }
    let tmp = tmp_path_for(path);
    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| (ERR_IO, format!("creating {}: {e}", tmp.display())))?;
        if let Err(e) = f.write_all(bytes).and_then(|_| f.sync_all()) {
            let _ = fs::remove_file(&tmp);
            return io_err(&format!("writing {}", tmp.display()), &e);
        }
    }
    if keep_backup && path.exists() {
        let bak = backup_path_for(path);
        if let Err(e) = fs::copy(path, &bak) {
            let _ = fs::remove_file(&tmp);
            return io_err(
                &format!("backing up {} to {}", path.display(), bak.display()),
                &e,
            );
        }
    }
    if let Err(e) = replace_file(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err((ERR_IO, format!("replacing {}: {e}", path.display())));
    }
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(s)
}

fn backup_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        fn GetLastError() -> u32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    fn wide(p: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let from_w = wide(from);
    let to_w = wide(to);
    // SAFETY: both buffers are NUL-terminated wide strings that
    // outlive the call; MoveFileExW is the documented atomic
    // replace-with-existing API.
    let ok = unsafe {
        MoveFileExW(
            from_w.as_ptr(),
            to_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        Err(std::io::Error::from_raw_os_error(code as i32))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "settingskit-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_creates_and_replaces() {
        let dir = temp_dir("replace");
        let path = dir.join("settings.json");
        atomic_write(&path, b"one", false).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"one");
        atomic_write(&path, b"two", false).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        // No temp leftovers, no backup when not requested.
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["settings.json".to_string()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_write_backup() {
        let dir = temp_dir("backup");
        let path = dir.join("settings.json");
        atomic_write(&path, b"one", true).unwrap();
        atomic_write(&path, b"two", true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(fs::read(dir.join("settings.json.bak")).unwrap(), b"one");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_missing_is_none() {
        let dir = temp_dir("missing");
        assert_eq!(read_file(&dir.join("nope.json")).unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
