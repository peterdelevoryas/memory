//! `GET /health`: an unauthenticated summary for an external monitor. It
//! reports whether the database answers, how old the last successful backup is
//! (backup.sh writes a timestamp file), and how full the disk is. 200 when all
//! is well, 503 with the problems otherwise. It reveals no note content.

use std::{path::PathBuf, sync::Arc};

use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;

use crate::store::Store;

#[derive(Clone)]
pub struct Health(Arc<Config>);

pub struct Config {
    pub store: Store,
    /// Directory whose filesystem is checked for free space (the database's).
    pub data_dir: PathBuf,
    pub max_disk_percent: f64,
    /// File holding the RFC 3339 time of the last successful backup; no
    /// backup check when unset.
    pub backup_stamp: Option<PathBuf>,
    pub max_backup_age_hours: f64,
}

impl Health {
    pub fn new(config: Config) -> Self {
        Self(Arc::new(config))
    }
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Report {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_age_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_used_percent: Option<f64>,
    pub problems: Vec<String>,
}

pub async fn handler(State(Health(c)): State<Health>) -> (StatusCode, Json<Report>) {
    let notes = c
        .store
        .count()
        .await
        .map_err(|e| format!("database: {e:#}"));
    let disk = disk_used_percent(&c.data_dir).map_err(|e| format!("disk: {e}"));
    let backup = c.backup_stamp.as_ref().map(|path| {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("backup: can't read {}: {e}", path.display()))?;
        let at = chrono::DateTime::parse_from_rfc3339(text.trim())
            .map_err(|e| format!("backup: bad timestamp in {}: {e}", path.display()))?;
        Ok((chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_minutes() as f64 / 60.0)
    });
    let report = evaluate(notes, disk, backup, &c);
    let status = if report.ok {
        StatusCode::OK
    } else {
        tracing::warn!(problems = ?report.problems, "health check failing");
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(report))
}

fn evaluate(
    notes: Result<u64, String>,
    disk: Result<f64, String>,
    backup: Option<Result<f64, String>>,
    c: &Config,
) -> Report {
    let mut problems = Vec::new();
    let notes = notes.map_err(|e| problems.push(e)).ok();
    let disk = disk.map_err(|e| problems.push(e)).ok();
    if let Some(used) = disk
        && used > c.max_disk_percent
    {
        problems.push(format!(
            "disk is {used:.0}% full (limit {:.0}%)",
            c.max_disk_percent
        ));
    }
    let backup_age = match backup {
        Some(Ok(age)) => {
            if age > c.max_backup_age_hours {
                problems.push(format!(
                    "last backup was {age:.1} hours ago (limit {:.0})",
                    c.max_backup_age_hours
                ));
            }
            Some(age)
        }
        Some(Err(e)) => {
            problems.push(e);
            None
        }
        None => None,
    };
    Report {
        ok: problems.is_empty(),
        notes,
        backup_age_hours: backup_age.map(|a| (a * 10.0).round() / 10.0),
        disk_used_percent: disk.map(|d| d.round()),
        problems,
    }
}

fn disk_used_percent(dir: &std::path::Path) -> std::io::Result<f64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid NUL-terminated string and `stat` is a valid out-pointer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let total = stat.f_blocks as f64;
    let available = stat.f_bavail as f64;
    if total == 0.0 {
        return Ok(0.0);
    }
    Ok((total - available) / total * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn config() -> Config {
        let dir = std::env::temp_dir().join(format!("memory-health-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Config {
            store: Store::open(dir.join("t.db").to_str().unwrap())
                .await
                .unwrap(),
            data_dir: dir,
            max_disk_percent: 85.0,
            backup_stamp: None,
            max_backup_age_hours: 26.0,
        }
    }

    #[tokio::test]
    async fn healthy_and_unhealthy_reports() {
        let c = config().await;
        let ok = evaluate(Ok(3), Ok(40.2), Some(Ok(5.04)), &c);
        assert_eq!(
            ok,
            Report {
                ok: true,
                notes: Some(3),
                backup_age_hours: Some(5.0),
                disk_used_percent: Some(40.0),
                problems: vec![]
            }
        );
        let bad = evaluate(Err("database: down".into()), Ok(91.0), Some(Ok(30.0)), &c);
        assert!(!bad.ok);
        assert_eq!(bad.problems.len(), 3, "{:?}", bad.problems);
        let missing_stamp = evaluate(Ok(1), Ok(1.0), Some(Err("backup: can't read".into())), &c);
        assert!(!missing_stamp.ok);
        // No backup stamp configured: the backup isn't checked.
        assert!(evaluate(Ok(1), Ok(1.0), None, &c).ok);
    }

    #[tokio::test]
    async fn real_disk_and_stamp() {
        let mut c = config().await;
        let used = disk_used_percent(&c.data_dir).unwrap();
        assert!((0.0..=100.0).contains(&used));
        let stamp = c.data_dir.join("last-backup");
        std::fs::write(&stamp, format!("{}\n", chrono::Utc::now().to_rfc3339())).unwrap();
        c.backup_stamp = Some(stamp);
        let (status, Json(report)) = handler(State(Health::new(c))).await;
        assert_eq!(status, StatusCode::OK, "{report:?}");
        assert!(report.backup_age_hours.unwrap() < 0.1);
    }
}
