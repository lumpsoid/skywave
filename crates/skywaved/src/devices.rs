//! The device registry (`devices` table, architecture.md "Pairing"): who may connect.
//!
//! Pairing inserts a device; every connection looks its certificate up by
//! fingerprint and refuses revoked devices. Admin commands (`devices list`,
//! `devices revoke`) use these functions on their own connection while
//! `serve` runs.

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use skywave_proto::{DeviceId, Fingerprint};

use crate::error::{Error, Result};

/// A row of the registry.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub cert_fingerprint: Fingerprint,
    pub paired_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
}

impl Device {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// What [`revoke`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum Revoke {
    /// The device was active and is now revoked.
    Revoked(Device),
    /// The device was already revoked; nothing changed.
    AlreadyRevoked(Device),
}

const COLUMNS: &str = "id, name, cert_fingerprint, paired_at, revoked_at, last_seen";

/// Records a newly paired device.
pub fn insert(
    conn: &Connection,
    id: DeviceId,
    name: &str,
    cert_fingerprint: Fingerprint,
    paired_at: DateTime<Utc>,
) -> Result<Device> {
    conn.execute(
        "INSERT INTO devices (id, name, cert_fingerprint, paired_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            id_text(id),
            name,
            cert_fingerprint.to_hex(),
            timestamp(paired_at)
        ],
    )?;
    Ok(Device {
        id,
        name: name.to_owned(),
        cert_fingerprint,
        paired_at,
        revoked_at: None,
        last_seen: None,
    })
}

/// Every device, oldest pairing first.
pub fn list(conn: &Connection) -> Result<Vec<Device>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM devices ORDER BY paired_at, id"
    ))?;
    let devices = stmt
        .query_map([], from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(devices)
}

pub fn get(conn: &Connection, id: DeviceId) -> Result<Option<Device>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM devices WHERE id = ?1"),
            [id_text(id)],
            from_row,
        )
        .optional()?)
}

/// The device holding the certificate with this fingerprint, revoked or
/// not; callers check [`Device::is_revoked`].
pub fn by_fingerprint(conn: &Connection, fingerprint: Fingerprint) -> Result<Option<Device>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM devices WHERE cert_fingerprint = ?1"),
            [fingerprint.to_hex()],
            from_row,
        )
        .optional()?)
}

/// Marks the device revoked at `at`. Revoking a revoked device keeps the
/// original time; an unknown id is [`Error::UnknownDevice`].
pub fn revoke(conn: &Connection, id: DeviceId, at: DateTime<Utc>) -> Result<Revoke> {
    let changed = conn.execute(
        "UPDATE devices SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
        params![id_text(id), timestamp(at)],
    )?;
    let device = get(conn, id)?.ok_or(Error::UnknownDevice(id))?;
    Ok(if changed == 1 {
        Revoke::Revoked(device)
    } else {
        Revoke::AlreadyRevoked(device)
    })
}

/// Records that the device connected at `at`.
pub fn touch(conn: &Connection, id: DeviceId, at: DateTime<Utc>) -> Result<()> {
    let changed = conn.execute(
        "UPDATE devices SET last_seen = ?2 WHERE id = ?1",
        params![id_text(id), timestamp(at)],
    )?;
    if changed == 0 {
        return Err(Error::UnknownDevice(id));
    }
    Ok(())
}

/// Parses a device id as typed on the command line.
pub fn parse_id(s: &str) -> Result<DeviceId> {
    uuid::Uuid::parse_str(s.trim())
        .map(DeviceId)
        .map_err(|_| Error::InvalidDeviceId(s.to_owned()))
}

/// The `devices list` table.
pub fn format_table(devices: &[Device]) -> String {
    if devices.is_empty() {
        return "no paired devices\n".to_owned();
    }
    let rows: Vec<[String; 5]> = devices
        .iter()
        .map(|d| {
            [
                id_text(d.id),
                d.name.clone(),
                human_time(d.paired_at),
                d.last_seen.map_or_else(|| "never".to_owned(), human_time),
                match d.revoked_at {
                    Some(at) => format!("revoked {}", human_time(at)),
                    None => "active".to_owned(),
                },
            ]
        })
        .collect();
    let header = ["ID", "NAME", "PAIRED", "LAST SEEN", "STATUS"].map(str::to_owned);
    let mut widths = header.clone().map(|h| h.chars().count());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let mut line = String::new();
        for (i, (cell, w)) in row.iter().zip(widths).enumerate() {
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<w$}  ");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn human_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%SZ").to_string()
}

fn id_text(id: DeviceId) -> String {
    id.0.hyphenated().to_string()
}

/// RFC 3339 UTC, the database's timestamp convention.
fn timestamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: DeviceId(column(row, 0, uuid::Uuid::parse_str)?),
        name: row.get(1)?,
        cert_fingerprint: column(row, 2, |s| s.parse::<Fingerprint>())?,
        paired_at: column(row, 3, parse_timestamp)?,
        revoked_at: optional_column(row, 4, parse_timestamp)?,
        last_seen: optional_column(row, 5, parse_timestamp)?,
    })
}

fn parse_timestamp(s: &str) -> chrono::ParseResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).map(|t| t.with_timezone(&Utc))
}

fn column<T, E>(
    row: &Row<'_>,
    idx: usize,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> rusqlite::Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let text: String = row.get(idx)?;
    parse(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn optional_column<T, E>(
    row: &Row<'_>,
    idx: usize,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> rusqlite::Result<Option<T>>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match row.get::<_, Option<String>>(idx)? {
        Some(_) => column(row, idx, parse).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::db::Db;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    fn open() -> (tempfile::TempDir, Db) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        (tmp, db)
    }

    fn add(db: &Db, name: &str, secs: i64) -> Device {
        let fp = Fingerprint::of_der(name.as_bytes());
        insert(db.conn(), DeviceId::new(), name, fp, at(secs)).unwrap()
    }

    #[test]
    fn insert_then_list_in_pairing_order() {
        let (_tmp, db) = open();
        assert!(list(db.conn()).unwrap().is_empty());
        let phone = add(&db, "phone", 10);
        let laptop = add(&db, "laptop", 5);
        assert_eq!(list(db.conn()).unwrap(), [laptop, phone]);
    }

    #[test]
    fn duplicate_fingerprint_is_rejected() {
        let (_tmp, db) = open();
        let first = add(&db, "phone", 0);
        let again = insert(
            db.conn(),
            DeviceId::new(),
            "other",
            first.cert_fingerprint,
            at(1),
        );
        assert!(again.is_err());
    }

    #[test]
    fn revoke_sets_revoked_at_once() {
        let (_tmp, db) = open();
        let phone = add(&db, "phone", 0);
        let other = add(&db, "other", 1);

        let Revoke::Revoked(revoked) = revoke(db.conn(), phone.id, at(100)).unwrap() else {
            panic!("expected Revoked");
        };
        assert_eq!(revoked.revoked_at, Some(at(100)));

        let Revoke::AlreadyRevoked(again) = revoke(db.conn(), phone.id, at(200)).unwrap() else {
            panic!("expected AlreadyRevoked");
        };
        assert_eq!(again.revoked_at, Some(at(100)), "original time kept");
        assert!(!get(db.conn(), other.id).unwrap().unwrap().is_revoked());
    }

    #[test]
    fn revoke_unknown_device_is_an_error() {
        let (_tmp, db) = open();
        let id = DeviceId::new();
        assert!(matches!(
            revoke(db.conn(), id, at(0)),
            Err(Error::UnknownDevice(got)) if got == id
        ));
    }

    #[test]
    fn revoke_from_admin_connection_is_seen_by_serve() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let serve = Db::open(&path).unwrap();
        let admin = Db::open(&path).unwrap();
        let phone = add(&serve, "phone", 0);

        revoke(admin.conn(), phone.id, at(5)).unwrap();
        let seen = by_fingerprint(serve.conn(), phone.cert_fingerprint)
            .unwrap()
            .unwrap();
        assert!(seen.is_revoked());
    }

    #[test]
    fn lookup_by_fingerprint_and_touch() {
        let (_tmp, db) = open();
        let phone = add(&db, "phone", 0);
        assert_eq!(
            by_fingerprint(db.conn(), Fingerprint::of_der(b"unknown")).unwrap(),
            None
        );
        touch(db.conn(), phone.id, at(42)).unwrap();
        let found = by_fingerprint(db.conn(), phone.cert_fingerprint)
            .unwrap()
            .unwrap();
        assert_eq!(found.last_seen, Some(at(42)));
        assert!(!found.is_revoked());
        assert!(matches!(
            touch(db.conn(), DeviceId::new(), at(0)),
            Err(Error::UnknownDevice(_))
        ));
    }

    #[test]
    fn parse_id_accepts_uuids_only() {
        let id = DeviceId::new();
        assert_eq!(parse_id(&id_text(id)).unwrap(), id);
        assert!(matches!(parse_id("phone"), Err(Error::InvalidDeviceId(_))));
    }

    #[test]
    fn table_shows_every_column() {
        assert_eq!(format_table(&[]), "no paired devices\n");
        let (_tmp, db) = open();
        let phone = add(&db, "phone", 0);
        add(&db, "laptop", 60);
        touch(db.conn(), phone.id, at(3600)).unwrap();
        revoke(db.conn(), phone.id, at(7200)).unwrap();

        let table = format_table(&list(db.conn()).unwrap());
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("ID"));
        for col in ["NAME", "PAIRED", "LAST SEEN", "STATUS"] {
            assert!(lines[0].contains(col), "{col}");
        }
        assert!(lines[1].starts_with(&id_text(phone.id)));
        assert!(lines[1].contains("phone"));
        assert!(lines[1].contains("2027-01-15 08:00:00Z"));
        assert!(lines[1].ends_with("revoked 2027-01-15 10:00:00Z"));
        assert!(lines[2].contains("laptop") && lines[2].contains("never"));
        assert!(lines[2].ends_with("active"));
        let name_col = lines[0].find("NAME").unwrap();
        assert_eq!(lines[1].find("phone"), Some(name_col));
    }
}
