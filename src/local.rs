use crate::osu_db::OsuDbIndex;
use anyhow::{Context, Result};
use rosu_pp::{Beatmap, Difficulty};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread,
    time::{Duration, UNIX_EPOCH},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalBeatmap {
    pub path: PathBuf,
    pub folder: PathBuf,
    pub md5: String,
    pub beatmap_id: Option<i64>,
    pub beatmapset_id: Option<i64>,
    pub artist: String,
    pub title: String,
    pub source: String,
    pub creator: String,
    pub version: String,
    pub tags: String,
    pub audio_filename: Option<String>,
    pub background_filename: Option<String>,
    pub mode: Option<u8>,
    /// Whether the .osu file declared a `Mode` field at all. std maps usually
    /// omit it; its absence is a fact about the file, not a parse failure.
    pub has_mode_field: bool,
    pub ar: Option<f32>,
    pub cs: Option<f32>,
    pub od: Option<f32>,
    pub hp: Option<f32>,
    pub stars: Option<f32>,
    pub bpm: Option<f32>,
    pub length_seconds: Option<f32>,
    pub circles: u32,
    pub sliders: u32,
}

impl LocalBeatmap {
    pub fn label(&self) -> String {
        format!("{} - {} [{}]", self.artist, self.title, self.version)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalBeatmapSet {
    pub beatmapset_id: Option<i64>,
    pub folder: PathBuf,
    pub maps: Vec<LocalBeatmap>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryScan {
    pub sets: Vec<LocalBeatmapSet>,
    pub maps: Vec<LocalBeatmap>,
    pub problems: Vec<RepairIssue>,
    /// `(file length, mtime seconds)` per scanned `.osu` path, used to detect
    /// files modified since the cached scan so they are re-parsed instead of
    /// trusted blindly.
    #[serde(default)]
    pub file_meta: BTreeMap<PathBuf, (u64, u64)>,
}

#[derive(Debug)]
pub enum ScanEvent {
    Started {
        total_osu_files: usize,
        star_parse_error: Option<String>,
    },
    Map {
        map: Box<LocalBeatmap>,
        file_len: u64,
        file_mtime_secs: u64,
        issues: Vec<RepairIssue>,
    },
    Problem {
        issue: RepairIssue,
    },
    Finished {
        sets: Vec<LocalBeatmapSet>,
    },
    Stopped {
        sets: Vec<LocalBeatmapSet>,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairIssue {
    pub beatmap: PathBuf,
    pub message: String,
    pub severity: RepairSeverity,
    /// Filename the issue is about (audio/background), carried from the scan
    /// so repair grouping can list what to restore without re-statting every
    /// flagged file. `None` for parse warnings and for scan caches written by
    /// older builds (`serde(default)` keeps those caches loadable).
    #[serde(default)]
    pub missing_file: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepairSeverity {
    MissingRequiredFile,
    ParseWarning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParseIssueKind {
    Timeout,
    ParseError,
}

pub fn scan_songs_dir_streaming(
    songs_dir: PathBuf,
    osu_root: Option<PathBuf>,
    cached_scan: Option<LibraryScan>,
    cancel: Arc<AtomicBool>,
    skip_issue_kinds: BTreeSet<String>,
    tx: Sender<ScanEvent>,
) {
    let result = scan_songs_dir_streaming_inner(
        &songs_dir,
        osu_root.as_deref(),
        cached_scan,
        &cancel,
        &skip_issue_kinds,
        &tx,
    );
    if let Err(err) = result {
        let _ = tx.send(ScanEvent::Failed {
            message: format!("{err:#}"),
        });
    }
}

fn scan_songs_dir_streaming_inner(
    songs_dir: &Path,
    osu_root: Option<&Path>,
    cached_scan: Option<LibraryScan>,
    cancel: &AtomicBool,
    skip_issue_kinds: &BTreeSet<String>,
    tx: &Sender<ScanEvent>,
) -> Result<()> {
    if !songs_dir.exists() {
        anyhow::bail!("Songs directory does not exist: {}", songs_dir.display());
    }

    let (db_index, star_parse_error) = match osu_root {
        Some(root) => match OsuDbIndex::load(root) {
            Ok(index) => (Some(index), None),
            Err(err) => (None, Some(format!("{err:#}"))),
        },
        None => (
            None,
            Some("No osu! root selected; osu!.db unavailable".to_owned()),
        ),
    };
    // Count upfront so progress can be shown as `read/total`.
    let total_osu_files = count_osu_files(songs_dir);
    let _ = tx.send(ScanEvent::Started {
        total_osu_files,
        star_parse_error,
    });
    let mut maps = Vec::new();
    let mut thread_budget = ParseThreadBudget::new(MAX_LINGERING_PARSE_THREADS);
    let cached_maps = cached_scan
        .as_ref()
        .map(|scan| {
            scan.maps
                .iter()
                .map(|map| (map.path.clone(), map.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let cached_problems = cached_scan
        .as_ref()
        .map(|scan| {
            let mut grouped = BTreeMap::<PathBuf, Vec<RepairIssue>>::new();
            for issue in &scan.problems {
                grouped
                    .entry(issue.beatmap.clone())
                    .or_default()
                    .push(issue.clone());
            }
            grouped
        })
        .unwrap_or_default();
    let cached_meta = cached_scan
        .as_ref()
        .map(|scan| scan.file_meta.clone())
        .unwrap_or_default();

    for entry in
        fs::read_dir(songs_dir).with_context(|| format!("reading {}", songs_dir.display()))?
    {
        if cancel.load(Ordering::Relaxed) {
            let _ = tx.send(ScanEvent::Stopped {
                sets: build_sets(&maps),
            });
            return Ok(());
        }

        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let folder = entry.path();

        for osu in fs::read_dir(folder)? {
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(ScanEvent::Stopped {
                    sets: build_sets(&maps),
                });
                return Ok(());
            }

            let osu = osu?;
            let path = osu.path();
            if !is_osu_file(&path) {
                continue;
            }

            // Reuse the cached entry only when the file is byte-identical to
            // the scan it came from (same length and mtime); anything else
            // falls through to a fresh parse below.
            let current_meta = file_meta(&path);
            if let Some(map) = cached_maps.get(&path)
                && cached_entry_fresh(cached_meta.get(&path), current_meta.as_ref())
            {
                let issues = cached_problems.get(&path).cloned().unwrap_or_default();
                let (file_len, file_mtime_secs) =
                    current_meta.expect("freshness implies metadata was read");
                maps.push(map.clone());
                let _ = tx.send(ScanEvent::Map {
                    map: Box::new(map.clone()),
                    file_len,
                    file_mtime_secs,
                    issues,
                });
                continue;
            }

            let calculate_local_stars = db_index.is_none();
            match parse_osu_file_with_timeout(
                path.clone(),
                Duration::from_secs(8),
                calculate_local_stars,
                &mut thread_budget,
            ) {
                Ok(mut map) => {
                    if let Some(index) = &db_index
                        && map.stars.is_none()
                        && let Some(file_name) = map.path.file_name().and_then(|file| file.to_str())
                        && let Some(meta) = index.get(&map.md5, file_name)
                    {
                        map.stars = meta.standard_stars;
                    }
                    if map.stars.is_none() && map.mode.unwrap_or(0) == 0 {
                        map.stars = calculate_stars_for_path_with_timeout(
                            path.clone(),
                            Duration::from_secs(8),
                            &mut thread_budget,
                        )
                        .ok()
                        .flatten();
                    }
                    let issues = find_repair_issues(&map);
                    // Re-stat after parsing: the file could theoretically have
                    // changed mid-parse, in which case the next scan picks up
                    // the newer metadata instead of trusting this entry.
                    let (file_len, file_mtime_secs) =
                        current_meta.or_else(|| file_meta(&path)).unwrap_or((0, 0));
                    maps.push(map.clone());
                    let _ = tx.send(ScanEvent::Map {
                        map: Box::new(map),
                        file_len,
                        file_mtime_secs,
                        issues,
                    });
                }
                Err((kind, err)) => {
                    let issue_kind = kind.as_str();
                    if skip_issue_kinds.contains(issue_kind) {
                        continue;
                    }
                    let issue = RepairIssue {
                        beatmap: path,
                        message: match kind {
                            ParseIssueKind::Timeout => {
                                format!("Timed out parsing map file after 8 seconds: {err}")
                            }
                            ParseIssueKind::ParseError => err.to_string(),
                        },
                        severity: RepairSeverity::ParseWarning,
                        missing_file: None,
                    };
                    let _ = tx.send(ScanEvent::Problem { issue });
                }
            }
        }
    }

    let _ = tx.send(ScanEvent::Finished {
        sets: build_sets(&maps),
    });
    Ok(())
}

impl ParseIssueKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Timeout => "parse_timeout",
            Self::ParseError => "parse_error",
        }
    }
}

fn build_sets(maps: &[LocalBeatmap]) -> Vec<LocalBeatmapSet> {
    let mut grouped: BTreeMap<(Option<i64>, PathBuf), Vec<LocalBeatmap>> = BTreeMap::new();
    for map in maps {
        grouped
            .entry((map.beatmapset_id, map.folder.clone()))
            .or_default()
            .push(map.clone());
    }
    grouped
        .into_iter()
        .map(|((beatmapset_id, folder), maps)| LocalBeatmapSet {
            beatmapset_id,
            folder,
            maps,
        })
        .collect()
}

/// How many timed-out parse workers may still be running before the scan
/// loop pauses spawning new ones.
const MAX_LINGERING_PARSE_THREADS: usize = 4;

/// Bounds pile-up of timed-out parse workers. The scan loop is sequential
/// (spawn, wait, next), but a worker that hits the timeout keeps running
/// detached — a run of slow files would otherwise accumulate CPU-heavy
/// threads with no bound.
struct ParseThreadBudget {
    handles: Vec<thread::JoinHandle<()>>,
    max_lingering: usize,
}

impl ParseThreadBudget {
    fn new(max_lingering: usize) -> Self {
        Self {
            handles: Vec::new(),
            max_lingering,
        }
    }

    /// Blocks until fewer than `max_lingering` previously spawned workers are
    /// still running, reaping finished ones along the way.
    fn wait_for_slot(&mut self) {
        loop {
            self.handles.retain(|handle| !handle.is_finished());
            if self.handles.len() < self.max_lingering.max(1) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn track(&mut self, handle: thread::JoinHandle<()>) {
        self.handles.push(handle);
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.handles.len()
    }
}

fn parse_osu_file_with_timeout(
    path: PathBuf,
    timeout: Duration,
    calculate_local_stars: bool,
    budget: &mut ParseThreadBudget,
) -> std::result::Result<LocalBeatmap, (ParseIssueKind, anyhow::Error)> {
    let (tx, rx) = mpsc::channel();
    budget.wait_for_slot();
    let handle = thread::spawn(move || {
        let mut result = parse_osu_file_inner(&path, false);
        if calculate_local_stars
            && let Ok(map) = &mut result
            && map.mode.unwrap_or(0) == 0
        {
            map.stars = fs::read(&path)
                .ok()
                .and_then(|bytes| calculate_stars(&bytes));
        }
        let _ = tx.send(result);
    });
    budget.track(handle);

    match rx.recv_timeout(timeout) {
        Ok(Ok(map)) => Ok(map),
        Ok(Err(err)) => Err((ParseIssueKind::ParseError, err)),
        Err(mpsc::RecvTimeoutError::Timeout) => Err((
            ParseIssueKind::Timeout,
            anyhow::anyhow!("parser did not finish in time"),
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err((
            ParseIssueKind::ParseError,
            anyhow::anyhow!("parser worker disconnected"),
        )),
    }
}

#[allow(dead_code)]
pub fn parse_osu_file(path: &Path) -> Result<LocalBeatmap> {
    parse_osu_file_inner(path, true)
}

fn parse_osu_file_inner(path: &Path, calculate_local_stars: bool) -> Result<LocalBeatmap> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let md5 = format!("{:x}", md5::compute(&bytes));
    let text = String::from_utf8_lossy(&bytes);

    let mut section = "";
    let mut values = BTreeMap::<String, String>::new();
    let mut background_filename = None;
    let mut timing_points = Vec::<(i32, f32)>::new();
    let mut last_object_time = None;
    let mut circles = 0;
    let mut sliders = 0;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
            continue;
        }

        if section == "Events" && is_background_event(line) {
            background_filename = parse_event_filename(line);
        }

        if section == "TimingPoints"
            && let Some(point) = parse_timing_point(line)
        {
            timing_points.push(point);
        }

        if section == "HitObjects"
            && let Some((time, object_type)) = parse_hit_object(line)
        {
            last_object_time =
                Some(last_object_time.map_or(time, |current: i32| current.max(time)));
            if object_type & 1 != 0 {
                circles += 1;
            }
            if object_type & 2 != 0 {
                sliders += 1;
            }
        }

        // Only sections carrying `Key: Value` metadata populate the table.
        // HitObjects/Events/TimingPoints lines contain colons too (hit-sample
        // `0:0:0:0` extras), which must not become phantom metadata keys.
        if matches!(section, "General" | "Metadata" | "Difficulty")
            && let Some((key, value)) = line.split_once(':')
        {
            values.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }

    let folder = path.parent().unwrap_or_else(|| Path::new("")).to_owned();
    let has_mode_field = values.keys().any(|key| key.eq_ignore_ascii_case("mode"));

    let stars = calculate_local_stars
        .then(|| calculate_stars(&bytes))
        .flatten();
    let bpm = main_bpm(&timing_points, last_object_time);

    Ok(LocalBeatmap {
        path: path.to_owned(),
        folder: folder.clone(),
        md5,
        beatmap_id: parse_i64(values.get("BeatmapID")),
        beatmapset_id: parse_i64(values.get("BeatmapSetID"))
            .or_else(|| parse_folder_set_id(&folder)),
        artist: values.get("Artist").cloned().unwrap_or_default(),
        title: values.get("Title").cloned().unwrap_or_default(),
        source: values.get("Source").cloned().unwrap_or_default(),
        creator: values.get("Creator").cloned().unwrap_or_default(),
        version: values.get("Version").cloned().unwrap_or_default(),
        tags: values.get("Tags").cloned().unwrap_or_default(),
        audio_filename: values.get("AudioFilename").cloned(),
        background_filename,
        mode: parse_mode(&values),
        has_mode_field,
        ar: parse_f32(values.get("ApproachRate")),
        cs: parse_f32(values.get("CircleSize")),
        od: parse_f32(values.get("OverallDifficulty")),
        hp: parse_f32(values.get("HPDrainRate")),
        stars,
        bpm,
        length_seconds: last_object_time.map(|time| time as f32 / 1000.0),
        circles,
        sliders,
    })
}

fn calculate_stars(bytes: &[u8]) -> Option<f32> {
    let beatmap = Beatmap::from_bytes(bytes).ok()?;
    let attributes = Difficulty::new().checked_calculate(&beatmap).ok()?;
    Some(attributes.stars() as f32)
}

fn calculate_stars_for_path_with_timeout(
    path: PathBuf,
    timeout: Duration,
    budget: &mut ParseThreadBudget,
) -> std::result::Result<Option<f32>, mpsc::RecvTimeoutError> {
    let (tx, rx) = mpsc::channel();
    budget.wait_for_slot();
    let handle = thread::spawn(move || {
        let stars = fs::read(path)
            .ok()
            .and_then(|bytes| calculate_stars(&bytes));
        let _ = tx.send(stars);
    });
    budget.track(handle);
    rx.recv_timeout(timeout)
}

fn find_repair_issues(map: &LocalBeatmap) -> Vec<RepairIssue> {
    let mut issues = Vec::new();
    if let Some(audio) = &map.audio_filename {
        let audio_path = map.folder.join(audio);
        if !audio_path.exists() {
            issues.push(RepairIssue {
                beatmap: map.path.clone(),
                message: format!("Missing audio file: {audio}"),
                severity: RepairSeverity::MissingRequiredFile,
                missing_file: Some(audio.clone()),
            });
        }
    }
    if let Some(background) = &map.background_filename {
        let background_path = map.folder.join(background);
        if !background_path.exists() {
            issues.push(RepairIssue {
                beatmap: map.path.clone(),
                message: format!("Missing background file: {background}"),
                severity: RepairSeverity::MissingRequiredFile,
                missing_file: Some(background.clone()),
            });
        }
    }
    issues
}

fn parse_event_filename(line: &str) -> Option<String> {
    let mut in_quotes = false;
    let mut current = String::new();
    let mut fields = Vec::new();

    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(current.trim_matches('"').to_owned());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    fields.push(current.trim_matches('"').to_owned());
    fields
        .get(2)
        .map(|value| value.trim())
        .filter(|value| is_repairable_asset_filename(value))
        .map(ToOwned::to_owned)
}

fn is_background_event(line: &str) -> bool {
    let first_field = line
        .split(',')
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(first_field.as_str(), "0" | "background")
}

fn is_repairable_asset_filename(value: &str) -> bool {
    let value = value.trim().trim_matches('"').trim();
    if value.is_empty() || value == "0" {
        return false;
    }

    let extension = Path::new(value)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);

    matches!(
        extension.as_deref(),
        Some("jpg" | "jpeg" | "png" | "webp" | "bmp")
    )
}

/// Parses a timing point into `(offset_ms, beat_length_ms)`. Only uninherited
/// (red) lines carry a tempo (`beat_length > 0`); inherited (green) lines
/// carry slider velocity instead and are skipped.
fn parse_timing_point(line: &str) -> Option<(i32, f32)> {
    let mut fields = line.split(',');
    let offset = fields.next()?.parse::<i32>().ok()?;
    let beat_length = fields.next()?.parse::<f32>().ok()?;
    (beat_length > 0.0).then_some((offset, beat_length))
}

/// The map's main tempo: the red line active for the longest stretch of the
/// song. Single-BPM maps trivially resolve to their only red line, matching
/// the old first-point behavior; multi-BPM maps now resolve to the dominant
/// tempo instead of whichever came first.
fn main_bpm(points: &[(i32, f32)], song_end_ms: Option<i32>) -> Option<f32> {
    if points.is_empty() {
        return None;
    }
    let mut sorted = points.to_vec();
    sorted.sort_by_key(|&(offset, _)| offset);
    let end = song_end_ms.unwrap_or_else(|| sorted.last().map(|&(offset, _)| offset).unwrap_or(0));
    let mut best: Option<(i32, f32)> = None;
    for (index, &(offset, beat_length)) in sorted.iter().enumerate() {
        let active_until = sorted.get(index + 1).map(|&(next, _)| next).unwrap_or(end);
        let active_ms = (active_until - offset).max(0);
        if best.is_none_or(|(best_ms, _)| active_ms > best_ms) {
            best = Some((active_ms, beat_length));
        }
    }
    best.map(|(_, beat_length)| 60_000.0 / beat_length)
}

fn parse_hit_object(line: &str) -> Option<(i32, i32)> {
    let mut fields = line.split(',');
    let _x = fields.next()?;
    let _y = fields.next()?;
    let time = fields.next()?.parse::<i32>().ok()?;
    let object_type = fields.next()?.parse::<i32>().ok()?;
    Some((time, object_type))
}

fn parse_i64(value: Option<&String>) -> Option<i64> {
    value
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
}

fn parse_folder_set_id(folder: &Path) -> Option<i64> {
    let name = folder.file_name()?.to_str()?.trim();
    let digits: String = name.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse::<i64>().ok().filter(|value| *value > 0)
}

/// Looks up the game mode tolerantly: the key match ignores case (some
/// third-party files write `mode:`), and the value parses a leading integer so
/// trailing comments or whitespace cannot silently drop the mode. A missing
/// `Mode` field means osu!std and stays `None`.
fn parse_mode(values: &BTreeMap<String, String>) -> Option<u8> {
    let value = values.get("Mode").or_else(|| {
        values
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("mode"))
            .map(|(_, value)| value)
    })?;
    let digits: String = value
        .trim()
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Whether a directory entry is a beatmap file. The comparison ignores case
/// so packs using `.OSU`/`.Osu` are not silently skipped.
pub fn is_osu_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("osu"))
}

/// `(file length, mtime seconds)` for a path, or `None` when the file cannot
/// be statted. Used to validate cached scan entries.
fn file_meta(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((meta.len(), mtime))
}

/// Whether a cached scan entry may be reused: only when both sides recorded
/// metadata and it matches. A missing cache entry (old cache version or a
/// file never scanned) always forces a fresh parse.
fn cached_entry_fresh(cached: Option<&(u64, u64)>, current: Option<&(u64, u64)>) -> bool {
    match (cached, current) {
        (Some(cached), Some(current)) => cached == current,
        _ => false,
    }
}

/// Counts beatmap files under `songs_dir` (one level of set folders) so scan
/// progress can be reported as `read/total` without parsing anything.
fn count_osu_files(songs_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(songs_dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
            && let Ok(files) = fs::read_dir(entry.path())
        {
            total += files
                .flatten()
                .filter(|file| is_osu_file(&file.path()))
                .count();
        }
    }
    total
}

fn parse_f32(value: Option<&String>) -> Option<f32> {
    value.and_then(|value| value.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Tests run in parallel and must not share one temp folder: one test's
    /// cleanup would delete another test's audio file mid-assert.
    fn unique_temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn missing_background_is_not_reported_when_map_has_no_background() {
        let folder = unique_temp_dir("osu-map-manager-test-nobg");
        fs::create_dir_all(&folder).unwrap();
        let osu_path = folder.join("no-bg.osu");
        let mut file = fs::File::create(&osu_path).unwrap();
        writeln!(
            file,
            "osu file format v14

[General]
AudioFilename: audio.mp3

[Metadata]
Title:No Background
Artist:Test
Creator:Mapper
Version:Normal
BeatmapSetID:123
BeatmapID:456

[Events]
//Background and Video events
//Break Periods

[Difficulty]
HPDrainRate:5
CircleSize:4
OverallDifficulty:7
ApproachRate:9

[HitObjects]
256,192,1000,1,0,0:0:0:0:"
        )
        .unwrap();
        fs::write(folder.join("audio.mp3"), b"fake").unwrap();

        let map = parse_osu_file(&osu_path).unwrap();
        let issues = find_repair_issues(&map);

        assert!(issues.is_empty());

        let _ = fs::remove_dir_all(folder);
    }

    #[test]
    fn missing_file_issues_carry_their_filename() {
        let folder = unique_temp_dir("osu-map-manager-test-bg");
        fs::create_dir_all(&folder).unwrap();
        let osu_path = folder.join("deleted-bg.osu");
        let mut file = fs::File::create(&osu_path).unwrap();
        writeln!(
            file,
            "osu file format v14

[General]
AudioFilename: audio.mp3

[Metadata]
Title:Deleted Background
Artist:Test
Creator:Mapper
Version:Normal
BeatmapSetID:123
BeatmapID:456

[Events]
//Background and Video events
0,0,\"bg.jpg\",0,0

[Difficulty]
HPDrainRate:5
CircleSize:4

[HitObjects]
256,192,1000,1,0,0:0:0:0:"
        )
        .unwrap();
        fs::write(folder.join("audio.mp3"), b"fake").unwrap();

        let map = parse_osu_file(&osu_path).unwrap();
        let issues = find_repair_issues(&map);

        // The background was deleted (the whole point of the issue report);
        // audio is present, so exactly one issue and it names the file.
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].missing_file.as_deref(), Some("bg.jpg"));

        let _ = fs::remove_dir_all(folder);
    }

    #[test]
    fn issues_from_older_caches_without_missing_file_still_load() {
        // Scan caches written before `missing_file` existed must keep loading.
        let issue: RepairIssue = serde_json::from_str(
            r#"{"beatmap":"C:\\osu!\\Songs\\1\\map.osu","message":"Missing background file: bg.jpg","severity":"MissingRequiredFile"}"#,
        )
        .unwrap();
        assert_eq!(issue.severity, RepairSeverity::MissingRequiredFile);
        assert_eq!(issue.missing_file, None);
    }

    #[test]
    fn main_bpm_prefers_the_longest_active_red_line() {
        // Intro at 120 BPM for 10s, then 200 BPM for 100s.
        let points = vec![(0, 500.0), (10_000, 300.0)];
        assert_eq!(main_bpm(&points, Some(110_000)), Some(200.0));
        // Order in the file does not matter.
        let shuffled = vec![(10_000, 300.0), (0, 500.0)];
        assert_eq!(main_bpm(&shuffled, Some(110_000)), Some(200.0));
        // Single red line behaves like the old first-point logic.
        assert_eq!(main_bpm(&[(500, 400.0)], Some(90_000)), Some(150.0));
        // No red lines, no tempo.
        assert_eq!(main_bpm(&[], Some(90_000)), None);
        // Unknown song end falls back to the last point's offset, so the
        // trailing red line measures zero active time here.
        assert_eq!(main_bpm(&points, None), Some(120.0));
    }

    #[test]
    fn parse_thread_budget_waits_for_lingering_workers() {
        let mut budget = ParseThreadBudget::new(1);
        budget.track(thread::spawn(|| {}));
        budget.wait_for_slot();
        assert_eq!(budget.pending(), 0);
    }

    #[test]
    fn cached_entries_are_reused_only_when_file_meta_matches() {
        assert!(cached_entry_fresh(Some(&(10, 100)), Some(&(10, 100))));
        // Changed size or mtime: re-parse.
        assert!(!cached_entry_fresh(Some(&(10, 100)), Some(&(11, 100))));
        assert!(!cached_entry_fresh(Some(&(10, 100)), Some(&(10, 101))));
        // Old caches have no metadata at all: never trust blindly.
        assert!(!cached_entry_fresh(None, Some(&(10, 100))));
        assert!(!cached_entry_fresh(Some(&(10, 100)), None));
        assert!(!cached_entry_fresh(None, None));
    }

    #[test]
    fn game_mode_parsing_is_tolerant() {
        let mut values = BTreeMap::new();
        assert_eq!(parse_mode(&values), None);

        values.insert("Mode".to_owned(), "3".to_owned());
        assert_eq!(parse_mode(&values), Some(3));

        values.insert("Mode".to_owned(), "1 // taiko".to_owned());
        assert_eq!(parse_mode(&values), Some(1));

        let mut lower = BTreeMap::new();
        lower.insert("mode".to_owned(), "2".to_owned());
        assert_eq!(parse_mode(&lower), Some(2));

        let mut bad = BTreeMap::new();
        bad.insert("Mode".to_owned(), "mania".to_owned());
        assert_eq!(parse_mode(&bad), None);
    }

    #[test]
    fn osu_file_extension_check_ignores_case() {
        assert!(is_osu_file(Path::new("song.osu")));
        assert!(is_osu_file(Path::new("SONG.OSU")));
        assert!(is_osu_file(Path::new("song.Osu")));
        assert!(!is_osu_file(Path::new("song.osb")));
        assert!(!is_osu_file(Path::new("song")));
    }

    #[test]
    fn non_std_map_metadata_parses_without_star_calculation() {
        let folder =
            std::env::temp_dir().join(format!("osu-map-manager-mode-test-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let osu_path = folder.join("mania.osu");
        fs::write(
            &osu_path,
            "osu file format v14

[General]
AudioFilename: audio.mp3
Mode: 3

[Metadata]
Title:Mania Map
Artist:Test
Creator:Mapper
Version:Keys
BeatmapSetID:123
BeatmapID:789

[Difficulty]
CircleSize:4

[HitObjects]
64,192,1000,1,0,0:0:0:0:
",
        )
        .unwrap();

        let mut budget = ParseThreadBudget::new(MAX_LINGERING_PARSE_THREADS);
        let map = parse_osu_file_with_timeout(osu_path, Duration::from_secs(1), true, &mut budget)
            .unwrap();

        assert_eq!(map.mode, Some(3));
        assert!(map.has_mode_field);
        assert_eq!(map.title, "Mania Map");

        let _ = fs::remove_dir_all(folder);
    }
}
