//! Detection and download of outdated ("update to latest version") beatmaps.
//!
//! osu! flags a map for update when the local `.osu` file no longer matches
//! the online version. The same signal is available through the osu! API: every
//! beatmap carries a `checksum` (md5 of its `.osu` file), which we compare
//! against the md5 computed during the library scan. Anything with a matching
//! `beatmap_id` but a different checksum is outdated and can be refreshed by
//! redownloading the whole beatmapset.
//!
//! Metadata goes through the backend Worker (`GET /beatmapsets/:id` and
//! `GET /beatmaps/:id`, authenticated with the Worker's own credentials), so
//! checking works without signing in. Downloads reuse the repair pipeline:
//! official osu! API when signed in, mirror fallback otherwise.

use crate::local::LocalBeatmap;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io,
    path::{Path, PathBuf},
};

/// Remote beatmap entry inside a beatmapset response. Fields mirror the osu!
/// API; not every field is consumed locally.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RemoteBeatmap {
    pub id: i64,
    #[serde(default)]
    pub version: String,
    /// md5 of the online `.osu` file; `None` when osu! does not report one.
    #[serde(default)]
    pub checksum: Option<String>,
}

/// Remote beatmapset metadata from `GET /beatmapsets/:id`. Fields mirror the
/// osu! API; not every field is consumed locally.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RemoteSetMeta {
    pub id: i64,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub creator: String,
    #[serde(default)]
    pub last_updated: Option<String>,
    #[serde(default)]
    pub beatmaps: Vec<RemoteBeatmap>,
}

/// Remote beatmap from `GET /beatmaps/:id` (used to resolve a beatmapset id
/// when the local folder/.osu does not record one). Fields mirror the osu!
/// API.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RemoteBeatmapLookup {
    pub id: i64,
    #[serde(default)]
    pub beatmapset_id: Option<i64>,
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub version: String,
}

/// One locally installed difficulty that can be checked online.
#[derive(Debug, Clone)]
pub struct LocalDiffRef {
    pub beatmap_id: Option<i64>,
    pub md5: String,
    pub label: String,
    pub folder: PathBuf,
}

impl LocalDiffRef {
    pub fn from_map(map: &LocalBeatmap) -> Self {
        Self {
            beatmap_id: map.beatmap_id,
            md5: map.md5.clone(),
            label: map.label(),
            folder: map.folder.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutdatedDiff {
    pub label: String,
}

/// A beatmapset with at least one outdated local difficulty.
#[derive(Debug, Clone)]
pub struct OutdatedSet {
    pub beatmapset_id: i64,
    pub title: String,
    pub remote_updated: Option<String>,
    pub outdated: Vec<OutdatedDiff>,
    pub up_to_date: usize,
    /// Local diffs without a beatmap id that could not be checked.
    pub unchecked: usize,
    pub folders: Vec<PathBuf>,
}

impl OutdatedSet {
    pub fn outdated_count(&self) -> usize {
        self.outdated.len()
    }

    pub fn total_checked(&self) -> usize {
        self.outdated.len() + self.up_to_date
    }
}

/// Compares local difficulties against fresh remote metadata. Local diffs
/// without a beatmap id cannot be matched and count as `unchecked`.
pub fn detect_outdated(
    beatmapset_id: i64,
    title: String,
    remote_updated: Option<String>,
    locals: &[LocalDiffRef],
    remote: &RemoteSetMeta,
    folders: Vec<PathBuf>,
) -> OutdatedSet {
    let remote_by_id = remote
        .beatmaps
        .iter()
        .map(|beatmap| (beatmap.id, beatmap))
        .collect::<BTreeMap<_, _>>();
    let mut outdated = Vec::new();
    let mut up_to_date = 0;
    let mut unchecked = 0;

    for local in locals {
        let Some(beatmap_id) = local.beatmap_id else {
            unchecked += 1;
            continue;
        };
        match remote_by_id.get(&beatmap_id) {
            None => outdated.push(OutdatedDiff {
                label: format!("{} (removed upstream)", local.label),
            }),
            // A missing online checksum can never confirm freshness; counting
            // it as outdated would flag the diff again after every update, so
            // it stays unchecked instead.
            Some(remote_beatmap) if remote_beatmap.checksum.is_none() => {
                unchecked += 1;
            }
            Some(remote_beatmap) => match &remote_beatmap.checksum {
                Some(checksum) if checksum.eq_ignore_ascii_case(&local.md5) => {
                    up_to_date += 1;
                }
                _ => outdated.push(OutdatedDiff {
                    label: local.label.clone(),
                }),
            },
        }
    }

    OutdatedSet {
        beatmapset_id,
        title,
        remote_updated,
        outdated,
        up_to_date,
        unchecked,
        folders,
    }
}

/// Groups scanned maps into per-set check targets. Returns the targets plus
/// the number of diffs that cannot be checked at all (no beatmap id and no
/// set id to resolve them through).
pub fn build_check_targets(maps: &[LocalBeatmap]) -> (Vec<CheckTarget>, usize) {
    let mut by_set = BTreeMap::<i64, Vec<LocalDiffRef>>::new();
    let mut without_set = Vec::new();
    let mut uncheckable = 0;

    for map in maps {
        let local = LocalDiffRef::from_map(map);
        match map.beatmapset_id {
            Some(set_id) => by_set.entry(set_id).or_default().push(local),
            None => {
                if local.beatmap_id.is_some() {
                    without_set.push(local);
                } else {
                    uncheckable += 1;
                }
            }
        }
    }

    let mut targets = by_set
        .into_iter()
        .map(|(beatmapset_id, locals)| CheckTarget {
            beatmapset_id: Some(beatmapset_id),
            locals,
        })
        .collect::<Vec<_>>();
    if !without_set.is_empty() {
        targets.push(CheckTarget {
            beatmapset_id: None,
            locals: without_set,
        });
    }
    (targets, uncheckable)
}

/// Beatmapset ids resolved from per-difficulty online lookups, plus diffs
/// that could not be attributed to any online set.
pub type ResolvedGroups = (
    BTreeMap<i64, Vec<LocalDiffRef>>,
    Vec<LocalDiffRef>,
);

#[derive(Debug, Clone)]
pub struct CheckTarget {
    pub beatmapset_id: Option<i64>,
    pub locals: Vec<LocalDiffRef>,
}

/// Resolves diffs that lack a beatmapset id by looking each beatmap id up
/// online, grouping the locals under the discovered set ids. Diffs that no
/// longer exist online land in `unresolved`. Transport errors abort the whole
/// resolution so a network failure cannot silently skip checks.
pub fn resolve_unknown_sets(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    locals: &[LocalDiffRef],
) -> Result<ResolvedGroups> {
    let mut grouped = BTreeMap::<i64, Vec<LocalDiffRef>>::new();
    let mut unresolved = Vec::new();

    for local in locals {
        let Some(beatmap_id) = local.beatmap_id else {
            unresolved.push(local.clone());
            continue;
        };
        match fetch_beatmap_blocking(client, backend_url, beatmap_id)? {
            Some(remote) => match remote.beatmapset_id {
                Some(set_id) => grouped.entry(set_id).or_default().push(local.clone()),
                None => unresolved.push(local.clone()),
            },
            None => unresolved.push(local.clone()),
        }
    }

    Ok((grouped, unresolved))
}

/// Fetches one beatmapset's metadata through the Worker. Returns `None` when
/// the set no longer exists online (deleted).
pub fn fetch_set_meta_blocking(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    beatmapset_id: i64,
) -> Result<Option<RemoteSetMeta>> {
    let url = format!(
        "{}/beatmapsets/{beatmapset_id}",
        backend_url.trim().trim_end_matches('/')
    );
    let response = client
        .get(&url)
        .send()
        .with_context(|| format!("fetching beatmapset {beatmapset_id}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response
        .error_for_status()
        .with_context(|| format!("fetching beatmapset {beatmapset_id}"))?
        .json::<RemoteSetMeta>()
        .context("decoding beatmapset metadata")
        .map(Some)
}

/// Fetches one beatmap through the Worker. Returns `None` when it no longer
/// exists online.
pub fn fetch_beatmap_blocking(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    beatmap_id: i64,
) -> Result<Option<RemoteBeatmapLookup>> {
    let url = format!(
        "{}/beatmaps/{beatmap_id}",
        backend_url.trim().trim_end_matches('/')
    );
    let response = client
        .get(&url)
        .send()
        .with_context(|| format!("fetching beatmap {beatmap_id}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response
        .error_for_status()
        .with_context(|| format!("fetching beatmap {beatmap_id}"))?
        .json::<RemoteBeatmapLookup>()
        .context("decoding beatmap metadata")
        .map(Some)
}

/// Downloads a beatmapset `.osz` through the Worker. Sends the osu! OAuth
/// token when signed in so the Worker can use the official osu! API; without
/// a token the Worker serves the mirror. Returns the download source reported
/// by the Worker (`osu-api` or `mirror`).
pub fn download_beatmapset_file(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    beatmapset_id: i64,
    access_token: Option<&str>,
    destination: &Path,
) -> Result<String> {
    let url = format!(
        "{}/beatmapsets/{beatmapset_id}/download",
        backend_url.trim().trim_end_matches('/')
    );
    let mut request = client.get(&url);
    if let Some(token) = access_token
        && !token.trim().is_empty()
    {
        request = request.bearer_auth(token.trim());
    }
    let response = request
        .send()
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let download_source = response
        .headers()
        .get("X-Download-Source")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("mirror")
        .to_owned();
    let mut response = response;
    let mut file = fs::File::create(destination)?;
    io::copy(&mut response, &mut file)?;
    Ok(download_source)
}

pub fn verify_osz(osz_path: &Path) -> Result<()> {
    let metadata =
        fs::metadata(osz_path).with_context(|| format!("reading {}", osz_path.display()))?;
    if metadata.len() < 64 {
        anyhow::bail!(
            "download for {} is too small to be a beatmapset ({} bytes); the backend may have returned an error page",
            osz_path.display(),
            metadata.len()
        );
    }
    let file = fs::File::open(osz_path)?;
    zip::ZipArchive::new(file).with_context(|| {
        format!(
            "download for {} is not a valid .osz archive",
            osz_path.display()
        )
    })?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct UpdateJob {
    pub beatmapset_id: i64,
    pub folders: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOutcome {
    pub written_files: Vec<String>,
    pub removed_files: Vec<String>,
    pub download_source: String,
}

pub fn temp_osz_path(beatmapset_id: i64) -> PathBuf {
    std::env::temp_dir()
        .join("osu-map-manager-updates")
        .join(format!("{beatmapset_id}.osz"))
}

/// Applies a full update: downloads the latest `.osz`, overwrites every file
/// it contains, deletes local `.osu` files the new version dropped, and
/// verifies each surviving difficulty against its online checksum. Remote
/// metadata is fetched fresh so the verification always targets the version
/// being installed.
pub fn apply_update_blocking(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    access_token: Option<&str>,
    job: &UpdateJob,
) -> Result<UpdateOutcome> {
    if backend_url.trim().is_empty() {
        anyhow::bail!("backend URL is required for beatmap update downloads");
    }
    let remote = fetch_set_meta_blocking(client, backend_url, job.beatmapset_id)?
        .with_context(|| {
            format!(
                "beatmapset {} is no longer available online",
                job.beatmapset_id
            )
        })?;
    let expected_checksums = remote
        .beatmaps
        .iter()
        .filter_map(|beatmap| {
            beatmap
                .checksum
                .clone()
                .map(|checksum| (beatmap.id, checksum))
        })
        .collect::<BTreeMap<_, _>>();

    let temp_path = temp_osz_path(job.beatmapset_id);
    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let download_source = download_beatmapset_file(
        client,
        backend_url,
        job.beatmapset_id,
        access_token,
        &temp_path,
    )?;
    verify_osz(&temp_path)?;

    let mut outcome = UpdateOutcome {
        download_source,
        ..UpdateOutcome::default()
    };
    let mut written = BTreeSet::new();
    let mut removed = BTreeSet::new();
    for folder in &job.folders {
        let report = extract_full_into_folder(&temp_path, folder)
            .with_context(|| format!("extracting into {}", folder.display()))?;
        written.extend(report.written);
        let stale = remove_stale_osu_files(folder, &report.archived_osu_names)
            .with_context(|| format!("cleaning {}", folder.display()))?;
        removed.extend(stale);
    }
    outcome.written_files = written.into_iter().collect();
    outcome.removed_files = removed.into_iter().collect();

    verify_updated_checksums(job, &expected_checksums)?;
    Ok(outcome)
}

struct FullExtractReport {
    written: Vec<String>,
    /// Lowercase `.osu` file names contained in the archive.
    archived_osu_names: BTreeSet<String>,
}

fn extract_full_into_folder(osz_path: &Path, folder: &Path) -> Result<FullExtractReport> {
    let file = fs::File::open(osz_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    fs::create_dir_all(folder)?;
    let mut written = Vec::new();
    let mut archived_osu_names = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let Some(enclosed_name) = entry.enclosed_name() else {
            continue;
        };
        let output_path = folder.join(&enclosed_name);
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::File::create(&output_path)?;
        io::copy(&mut entry, &mut output)?;
        if let Some(file_name) = enclosed_name
            .file_name()
            .and_then(|name| name.to_str())
        {
            written.push(file_name.to_owned());
            if enclosed_name
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("osu"))
            {
                archived_osu_names.insert(file_name.to_ascii_lowercase());
            }
        }
    }
    written.sort();
    written.dedup();
    Ok(FullExtractReport {
        written,
        archived_osu_names,
    })
}

/// Deletes local `.osu` files that the new version no longer contains
/// (difficulties removed upstream). Only `.osu` files are touched; audio,
/// images, and skins are left alone.
fn remove_stale_osu_files(
    folder: &Path,
    archived_osu_names: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for entry in fs::read_dir(folder).with_context(|| format!("reading {}", folder.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        let is_osu = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("osu"));
        if !is_osu {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        if !archived_osu_names.contains(&file_name.to_ascii_lowercase()) {
            fs::remove_file(&path)
                .with_context(|| format!("removing {}", path.display()))?;
            removed.push(file_name);
        }
    }
    removed.sort();
    Ok(removed)
}

/// Re-reads every `.osu` in the updated folders and confirms difficulties
/// with a known online checksum match it.
fn verify_updated_checksums(
    job: &UpdateJob,
    expected_checksums: &BTreeMap<i64, String>,
) -> Result<()> {
    if expected_checksums.is_empty() {
        return Ok(());
    }
    let mut mismatched = Vec::new();
    for folder in &job.folders {
        let entries =
            fs::read_dir(folder).with_context(|| format!("reading {}", folder.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file() {
                continue;
            }
            if !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("osu"))
            {
                continue;
            }
            let bytes = fs::read(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let md5 = format!("{:x}", md5::compute(&bytes));
            let Some(beatmap_id) = read_beatmap_id(&bytes) else {
                continue;
            };
            if let Some(expected) = expected_checksums.get(&beatmap_id)
                && !expected.eq_ignore_ascii_case(&md5)
            {
                mismatched.push(format!(
                    "{} (beatmap {beatmap_id})",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("unknown.osu")
                ));
            }
        }
    }
    if mismatched.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "updated files do not match the online checksums (set updated again afterwards?): {}",
        mismatched.join(", ")
    )
}

/// Reads `BeatmapID` from raw `.osu` bytes without full parsing.
fn read_beatmap_id(bytes: &[u8]) -> Option<i64> {
    let text = String::from_utf8_lossy(bytes);
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if let Some((key, value)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case("beatmapid")
        {
            let id: i64 = value.trim().parse().ok()?;
            if id > 0 {
                return Some(id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_fixture() -> RemoteSetMeta {
        serde_json::from_value(serde_json::json!({
            "id": 123,
            "artist": "Artist",
            "title": "Title",
            "creator": "Mapper",
            "last_updated": "2026-01-01T00:00:00+00:00",
            "beatmaps": [
                {"id": 1, "version": "Normal", "checksum": "aaa"},
                {"id": 2, "version": "Hard", "checksum": "bbb"},
                {"id": 3, "version": "Insane", "checksum": null}
            ]
        }))
        .unwrap()
    }

    fn local(id: Option<i64>, md5: &str, label: &str) -> LocalDiffRef {
        LocalDiffRef {
            beatmap_id: id,
            md5: md5.to_owned(),
            label: label.to_owned(),
            folder: PathBuf::from("folder"),
        }
    }

    #[test]
    fn outdated_detection_compares_checksums() {
        let remote = remote_fixture();
        let locals = vec![
            local(Some(1), "aaa", "Normal"),
            local(Some(2), "stale", "Hard"),
            local(Some(3), "anything", "Insane"),
            local(Some(9), "zzz", "Deleted diff"),
            local(None, "qqq", "No id"),
        ];
        let set = detect_outdated(
            123,
            "Artist - Title".to_owned(),
            None,
            &locals,
            &remote,
            vec![PathBuf::from("folder")],
        );

        assert_eq!(set.up_to_date, 1);
        assert_eq!(set.unchecked, 2);
        // Hard (checksum changed) and the deleted diff (removed upstream).
        // Insane has no online checksum to confirm against, so it stays
        // unchecked rather than looping forever.
        assert_eq!(set.outdated_count(), 2);
        assert!(
            set.outdated
                .iter()
                .any(|diff| diff.label.contains("removed upstream"))
        );
    }

    #[test]
    fn everything_current_is_not_outdated() {
        let remote = RemoteSetMeta {
            id: 7,
            artist: String::new(),
            title: String::new(),
            creator: String::new(),
            last_updated: None,
            beatmaps: vec![RemoteBeatmap {
                id: 1,
                version: String::new(),
                checksum: Some("AAA".to_owned()),
            }],
        };
        let locals = vec![local(Some(1), "aaa", "Normal")];
        let set = detect_outdated(
            7,
            "set".to_owned(),
            None,
            &locals,
            &remote,
            Vec::new(),
        );
        assert_eq!(set.outdated_count(), 0);
        assert_eq!(set.up_to_date, 1);
    }

    #[test]
    fn stale_osu_files_are_removed_but_assets_kept() {
        let root = std::env::temp_dir().join(format!(
            "osu-update-stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&root).unwrap();

        let osz = root.join("set.osz");
        {
            let file = fs::File::create(&osz).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            for (name, bytes) in [("new.osu", &b"osu"[..]), ("audio.mp3", &b"mp3"[..])] {
                writer
                    .start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                std::io::Write::write_all(&mut writer, bytes).unwrap();
            }
            writer.finish().unwrap();
        }

        let folder = root.join("song");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("old.osu"), b"old").unwrap();
        fs::write(folder.join("skin.ini"), b"skin").unwrap();

        let report = extract_full_into_folder(&osz, &folder).unwrap();
        assert!(report.archived_osu_names.contains("new.osu"));
        let removed = remove_stale_osu_files(&folder, &report.archived_osu_names).unwrap();

        assert_eq!(removed, vec!["old.osu".to_owned()]);
        assert!(folder.join("new.osu").exists());
        assert!(folder.join("skin.ini").exists());
        assert!(folder.join("audio.mp3").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn beatmap_id_reads_without_full_parse() {
        let bytes = b"[Metadata]\nBeatmapID: 456\nBeatmapSetID: 123\n";
        assert_eq!(read_beatmap_id(bytes), Some(456));
        assert_eq!(read_beatmap_id(b"[Metadata]\nTitle:X\n"), None);
    }
}
