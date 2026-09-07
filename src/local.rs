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
    time::Duration,
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
}

#[derive(Debug)]
pub enum ScanEvent {
    Started {
        total_osu_files: usize,
        star_parse_error: Option<String>,
    },
    Map {
        map: LocalBeatmap,
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

#[allow(dead_code)]
pub fn scan_songs_dir(songs_dir: &Path) -> Result<LibraryScan> {
    let mut scan = LibraryScan::default();
    if !songs_dir.exists() {
        anyhow::bail!("Songs directory does not exist: {}", songs_dir.display());
    }

    for entry in
        fs::read_dir(songs_dir).with_context(|| format!("reading {}", songs_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        for osu in fs::read_dir(entry.path())? {
            let osu = osu?;
            let path = osu.path();
            if !is_osu_file(&path) {
                continue;
            }

            match parse_osu_file(&path) {
                Ok(map) => {
                    scan.problems.extend(find_repair_issues(&map));
                    scan.maps.push(map);
                }
                Err(err) => scan.problems.push(RepairIssue {
                    beatmap: path,
                    message: err.to_string(),
                    severity: RepairSeverity::ParseWarning,
                }),
            }
        }
    }

    scan.sets = build_sets(&scan.maps);

    Ok(scan)
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

            if let Some(map) = cached_maps.get(&path) {
                let issues = cached_problems.get(&path).cloned().unwrap_or_default();
                maps.push(map.clone());
                let _ = tx.send(ScanEvent::Map {
                    map: map.clone(),
                    issues,
                });
                continue;
            }

            let calculate_local_stars = db_index.is_none();
            match parse_osu_file_with_timeout(
                path.clone(),
                Duration::from_secs(8),
                calculate_local_stars,
            ) {
                Ok(mut map) => {
                    if let Some(index) = &db_index {
                        if map.stars.is_none()
                            && let Some(file_name) =
                                map.path.file_name().and_then(|file| file.to_str())
                            && let Some(meta) = index.get(&map.md5, file_name)
                        {
                            map.stars = meta.standard_stars;
                        }
                    }
                    if map.stars.is_none() && map.mode.unwrap_or(0) == 0 {
                        map.stars = calculate_stars_for_path_with_timeout(
                            path.clone(),
                            Duration::from_secs(8),
                        )
                        .ok()
                        .flatten();
                    }
                    let issues = find_repair_issues(&map);
                    maps.push(map.clone());
                    let _ = tx.send(ScanEvent::Map { map, issues });
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

fn parse_osu_file_with_timeout(
    path: PathBuf,
    timeout: Duration,
    calculate_local_stars: bool,
) -> std::result::Result<LocalBeatmap, (ParseIssueKind, anyhow::Error)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
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
    let mut bpm = None;
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

        if section == "TimingPoints" && bpm.is_none() {
            bpm = parse_timing_point_bpm(line);
        }

        if section == "HitObjects" {
            if let Some((time, object_type)) = parse_hit_object(line) {
                last_object_time =
                    Some(last_object_time.map_or(time, |current: i32| current.max(time)));
                if object_type & 1 != 0 {
                    circles += 1;
                }
                if object_type & 2 != 0 {
                    sliders += 1;
                }
            }
        }

        if let Some((key, value)) = line.split_once(':') {
            values.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }

    let folder = path.parent().unwrap_or_else(|| Path::new("")).to_owned();
    let has_mode_field = values.keys().any(|key| key.eq_ignore_ascii_case("mode"));

    let stars = calculate_local_stars
        .then(|| calculate_stars(&bytes))
        .flatten();

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
) -> std::result::Result<Option<f32>, mpsc::RecvTimeoutError> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let stars = fs::read(path)
            .ok()
            .and_then(|bytes| calculate_stars(&bytes));
        let _ = tx.send(stars);
    });
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

fn parse_timing_point_bpm(line: &str) -> Option<f32> {
    let mut fields = line.split(',');
    let _offset = fields.next()?;
    let beat_length = fields.next()?.parse::<f32>().ok()?;
    (beat_length > 0.0).then_some(60_000.0 / beat_length)
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
    let value = values
        .get("Mode")
        .or_else(|| {
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

    #[test]
    fn missing_background_is_not_reported_when_map_has_no_background() {
        let folder =
            std::env::temp_dir().join(format!("osu-map-manager-test-{}", std::process::id()));
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

        let map = parse_osu_file_with_timeout(osu_path, Duration::from_secs(1), true).unwrap();

        assert_eq!(map.mode, Some(3));
        assert!(map.has_mode_field);
        assert_eq!(map.title, "Mania Map");

        let _ = fs::remove_dir_all(folder);
    }
}
