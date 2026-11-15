// SPDX-License-Identifier: GPL-3.0-or-later
//! Hashing, timestamps and atomic file writes.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::ImportError;

/// Largest agent/broker config file read (guards against odd files).
pub(crate) const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Lowercase hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// `YYYYMMDDTHHMMSSZ` for a Unix timestamp (UTC).
pub fn utc_timestamp(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// RFC 3339 form of [`utc_timestamp`].
pub(crate) fn utc_iso(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Days-to-civil conversion (Howard Hinnant's algorithm).
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Read a file as bytes with a size cap.
pub(crate) fn read_capped(path: &Path) -> Result<Vec<u8>, ImportError> {
    use std::io::Read;
    let f = fs::File::open(path).map_err(|e| ImportError::io(path, e))?;
    let mut buf = Vec::new();
    f.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| ImportError::io(path, e))?;
    if buf.len() as u64 > MAX_FILE_BYTES {
        return Err(ImportError::Invalid(format!(
            "{}: file is larger than {MAX_FILE_BYTES} bytes",
            path.display()
        )));
    }
    Ok(buf)
}

/// Write `data` to `path` via a temp file in the same directory and a
/// rename. An existing file's permissions are kept.
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> Result<(), ImportError> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(p) = parent {
        fs::create_dir_all(p).map_err(|e| ImportError::io(p, e))?;
    }
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".mcpdoze-tmp-{}", std::process::id()));
    let tmp: PathBuf = match parent {
        Some(p) => p.join(&name),
        None => PathBuf::from(&name),
    };
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        if let Ok(meta) = fs::metadata(path) {
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(ImportError::io(path, e));
    }
    Ok(())
}

pub(crate) fn to_text(path: &Path, bytes: Vec<u8>) -> Result<String, ImportError> {
    String::from_utf8(bytes).map_err(|_| ImportError::Parse {
        path: path.to_path_buf(),
        message: "not valid UTF-8".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(utc_timestamp(0), "19700101T000000Z");
        assert_eq!(utc_timestamp(1_709_210_096), "20240229T123456Z");
        assert_eq!(utc_iso(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
