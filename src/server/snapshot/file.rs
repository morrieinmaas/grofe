//! Reading and atomically writing `state.json`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{SNAPSHOT_VERSION, Snapshot, SnapshotError};
use crate::paths;

/// Reads the snapshot at `path`. A `version` this grofe does not know
/// moves the file to `state.json.v<N>`, and one that does not parse to
/// `state.json.corrupt-<unix time>`, each with one log line, so the next
/// write never overwrites another grofe's state.
pub fn read(path: &Path) -> Result<Snapshot, SnapshotError> {
    let bytes = fs::read(path).map_err(SnapshotError::Io)?;
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(e) => return Err(corrupt(path, &e.to_string())),
    };
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(SNAPSHOT_VERSION) => {
            serde_json::from_value(value).map_err(|e| corrupt(path, &e.to_string()))
        }
        Some(v) => match u32::try_from(v) {
            Ok(v) => {
                let aside = beside(path, &format!("v{v}"));
                move_aside(path, &aside, &format!("has unknown version {v}"));
                Err(SnapshotError::UnknownVersion(v))
            }
            Err(_) => Err(corrupt(path, &format!("version {v} is out of range"))),
        },
        None => Err(corrupt(path, "no version")),
    }
}

/// `path` with `.<suffix>` appended to its file name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

fn corrupt(path: &Path, why: &str) -> SnapshotError {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let aside = beside(path, &format!("corrupt-{secs}"));
    move_aside(path, &aside, &format!("is unreadable ({why})"));
    SnapshotError::Corrupt(why.to_string())
}

/// Renames `path` to `aside`, with one log line either way.
fn move_aside(path: &Path, aside: &Path, why: &str) {
    match fs::rename(path, aside) {
        Ok(()) => eprintln!(
            "grofe server: snapshot {} {why}; moved to {}",
            path.display(),
            aside.display()
        ),
        Err(e) => eprintln!(
            "grofe server: snapshot {} {why}; cannot move it to {}: {e}",
            path.display(),
            aside.display()
        ),
    }
}

/// Writes `snap` to `path` atomically: a temp file in the same directory
/// (mode `0600`), `fsync`, `rename` over `path`, `fsync` of the directory.
/// A crash at any point leaves either the old file or the new one.
pub fn write_atomic(path: &Path, snap: &Snapshot) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("snapshot path has no directory"))?;
    paths::ensure_private_dir(dir)?;
    let mut bytes = serde_json::to_vec_pretty(snap).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = dir.join(name);
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        // A leftover temp file keeps its old mode through `open`.
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `t` as RFC 3339, UTC, whole seconds.
pub(crate) fn rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::super::sample;
    use super::*;
    use std::time::Duration;

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::from_secs(951_782_400 + 3_661);
        assert_eq!(rfc3339(t), "2000-02-29T01:01:01Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_798_761_599);
        assert_eq!(rfc3339(t), "2026-12-31T23:59:59Z");
    }

    #[test]
    fn write_then_read_round_trips_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inst").join("state.json");
        let snap = sample();
        write_atomic(&path, &snap).unwrap();
        assert_eq!(read(&path).unwrap(), snap);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
    }

    #[test]
    fn an_unknown_version_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, r#"{"version": 9}"#).unwrap();
        assert!(matches!(read(&path), Err(SnapshotError::UnknownVersion(9))));
        assert!(!path.exists());
        assert!(dir.path().join("state.json.v9").exists());
    }

    #[test]
    fn garbage_and_a_bad_shape_are_moved_aside_as_corrupt() {
        for text in ["nope", r#"{"version": 1}"#, r#"{"sessions": []}"#] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            fs::write(&path, text).unwrap();
            assert!(
                matches!(read(&path), Err(SnapshotError::Corrupt(_))),
                "{text}"
            );
            let moved: Vec<String> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            assert_eq!(moved.len(), 1);
            assert!(moved[0].starts_with("state.json.corrupt-"), "{moved:?}");
        }
    }

    #[test]
    fn a_missing_file_is_an_io_error_and_nothing_moves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert!(matches!(read(&path), Err(SnapshotError::Io(_))));
    }
}
