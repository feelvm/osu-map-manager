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
    io::{self, Read},
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
    /// Ranking status reported by osu! (`ranked`, `loved`, `qualified`,
    /// `pending`, `wip`, `graveyard`). Empty when the API omits it.
    #[serde(default)]
    pub status: String,
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
    /// Name of the `.osu` file; lets osu!.db lookups fall back to the
    /// filename when the map's md5 changed after osu! last imported it.
    pub osu_filename: String,
    pub label: String,
    pub folder: PathBuf,
}

impl LocalDiffRef {
    pub fn from_map(map: &LocalBeatmap) -> Self {
        Self {
            beatmap_id: map.beatmap_id,
            md5: map.md5.clone(),
            osu_filename: map
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
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
    /// Online ranking status at check time (`RemoteSetMeta::status`).
    pub status: String,
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

/// Whether a beatmapset in the given online ranking status is checked for
/// updates at all. Ranked, approved and qualified sets are frozen: osu!
/// does not publish a new version while a set holds one of those statuses,
/// so a checksum mismatch there is local corruption (the repair tab's job),
/// not an available update. Loved sets are skipped as well by policy, so
/// the check only spends requests on statuses whose content can change.
pub fn can_receive_updates(status: &str) -> bool {
    !matches!(
        status.to_ascii_lowercase().as_str(),
        "ranked" | "approved" | "qualified" | "loved"
    )
}

/// Same decision for the raw rank-status byte stored in osu!.db (see
/// `osu_db::DB_STATUS_*`), used to skip those sets before spending any
/// online request.
pub fn db_status_can_receive_updates(status: u8) -> bool {
    !matches!(
        status,
        crate::osu_db::DB_STATUS_RANKED
            | crate::osu_db::DB_STATUS_APPROVED
            | crate::osu_db::DB_STATUS_QUALIFIED
            | crate::osu_db::DB_STATUS_LOVED
    )
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
        status: remote.status.clone(),
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
pub type ResolvedGroups = (BTreeMap<i64, Vec<LocalDiffRef>>, Vec<LocalDiffRef>);

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
    access_token: Option<&str>,
    locals: &[LocalDiffRef],
) -> Result<ResolvedGroups> {
    let mut grouped = BTreeMap::<i64, Vec<LocalDiffRef>>::new();
    let mut unresolved = Vec::new();

    for local in locals {
        let Some(beatmap_id) = local.beatmap_id else {
            unresolved.push(local.clone());
            continue;
        };
        match fetch_beatmap_blocking(client, backend_url, access_token, beatmap_id)? {
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
    access_token: Option<&str>,
    beatmapset_id: i64,
) -> Result<Option<RemoteSetMeta>> {
    let url = format!(
        "{}/beatmapsets/{beatmapset_id}",
        backend_url.trim().trim_end_matches('/')
    );
    let mut request = client.get(&url);
    if let Some(token) = access_token
        && !token.trim().is_empty()
    {
        request = request.bearer_auth(token.trim());
    }
    let context = format!("fetching beatmapset {beatmapset_id}");
    let response = request
        .send()
        .map_err(|err| transport_error(&context, &err))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response
        .error_for_status()
        .map_err(|err| transport_error(&context, &err))?
        .json::<RemoteSetMeta>()
        .context("decoding beatmapset metadata")
        .map(Some)
}

/// Fetches one beatmap through the Worker. Returns `None` when it no longer
/// exists online.
pub fn fetch_beatmap_blocking(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    access_token: Option<&str>,
    beatmap_id: i64,
) -> Result<Option<RemoteBeatmapLookup>> {
    let url = format!(
        "{}/beatmaps/{beatmap_id}",
        backend_url.trim().trim_end_matches('/')
    );
    let mut request = client.get(&url);
    if let Some(token) = access_token
        && !token.trim().is_empty()
    {
        request = request.bearer_auth(token.trim());
    }
    let context = format!("fetching beatmap {beatmap_id}");
    let response = request
        .send()
        .map_err(|err| transport_error(&context, &err))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response
        .error_for_status()
        .map_err(|err| transport_error(&context, &err))?
        .json::<RemoteBeatmapLookup>()
        .context("decoding beatmap metadata")
        .map(Some)
}

/// Official osu! API download endpoint — the same one the website uses,
/// called with the signed-in user's own token. `download_from` appends the
/// `{id}/download` suffix; the bare `beatmapsets/{id}` endpoint is the
/// metadata API and would return JSON instead of the archive.
const OSU_API_DOWNLOAD_BASE_URL: &str = "https://osu.ppy.sh/api/v2/beatmapsets";
/// Public beatmap mirror used when there is no osu! sign-in. Downloads go
/// straight to the mirror instead of through the backend Worker.
const MIRROR_DOWNLOAD_BASE_URL: &str = "https://catboy.best/d";

/// Identifies the app to osu!'s official API.
const APP_USER_AGENT: &str = concat!("osu-map-manager/", env!("CARGO_PKG_VERSION"));
/// The mirror rejects user agents it cannot classify as a browser, so
/// mirror downloads present one.
const MIRROR_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Upper bound for a downloaded `.osz`: generous for video maps, but a
/// mirror streaming forever must not fill the disk.
pub const MAX_OSZ_DOWNLOAD_BYTES: u64 = 1_073_741_824; // 1 GiB

/// Extraction guardrails: a corrupt or hostile archive must fail fast
/// instead of filling the disk.
pub const MAX_EXTRACT_FILES: usize = 10_000;
pub const MAX_EXTRACT_BYTES: u64 = 2_147_483_648; // 2 GiB total per folder

/// Enforces the extraction guardrails against actually-written bytes (not
/// declared archive sizes, which a hostile archive can lie about).
pub fn check_extract_budget(file_count: usize, total_bytes: u64, folder: &Path) -> Result<()> {
    if file_count > MAX_EXTRACT_FILES {
        anyhow::bail!(
            "archive for {} lists too many files; refusing to extract",
            folder.display()
        );
    }
    if total_bytes > MAX_EXTRACT_BYTES {
        anyhow::bail!(
            "archive for {} is too large to extract safely",
            folder.display()
        );
    }
    Ok(())
}

/// Downloads a beatmapset `.osz` straight from the best available source:
/// the official osu! API when signed in (the user's own token, exactly what
/// the website uses), falling back to the public mirror otherwise. Returns
/// the download source (`osu-api` or `mirror`).
pub fn download_beatmapset_file(
    client: &reqwest::blocking::Client,
    beatmapset_id: i64,
    access_token: Option<&str>,
    destination: &Path,
) -> Result<String> {
    let mut signed_in_error = None;
    if let Some(token) = access_token
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        match download_from(
            client,
            &format!("{OSU_API_DOWNLOAD_BASE_URL}/{beatmapset_id}/download"),
            beatmapset_id,
            Some(token),
            APP_USER_AGENT,
            destination,
        ) {
            Ok(()) => return Ok("osu-api".to_owned()),
            Err(err) => signed_in_error = Some(err),
        }
    }
    download_from(
        client,
        &format!("{MIRROR_DOWNLOAD_BASE_URL}/{beatmapset_id}"),
        beatmapset_id,
        None,
        MIRROR_USER_AGENT,
        destination,
    )
    .map_err(|err| match signed_in_error {
        Some(official) => err.context(format!("official download failed first: {official:#}")),
        None => err,
    })
    .map(|_| "mirror".to_owned())
}

fn download_from(
    client: &reqwest::blocking::Client,
    url: &str,
    beatmapset_id: i64,
    access_token: Option<&str>,
    user_agent: &str,
    destination: &Path,
) -> Result<()> {
    let context = format!("downloading beatmapset {beatmapset_id}");
    let mut request = client
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent)
        .header(reqwest::header::ACCEPT, "application/octet-stream");
    if let Some(token) = access_token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|err| transport_error(&context, &err))?
        .error_for_status()
        .map_err(|err| transport_error(&context, &err))?;
    // A 200 can still carry metadata or an error page (wrong endpoint, a
    // queue/login interstitial). Refuse non-archive payloads here so the
    // fallback source gets a chance, instead of writing a corrupt .osz that
    // only fails later in zip validation.
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.contains("json")
        || content_type.contains("html")
        || content_type.starts_with("text/")
    {
        anyhow::bail!(
            "{context}: the server returned {content_type} instead of a beatmapset archive"
        );
    }
    let mut file = fs::File::create(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    // Read one byte past the cap so an over-long stream is detected instead
    // of being silently truncated into a corrupt archive.
    let mut limited = response.take(MAX_OSZ_DOWNLOAD_BYTES + 1);
    let copied = io::copy(&mut limited, &mut file)?;
    if copied > MAX_OSZ_DOWNLOAD_BYTES {
        anyhow::bail!("download for set {beatmapset_id} exceeds 1 GiB; refusing to keep it");
    }
    Ok(())
}

/// Renders a request failure without ever surfacing the backend URL, which
/// must not be visible to users.
pub(crate) fn transport_error(context: &str, err: &reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!("{context}: {}", describe_transport_error(err))
}

pub(crate) fn describe_transport_error(err: &reqwest::Error) -> String {
    if let Some(status) = err.status() {
        return format!("HTTP {status}");
    }
    if err.is_timeout() {
        return "request timed out".to_owned();
    }
    if err.is_connect() {
        return "could not connect".to_owned();
    }
    // reqwest appends " for url (...)" to its Display — strip it so the
    // backend URL never appears in user-facing messages.
    let text = err.to_string();
    match text.find(" for url (") {
        Some(index) => text[..index].trim_end().to_owned(),
        None => text,
    }
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
            "download for {} is not a valid .osz archive — the download was \
             interrupted or the server sent something other than the beatmapset",
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
    /// Set when the update installed cleanly but some difficulties still
    /// disagree with osu!web's checksums after a re-check — reported to the
    /// user as a note on the success instead of failing the update.
    pub checksum_note: Option<String>,
}

pub fn temp_osz_path(beatmapset_id: i64) -> PathBuf {
    std::env::temp_dir()
        .join("osu-map-manager-updates")
        .join(format!("{beatmapset_id}.osz"))
}

/// Applies a full update: downloads the latest `.osz`, overwrites every file
/// it contains, deletes local `.osu` files the new version dropped, and
/// checks each surviving difficulty against its online checksum. Remote
/// metadata is fetched fresh so the verification targets the version being
/// installed; a residual checksum disagreement is reported as a note on the
/// outcome instead of a failure, because the installed files are by then
/// byte-identical to what the download server serves (osu!'s metadata can
/// lag its own file store, and mirror copies can lag a fresh update).
pub fn apply_update_blocking(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    access_token: Option<&str>,
    job: &UpdateJob,
) -> Result<UpdateOutcome> {
    if backend_url.trim().is_empty() {
        anyhow::bail!("backend URL is required for beatmap update downloads");
    }
    let remote = fetch_set_meta_blocking(client, backend_url, access_token, job.beatmapset_id)?
        .with_context(|| {
            format!(
                "beatmapset {} is no longer available online",
                job.beatmapset_id
            )
        })?;
    let expected_checksums = checksums_from_set(&remote);

    let temp_path = temp_osz_path(job.beatmapset_id);
    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let download_source =
        download_beatmapset_file(client, job.beatmapset_id, access_token, &temp_path)?;
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

    // The freshly served archive is the download server's current copy, so a
    // checksum disagreement means the metadata and the file store disagreed
    // mid-update (cache lag, or the set changed again during the download).
    // Give the metadata one moment to settle, then re-check; whatever still
    // disagrees is surfaced as a note rather than failing the whole update —
    // the next check re-evaluates against osu!web either way.
    let mut mismatches = match updated_checksum_mismatches(job, &expected_checksums) {
        Ok(mismatches) => mismatches,
        Err(err) => {
            outcome.checksum_note = Some(format!(
                "the updated files could not be re-checked against osu!web: {err:#}"
            ));
            return Ok(outcome);
        }
    };
    if !mismatches.is_empty() {
        std::thread::sleep(std::time::Duration::from_secs(3));
        if let Ok(Some(fresh)) =
            fetch_set_meta_blocking(client, backend_url, access_token, job.beatmapset_id)
            && let Ok(fresh_mismatches) =
                updated_checksum_mismatches(job, &checksums_from_set(&fresh))
        {
            mismatches = fresh_mismatches;
        }
    }
    if !mismatches.is_empty() {
        let shown = mismatches
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = if mismatches.len() > 3 {
            format!(" (+{} more)", mismatches.len() - 3)
        } else {
            String::new()
        };
        let cause = if outcome.download_source == "mirror" {
            "the metadata may still be catching up, or the mirror's copy is stale"
        } else {
            "osu!'s metadata may still be catching up"
        };
        outcome.checksum_note = Some(format!(
            "{} {} osu!web's checksums ({shown}{more}; {cause}) — run Check for updates again later",
            mismatches.len(),
            if mismatches.len() == 1 {
                "difficulty does not match"
            } else {
                "difficulties do not match"
            },
        ));
    }
    Ok(outcome)
}

/// `beatmap id → checksum` for every difficulty the metadata carries a
/// checksum for.
fn checksums_from_set(remote: &RemoteSetMeta) -> BTreeMap<i64, String> {
    remote
        .beatmaps
        .iter()
        .filter_map(|beatmap| {
            beatmap
                .checksum
                .clone()
                .map(|checksum| (beatmap.id, checksum))
        })
        .collect()
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
    let mut extracted_files = 0_usize;
    let mut extracted_bytes = 0_u64;
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
        extracted_bytes += io::copy(&mut entry, &mut output)?;
        extracted_files += 1;
        check_extract_budget(extracted_files, extracted_bytes, folder)?;
        if let Some(file_name) = enclosed_name.file_name().and_then(|name| name.to_str()) {
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
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            removed.push(file_name);
        }
    }
    removed.sort();
    Ok(removed)
}

/// Re-reads every `.osu` in the updated folders and lists difficulties with
/// a known online checksum whose local md5 disagrees with it. Empty means
/// everything that can be confirmed matches.
fn updated_checksum_mismatches(
    job: &UpdateJob,
    expected_checksums: &BTreeMap<i64, String>,
) -> Result<Vec<String>> {
    if expected_checksums.is_empty() {
        return Ok(Vec::new());
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
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
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
    Ok(mismatched)
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
            "status": "ranked",
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
            osu_filename: String::new(),
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
        assert_eq!(set.status, "ranked");
        assert!(
            set.outdated
                .iter()
                .any(|diff| diff.label.contains("removed upstream"))
        );
    }

    #[test]
    fn skipped_statuses_cannot_receive_updates() {
        assert!(!can_receive_updates("ranked"));
        assert!(!can_receive_updates("Ranked"));
        assert!(!can_receive_updates("approved"));
        assert!(!can_receive_updates("qualified"));
        assert!(!can_receive_updates("QUALIFIED"));
        assert!(!can_receive_updates("loved"));
        assert!(!can_receive_updates("LOVED"));
        assert!(can_receive_updates("pending"));
        assert!(can_receive_updates("graveyard"));
        assert!(can_receive_updates("wip"));
        assert!(can_receive_updates(""));
    }

    #[test]
    fn skipped_db_statuses_cannot_receive_updates() {
        use crate::osu_db::{
            DB_STATUS_APPROVED, DB_STATUS_LOVED, DB_STATUS_QUALIFIED, DB_STATUS_RANKED,
        };
        assert!(!db_status_can_receive_updates(DB_STATUS_RANKED));
        assert!(!db_status_can_receive_updates(DB_STATUS_APPROVED));
        assert!(!db_status_can_receive_updates(DB_STATUS_QUALIFIED));
        assert!(!db_status_can_receive_updates(DB_STATUS_LOVED));
        assert!(db_status_can_receive_updates(2)); // pending
        assert!(db_status_can_receive_updates(0)); // unknown
    }

    #[test]
    fn everything_current_is_not_outdated() {
        let remote = RemoteSetMeta {
            id: 7,
            artist: String::new(),
            title: String::new(),
            creator: String::new(),
            status: String::new(),
            last_updated: None,
            beatmaps: vec![RemoteBeatmap {
                id: 1,
                version: String::new(),
                checksum: Some("AAA".to_owned()),
            }],
        };
        let locals = vec![local(Some(1), "aaa", "Normal")];
        let set = detect_outdated(7, "set".to_owned(), None, &locals, &remote, Vec::new());
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

    #[test]
    fn extraction_budget_allows_normal_archives() {
        let folder = Path::new("song");
        assert!(check_extract_budget(3, 1024, folder).is_ok());
        assert!(check_extract_budget(MAX_EXTRACT_FILES, MAX_EXTRACT_BYTES, folder).is_ok());
        assert!(check_extract_budget(MAX_EXTRACT_FILES + 1, 0, folder).is_err());
        assert!(check_extract_budget(0, MAX_EXTRACT_BYTES + 1, folder).is_err());
    }

    #[test]
    fn extraction_refuses_absurd_file_counts() {
        let root = std::env::temp_dir().join(format!(
            "osu-update-budget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&root).unwrap();

        let osz = root.join("many.osz");
        {
            let file = fs::File::create(&osz).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            for index in 0..=MAX_EXTRACT_FILES {
                writer
                    .start_file(
                        format!("f{index}.bin"),
                        zip::write::SimpleFileOptions::default()
                            .compression_method(zip::CompressionMethod::Stored),
                    )
                    .unwrap();
                std::io::Write::write_all(&mut writer, b"x").unwrap();
            }
            writer.finish().unwrap();
        }

        let folder = root.join("song");
        fs::create_dir_all(&folder).unwrap();
        assert!(extract_full_into_folder(&osz, &folder).is_err());

        let _ = fs::remove_dir_all(root);
    }
}
