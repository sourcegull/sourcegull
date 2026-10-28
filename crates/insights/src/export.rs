//! Data export of a view (`/.api/insights/export/{id}`): a
//! ZIP archive named `<title>-<RFC 3339 time>.zip` holding one CSV of the same name with the
//! columns `title, label, query, recording_time, repository, value, capture`, one row per
//! series, point time, repository and captured value. Repositories the viewer cannot read
//! and those the view's default filters leave out are not exported.

use std::collections::HashMap;
use std::io::Write as _;

use chrono::{DateTime, Utc};
use roaring::RoaringBitmap;
use sourcegull_db::Result;
use sourcegull_types::RepoAccess;

use crate::store::InsightStore;
use crate::views::View;

/// The CSV header.
pub const HEADER: [&str; 7] = [
    "title",
    "label",
    "query",
    "recording_time",
    "repository",
    "value",
    "capture",
];

/// One exported value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRow {
    pub title: String,
    pub label: String,
    pub query: String,
    pub time: DateTime<Utc>,
    pub repository: String,
    pub value: i64,
    /// The captured or grouped value; empty for a search series.
    pub capture: String,
}

/// The rows of `view` the viewer with `access` may see, restricted to `only` when given.
pub async fn rows(
    store: &InsightStore,
    view: &View,
    access: &RepoAccess,
    only: Option<&RoaringBitmap>,
    names: &HashMap<i32, String>,
) -> Result<Vec<ExportRow>> {
    let mut out = Vec::new();
    for vs in &view.series {
        let mut readable = access.readable(&vs.series.repos);
        if let Some(o) = only {
            readable &= o;
        }
        let repos: Vec<i32> = readable
            .iter()
            .filter_map(|r| i32::try_from(r).ok())
            .collect();
        if repos.is_empty() {
            continue;
        }
        for (time, repo, value, count) in store.repo_points(vs.series.id, &repos).await? {
            out.push(ExportRow {
                title: view.title.clone(),
                label: vs.label.clone(),
                query: vs.series.query.clone(),
                time,
                repository: names.get(&repo).cloned().unwrap_or_default(),
                value: count,
                capture: value,
            });
        }
    }
    Ok(out)
}

fn csv_field(out: &mut String, f: &str) {
    if f.contains([',', '"', '\n', '\r']) {
        out.push('"');
        out.push_str(&f.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(f);
    }
}

fn csv_line(out: &mut String, fields: &[&str]) {
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        csv_field(out, f);
    }
    out.push('\n');
}

/// The CSV of `rows` with its header. Times are written as
/// `2026-03-01 12:00:00 +0000 UTC`.
pub fn csv(rows: &[ExportRow]) -> String {
    let mut out = String::new();
    csv_line(&mut out, &HEADER);
    for r in rows {
        let time = r.time.format("%Y-%m-%d %H:%M:%S +0000 UTC").to_string();
        let value = r.value.to_string();
        csv_line(
            &mut out,
            &[
                &r.title,
                &r.label,
                &r.query,
                &time,
                &r.repository,
                &value,
                &r.capture,
            ],
        );
    }
    out
}

/// The archive's base name: the title with runs of non-word characters as `-`, then the
/// time.
pub fn archive_name(title: &str, now: DateTime<Utc>) -> String {
    let mut name = String::new();
    let mut gap = false;
    for c in title.chars() {
        if c.is_alphanumeric() || c == '_' {
            name.push(c);
            gap = false;
        } else if !gap {
            name.push('-');
            gap = true;
        }
    }
    format!(
        "{name}-{}",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

/// A ZIP archive holding one stored (uncompressed) file.
pub fn zip(file_name: &str, data: &[u8]) -> Vec<u8> {
    let crc = crc32fast::hash(data);
    let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let name = file_name.as_bytes();
    let name_len = u16::try_from(name.len()).unwrap_or(u16::MAX);
    let name = &name[..usize::from(name_len)];
    // MS-DOS time and date of 1980-01-01 00:00.
    let (dos_time, dos_date) = (0u16, (1u16 << 5) | 1);
    let mut out = Vec::with_capacity(data.len() + 2 * name.len() + 128);
    let mut w = |b: &[u8]| out.extend_from_slice(b);
    // Local file header.
    w(&0x0403_4b50u32.to_le_bytes());
    w(&20u16.to_le_bytes()); // version needed
    w(&0x0800u16.to_le_bytes()); // flags: UTF-8 names
    w(&0u16.to_le_bytes()); // stored
    w(&dos_time.to_le_bytes());
    w(&dos_date.to_le_bytes());
    w(&crc.to_le_bytes());
    w(&size.to_le_bytes());
    w(&size.to_le_bytes());
    w(&name_len.to_le_bytes());
    w(&0u16.to_le_bytes());
    w(name);
    w(data);
    let central_at = u32::try_from(out.len()).unwrap_or(u32::MAX);
    let mut c = Vec::with_capacity(46 + name.len());
    let mut wc = |b: &[u8]| c.extend_from_slice(b);
    wc(&0x0201_4b50u32.to_le_bytes());
    wc(&20u16.to_le_bytes()); // version made by
    wc(&20u16.to_le_bytes()); // version needed
    wc(&0x0800u16.to_le_bytes());
    wc(&0u16.to_le_bytes());
    wc(&dos_time.to_le_bytes());
    wc(&dos_date.to_le_bytes());
    wc(&crc.to_le_bytes());
    wc(&size.to_le_bytes());
    wc(&size.to_le_bytes());
    wc(&name_len.to_le_bytes());
    wc(&0u16.to_le_bytes()); // extra
    wc(&0u16.to_le_bytes()); // comment
    wc(&0u16.to_le_bytes()); // disk
    wc(&0u16.to_le_bytes()); // internal attributes
    wc(&0u32.to_le_bytes()); // external attributes
    wc(&0u32.to_le_bytes()); // local header offset
    wc(name);
    let central_len = u32::try_from(c.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&c);
    let mut e = Vec::with_capacity(22);
    e.write_all(&0x0605_4b50u32.to_le_bytes()).ok();
    e.write_all(&0u16.to_le_bytes()).ok();
    e.write_all(&0u16.to_le_bytes()).ok();
    e.write_all(&1u16.to_le_bytes()).ok();
    e.write_all(&1u16.to_le_bytes()).ok();
    e.write_all(&central_len.to_le_bytes()).ok();
    e.write_all(&central_at.to_le_bytes()).ok();
    e.write_all(&0u16.to_le_bytes()).ok();
    out.extend_from_slice(&e);
    out
}

/// The one file of an archive written by [`zip`]: its name and content.
pub fn unzip(archive: &[u8]) -> Option<(String, Vec<u8>)> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(archive.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(archive.get(o..o + 4)?.try_into().ok()?));
    if u32_at(0)? != 0x0403_4b50 || u16_at(8)? != 0 {
        return None;
    }
    let size = usize::try_from(u32_at(18)?).ok()?;
    let name_len = usize::from(u16_at(26)?);
    let extra = usize::from(u16_at(28)?);
    let name = String::from_utf8(archive.get(30..30 + name_len)?.to_vec()).ok()?;
    let start = 30 + name_len + extra;
    let data = archive.get(start..start + size)?.to_vec();
    (crc32fast::hash(&data) == u32_at(14)?).then_some((name, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn csv_rows_are_quoted_as_needed() {
        let t = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
        let rows = vec![ExportRow {
            title: "TODOs, by team".into(),
            label: "say \"hi\"".into(),
            query: "TODO".into(),
            time: t,
            repository: "gitlab.example.com/acme/web".into(),
            value: 3,
            capture: String::new(),
        }];
        assert_eq!(
            csv(&rows),
            "title,label,query,recording_time,repository,value,capture\n\
             \"TODOs, by team\",\"say \"\"hi\"\"\",TODO,2026-03-01 12:00:00 +0000 UTC,\
             gitlab.example.com/acme/web,3,\n"
        );
        assert_eq!(
            archive_name("TODOs, by team!", t),
            "TODOs-by-team--2026-03-01T12:00:00Z"
        );
    }

    #[test]
    fn archives_round_trip() {
        let z = zip("a b.csv", b"x,y\n1,2\n");
        assert_eq!(unzip(&z), Some(("a b.csv".into(), b"x,y\n1,2\n".to_vec())));
        // End of central directory: one entry.
        assert_eq!(
            &z[z.len() - 22..z.len() - 18],
            &0x0605_4b50u32.to_le_bytes()
        );
    }
}
