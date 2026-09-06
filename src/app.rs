use crate::{
    collection,
    local::{self, LibraryScan, LocalBeatmap, LocalBeatmapSet, RepairSeverity, ScanEvent},
    osu_oauth::{self, OauthSession},
    query::{BeatmapQuery, Operator, QueryClause, SearchField},
    updates::{self, OutdatedSet},
};
use anyhow::{Context, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::Duration,
};

const BEATMAPSET_DOWNLOAD_DELAY: Duration = Duration::from_secs(2);
const SCAN_CACHE_VERSION: u32 = 4;
const BACKGROUND_CACHE_LIMIT: usize = 24;
const BACKGROUND_PREVIEW_WIDTH: u16 = 1200;
const BACKGROUND_PREVIEW_HEIGHT: u16 = 675;
/// How many neighbors on each side of the selected map get decoded ahead of
/// time so stepping through the list usually hits the cache.
const BACKGROUND_PREFETCH_RADIUS: usize = 3;
/// Upper bound on concurrent background decodes so fast scrolling cannot pile
/// up threads and starve the currently visible preview.
const BACKGROUND_MAX_IN_FLIGHT: usize = 4;
/// Delay between update-check metadata requests so a big library does not
/// hammer the osu! API through the Worker.
const UPDATE_CHECK_DELAY: Duration = Duration::from_millis(300);

pub struct MapManagerApp {
    active_tab: AppTab,
    query: BeatmapQuery,
    songs_dir: String,
    osu_root: String,
    collection_name: String,
    collections: Vec<collection::CollectionEntry>,
    selected_collection_index: Option<usize>,
    collection_missing_hashes: Vec<String>,
    repair_backend_url: String,
    oauth_session: Option<OauthSession>,
    oauth_status: String,
    oauth_pending_url: Option<String>,
    is_signing_in: bool,
    selected_maps: Vec<LocalBeatmap>,
    selected_md5s: BTreeSet<String>,
    filtered_map_indexes: Vec<usize>,
    filtered_cache_key: String,
    repair_jobs_cache: Vec<RepairJob>,
    repair_jobs_cache_key: String,
    scan: Option<LibraryScan>,
    is_scanning: bool,
    is_repairing: bool,
    current_folder: String,
    current_map: String,
    skip_parse_timeouts: bool,
    skip_parse_errors: bool,
    only_osu_std: bool,
    delete_taiko: bool,
    delete_catch: bool,
    delete_mania: bool,
    scanned_folders: usize,
    scanned_maps: usize,
    matched_maps: usize,
    star_ratings_loaded: usize,
    maps_with_stars: usize,
    star_parse_error: Option<String>,
    repair_progress: String,
    repair_total: usize,
    repair_done: usize,
    repair_successes: usize,
    repair_failures: usize,
    repair_log: Vec<RepairLogEntry>,
    repair_ignores: RepairIgnoreStore,
    outdated_sets: Vec<OutdatedSet>,
    is_checking_updates: bool,
    update_check_done: usize,
    update_check_total: usize,
    update_check_uncheckable: usize,
    update_unavailable: usize,
    update_check_status: String,
    is_updating: bool,
    update_progress: String,
    update_total: usize,
    update_done: usize,
    update_successes: usize,
    update_failures: usize,
    update_log: Vec<RepairLogEntry>,
    update_touched_folders: BTreeSet<PathBuf>,
    expanded_map_md5: Option<String>,
    delete_confirmation: Option<DeleteIntent>,
    background_preview_path: Option<PathBuf>,
    background_preview: Option<egui::TextureHandle>,
    background_preview_error: Option<String>,
    background_load_tx: mpsc::Sender<(PathBuf, Result<egui::ColorImage>)>,
    background_load_rx: mpsc::Receiver<(PathBuf, Result<egui::ColorImage>)>,
    background_in_flight: HashSet<PathBuf>,
    background_cache: HashMap<PathBuf, egui::TextureHandle>,
    background_cache_order: Vec<PathBuf>,
    audio_player: Option<AudioPlayer>,
    audio_volume: f32,
    status: String,
    scan_rx: Option<Receiver<ScanEvent>>,
    scan_cancel: Option<Arc<AtomicBool>>,
    repair_rx: Option<Receiver<RepairEvent>>,
    oauth_rx: Option<Receiver<Result<OauthSession>>>,
    update_check_rx: Option<Receiver<UpdateCheckEvent>>,
    update_rx: Option<Receiver<UpdateEvent>>,
}

#[derive(Debug, Clone)]
enum DeleteIntent {
    Collection(String),
    NonStdModes(usize),
    RestoreBackup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppTab {
    ScanCollections,
    Collections,
    RepairsDelete,
}

struct AudioPlayer {
    _stream: rodio::OutputStream,
    sink: rodio::Sink,
    path: PathBuf,
}

struct FfmpegPcmSource {
    child: std::process::Child,
    stdout: io::BufReader<std::process::ChildStdout>,
}

impl AudioPlayer {
    fn start(path: &Path, volume: f32) -> Result<Self> {
        let (stream, stream_handle) = rodio::OutputStream::try_default()
            .context("opening the default audio output device")?;
        let sink = rodio::Sink::try_new(&stream_handle).context("creating the audio player")?;
        let source = FfmpegPcmSource::spawn(path)?;
        sink.set_volume(volume);
        sink.append(source);
        sink.play();
        Ok(Self {
            _stream: stream,
            sink,
            path: path.to_owned(),
        })
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.sink.stop();
    }
}

impl FfmpegPcmSource {
    const CHANNELS: u16 = 2;
    const SAMPLE_RATE: u32 = 44_100;

    fn spawn(path: &Path) -> Result<Self> {
        let mut command = std::process::Command::new(ffmpeg_executable());
        command
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
            .arg(path)
            .args([
                "-vn",
                "-f",
                "s16le",
                "-acodec",
                "pcm_s16le",
                "-ac",
                "2",
                "-ar",
                "44100",
                "pipe:1",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }

        let mut child = command.spawn().with_context(
            || "starting FFmpeg; install FFmpeg or place ffmpeg.exe beside the app",
        )?;
        let stdout = child
            .stdout
            .take()
            .context("capturing decoded audio from FFmpeg")?;
        Ok(Self {
            child,
            stdout: io::BufReader::new(stdout),
        })
    }
}

impl Iterator for FfmpegPcmSource {
    type Item = i16;

    fn next(&mut self) -> Option<Self::Item> {
        let mut bytes = [0_u8; 2];
        std::io::Read::read_exact(&mut self.stdout, &mut bytes)
            .ok()
            .map(|()| i16::from_le_bytes(bytes))
    }
}

impl rodio::Source for FfmpegPcmSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        Self::CHANNELS
    }

    fn sample_rate(&self) -> u32 {
        Self::SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

impl Drop for FfmpegPcmSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
enum RepairEvent {
    Started {
        total: usize,
    },
    Opening {
        beatmapset_id: i64,
        index: usize,
        total: usize,
    },
    Repaired {
        beatmapset_id: i64,
        folder_count: usize,
        restored_files: Vec<String>,
        download_source: String,
        ignored_after_success: Vec<IgnoredRepairIssue>,
    },
    Failed {
        beatmapset_id: i64,
        message: String,
    },
    Finished,
}

#[derive(Debug)]
enum UpdateCheckEvent {
    Started {
        total: usize,
        uncheckable: usize,
    },
    Checked {
        done: usize,
        total: usize,
    },
    Found(OutdatedSet),
    Unavailable {
        beatmapset_id: Option<i64>,
        reason: String,
    },
    Finished,
}

#[derive(Debug)]
enum UpdateEvent {
    Started {
        total: usize,
    },
    Opening {
        beatmapset_id: i64,
        index: usize,
        total: usize,
    },
    Updated {
        beatmapset_id: i64,
        folders: Vec<PathBuf>,
        written: usize,
        removed: usize,
        download_source: String,
    },
    Failed {
        beatmapset_id: i64,
        message: String,
    },
    Finished,
}

#[derive(Debug, Clone)]
struct RepairLogEntry {
    beatmapset_id: i64,
    status: RepairLogStatus,
    message: String,
}

#[derive(Debug, Clone, Copy)]
enum RepairLogStatus {
    InProgress,
    Success,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RepairIgnoreStore {
    entries: Vec<IgnoredRepairIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScanCacheFile {
    version: u32,
    scan: LibraryScan,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct IgnoredRepairIssue {
    beatmapset_id: Option<i64>,
    beatmap_id: Option<i64>,
    beatmap_file: String,
    message: String,
}

impl RepairIgnoreStore {
    fn ignores(&self, map: &LocalBeatmap, issue: &local::RepairIssue) -> bool {
        if issue.severity != RepairSeverity::MissingRequiredFile {
            return false;
        }

        let file = map
            .path
            .file_name()
            .and_then(|file| file.to_str())
            .unwrap_or_default();

        self.entries.iter().any(|entry| {
            entry.beatmapset_id == map.beatmapset_id
                && entry.beatmap_id == map.beatmap_id
                && entry.beatmap_file == file
                && entry.message == issue.message
        })
    }
}

impl MapManagerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);

        let osu_root = default_osu_root()
            .map(|path| display_prefilled_path(&path))
            .unwrap_or_default();
        let songs_dir = if osu_root.is_empty() {
            String::new()
        } else {
            format!("{osu_root}\\Songs")
        };
        let repair_ignores = load_repair_ignores(&osu_root).unwrap_or_default();
        let oauth_session = osu_oauth::load_oauth_session(&osu_root);
        let oauth_status = if oauth_session.is_some() {
            "Signed in with osu! (token loaded from disk)".to_owned()
        } else {
            "Not signed in with osu!".to_owned()
        };
        let query = BeatmapQuery {
            clauses: vec![
                QueryClause {
                    field: SearchField::StarRating,
                    operator: Operator::Ge,
                    value: "6".to_owned(),
                    enabled: true,
                },
                QueryClause {
                    field: SearchField::Bpm,
                    operator: Operator::Le,
                    value: "180".to_owned(),
                    enabled: true,
                },
            ],
        };
        let cached_scan = load_scan_cache(&osu_root).ok().flatten();
        let cached_status = cached_scan.as_ref().map(|scan| {
            let matching_maps = scan
                .maps
                .iter()
                .filter(|map| matches_visible_filters(&query, true, map))
                .count();
            format!(
                "Loaded cached scan: {matching_maps} matching maps from {} scanned maps, {} sets, {} repair issue(s)",
                scan.maps.len(),
                scan.sets.len(),
                scan.problems.len()
            )
        });

        let (background_load_tx, background_load_rx) = mpsc::channel();

        let mut app = Self {
            active_tab: AppTab::ScanCollections,
            query,
            songs_dir,
            osu_root,
            collection_name: "osu-map-manager".to_owned(),
            collections: Vec::new(),
            selected_collection_index: None,
            collection_missing_hashes: Vec::new(),
            repair_backend_url: "https://osu-map-manager.stanislavberman.workers.dev".to_owned(),
            oauth_session,
            oauth_status,
            oauth_pending_url: None,
            is_signing_in: false,
            selected_maps: Vec::new(),
            selected_md5s: BTreeSet::new(),
            filtered_map_indexes: Vec::new(),
            filtered_cache_key: String::new(),
            repair_jobs_cache: Vec::new(),
            repair_jobs_cache_key: String::new(),
            scan: cached_scan,
            is_scanning: false,
            is_repairing: false,
            current_folder: String::new(),
            current_map: String::new(),
            skip_parse_timeouts: false,
            skip_parse_errors: false,
            only_osu_std: true,
            delete_taiko: true,
            delete_catch: true,
            delete_mania: true,
            scanned_folders: 0,
            scanned_maps: 0,
            matched_maps: 0,
            star_ratings_loaded: 0,
            maps_with_stars: 0,
            star_parse_error: None,
            repair_progress: String::new(),
            repair_total: 0,
            repair_done: 0,
            repair_successes: 0,
            repair_failures: 0,
            repair_log: Vec::new(),
            repair_ignores,
            outdated_sets: Vec::new(),
            is_checking_updates: false,
            update_check_done: 0,
            update_check_total: 0,
            update_check_uncheckable: 0,
            update_unavailable: 0,
            update_check_status: String::new(),
            is_updating: false,
            update_progress: String::new(),
            update_total: 0,
            update_done: 0,
            update_successes: 0,
            update_failures: 0,
            update_log: Vec::new(),
            update_touched_folders: BTreeSet::new(),
            expanded_map_md5: None,
            delete_confirmation: None,
            background_preview_path: None,
            background_preview: None,
            background_preview_error: None,
            background_load_tx,
            background_load_rx,
            background_in_flight: HashSet::new(),
            background_cache: HashMap::new(),
            background_cache_order: Vec::new(),
            audio_player: None,
            audio_volume: 0.8,
            status: cached_status.unwrap_or_else(|| "Ready".to_owned()),
            scan_rx: None,
            scan_cancel: None,
            repair_rx: None,
            oauth_rx: None,
            update_check_rx: None,
            update_rx: None,
        };
        app.load_collections();
        app
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        self.poll_background_load(ctx);
        if let Some(rx) = self.scan_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    ScanEvent::Started {
                        star_ratings_loaded,
                        star_parse_error,
                    } => {
                        self.scan = Some(LibraryScan::default());
                        self.expanded_map_md5 = None;
                        self.clear_background_preview();
                        self.selected_maps.clear();
                        self.selected_md5s.clear();
                        self.invalidate_scan_caches();
                        self.is_scanning = true;
                        self.current_folder.clear();
                        self.current_map.clear();
                        self.scanned_folders = 0;
                        self.scanned_maps = 0;
                        self.matched_maps = 0;
                        self.star_ratings_loaded = star_ratings_loaded;
                        self.maps_with_stars = 0;
                        self.star_parse_error = star_parse_error;
                        self.status = scan_progress_status(self.scanned_maps, self.matched_maps);
                    }
                    ScanEvent::Folder { path } => {
                        if self.scan_cancel.is_none() {
                            continue;
                        }
                        self.scanned_folders += 1;
                        self.current_folder = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_owned();
                        self.status = scan_progress_status(self.scanned_maps, self.matched_maps);
                    }
                    ScanEvent::Parsing { path } => {
                        if self.scan_cancel.is_none() {
                            continue;
                        }
                        self.current_map = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_owned();
                    }
                    ScanEvent::Map { map, issues } => {
                        if self.scan_cancel.is_none() {
                            continue;
                        }
                        self.scanned_maps += 1;
                        if map.stars.is_some() {
                            self.maps_with_stars += 1;
                        }
                        if matches_visible_filters(&self.query, self.only_osu_std, &map) {
                            self.matched_maps += 1;
                        }
                        let visible_issues = issues
                            .into_iter()
                            .filter(|issue| !self.repair_ignores.ignores(&map, issue))
                            .collect::<Vec<_>>();
                        if let Some(scan) = &mut self.scan {
                            scan.problems.extend(visible_issues);
                            scan.maps.push(map);
                        }
                        self.status = scan_progress_status(self.scanned_maps, self.matched_maps);
                    }
                    ScanEvent::Problem { issue } => {
                        if let Some(scan) = &mut self.scan {
                            scan.problems.push(issue);
                        }
                    }
                    ScanEvent::Finished { sets } => {
                        self.is_scanning = false;
                        self.scan_cancel = None;
                        if let Some(scan) = &mut self.scan {
                            scan.sets = sets;
                            let matching_maps = scan
                                .maps
                                .iter()
                                .filter(|map| {
                                    matches_visible_filters(&self.query, self.only_osu_std, map)
                                })
                                .count();
                            self.status = format!(
                                "Scan complete: {} matching maps from {} scanned maps, {} sets, {} repair issue(s)",
                                matching_maps,
                                scan.maps.len(),
                                scan.sets.len(),
                                scan.problems.len()
                            );
                            if let Err(err) = save_scan_cache(&self.osu_root, scan) {
                                self.status =
                                    format!("{}; cache save failed: {err:#}", self.status);
                            }
                        } else {
                            self.status = "Scan complete".to_owned();
                        }
                        keep_rx = false;
                    }
                    ScanEvent::Stopped { sets } => {
                        self.is_scanning = false;
                        self.scan_cancel = None;
                        if let Some(scan) = &mut self.scan {
                            scan.sets = sets;
                            let matching_maps = scan
                                .maps
                                .iter()
                                .filter(|map| {
                                    matches_visible_filters(&self.query, self.only_osu_std, map)
                                })
                                .count();
                            self.status = format!(
                                "Scan stopped: {} matching maps from {} scanned maps, {} sets, {} repair issue(s)",
                                matching_maps,
                                scan.maps.len(),
                                scan.sets.len(),
                                scan.problems.len()
                            );
                        } else {
                            self.status = "Scan stopped".to_owned();
                        }
                        keep_rx = false;
                    }
                    ScanEvent::Failed { message } => {
                        self.is_scanning = false;
                        self.scan_cancel = None;
                        self.status = format!("Scan failed: {message}");
                        keep_rx = false;
                    }
                }
            }

            if keep_rx {
                if disconnected {
                    self.is_scanning = false;
                    self.scan_cancel = None;
                    self.status = "Scan worker disconnected".to_owned();
                } else {
                    self.scan_rx = Some(rx);
                }
            }
        }

        if let Some(rx) = self.repair_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    RepairEvent::Started { total } => {
                        self.is_repairing = true;
                        self.repair_total = total;
                        self.repair_done = 0;
                        self.repair_successes = 0;
                        self.repair_failures = 0;
                        self.repair_log.clear();
                        self.repair_progress = format!("Preparing to repair {total} beatmapset(s)");
                        self.status = self.repair_progress.clone();
                    }
                    RepairEvent::Opening {
                        beatmapset_id,
                        index,
                        total,
                    } => {
                        self.upsert_repair_log(
                            beatmapset_id,
                            RepairLogStatus::InProgress,
                            format!("Downloading {index}/{total}"),
                        );
                        self.repair_progress =
                            format!("Downloading repair {index}/{total}: set {beatmapset_id}");
                        self.status = self.repair_progress.clone();
                    }
                    RepairEvent::Repaired {
                        beatmapset_id,
                        folder_count,
                        restored_files,
                        download_source,
                        ignored_after_success,
                    } => {
                        self.repair_done += 1;
                        self.repair_successes += 1;
                        let ignored_count = self.add_repair_ignores(ignored_after_success);
                        let restored = if restored_files.is_empty() {
                            "no missing files remained".to_owned()
                        } else {
                            format!("restored {}", restored_files.join(", "))
                        };
                        self.upsert_repair_log(
                            beatmapset_id,
                            RepairLogStatus::Success,
                            format!(
                                "Repaired {folder_count} folder(s) via {download_source}; {restored}; ignored {ignored_count} missing background issue(s)"
                            ),
                        );
                        self.repair_progress =
                            format!("Repaired set {beatmapset_id} in {folder_count} folder(s)");
                        self.status = self.repair_progress.clone();
                    }
                    RepairEvent::Failed {
                        beatmapset_id,
                        message,
                    } => {
                        self.repair_done += 1;
                        self.repair_failures += 1;
                        self.upsert_repair_log(
                            beatmapset_id,
                            RepairLogStatus::Failed,
                            message.clone(),
                        );
                        self.repair_progress =
                            format!("Repair failed for set {beatmapset_id}: {message}");
                        self.status = self.repair_progress.clone();
                    }
                    RepairEvent::Finished => {
                        self.is_repairing = false;
                        self.status = format!(
                            "Repair finished: {} succeeded, {} failed out of {}",
                            self.repair_successes, self.repair_failures, self.repair_total
                        );
                        keep_rx = false;
                    }
                }
            }

            if keep_rx {
                if disconnected {
                    self.is_repairing = false;
                    self.status = "Repair worker disconnected".to_owned();
                } else {
                    self.repair_rx = Some(rx);
                }
            }
        }

        if let Some(rx) = self.oauth_rx.take() {
            match rx.try_recv() {
                Ok(result) => {
                    self.is_signing_in = false;
                    self.oauth_pending_url = None;
                    match result {
                        Ok(session) => {
                            self.oauth_session = Some(session.clone());
                            if let Err(err) =
                                osu_oauth::save_oauth_session(&self.osu_root, &session)
                            {
                                self.oauth_status =
                                    format!("Signed in, but saving the session failed: {err:#}");
                            } else {
                                self.oauth_status =
                                    "Signed in with osu! (downloads use the official osu! API)"
                                        .to_owned();
                            }
                            self.status = self.oauth_status.clone();
                        }
                        Err(err) => {
                            self.oauth_status = format!("osu! sign-in failed: {err:#}");
                            self.status = self.oauth_status.clone();
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.oauth_rx = Some(rx);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.is_signing_in = false;
                    self.oauth_status = "osu! sign-in was cancelled".to_owned();
                }
            }
        }

        if let Some(rx) = self.update_check_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    UpdateCheckEvent::Started { total, uncheckable } => {
                        self.is_checking_updates = true;
                        self.update_check_done = 0;
                        self.update_check_total = total;
                        self.update_check_uncheckable = uncheckable;
                        self.update_unavailable = 0;
                        self.outdated_sets.clear();
                        self.update_check_status =
                            format!("Checking {total} beatmapset(s) against osu!web");
                        self.status = self.update_check_status.clone();
                    }
                    UpdateCheckEvent::Checked { done, total } => {
                        self.update_check_done = done;
                        self.update_check_status = format!(
                            "Checked {done}/{total} beatmapset(s), {} outdated",
                            self.outdated_sets.len()
                        );
                        self.status = self.update_check_status.clone();
                    }
                    UpdateCheckEvent::Found(set) => {
                        upsert_log(
                            &mut self.update_log,
                            set.beatmapset_id,
                            RepairLogStatus::InProgress,
                            format!(
                                "{} of {} diffs outdated",
                                set.outdated_count(),
                                set.total_checked()
                            ),
                        );
                        self.outdated_sets.push(set);
                    }
                    UpdateCheckEvent::Unavailable { beatmapset_id, reason } => {
                        self.update_unavailable += 1;
                        if let Some(set_id) = beatmapset_id {
                            upsert_log(
                                &mut self.update_log,
                                set_id,
                                RepairLogStatus::Failed,
                                reason,
                            );
                        }
                    }
                    UpdateCheckEvent::Finished => {
                        self.is_checking_updates = false;
                        self.update_check_status = format!(
                            "Update check finished: {} outdated out of {} checked ({} unavailable, {} uncheckable)",
                            self.outdated_sets.len(),
                            self.update_check_total,
                            self.update_unavailable,
                            self.update_check_uncheckable
                        );
                        self.status = self.update_check_status.clone();
                        keep_rx = false;
                    }
                }
            }

            if keep_rx {
                if disconnected {
                    self.is_checking_updates = false;
                    self.status = "Update check worker disconnected".to_owned();
                } else {
                    self.update_check_rx = Some(rx);
                }
            }
        }

        if let Some(rx) = self.update_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    UpdateEvent::Started { total } => {
                        self.is_updating = true;
                        self.update_total = total;
                        self.update_done = 0;
                        self.update_successes = 0;
                        self.update_failures = 0;
                        self.update_touched_folders.clear();
                        self.update_progress =
                            format!("Preparing to update {total} beatmapset(s)");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Opening {
                        beatmapset_id,
                        index,
                        total,
                    } => {
                        upsert_log(
                            &mut self.update_log,
                            beatmapset_id,
                            RepairLogStatus::InProgress,
                            format!("Downloading {index}/{total}"),
                        );
                        self.update_progress =
                            format!("Downloading update {index}/{total}: set {beatmapset_id}");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Updated {
                        beatmapset_id,
                        folders,
                        written,
                        removed,
                        download_source,
                    } => {
                        self.update_done += 1;
                        self.update_successes += 1;
                        self.update_touched_folders.extend(folders.clone());
                        upsert_log(
                            &mut self.update_log,
                            beatmapset_id,
                            RepairLogStatus::Success,
                            format!(
                                "Updated via {download_source}: {written} file(s) written, {removed} removed upstream"
                            ),
                        );
                        self.update_progress =
                            format!("Updated set {beatmapset_id}");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Failed {
                        beatmapset_id,
                        message,
                    } => {
                        self.update_done += 1;
                        self.update_failures += 1;
                        upsert_log(
                            &mut self.update_log,
                            beatmapset_id,
                            RepairLogStatus::Failed,
                            message.clone(),
                        );
                        self.update_progress =
                            format!("Update failed for set {beatmapset_id}: {message}");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Finished => {
                        self.is_updating = false;
                        self.outdated_sets.retain(|set| {
                            !self.update_log.iter().any(|entry| {
                                entry.beatmapset_id == set.beatmapset_id
                                    && matches!(entry.status, RepairLogStatus::Success)
                            })
                        });
                        self.status = format!(
                            "Update finished: {} succeeded, {} failed out of {}",
                            self.update_successes, self.update_failures, self.update_total
                        );
                        let touched =
                            std::mem::take(&mut self.update_touched_folders);
                        if !touched.is_empty() {
                            self.prune_scan_folders(&touched);
                            self.status.push_str("; rescanning updated files");
                        }
                        keep_rx = false;
                        if !touched.is_empty() && !self.is_scanning {
                            self.start_scan();
                        }
                    }
                }
            }

            if keep_rx {
                if disconnected {
                    self.is_updating = false;
                    self.status = "Update worker disconnected".to_owned();
                } else {
                    self.update_rx = Some(rx);
                }
            }
        }
    }

    fn poll_background_load(&mut self, ctx: &egui::Context) {
        // Drain every completed decode (current preview plus prefetches) so a
        // burst of finished workers cannot stall behind a single try_recv.
        loop {
            match self.background_load_rx.try_recv() {
                Ok((path, result)) => {
                    self.background_in_flight.remove(&path);
                    match result {
                        Ok(image) => {
                            let texture = ctx.load_texture(
                                format!("map-background:{}", path.display()),
                                image,
                                egui::TextureOptions::LINEAR,
                            );
                            self.cache_background(path.clone(), texture.clone());
                            if self.background_preview_path.as_deref() == Some(&path) {
                                self.background_preview = Some(texture);
                                self.background_preview_error = None;
                            }
                        }
                        Err(err) => {
                            if self.background_preview_path.as_deref() == Some(&path) {
                                self.background_preview_error = Some(format!(
                                    "Could not load background: {err:#}"
                                ));
                            }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }

    fn start_scan(&mut self) {
        if self.is_scanning {
            self.status = "A scan is already running".to_owned();
            return;
        }

        let songs_dir = expand_prefilled_path(&self.songs_dir);
        let osu_root =
            (!self.osu_root.trim().is_empty()).then(|| expand_prefilled_path(&self.osu_root));
        let cached_scan = self.scan.clone();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let mut skip_issue_kinds = BTreeSet::new();
        if self.skip_parse_timeouts {
            skip_issue_kinds.insert("parse_timeout".to_owned());
        }
        if self.skip_parse_errors {
            skip_issue_kinds.insert("parse_error".to_owned());
        }
        self.status = scan_progress_status(self.scanned_maps, self.matched_maps);
        std::thread::spawn(move || {
            local::scan_songs_dir_streaming(
                songs_dir,
                osu_root,
                cached_scan,
                worker_cancel,
                skip_issue_kinds,
                tx,
            );
        });
        self.scan_cancel = Some(cancel);
        self.scan_rx = Some(rx);
        self.background_cache.clear();
        self.background_cache_order.clear();
        self.background_in_flight.clear();
    }

    fn stop_scan(&mut self) {
        if let Some(cancel) = &self.scan_cancel {
            cancel.store(true, Ordering::Relaxed);
            self.is_scanning = false;
            self.scan_cancel = None;
            self.status = format!(
                "Scan stop requested: {} maps read, {} match current filters",
                self.scanned_maps, self.matched_maps
            );
        }
    }

    fn create_collection(&mut self, name: &str) {
        let path = self.collection_db_path();
        match collection::create_collection(&path, name) {
            Ok(()) => {
                self.status = format!("Created collection \"{name}\" in {}", path.display());
                self.load_collections();
                self.selected_collection_index = self
                    .collections
                    .iter()
                    .position(|collection| collection.name == name);
            }
            Err(err) => self.status = format!("Create collection failed: {err:#}"),
        }
    }

    fn add_selected_to_collection(&mut self, name: &str) {
        if self.selected_maps.is_empty() && self.selected_md5s.is_empty() {
            self.status = "Select at least one map before adding to a collection".to_owned();
            return;
        }

        let path = self.collection_db_path();
        let mut hashes = self
            .selected_maps
            .iter()
            .map(|map| map.md5.clone())
            .collect::<Vec<_>>();
        for hash in &self.collection_missing_hashes {
            if self.selected_md5s.contains(hash) && !hashes.iter().any(|h| h == hash) {
                hashes.push(hash.clone());
            }
        }

        match collection::add_to_collection(&path, name, &hashes) {
            Ok(()) => {
                self.status =
                    format!("Added {} map(s) to collection \"{name}\"", hashes.len());
                self.load_collections();
            }
            Err(err) => self.status = format!("Add to collection failed: {err:#}"),
        }
    }

    fn export_manifest(&mut self) {
        let path = PathBuf::from("selected_maps.tsv");
        match collection::write_manifest(&path, &self.selected_maps) {
            Ok(()) => self.status = format!("Wrote {}", path.display()),
            Err(err) => self.status = format!("Manifest export failed: {err:#}"),
        }
    }

    fn restore_collection_backup(&mut self) {
        let path = self.collection_db_path();

        match collection::restore_collection_backup(&path) {
            Ok(()) => {
                self.status = format!(
                    "Restored {} from {}",
                    path.display(),
                    path.with_extension("db.bak").display()
                );
                self.load_collections();
            }
            Err(err) => self.status = format!("Collection restore failed: {err:#}"),
        }
    }

    fn collection_db_path(&self) -> PathBuf {
        if self.osu_root.trim().is_empty() {
            PathBuf::from("collection.db")
        } else {
            expand_prefilled_path(&self.osu_root).join("collection.db")
        }
    }

    fn load_collections(&mut self) {
        let path = self.collection_db_path();
        match collection::load_collection_db(&path) {
            Ok(db) => {
                let count = db.collections.len();
                self.collections = db.collections;
                self.selected_collection_index = self
                    .selected_collection_index
                    .filter(|&index| index < self.collections.len());
                self.collection_missing_hashes.clear();
                self.status = format!("Loaded {count} collection(s) from {}", path.display());
            }
            Err(err) => {
                self.collections.clear();
                self.selected_collection_index = None;
                self.collection_missing_hashes.clear();
                self.status = format!("Collection load failed: {err:#}");
            }
        }
    }

    fn load_selected_collection_into_selection(&mut self) {
        let Some(collection) = self
            .selected_collection_index
            .and_then(|index| self.collections.get(index))
            .cloned()
        else {
            self.status = "Select a collection to load".to_owned();
            return;
        };

        self.collection_name = collection.name.clone();
        self.selected_maps.clear();
        self.selected_md5s = collection.hashes.iter().cloned().collect();
        self.collection_missing_hashes.clear();

        if let Some(scan) = &self.scan {
            let by_hash = scan
                .maps
                .iter()
                .map(|map| (map.md5.as_str(), map))
                .collect::<BTreeMap<_, _>>();
            for hash in &collection.hashes {
                if let Some(map) = by_hash.get(hash.as_str()) {
                    self.selected_maps.push((*map).clone());
                } else {
                    self.collection_missing_hashes.push(hash.clone());
                }
            }
            self.status = format!(
                "Loaded {} scanned map(s) from {}; {} hash(es) were not found in the current scan",
                self.selected_maps.len(),
                collection.name,
                self.collection_missing_hashes.len()
            );
        } else {
            self.collection_missing_hashes = collection.hashes.clone();
            self.status = format!(
                "Loaded {} hash(es) from {}; scan Songs to match them to local maps",
                collection.hashes.len(),
                collection.name
            );
        }
    }

    fn save_selection_to_collection(&mut self) {
        let path = self.collection_db_path();
        let original_name = self
            .selected_collection_index
            .and_then(|index| self.collections.get(index))
            .map(|collection| collection.name.clone());
        let mut db = match collection::load_collection_db(&path) {
            Ok(db) => db,
            Err(err) if !path.exists() => {
                let _ = err;
                collection::CollectionDb::empty()
            }
            Err(err) => {
                self.status = format!("Collection save failed: {err:#}");
                return;
            }
        };

        if let Some(original_name) = original_name.as_deref()
            && original_name != self.collection_name
        {
            collection::delete_collection(&mut db, original_name);
        }

        let mut hashes = self
            .selected_maps
            .iter()
            .map(|map| map.md5.clone())
            .collect::<Vec<_>>();
        for hash in &self.collection_missing_hashes {
            if !self.selected_md5s.contains(hash) {
                continue;
            }
            if !hashes.iter().any(|existing| existing == hash) {
                hashes.push(hash.clone());
            }
        }

        collection::upsert_collection_hashes(&mut db, &self.collection_name, hashes);
        match collection::write_db(&path, &db) {
            Ok(()) => {
                self.status = format!(
                    "Saved collection {} to {}",
                    self.collection_name,
                    path.display()
                );
                self.load_collections();
                self.selected_collection_index = self
                    .collections
                    .iter()
                    .position(|collection| collection.name == self.collection_name);
            }
            Err(err) => self.status = format!("Collection save failed: {err:#}"),
        }
    }

    fn delete_selected_collection(&mut self) {
        let Some(collection_name) = self
            .selected_collection_index
            .and_then(|index| self.collections.get(index))
            .map(|collection| collection.name.clone())
        else {
            self.status = "Select a collection to delete".to_owned();
            return;
        };

        let path = self.collection_db_path();
        let mut db = match collection::load_collection_db(&path) {
            Ok(db) => db,
            Err(err) => {
                self.status = format!("Collection delete failed: {err:#}");
                return;
            }
        };

        if !collection::delete_collection(&mut db, &collection_name) {
            self.status = format!("Collection not found: {collection_name}");
            return;
        }

        match collection::write_db(&path, &db) {
            Ok(()) => {
                self.status = format!("Deleted collection {collection_name}");
                self.selected_collection_index = None;
                self.collection_missing_hashes.clear();
                self.load_collections();
            }
            Err(err) => self.status = format!("Collection delete failed: {err:#}"),
        }
    }

    fn delete_selected_non_std_modes(&mut self) {
        let selection = DeleteModeSelection {
            taiko: self.delete_taiko,
            catch: self.delete_catch,
            mania: self.delete_mania,
        };
        if !selection.any() {
            self.status = "Select at least one non-std mode to delete".to_owned();
            return;
        }

        let Some(scan) = &mut self.scan else {
            self.status = "Scan your Songs directory before deleting non-std maps".to_owned();
            return;
        };

        let targets = scan
            .maps
            .iter()
            .filter(|map| selection.matches(map.mode))
            .map(|map| map.path.clone())
            .collect::<BTreeSet<_>>();
        if targets.is_empty() {
            self.status = "No scanned maps match the selected non-std modes".to_owned();
            return;
        }

        let mut deleted = BTreeSet::new();
        let mut failures = Vec::new();
        for path in &targets {
            match fs::remove_file(path) {
                Ok(()) => {
                    deleted.insert(path.clone());
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    deleted.insert(path.clone());
                }
                Err(err) => failures.push(format!("{}: {err}", path.display())),
            }
        }

        if !deleted.is_empty() {
            scan.maps.retain(|map| !deleted.contains(&map.path));
            scan.problems
                .retain(|issue| !deleted.contains(&issue.beatmap));
            scan.sets = build_sets_for_scan(&scan.maps);
            let only_osu_std = self.only_osu_std;
            self.retain_selected_maps(|map| {
                !deleted.contains(&map.path) && (!only_osu_std || is_osu_std(map))
            });
            self.invalidate_scan_caches();
        }

        self.status = if failures.is_empty() {
            format!("Deleted {} non-std .osu file(s)", deleted.len())
        } else {
            format!(
                "Deleted {} non-std .osu file(s); {} deletion(s) failed: {}",
                deleted.len(),
                failures.len(),
                failures.join("; ")
            )
        };
    }

    fn start_repair_all(&mut self) {
        if self.is_repairing {
            self.status = "A repair job is already running".to_owned();
            return;
        }

        if self.scan.is_none() {
            self.status = "Scan your Songs directory before repairing maps".to_owned();
            return;
        }

        self.refresh_repair_jobs();
        let jobs = self.repair_jobs_cache.clone();
        if jobs.is_empty() {
            self.status = "No repairable corrupted beatmapsets found".to_owned();
            return;
        }

        self.spawn_repair_jobs(jobs);
    }

    fn start_repair_single(&mut self, beatmapset_id: i64) {
        if self.is_repairing {
            self.status = "A repair job is already running".to_owned();
            return;
        }

        self.refresh_repair_jobs();
        let Some(job) = self
            .repair_jobs_cache
            .iter()
            .find(|job| job.beatmapset_id == beatmapset_id)
            .cloned()
        else {
            self.status = format!("No repair job found for set {beatmapset_id}");
            return;
        };

        self.spawn_repair_jobs(vec![job]);
    }

    fn spawn_repair_jobs(&mut self, jobs: Vec<RepairJob>) {
        let (tx, rx) = mpsc::channel();
        self.status = format!("Starting repair for {} beatmapset(s)", jobs.len());
        let backend_url = self.repair_backend_url.trim().to_owned();
        let osu_root = self.osu_root.clone();
        let oauth_session = self.oauth_session.clone();
        std::thread::spawn(move || {
            run_repair_jobs(jobs, backend_url, osu_root, oauth_session, tx);
        });
        self.repair_rx = Some(rx);
    }

    fn start_update_check(&mut self) {
        if self.is_checking_updates || self.is_updating {
            self.status = "An update check or update is already running".to_owned();
            return;
        }

        let Some(scan) = &self.scan else {
            self.status = "Scan your Songs directory before checking for updates".to_owned();
            return;
        };

        if self.repair_backend_url.trim().is_empty() {
            self.status = "Set the backend URL before checking for updates".to_owned();
            return;
        }

        let (targets, uncheckable) = updates::build_check_targets(&scan.maps);
        if targets.is_empty() {
            self.status = "No beatmapsets with online ids found to check".to_owned();
            return;
        }

        let (tx, rx) = mpsc::channel();
        self.update_check_rx = Some(rx);
        self.update_log.clear();
        self.status = format!("Starting update check for {} beatmapset(s)", targets.len());
        let backend_url = self.repair_backend_url.trim().to_owned();
        std::thread::spawn(move || {
            run_update_check(targets, uncheckable, backend_url, tx);
        });
    }

    fn update_job_for_set(&self, beatmapset_id: i64) -> Option<updates::UpdateJob> {
        self.outdated_sets
            .iter()
            .find(|set| set.beatmapset_id == beatmapset_id)
            .map(|set| updates::UpdateJob {
                beatmapset_id,
                folders: set.folders.clone(),
            })
    }

    fn start_update_all(&mut self) {
        if self.is_updating || self.is_checking_updates {
            self.status = "An update check or update is already running".to_owned();
            return;
        }
        if self.outdated_sets.is_empty() {
            self.status = "Check for updates first".to_owned();
            return;
        }
        let jobs = self
            .outdated_sets
            .iter()
            .filter_map(|set| self.update_job_for_set(set.beatmapset_id))
            .collect::<Vec<_>>();
        self.spawn_update_jobs(jobs);
    }

    fn start_update_single(&mut self, beatmapset_id: i64) {
        if self.is_updating || self.is_checking_updates {
            self.status = "An update check or update is already running".to_owned();
            return;
        }
        let Some(job) = self.update_job_for_set(beatmapset_id) else {
            self.status = format!("No pending update found for set {beatmapset_id}");
            return;
        };
        self.spawn_update_jobs(vec![job]);
    }

    fn spawn_update_jobs(&mut self, jobs: Vec<updates::UpdateJob>) {
        if jobs.is_empty() {
            self.status = "Nothing to update".to_owned();
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        self.status = format!("Starting update for {} beatmapset(s)", jobs.len());
        let backend_url = self.repair_backend_url.trim().to_owned();
        let osu_root = self.osu_root.clone();
        let oauth_session = self.oauth_session.clone();
        std::thread::spawn(move || {
            run_update_jobs(jobs, backend_url, osu_root, oauth_session, tx);
        });
    }

    /// Drops every scan entry under the given folders so the next scan
    /// reparses the updated files instead of reusing stale cached data.
    fn prune_scan_folders(&mut self, folders: &BTreeSet<PathBuf>) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        scan.maps.retain(|map| !folders.contains(&map.folder));
        scan.problems.retain(|issue| {
            issue
                .beatmap
                .parent()
                .is_none_or(|parent| !folders.contains(parent))
        });
        scan.sets = build_sets_for_scan(&scan.maps);
        let expanded_gone = self
            .expanded_map_md5
            .as_ref()
            .is_some_and(|md5| !scan.maps.iter().any(|map| &map.md5 == md5));

        self.retain_selected_maps(|map| !folders.contains(&map.folder));
        if expanded_gone {
            self.expanded_map_md5 = None;
            self.clear_background_preview();
        }
        self.invalidate_scan_caches();
    }

    fn start_oauth_login(&mut self) {
        if self.is_signing_in {
            self.status = "osu! sign-in is already in progress".to_owned();
            return;
        }
        let backend_url = self.repair_backend_url.trim().to_owned();
        let backend = match osu_oauth::validated_backend_base_url(&backend_url) {
            Ok(backend) => backend,
            Err(err) => {
                self.oauth_status = format!("osu! sign-in failed: {err:#}");
                self.status = self.oauth_status.clone();
                return;
            }
        };
        let state = osu_oauth::generate_state();
        self.oauth_pending_url = Some(osu_oauth::authorize_url(&backend, &state));
        let (tx, rx) = mpsc::channel();
        self.oauth_rx = Some(rx);
        self.is_signing_in = true;
        self.oauth_status = "Waiting for osu! sign-in in your browser...".to_owned();
        self.status = self.oauth_status.clone();
        std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build();
            let result = match client {
                Ok(client) => osu_oauth::login_with_state_blocking(&client, &backend_url, &state),
                Err(err) => Err(err.into()),
            };
            let _ = tx.send(result);
        });
    }

    fn sign_out(&mut self) {
        self.oauth_session = None;
        self.oauth_pending_url = None;
        osu_oauth::clear_oauth_session(&self.osu_root);
        self.oauth_status = "Not signed in with osu! (downloads use the mirror)".to_owned();
        self.status = self.oauth_status.clone();
    }

    fn upsert_repair_log(&mut self, beatmapset_id: i64, status: RepairLogStatus, message: String) {
        upsert_log(&mut self.repair_log, beatmapset_id, status, message);
    }

    fn add_repair_ignores(&mut self, entries: Vec<IgnoredRepairIssue>) -> usize {
        let mut added = 0;
        for entry in entries {
            if !self.repair_ignores.entries.contains(&entry) {
                self.repair_ignores.entries.push(entry);
                added += 1;
            }
        }
        if added > 0 {
            if let Err(err) = save_repair_ignores(&self.osu_root, &self.repair_ignores) {
                self.status = format!("Repair ignore save failed: {err:#}");
            }
        }
        added
    }

    fn invalidate_scan_caches(&mut self) {
        self.filtered_cache_key.clear();
        self.filtered_map_indexes.clear();
        self.repair_jobs_cache_key.clear();
        self.repair_jobs_cache.clear();
    }

    fn filtered_cache_key(&self) -> String {
        let map_count = self.scan.as_ref().map_or(0, |scan| scan.maps.len());
        let query = serde_json::to_string(&self.query).unwrap_or_default();
        format!("{map_count}:{}:{query}", self.only_osu_std)
    }

    fn refresh_filtered_maps(&mut self) {
        let key = self.filtered_cache_key();
        if key == self.filtered_cache_key {
            return;
        }

        self.filtered_map_indexes.clear();
        if let Some(scan) = &self.scan {
            self.filtered_map_indexes
                .extend(scan.maps.iter().enumerate().filter_map(|(index, map)| {
                    matches_visible_filters(&self.query, self.only_osu_std, map).then_some(index)
                }));
        }
        self.filtered_cache_key = key;
    }

    fn repair_jobs_cache_key(&self) -> String {
        let Some(scan) = &self.scan else {
            return String::new();
        };
        format!("{}:{}", scan.maps.len(), scan.problems.len())
    }

    fn refresh_repair_jobs(&mut self) {
        let key = self.repair_jobs_cache_key();
        if key == self.repair_jobs_cache_key {
            return;
        }

        self.repair_jobs_cache = self.scan.as_ref().map(repair_jobs).unwrap_or_default();
        self.repair_jobs_cache_key = key;
    }

    fn clear_selection(&mut self) {
        self.selected_maps.clear();
        self.selected_md5s.clear();
    }

    fn select_map(&mut self, map: &LocalBeatmap) {
        if self.selected_md5s.insert(map.md5.clone()) {
            self.selected_maps.push(map.clone());
        }
    }

    fn deselect_md5(&mut self, md5: &str) {
        self.selected_md5s.remove(md5);
        self.selected_maps.retain(|selected| selected.md5 != md5);
    }

    fn select_missing_hash(&mut self, md5: &str) {
        self.selected_md5s.insert(md5.to_owned());
    }

    fn retain_selected_maps(&mut self, mut keep: impl FnMut(&LocalBeatmap) -> bool) {
        self.selected_maps.retain(|map| keep(map));
        self.selected_md5s = self
            .selected_maps
            .iter()
            .map(|map| map.md5.clone())
            .collect();
    }

    fn map_result_label(&self, map: &LocalBeatmap) -> String {
        let mut seen_fields = BTreeSet::new();
        let mut fields = self
            .query
            .clauses
            .iter()
            .filter(|clause| clause.enabled && !clause.value.trim().is_empty())
            .filter(|clause| seen_fields.insert(clause.field))
            .filter_map(|clause| label_value_for_field(map, clause.field))
            .collect::<Vec<_>>();

        if fields.is_empty() {
            if let Some(stars) = map.stars {
                fields.push(format!("*{}", format_number(stars)));
            }
        }

        let title = format!("{} - {}", map.artist, map.title);
        if fields.is_empty() {
            title
        } else {
            format!("{title} ({})", fields.join(", "))
        }
    }

    fn render_map_workspace(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, max_height: f32) {
        let workspace_rect = egui::Rect::from_min_size(
            ui.cursor().min,
            egui::vec2(ui.available_width().max(1.0), max_height.max(1.0)),
        );
        let gap = 10.0;
        let list_width = (workspace_rect.width() * 0.42).clamp(300.0, 430.0);
        let inspector_left = workspace_rect.left() + list_width + gap;
        let list_rect = egui::Rect::from_min_size(
            workspace_rect.min,
            egui::vec2(list_width, workspace_rect.height()),
        );
        let inspector_rect = egui::Rect::from_min_max(
            egui::pos2(inspector_left, workspace_rect.top()),
            workspace_rect.right_bottom(),
        );

        let mut list_ui = ui.child_ui_with_id_source(
            list_rect,
            egui::Layout::top_down(egui::Align::Min),
            "map_list_pane",
        );
        list_ui.set_clip_rect(list_rect);
        list_ui.set_width(list_width);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(0x20, 0x21, 0x24))
            .stroke(egui::Stroke::new(
                1.0,
                egui::Color32::from_rgb(0x3a, 0x3b, 0x40),
            ))
            .show(&mut list_ui, |ui| {
                self.render_map_browser(ui, max_height - 2.0);
            });

        let inspected_map = self.expanded_map_md5.as_ref().and_then(|md5| {
            self.scan
                .as_ref()
                .and_then(|scan| scan.maps.iter().find(|map| &map.md5 == md5))
                .cloned()
        });
        if let Some(map) = inspected_map {
            self.render_map_inspector(ui, ctx, inspector_rect, &map);
        } else {
            ui.allocate_ui_at_rect(inspector_rect, |ui| {
                inspector_frame(ctx.style().as_ref()).show(ui, |ui| {
                    fill_tile_width(ui);
                    ui.set_min_height((inspector_rect.height() - 18.0).max(1.0));
                    ui.centered_and_justified(|ui| {
                        muted_label(ui, "Select a map from the list to inspect it.");
                    });
                });
            });
        }

        ui.allocate_rect(workspace_rect, egui::Sense::hover());
    }

    fn render_map_browser(&mut self, ui: &mut egui::Ui, max_height: f32) {
        const HEADER_HEIGHT: f32 = 46.0;

        let inspected_row = self.expanded_map_md5.as_ref().and_then(|expanded_md5| {
            self.scan.as_ref().and_then(|scan| {
                self.filtered_map_indexes.iter().position(|&index| {
                    scan.maps
                        .get(index)
                        .is_some_and(|map| &map.md5 == expanded_md5)
                })
            })
        });
        let row_count = self.filtered_map_indexes.len();
        let total_height = row_count as f32 * HEADER_HEIGHT;

        egui::ScrollArea::vertical()
            .id_source("local_maps")
            .max_height(max_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width().max(1.0);
                let (list_rect, _) = ui.allocate_exact_size(
                    egui::vec2(width, total_height.max(1.0)),
                    egui::Sense::hover(),
                );
                let clip_rect = ui.clip_rect();

                for row in 0..row_count {
                    let row_top = list_rect.top() + row as f32 * HEADER_HEIGHT;
                    let is_inspected = inspected_row == Some(row);
                    let row_rect = egui::Rect::from_min_size(
                        egui::pos2(list_rect.left(), row_top),
                        egui::vec2(width, HEADER_HEIGHT),
                    );
                    if !row_rect.intersects(clip_rect) {
                        continue;
                    }

                    let Some(map) = self.scan.as_ref().and_then(|scan| {
                        self.filtered_map_indexes
                            .get(row)
                            .and_then(|&index| scan.maps.get(index))
                            .cloned()
                    }) else {
                        continue;
                    };

                    let header_rect =
                        egui::Rect::from_min_size(row_rect.min, egui::vec2(width, HEADER_HEIGHT));
                    let selected = self.selected_md5s.contains(&map.md5);
                    let fill = if is_inspected {
                        egui::Color32::from_rgb(0x30, 0x2e, 0x2a)
                    } else if selected {
                        egui::Color32::from_rgb(0x2d, 0x2b, 0x27)
                    } else {
                        egui::Color32::from_rgb(0x23, 0x24, 0x27)
                    };
                    ui.painter().rect_filled(header_rect, 0.0, fill);
                    ui.painter().line_segment(
                        [header_rect.left_bottom(), header_rect.right_bottom()],
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(0x36, 0x37, 0x3b)),
                    );

                    let mut selected_value = selected;
                    let checkbox_column_width = 38.0;
                    let checkbox_area = egui::Rect::from_min_size(
                        egui::pos2(header_rect.left(), header_rect.top()),
                        egui::vec2(checkbox_column_width, HEADER_HEIGHT),
                    );
                    let selection_changed = ui
                        .allocate_ui_at_rect(checkbox_area, |ui| {
                            ui.centered_and_justified(|ui| {
                                ui.checkbox(&mut selected_value, "")
                            }).inner.changed()
                        })
                        .inner;
                    if selection_changed {
                        if selected_value {
                            self.select_map(&map);
                        } else {
                            self.deselect_md5(&map.md5);
                        }
                    }

                    let content_rect = egui::Rect::from_min_max(
                        egui::pos2(header_rect.left() + checkbox_column_width, header_rect.top()),
                        header_rect.right_bottom(),
                    );
                    let row_response = ui.interact(
                        content_rect,
                        ui.id().with(("map_inspector", &map.md5)),
                        egui::Sense::click(),
                    );
                    let title = format!("{} - {}", map.artist, map.title);
                    let details = compact_map_details(&map);
                    let title_rect = egui::Rect::from_min_max(
                        egui::pos2(content_rect.left() + 10.0, content_rect.top() + 4.0),
                        egui::pos2(content_rect.right() - 8.0, content_rect.top() + 23.0),
                    );
                    let details_rect = egui::Rect::from_min_max(
                        egui::pos2(content_rect.left() + 10.0, content_rect.top() + 23.0),
                        egui::pos2(content_rect.right() - 8.0, content_rect.bottom() - 3.0),
                    );
                    ui.painter().with_clip_rect(title_rect).text(
                        title_rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        title.clone(),
                        egui::TextStyle::Body.resolve(ui.style()),
                        ui.visuals().text_color(),
                    );
                    ui.painter().with_clip_rect(details_rect).text(
                        details_rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        details.clone(),
                        egui::TextStyle::Small.resolve(ui.style()),
                        egui::Color32::from_rgb(0xb3, 0xad, 0xa5),
                    );
                    let row_response =
                        row_response.on_hover_text(format!("{}\n{}", map.label(), details));
                    if row_response.clicked() {
                        self.expanded_map_md5 = Some(map.md5.clone());
                    }
                }
            });
    }

    fn render_map_inspector(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rect: egui::Rect,
        map: &LocalBeatmap,
    ) {
        let background_path = map
            .background_filename
            .as_ref()
            .map(|filename| map.folder.join(filename));
        if let Some(path) = background_path.as_deref() {
            self.ensure_background_preview(path, &map.md5);
        } else {
            self.clear_background_preview();
        }
        let preview = self.background_preview.clone();
        let preview_error = self.background_preview_error.clone();

        ui.allocate_ui_at_rect(rect, |ui| {
            inspector_frame(ctx.style().as_ref()).show(ui, |ui| {
                fill_tile_width(ui);
                ui.set_min_height((rect.height() - 18.0).max(1.0));

                if let Some(texture) = preview {
                    let source_size = texture.size_vec2();
                    let scale = (ui.available_width() / source_size.x)
                        .min(270.0 / source_size.y)
                        .max(0.01);
                    let display_size = source_size * scale;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - display_size.x) * 0.5).max(0.0));
                        ui.add(egui::Image::new((texture.id(), display_size)));
                    });
                } else {
                    let message = preview_error.unwrap_or_else(|| {
                        if background_path.is_some() {
                            "Background file is unavailable".to_owned()
                        } else {
                            "This map does not define a background".to_owned()
                        }
                    });
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(0x1b, 0x1c, 0x1f))
                        .stroke(egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(0x3a, 0x3b, 0x40),
                        ))
                        .inner_margin(egui::Margin::same(12.0))
                        .show(ui, |ui| {
                            ui.set_min_height(92.0);
                            ui.centered_and_justified(|ui| muted_label(ui, message));
                        });
                }

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("{} - {}", map.artist, map.title))
                        .size(18.0)
                        .strong(),
                );
                muted_label(ui, format!("{} · mapped by {}", map.version, map.creator));

                let audio_path = map
                    .audio_filename
                    .as_ref()
                    .map(|filename| map.folder.join(filename));
                let audio_available = audio_path.as_ref().is_some_and(|path| path.exists());
                let is_current_audio = audio_path.as_ref().is_some_and(|path| {
                    self.audio_player
                        .as_ref()
                        .is_some_and(|player| player.path == *path)
                });
                ui.horizontal_wrapped(|ui| {
                    if is_current_audio {
                        let paused = self
                            .audio_player
                            .as_ref()
                            .is_some_and(|player| player.sink.is_paused());
                        if ui.button(if paused { "Resume" } else { "Pause" }).clicked() {
                            self.toggle_audio_pause();
                        }
                        if ui.button("Stop").clicked() {
                            self.stop_audio_playback();
                        }
                    } else if ui
                        .add_enabled(audio_available, egui::Button::new("Play audio"))
                        .clicked()
                    {
                        if let Some(path) = audio_path.as_deref() {
                            self.start_audio_playback(path);
                        }
                    }
                    if ui.button("Open map folder").clicked() {
                        self.open_map_path(&map.folder, "map folder");
                    }
                });
                if is_current_audio {
                    ui.horizontal(|ui| {
                        ui.label("Volume");
                        let changed = ui
                            .add(
                                egui::Slider::new(&mut self.audio_volume, 0.0..=1.0)
                                    .show_value(false),
                            )
                            .changed();
                        if changed {
                            if let Some(player) = &self.audio_player {
                                player.sink.set_volume(self.audio_volume);
                            }
                        }
                        if let Some(filename) = &map.audio_filename {
                            muted_label(ui, format!("FFmpeg | {filename}"));
                        }
                    });
                } else if let Some(filename) = &map.audio_filename {
                    muted_label(ui, filename);
                }

                if !audio_available {
                    if map.audio_filename.is_some() {
                        muted_label(ui, "The referenced audio file is missing.");
                    } else {
                        muted_label(ui, "This map does not define an audio file.");
                    }
                }

                ui.separator();
                egui::Grid::new(("map_info", &map.md5))
                    .num_columns(4)
                    .spacing(egui::vec2(14.0, 6.0))
                    .show(ui, |ui| {
                        inspector_grid_value(ui, "Mode", mode_label(map.mode));
                        inspector_grid_value(
                            ui,
                            "Length",
                            map.length_seconds
                                .map(format_duration)
                                .unwrap_or_else(|| "—".to_owned()),
                        );
                        ui.end_row();
                        inspector_grid_value(
                            ui,
                            "Stars",
                            map.stars
                                .map(format_number)
                                .unwrap_or_else(|| "—".to_owned()),
                        );
                        inspector_grid_value(
                            ui,
                            "BPM",
                            map.bpm.map(format_number).unwrap_or_else(|| "—".to_owned()),
                        );
                        ui.end_row();
                        inspector_grid_value(ui, "AR", optional_number(map.ar));
                        inspector_grid_value(ui, "CS", optional_number(map.cs));
                        ui.end_row();
                        inspector_grid_value(ui, "OD", optional_number(map.od));
                        inspector_grid_value(ui, "HP", optional_number(map.hp));
                        ui.end_row();
                        inspector_grid_value(ui, "Circles", map.circles.to_string());
                        inspector_grid_value(ui, "Sliders", map.sliders.to_string());
                        ui.end_row();
                        inspector_grid_value(
                            ui,
                            "Map ID",
                            map.beatmap_id
                                .map_or_else(|| "—".to_owned(), |id| id.to_string()),
                        );
                        inspector_grid_value(
                            ui,
                            "Set ID",
                            map.beatmapset_id
                                .map_or_else(|| "—".to_owned(), |id| id.to_string()),
                        );
                        ui.end_row();
                    });
                if !map.source.trim().is_empty() {
                    scan_status_label(ui, "Source", &map.source);
                }
                scan_status_label(ui, "File", &map.path.display().to_string());
            });
        });
    }

    fn render_collections_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut load_selected = false;
        let mut create_collection = false;
        let mut add_to_collection = false;
        let mut selected_collection_name: Option<String> = None;

        let content_width = ui.available_width().max(1.0);
        egui::ScrollArea::vertical()
            .id_source("collections_page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                fix_ui_width(ui, content_width);
                section_frame(ctx.style().as_ref()).show(ui, |ui| {
                    fill_tile_width(ui);
                    ui.horizontal_wrapped(|ui| {
                        ui.heading("Collections");
                        muted_label(
                            ui,
                            format!("{} map(s) selected from the scan", self.selected_maps.len()),
                        );
                    });

                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                self.selected_collection_index.is_some(),
                                egui::Button::new("Load selected"),
                            )
                            .clicked()
                        {
                            load_selected = true;
                        }
                        if ui.button("Restore backup").clicked() {
                            self.delete_confirmation = Some(DeleteIntent::RestoreBackup);
                        }
                    });

                    ui.separator();
                    ui.label(egui::RichText::new("Create collection").strong());
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [ui.available_width().min(300.0) - 60.0, 28.0],
                            egui::TextEdit::singleline(&mut self.collection_name)
                                .vertical_align(egui::Align::Center),
                        );
                        if ui
                            .add_enabled(
                                !self.collection_name.trim().is_empty(),
                                egui::Button::new("Create"),
                            )
                            .clicked()
                        {
                            create_collection = true;
                        }
                    });

                    ui.separator();
                    ui.label(egui::RichText::new("Add to / delete").strong());
                    ui.horizontal(|ui| {
                        let selected_name = self
                            .selected_collection_index
                            .and_then(|i| self.collections.get(i))
                            .map(|c| c.name.as_str())
                            .unwrap_or("Choose collection");
                        egui::ComboBox::from_id_source("collections_manage")
                            .selected_text(selected_name)
                            .width(ui.available_width().min(300.0) - 130.0)
                            .show_ui(ui, |ui| {
                                for (i, collection) in
                                    self.collections.iter().enumerate()
                                {
                                    let label = format!("{} ({} map{})",
                                        collection.name,
                                        collection.hashes.len(),
                                        if collection.hashes.len() == 1 { "" } else { "s" }
                                    );
                                    ui.selectable_value(
                                        &mut self.selected_collection_index,
                                        Some(i),
                                        label,
                                    );
                                }
                            });
                        if ui
                            .add_enabled(
                                self.selected_collection_index.is_some(),
                                egui::Button::new("Add to"),
                            )
                            .clicked()
                        {
                            selected_collection_name = self
                                .selected_collection_index
                                .and_then(|i| self.collections.get(i))
                                .map(|c| c.name.clone());
                            add_to_collection = true;
                        }
                        if ui
                            .add_enabled(
                                self.selected_collection_index.is_some(),
                                egui::Button::new("Delete"),
                            )
                            .clicked()
                        {
                            self.delete_confirmation = self
                                .selected_collection_index
                                .and_then(|i| self.collections.get(i))
                                .map(|c| DeleteIntent::Collection(c.name.clone()));
                        }
                    });

                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Write TSV manifest").clicked() {
                            self.export_manifest();
                        }
                    });

                    ui.separator();
                    if self.collections.is_empty() {
                        muted_label(ui, "No collections loaded.");
                    } else {
                        let selected_name = self
                            .selected_collection_index
                            .and_then(|index| self.collections.get(index))
                            .map(|collection| collection.name.as_str())
                            .unwrap_or("Choose collection");
                        egui::ComboBox::from_id_source("collections_page_picker")
                            .selected_text(selected_name)
                            .width(ui.available_width().min(520.0))
                            .show_ui(ui, |ui| {
                                let previous = self.selected_collection_index;
                                for (index, collection) in self.collections.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut self.selected_collection_index,
                                        Some(index),
                                        format!(
                                            "{} ({} map{})",
                                            collection.name,
                                            collection.hashes.len(),
                                            if collection.hashes.len() == 1 { "" } else { "s" }
                                        ),
                                    );
                                }
                                if self.selected_collection_index != previous {
                                    load_selected = true;
                                }
                            });

                        if let Some(collection) = self
                            .selected_collection_index
                            .and_then(|index| self.collections.get(index))
                            .cloned()
                        {
                            let wanted_hashes = collection
                                .hashes
                                .iter()
                                .map(String::as_str)
                                .collect::<BTreeSet<_>>();
                            let scanned_maps = self.scan.as_ref().map(|scan| {
                                scan.maps
                                    .iter()
                                    .filter(|map| wanted_hashes.contains(map.md5.as_str()))
                                    .map(|map| (map.md5.clone(), map.clone()))
                                    .collect::<BTreeMap<_, _>>()
                            });
                            let matched = scanned_maps.as_ref().map_or(0, BTreeMap::len);
                            muted_label(
                                ui,
                                format!(
                                    "{} map(s), {} matched in the current scan",
                                    collection.hashes.len(),
                                    matched
                                ),
                            );
                            egui::ScrollArea::vertical()
                                .id_source("collections_page_maps")
                                .max_height(360.0)
                                .show(ui, |ui| {
                                    let mut shown = 0_usize;
                                    for hash in &collection.hashes {
                                        if shown >= 200 {
                                            break;
                                        }
                                        let mut selected = self.selected_md5s.contains(hash);
                                        if let Some(map) = scanned_maps
                                            .as_ref()
                                            .and_then(|maps| maps.get(hash.as_str()))
                                        {
                                            let changed = ui
                                                .horizontal(|ui| {
                                                    let changed = ui
                                                        .checkbox(&mut selected, "")
                                                        .changed();
                                                    ui.label(self.map_result_label(map));
                                                    changed
                                                })
                                                .inner;
                                            if changed {
                                                if selected {
                                                    self.select_map(map);
                                                } else {
                                                    self.deselect_md5(hash);
                                                }
                                            }
                                        } else {
                                            let changed = ui
                                                .horizontal(|ui| {
                                                    let changed = ui
                                                        .checkbox(&mut selected, "")
                                                        .changed();
                                                    muted_label(ui, format!("Missing locally: {hash}"));
                                                    changed
                                                })
                                                .inner;
                                            if changed {
                                                if selected {
                                                    self.select_missing_hash(hash);
                                                } else {
                                                    self.deselect_md5(hash);
                                                }
                                            }
                                        }
                                        shown += 1;
                                    }
                                    if collection.hashes.len() > shown {
                                        muted_label(
                                            ui,
                                            format!("{} more map(s)", collection.hashes.len() - shown),
                                        );
                                    }
                                });
                        }
                    }

                    if !self.collection_missing_hashes.is_empty() {
                        muted_label(
                            ui,
                            format!(
                                "{} loaded hash(es) are not present in the current scan and will be preserved while selected.",
                                self.collection_missing_hashes.len()
                            ),
                        );
                    }
                });
            });

        if load_selected {
            self.load_selected_collection_into_selection();
        }
        if create_collection {
            self.create_collection(&self.collection_name.clone());
        }
        if add_to_collection {
            if let Some(name) = selected_collection_name.as_deref() {
                self.add_selected_to_collection(name);
            }
        }
    }

    fn maybe_show_delete_confirmation(&mut self, ctx: &egui::Context) {
        let Some(intent) = self.delete_confirmation.clone() else {
            return;
        };

        let (title, message, confirm_label) = match &intent {
            DeleteIntent::Collection(name) => (
                "Delete collection?",
                format!("Delete collection \"{name}\"? This cannot be undone."),
                "Delete",
            ),
            DeleteIntent::NonStdModes(count) => (
                "Delete non-std maps?",
                format!(
                    "Permanently delete {count} .osu file(s) from disk? \
                     This cannot be undone."
                ),
                "Delete files",
            ),
            DeleteIntent::RestoreBackup => (
                "Restore collection backup?",
                "Restore collection.db from collection.db.bak? Collections saved \
                 since the last write will be lost."
                    .to_owned(),
                "Restore",
            ),
        };

        let mut confirmed = false;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.delete_confirmation = None;
                    }
                    if ui.button(confirm_label).clicked() {
                        confirmed = true;
                    }
                });
            });

        if confirmed {
            self.delete_confirmation = None;
            match intent {
                DeleteIntent::Collection(_) => self.delete_selected_collection(),
                DeleteIntent::NonStdModes(_) => self.delete_selected_non_std_modes(),
                DeleteIntent::RestoreBackup => self.restore_collection_backup(),
            }
        }
    }

    fn cache_background(&mut self, path: PathBuf, texture: egui::TextureHandle) {
        if self.background_cache.contains_key(&path) {
            return;
        }
        while self.background_cache.len() >= BACKGROUND_CACHE_LIMIT {
            match self.background_cache_order.first().cloned() {
                Some(oldest) => {
                    self.background_cache_order.remove(0);
                    self.background_cache.remove(&oldest);
                }
                None => break,
            }
        }
        self.background_cache_order.push(path.clone());
        self.background_cache.insert(path, texture);
    }

    fn ensure_background_preview(&mut self, path: &Path, md5: &str) {
        if self.background_preview_path.as_deref() != Some(path) {
            self.background_preview_path = Some(path.to_owned());
            self.background_preview_error = None;

            if let Some(texture) = self.background_cache.get(path).cloned() {
                self.background_preview = Some(texture);
            } else {
                self.background_preview = None;
                self.spawn_background_decode(path);
            }
        }
        // Runs every frame while the inspector is visible, so a decode that
        // was skipped under load is retried as soon as a slot frees up.
        self.prefetch_neighbor_backgrounds(md5);
    }

    fn spawn_background_decode(&mut self, path: &Path) {
        if self.background_cache.contains_key(path)
            || self.background_in_flight.contains(path)
            || self.background_in_flight.len() >= BACKGROUND_MAX_IN_FLIGHT
        {
            return;
        }
        self.background_in_flight.insert(path.to_owned());
        let tx = self.background_load_tx.clone();
        let path = path.to_owned();
        thread::spawn(move || {
            let result = load_background_image(&path);
            let _ = tx.send((path, result));
        });
    }

    /// Decodes the backgrounds around the inspected map ahead of time so
    /// stepping to the next/previous beatmap usually hits the cache instead
    /// of paying a full decode on selection.
    fn prefetch_neighbor_backgrounds(&mut self, md5: &str) {
        let current = match &self.background_preview_path {
            Some(current) => current.clone(),
            None => return,
        };
        // Spend decode slots on the visible preview first.
        if !self.background_cache.contains_key(&current)
            && !self.background_in_flight.contains(&current)
        {
            return;
        }
        for path in self.neighbor_background_paths(md5) {
            if self.background_in_flight.len() >= BACKGROUND_MAX_IN_FLIGHT {
                break;
            }
            self.spawn_background_decode(&path);
        }
    }

    fn neighbor_background_paths(&self, md5: &str) -> Vec<PathBuf> {
        let Some(scan) = &self.scan else {
            return Vec::new();
        };
        let Some(position) = self.filtered_map_indexes.iter().position(|&index| {
            scan.maps
                .get(index)
                .is_some_and(|map| map.md5 == md5)
        }) else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        // Nearest first so the closest maps win when decode slots run out.
        for distance in 1..=BACKGROUND_PREFETCH_RADIUS {
            for index in [
                position.checked_add(distance),
                position.checked_sub(distance),
            ]
            .into_iter()
            .flatten()
            {
                let Some(map_index) = self.filtered_map_indexes.get(index) else {
                    continue;
                };
                if let Some(map) = scan.maps.get(*map_index)
                    && let Some(filename) = &map.background_filename
                {
                    paths.push(map.folder.join(filename));
                }
            }
        }
        paths
    }

    fn clear_background_preview(&mut self) {
        self.background_preview_path = None;
        self.background_preview = None;
        self.background_preview_error = None;
    }

    fn start_audio_playback(&mut self, path: &Path) {
        self.audio_player = None;
        match AudioPlayer::start(path, self.audio_volume) {
            Ok(player) => {
                self.status = format!("Playing audio in app: {}", path.display());
                self.audio_player = Some(player);
            }
            Err(err) => self.status = format!("Audio playback failed: {err:#}"),
        }
    }

    fn toggle_audio_pause(&mut self) {
        let Some(player) = &self.audio_player else {
            return;
        };
        if player.sink.is_paused() {
            player.sink.play();
            self.status = format!("Resumed audio: {}", player.path.display());
        } else {
            player.sink.pause();
            self.status = format!("Paused audio: {}", player.path.display());
        }
    }

    fn stop_audio_playback(&mut self) {
        if let Some(player) = self.audio_player.take() {
            self.status = format!("Stopped audio: {}", player.path.display());
        }
    }

    fn poll_audio_playback(&mut self) {
        let finished = self
            .audio_player
            .as_ref()
            .is_some_and(|player| player.sink.empty());
        if finished {
            if let Some(player) = self.audio_player.take() {
                self.status = format!("Finished audio: {}", player.path.display());
            }
        }
    }

    fn open_map_path(&mut self, path: &Path, label: &str) {
        match open_with_default_app(path) {
            Ok(()) => self.status = format!("Opened {label}: {}", path.display()),
            Err(err) => self.status = format!("Could not open {label}: {err:#}"),
        }
    }
}

impl eframe::App for MapManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background(ctx);
        self.poll_audio_playback();
        if self.is_scanning || self.is_repairing || self.audio_player.is_some() || !self.background_in_flight.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        egui::TopBottomPanel::top("top")
            .frame(panel_frame(ctx.style().as_ref()))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("osu! Map Manager");
                    ui.separator();
                    let audio_state = self.audio_player.as_ref().map(|player| {
                        (
                            player.sink.is_paused(),
                            player
                                .path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("map audio")
                                .to_owned(),
                        )
                    });
                    if let Some((paused, filename)) = audio_state {
                        ui.add_sized(
                            [150.0, 20.0],
                            egui::Label::new(format!("Audio: {filename}")).truncate(true),
                        )
                        .on_hover_text(filename);
                        if ui
                            .small_button(if paused { "Resume" } else { "Pause" })
                            .clicked()
                        {
                            self.toggle_audio_pause();
                        }
                        if ui.small_button("Stop").clicked() {
                            self.stop_audio_playback();
                        }
                        ui.separator();
                    }
                    let status = if self.is_scanning {
                        scan_progress_status(self.scanned_maps, self.matched_maps)
                    } else {
                        self.status.clone()
                    };
                    let status_text = if self.is_scanning {
                        egui::RichText::new(status.clone()).monospace()
                    } else {
                        egui::RichText::new(status.clone())
                    };
                    ui.add_sized(
                        [ui.available_width(), 20.0],
                        egui::Label::new(status_text).truncate(true),
                    )
                    .on_hover_text(status);
                });
            });

        if self.active_tab == AppTab::ScanCollections {
            egui::SidePanel::left("filters")
            .resizable(false)
            .exact_width(340.0)
            .frame(panel_frame(ctx.style().as_ref()))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Local library");
                    ui.label(egui::RichText::new("osu! root").strong());
                    ui.horizontal(|ui| {
                        let input_width = (ui.available_width() - 58.0).max(80.0);
                        ui.add_sized(
                            [input_width, 28.0],
                            egui::TextEdit::singleline(&mut self.osu_root)
                                .vertical_align(egui::Align::Center),
                        );
                        if ui.button("Pick").clicked() {
                            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                self.osu_root = path.display().to_string();
                                self.songs_dir = path.join("Songs").display().to_string();
                                self.repair_ignores =
                                    load_repair_ignores(&self.osu_root).unwrap_or_default();
                                self.oauth_session =
                                    osu_oauth::load_oauth_session(&self.osu_root);
                                self.oauth_status = if self.oauth_session.is_some() {
                                    "Signed in with osu! (token loaded from disk)".to_owned()
                                } else {
                                    "Not signed in with osu!".to_owned()
                                };
                                self.collections.clear();
                                self.selected_collection_index = None;
                                self.collection_missing_hashes.clear();
                                self.scan = load_scan_cache(&self.osu_root).ok().flatten();
                                self.expanded_map_md5 = None;
                                self.clear_background_preview();
                                self.invalidate_scan_caches();
                                self.status = self.scan.as_ref().map_or_else(
                                    || "Ready".to_owned(),
                                    |scan| {
                                        format!(
                                            "Loaded cached scan: {} scanned maps, {} sets, {} repair issue(s)",
                                            scan.maps.len(),
                                            scan.sets.len(),
                                            scan.problems.len()
                                        )
                                    },
                                );
                            }
                        }
                    });
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Songs").strong());
                    ui.horizontal(|ui| {
                        let input_width = (ui.available_width() - 58.0).max(80.0);
                        ui.add_sized(
                            [input_width, 28.0],
                            egui::TextEdit::singleline(&mut self.songs_dir)
                                .vertical_align(egui::Align::Center),
                        );
                        if self.is_scanning {
                            if ui.button("Stop").clicked() {
                                self.stop_scan();
                            }
                        } else if ui.button("Scan").clicked() {
                            self.start_scan();
                        }
                    });

                    ui.separator();
                    ui.heading("Local filters");
                    muted_label(
                        ui,
                        "Rows are combined with AND against scanned maps in your Songs directory.",
                    );
                    ui.add_space(10.0);

                    for clause in &mut self.query.clauses {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut clause.enabled, "");
                            egui::ComboBox::from_id_source(("field", clause as *const _ as usize))
                                .selected_text(clause.field.label())
                                .width(132.0)
                                .show_ui(ui, |ui| {
                                    for field in SearchField::SORTED {
                                        ui.selectable_value(&mut clause.field, field, field.label());
                                    }
                                });
                            egui::ComboBox::from_id_source(("op", clause as *const _ as usize))
                                .selected_text(clause.operator.as_str())
                                .width(54.0)
                                .show_ui(ui, |ui| {
                                    for operator in Operator::ALL {
                                        ui.selectable_value(
                                            &mut clause.operator,
                                            operator,
                                            operator.as_str(),
                                        );
                                    }
                                });
                            let value_width = ui.available_width().max(64.0);
                            ui.add_sized(
                                [value_width, 28.0],
                                egui::TextEdit::singleline(&mut clause.value)
                                    .vertical_align(egui::Align::Center),
                            );
                        });
                    }

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("+ Add filter").clicked() {
                            self.query.clauses.push(QueryClause::default());
                        }
                        if ui.button("Remove disabled").clicked() {
                            self.query.clauses.retain(|clause| clause.enabled);
                        }
                    });

                    ui.separator();
                    ui.checkbox(&mut self.only_osu_std, "Only osu!std maps");
                    if self.only_osu_std {
                        self.retain_selected_maps(is_osu_std);
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("Scan issue handling").strong());
                    ui.checkbox(&mut self.skip_parse_timeouts, "Skip map parse timeouts");
                    ui.checkbox(&mut self.skip_parse_errors, "Skip map parse errors");
                    ui.separator();
                    ui.label(egui::RichText::new("Equivalent query text").strong());
                    let mut query_text = self.query.to_osu_search();
                    ui.add_sized(
                        [ui.available_width(), 76.0],
                        egui::TextEdit::multiline(&mut query_text),
                    );
                    muted_label(
                        ui,
                        "Some osu!web-only fields require database/API metadata and will not match local .osu files yet.",
                    );
                });
            });
        }

        self.refresh_filtered_maps();
        self.refresh_repair_jobs();

        let mut repair_requested = false;
        let mut repair_single_requested: Option<i64> = None;
        let mut update_check_requested = false;
        let mut update_all_requested = false;
        let mut update_single_requested: Option<i64> = None;
        let mut sign_in_requested = false;
        let mut sign_out_requested = false;
        let mut delete_non_std_requested = false;
        let mut load_collections_requested = false;
        let mut load_collection_selection_requested = false;
        let mut save_collection_requested = false;
        let mut create_collection_requested = false;
        let mut add_to_collection_requested = false;
        let mut selected_collection_name: Option<String> = None;

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ctx.style().as_ref()))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.active_tab,
                        AppTab::ScanCollections,
                        "Map scan",
                    );
                    ui.selectable_value(
                        &mut self.active_tab,
                        AppTab::Collections,
                        "Collections",
                    );
                    ui.selectable_value(
                        &mut self.active_tab,
                        AppTab::RepairsDelete,
                        "Repairs and delete",
                    );
                });
                ui.add_space(8.0);

                match self.active_tab {
                    AppTab::ScanCollections => {
                let rect = ui.available_rect_before_wrap();
                let gap = 8.0;
                let actions_width = 0.0;
                let library_width = if actions_width > 1.0 {
                    (rect.width() - actions_width - gap).max(1.0)
                } else {
                    rect.width().max(1.0)
                };
                let actions_left = rect.left() + library_width + gap;
                let library_rect = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(library_width, rect.height()),
                );
                let actions_rect = egui::Rect::from_min_size(
                    egui::pos2(actions_left, rect.top()),
                    egui::vec2(actions_width, rect.height()),
                );

                let mut library_ui = ui.child_ui_with_id_source(
                    library_rect,
                    egui::Layout::top_down(egui::Align::Min),
                    "library_fixed",
                );
                let library_clip_rect = egui::Rect::from_min_max(
                    library_rect.min,
                    egui::pos2(library_rect.max.x + (gap * 0.5), library_rect.max.y),
                );
                library_ui.set_clip_rect(library_clip_rect);
                library_ui.set_width(library_width);
                library_ui.set_max_width(library_width);
                let middle_width = library_ui.available_width();
                egui::ScrollArea::vertical()
                    .id_source("library_pane")
                    .auto_shrink([false, false])
                    .show(&mut library_ui, |ui| {
                        let content_width = middle_width.max(1.0);
                        let card_item_spacing = ui.spacing().item_spacing;
                        let card_gap = gap;
                        ui.spacing_mut().item_spacing.y = 0.0;

                        if self.is_scanning {
                            fix_ui_width(ui, content_width);
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.add(egui::Spinner::new());
                                wrapped_label(
                                    ui,
                                    format!(
                                        "Reading folder {} | {} maps scanned | {} matches",
                                        self.scanned_folders,
                                        self.scanned_maps,
                                        self.matched_maps
                                    ),
                                );
                                wrapped_label(
                                    ui,
                                    format!(
                                        "Star ratings: {} db entries, {} matched scanned maps",
                                        self.star_ratings_loaded, self.maps_with_stars
                                    ),
                                );
                                if let Some(err) = &self.star_parse_error {
                                    scan_status_label(ui, "osu!.db", err);
                                }
                                if !self.current_folder.is_empty() {
                                    scan_status_label(ui, "Current", &self.current_folder);
                                }
                                if !self.current_map.is_empty() {
                                    scan_status_label(ui, "Parsing", &self.current_map);
                                }
                            });
                        }

                        if self.scan.is_some() {
                            let (scanned_maps, scanned_sets, repair_issues) =
                                self.scan.as_ref().map_or((0, 0, 0), |scan| {
                                    (scan.maps.len(), scan.sets.len(), scan.problems.len())
                            });
                            if self.is_scanning {
                                ui.add_space(card_gap);
                            }
                            fix_ui_width(ui, content_width);
                            let results_card_height = ui.available_height().max(0.0);
                            let results_card_inner_height = (results_card_height - 24.0).max(0.0);
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.set_min_height(results_card_inner_height);
                                let results_content_top = ui.cursor().top();
                                ui.columns(3, |columns| {
                                    columns[0].heading("Map scan");
                                    columns[1].with_layout(
                                        egui::Layout::top_down(egui::Align::Center),
                                        |ui| {
                                            ui.horizontal(|ui| {
                                                if ui.button("Select all filtered").clicked() {
                                                    let maps = self
                                                        .scan
                                                        .as_ref()
                                                        .map(|scan| {
                                                            self.filtered_map_indexes
                                                                .iter()
                                                                .filter_map(|&index| {
                                                                    scan.maps.get(index).cloned()
                                                                })
                                                                .collect::<Vec<_>>()
                                                        })
                                                        .unwrap_or_default();
                                                    for map in &maps {
                                                        self.select_map(map);
                                                    }
                                                }
                                                if ui.button("Clear selection").clicked() {
                                                    self.clear_selection();
                                                }
                                            });
                                        },
                                    );
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Collection");
                                    ui.add_sized(
                                        [140.0, 24.0],
                                        egui::TextEdit::singleline(&mut self.collection_name)
                                            .vertical_align(egui::Align::Center),
                                    );
                                    if ui
                                        .add_enabled(
                                            !self.collection_name.trim().is_empty(),
                                            egui::Button::new("Create"),
                                        )
                                        .clicked()
                                    {
                                        create_collection_requested = true;
                                    }
                                });
                                ui.horizontal(|ui| {
                                    let add_to_req = &mut add_to_collection_requested;
                                    let sel_name = &mut selected_collection_name;
                                    let selected_text = self
                                        .selected_collection_index
                                        .and_then(|i| self.collections.get(i))
                                        .map(|c| c.name.as_str())
                                        .unwrap_or("Choose collection");
                                    egui::ComboBox::from_id_source("scan_add_to_collection")
                                        .selected_text(selected_text)
                                        .width(140.0)
                                        .show_ui(ui, |ui| {
                                            for (i, c) in self.collections.iter().enumerate() {
                                                ui.selectable_value(
                                                    &mut self.selected_collection_index,
                                                    Some(i),
                                                    format!("{} ({} map{})", c.name, c.hashes.len(),
                                                        if c.hashes.len() == 1 { "" } else { "s" }),
                                                );
                                            }
                                        });
                                    if ui
                                        .add_enabled(
                                            self.selected_collection_index.is_some(),
                                            egui::Button::new("Add to"),
                                        )
                                        .clicked()
                                    {
                                        *sel_name = self
                                            .selected_collection_index
                                            .and_then(|i| self.collections.get(i))
                                            .map(|c| c.name.clone());
                                        *add_to_req = true;
                                    }
                                    if ui
                                        .add_enabled(
                                            self.selected_collection_index.is_some(),
                                            egui::Button::new("Delete"),
                                        )
                                        .clicked()
                                    {
                                        self.delete_confirmation = self
                                            .selected_collection_index
                                            .and_then(|i| self.collections.get(i))
                                            .map(|c| DeleteIntent::Collection(c.name.clone()));
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(format!(
                                        "{} matching maps · {} selected",
                                        self.filtered_map_indexes.len(),
                                        self.selected_maps.len()
                                    ));
                                    muted_label(
                                        ui,
                                        format!(
                                            "{} scanned · {} sets · {} repair issues",
                                            scanned_maps, scanned_sets, repair_issues
                                        ),
                                    );
                                });
                                let used_height =
                                    (ui.cursor().top() - results_content_top).max(0.0);
                                let list_height =
                                    (results_card_inner_height - used_height).max(120.0);
                                let list_width = ui.available_width().max(1.0);
                                ui.scope(|ui| {
                                    fix_ui_width(ui, list_width);
                                    self.render_map_workspace(ui, ctx, list_height);
                                });
                            });
                        }
                    });
                if actions_width > 1.0 {
                    let mut actions_ui = ui.child_ui_with_id_source(
                        actions_rect,
                        egui::Layout::top_down(egui::Align::Min),
                        "actions_fixed",
                    );
                    actions_ui.set_clip_rect(actions_rect);
                    actions_ui.set_width(actions_width);
                    actions_ui.set_max_width(actions_width);
                    egui::ScrollArea::vertical()
                        .id_source("actions_pane")
                        .auto_shrink([false, false])
                        .show(&mut actions_ui, |ui| {
                            let actions_content_width = ui.available_width().max(1.0);
                            let card_item_spacing = ui.spacing().item_spacing;
                            let card_gap = gap;
                            ui.spacing_mut().item_spacing.y = 0.0;
                            fix_ui_width(ui, actions_content_width);
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.heading("Collection management");
                            });

                            ui.add_space(card_gap);
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.heading("Collections");
                                ui.horizontal_wrapped(|ui| {
                                    if ui.button("Load collection.db").clicked() {
                                        load_collections_requested = true;
                                    }
                                    if ui
                                        .add_enabled(
                                            self.selected_collection_index.is_some(),
                                            egui::Button::new("Load selected"),
                                        )
                                        .clicked()
                                    {
                                        load_collection_selection_requested = true;
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Name");
                                    let input_width = (ui.available_width() - 8.0).max(80.0);
                                    ui.add_sized(
                                        [input_width, 28.0],
                                        egui::TextEdit::singleline(&mut self.collection_name)
                                            .vertical_align(egui::Align::Center),
                                    );
                                });
                                muted_label(
                                    ui,
                                    format!("{} selected map(s)", self.selected_maps.len()),
                                );
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .add_enabled(
                                            !self.collection_name.trim().is_empty(),
                                            egui::Button::new("Save selection"),
                                        )
                                        .clicked()
                                    {
                                        save_collection_requested = true;
                                    }
                                    if ui
                                        .add_enabled(
                                            self.selected_collection_index.is_some(),
                                            egui::Button::new("Delete selected"),
                                        )
                                        .clicked()
                                    {
                                        self.delete_confirmation = self
                                            .selected_collection_index
                                            .and_then(|i| self.collections.get(i))
                                            .map(|c| DeleteIntent::Collection(c.name.clone()));
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    if ui.button("Write TSV manifest").clicked() {
                                        self.export_manifest();
                                    }
                                    ui.add_enabled_ui(true, |ui| {
                                        if ui.button("Restore backup").clicked() {
                                            self.delete_confirmation =
                                                Some(DeleteIntent::RestoreBackup);
                                        }
                                    })
                                    .response
                                    .on_hover_text("Restore collection.db from collection.db.bak");
                                });

                                if self.collections.is_empty() {
                                    muted_label(ui, "No collections loaded.");
                                } else {
                                    let selected_name = self
                                        .selected_collection_index
                                        .and_then(|index| self.collections.get(index))
                                        .map(|collection| collection.name.as_str())
                                        .unwrap_or("Choose collection");
                                    egui::ComboBox::from_id_source("existing_collections")
                                        .selected_text(selected_name)
                                        .width(ui.available_width())
                                        .show_ui(ui, |ui| {
                                            let previous_index = self.selected_collection_index;
                                            for (index, collection) in
                                                self.collections.iter().enumerate()
                                            {
                                                ui.selectable_value(
                                                    &mut self.selected_collection_index,
                                                    Some(index),
                                                    format!(
                                                        "{} ({} map{})",
                                                        collection.name,
                                                        collection.hashes.len(),
                                                        if collection.hashes.len() == 1 {
                                                            ""
                                                        } else {
                                                            "s"
                                                        }
                                                    ),
                                                );
                                            }
                                            if self.selected_collection_index != previous_index {
                                                load_collection_selection_requested = true;
                                            }
                                        });

                                    if let Some(collection) = self
                                        .selected_collection_index
                                        .and_then(|index| self.collections.get(index))
                                        .cloned()
                                    {
                                        let matched = self.scan.as_ref().map_or(0, |scan| {
                                            let scanned_hashes = scan
                                                .maps
                                                .iter()
                                                .map(|map| map.md5.as_str())
                                                .collect::<BTreeSet<_>>();
                                            collection
                                                .hashes
                                                .iter()
                                                .filter(|hash| scanned_hashes.contains(hash.as_str()))
                                                .count()
                                        });
                                        muted_label(
                                            ui,
                                            format!(
                                                "{} map(s), {} matched in current scan",
                                                collection.hashes.len(),
                                                matched
                                            ),
                                        );
                                        egui::ScrollArea::vertical()
                                            .id_source("collection_maps")
                                            .max_height(180.0)
                                            .show(ui, |ui| {
                                                let scanned_maps =
                                                    self.scan.as_ref().map(|scan| {
                                                        scan.maps
                                                            .iter()
                                                            .map(|map| (map.md5.clone(), map.clone()))
                                                            .collect::<BTreeMap<_, _>>()
                                                    });
                                                let mut shown = 0_usize;
                                                for hash in &collection.hashes {
                                                    if shown >= 80 {
                                                        break;
                                                    }
                                                    let mut selected =
                                                        self.selected_md5s.contains(hash);
                                                    if let Some(map) = scanned_maps
                                                        .as_ref()
                                                        .and_then(|maps| maps.get(hash.as_str()))
                                                    {
                                                        let label = self.map_result_label(map);
                                                        let changed = ui
                                                            .horizontal(|ui| {
                                                                let changed = ui
                                                                    .checkbox(&mut selected, "")
                                                                    .changed();
                                                                wrapped_label(ui, label);
                                                                changed
                                                            })
                                                            .inner;
                                                        if changed {
                                                            if selected {
                                                                self.select_map(map);
                                                            } else {
                                                                self.deselect_md5(hash);
                                                            }
                                                        }
                                                    } else if self.scan.is_some() {
                                                        let changed = ui
                                                            .horizontal(|ui| {
                                                                let changed = ui
                                                                    .checkbox(&mut selected, "")
                                                                    .changed();
                                                                scan_status_label(
                                                                    ui, "missing", hash,
                                                                );
                                                                changed
                                                            })
                                                            .inner;
                                                        if changed {
                                                            if selected {
                                                                self.select_missing_hash(hash);
                                                            } else {
                                                                self.deselect_md5(hash);
                                                            }
                                                        }
                                                    } else {
                                                        muted_label(
                                                            ui,
                                                            "Scan Songs to resolve this collection's hashes to local maps.",
                                                        );
                                                        break;
                                                    }
                                                    shown += 1;
                                                }
                                                if collection.hashes.len() > shown {
                                                    muted_label(
                                                        ui,
                                                        format!(
                                                            "{} more map(s)",
                                                            collection.hashes.len() - shown
                                                        ),
                                                    );
                                                }
                                            });
                                    }
                                }
                                if !self.collection_missing_hashes.is_empty() {
                                    muted_label(
                                        ui,
                                        format!(
                                            "{} loaded hash(es) are not present in the current scan and will be preserved while still selected.",
                                            self.collection_missing_hashes.len()
                                        ),
                                    );
                                }
                            });

                    });
                }
                    }
                    AppTab::Collections => {
                        self.render_collections_page(ui, ctx);
                    }
                    AppTab::RepairsDelete => {
                        let content_width = ui.available_width().max(1.0);
                        egui::ScrollArea::vertical()
                            .id_source("repairs_delete_pane")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let card_item_spacing = ui.spacing().item_spacing;
                                let card_gap = 8.0;
                                ui.spacing_mut().item_spacing.y = 0.0;
                                fix_ui_width(ui, content_width);

                                if let Some(scan) = &self.scan {
                                    let jobs = &self.repair_jobs_cache;
                                    let missing_file_issues = scan
                                        .problems
                                        .iter()
                                        .filter(|issue| {
                                            issue.severity == RepairSeverity::MissingRequiredFile
                                        })
                                        .count();
                                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = card_item_spacing;
                                        fill_tile_width(ui);
                                        ui.heading("Repair corrupted beatmaps");
                                        muted_label(
                                            ui,
                                            format!(
                                                "{} missing-file issue(s), {} downloadable beatmapset(s)",
                                                missing_file_issues,
                                                jobs.len()
                                            ),
                                        );
                                        muted_label(
                                            ui,
                                            "Repair redownloads the beatmapset and restores only the missing files.",
                                        );
                                        ui.horizontal(|ui| {
                                            ui.label("Backend URL");
                                            let input_width =
                                                (ui.available_width() - 8.0).max(120.0);
                                            ui.add_sized(
                                                [input_width, 24.0],
                                                egui::TextEdit::singleline(
                                                    &mut self.repair_backend_url,
                                                )
                                                .hint_text("https://<worker>.workers.dev"),
                                            );
                                        });
                                        ui.horizontal_wrapped(|ui| {
                                            if self.oauth_session.is_some() {
                                                if ui.button("Sign out of osu!").clicked() {
                                                    sign_out_requested = true;
                                                }
                                            } else if ui
                                                .add_enabled(
                                                    !self.is_signing_in,
                                                    egui::Button::new("Sign in with osu!"),
                                                )
                                                .clicked()
                                            {
                                                sign_in_requested = true;
                                            }
                                            if self.is_signing_in {
                                                ui.add(egui::Spinner::new());
                                            }
                                        });
                                        scan_status_label(ui, "osu! sign-in", &self.oauth_status.clone());
                                        if self.is_signing_in {
                                            if let Some(mut pending_url) =
                                                self.oauth_pending_url.clone()
                                            {
                                                muted_label(
                                                    ui,
                                                    "If no browser tab opened, paste this URL into your browser manually:",
                                                );
                                                ui.horizontal(|ui| {
                                                    let input_width =
                                                        (ui.available_width() - 60.0).max(120.0);
                                                    ui.add_sized(
                                                        [input_width, 24.0],
                                                        egui::TextEdit::singleline(&mut pending_url)
                                                            .interactive(false),
                                                    );
                                                    if ui.button("Copy").clicked() {
                                                        ui.ctx().copy_text(pending_url);
                                                    }
                                                });
                                                muted_label(
                                                    ui,
                                                    "If osu! shows 401 invalid_client instead of an approve page, the OAuth app's callback URL is misregistered: open it at osu.ppy.sh/home/account/edit#oauth and set it to exactly http://127.0.0.1:3000/callback, then sign in again.",
                                                );
                                            }
                                        } else if self.oauth_session.is_none() {
                                            muted_label(
                                                ui,
                                                "Without sign-in, downloads fall back to the mirror. Sign in to download via the official osu! API.",
                                            );
                                        }
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add_enabled(
                                                    !self.is_repairing && !jobs.is_empty(),
                                                    egui::Button::new("Repair all"),
                                                )
                                                .clicked()
                                            {
                                                repair_requested = true;
                                            }
                                            if self.is_repairing {
                                                ui.add(egui::Spinner::new());
                                            }
                                        });
                                        if jobs.is_empty() && missing_file_issues > 0 {
                                            wrapped_label(ui, "These findings do not have a usable beatmapset ID. Rescan after this build; the app now infers IDs from osu! song folder names.");
                                        }
                                        if self.is_repairing {
                                            wrapped_label(ui, &self.repair_progress);
                                        }
                                        if self.repair_total > 0 {
                                            let progress =
                                                self.repair_done as f32 / self.repair_total as f32;
                                            ui.add(egui::ProgressBar::new(progress).text(format!(
                                                "{}/{} complete | {} succeeded | {} failed",
                                                self.repair_done,
                                                self.repair_total,
                                                self.repair_successes,
                                                self.repair_failures
                                            )));
                                        }
                                        if !jobs.is_empty() || !self.repair_log.is_empty() {
                                            ui.add_space(6.0);
                                            egui::ScrollArea::vertical()
                                                .id_source("repairable_sets")
                                                .max_height(280.0)
                                                .show(ui, |ui| {
                                                    for entry in &self.repair_log {
                                                        status_log_label(
                                                            ui,
                                                            entry.status,
                                                            "repairing",
                                                            "repaired",
                                                            "failed",
                                                            entry.beatmapset_id,
                                                            &entry.message,
                                                        );
                                                    }
                                                    for job in jobs {
                                                        let beatmapset_id = job.beatmapset_id;
                                                        nested_frame(ui.style()).show(ui, |ui| {
                                                            fill_tile_width(ui);
                                                            ui.horizontal(|ui| {
                                                                wrapped_label(
                                                                    ui,
                                                                    format!(
                                                                        "Set {}: {} corrupted map(s)",
                                                                        beatmapset_id,
                                                                        job.labels.len()
                                                                    ),
                                                                );
                                                                if ui
                                                                    .add_enabled(
                                                                        !self.is_repairing,
                                                                        egui::Button::new(
                                                                            "Repair this set",
                                                                        ),
                                                                    )
                                                                    .clicked()
                                                                {
                                                                    repair_single_requested =
                                                                        Some(beatmapset_id);
                                                                }
                                                            });
                                                            if !job.missing_files.is_empty() {
                                                                wrapped_label(
                                                                    ui,
                                                                    format!(
                                                                        "  Missing: {}",
                                                                        job.missing_files.join(", ")
                                                                    ),
                                                                );
                                                            }
                                                            for issue in &job.issues {
                                                                wrapped_label(ui, format!("  {issue}"));
                                                            }
                                                        });
                                                    }
                                                });
                                ui.horizontal(|ui| {
                                    let add_to_req = &mut add_to_collection_requested;
                                    let sel_name = &mut selected_collection_name;
                                    let selected_text = self
                                        .selected_collection_index
                                        .and_then(|i| self.collections.get(i))
                                        .map(|c| c.name.as_str())
                                        .unwrap_or("Choose collection");
                                    egui::ComboBox::from_id_source("scan_add_to_collection")
                                        .selected_text(selected_text)
                                        .width(140.0)
                                        .show_ui(ui, |ui| {
                                            for (i, c) in self.collections.iter().enumerate() {
                                                ui.selectable_value(
                                                    &mut self.selected_collection_index,
                                                    Some(i),
                                                    format!("{} ({} map{})", c.name, c.hashes.len(),
                                                        if c.hashes.len() == 1 { "" } else { "s" }),
                                                );
                                            }
                                        });
                                    if ui
                                        .add_enabled(
                                            self.selected_collection_index.is_some(),
                                            egui::Button::new("Add to"),
                                        )
                                        .clicked()
                                    {
                                        *sel_name = self
                                            .selected_collection_index
                                            .and_then(|i| self.collections.get(i))
                                            .map(|c| c.name.clone());
                                        *add_to_req = true;
                                    }
                                    if ui
                                        .add_enabled(
                                            self.selected_collection_index.is_some(),
                                            egui::Button::new("Delete"),
                                        )
                                        .clicked()
                                    {
                                        self.delete_confirmation = self
                                            .selected_collection_index
                                            .and_then(|i| self.collections.get(i))
                                            .map(|c| DeleteIntent::Collection(c.name.clone()));
                                    }
                                });
                                        } else if !scan.problems.is_empty() {
                                            ui.add_space(6.0);
                                            egui::ScrollArea::vertical()
                                                .id_source("repair")
                                                .max_height(240.0)
                                                .show(ui, |ui| {
                                                    for issue in &scan.problems {
                                                        let severity = match issue.severity {
                                                            RepairSeverity::MissingRequiredFile => {
                                                                "missing"
                                                            }
                                                            RepairSeverity::ParseWarning => "parse",
                                                        };
                                                        wrapped_label(
                                                            ui,
                                                            format!(
                                                                "{severity}: {} ({})",
                                                                issue.message,
                                                                issue.beatmap.display()
                                                            ),
                                                        );
                                                    }
                                                });
                                        }
                                    });
                                    ui.add_space(card_gap);

                                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = card_item_spacing;
                                        fill_tile_width(ui);
                                        ui.heading("Update outdated beatmaps");
                                        muted_label(
                                            ui,
                                            "Compares your installed difficulties against osu!web checksums (the same signal behind osu!'s \"update to latest version\") and refreshes outdated sets in place. Checking needs only the backend URL; downloads use the osu! sign-in when available and the mirror otherwise.",
                                        );
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add_enabled(
                                                    !self.is_checking_updates
                                                        && !self.is_updating
                                                        && !self.is_scanning,
                                                    egui::Button::new("Check for updates"),
                                                )
                                                .clicked()
                                            {
                                                update_check_requested = true;
                                            }
                                            if self.is_checking_updates || self.is_updating {
                                                ui.add(egui::Spinner::new());
                                            }
                                        });
                                        if self.is_checking_updates {
                                            wrapped_label(ui, &self.update_check_status);
                                            if self.update_check_total > 0 {
                                                ui.add(
                                                    egui::ProgressBar::new(
                                                        self.update_check_done as f32
                                                            / self.update_check_total as f32,
                                                    )
                                                    .text(format!(
                                                        "{}/{} checked",
                                                        self.update_check_done,
                                                        self.update_check_total
                                                    )),
                                                );
                                            }
                                        } else if !self.update_check_status.is_empty() {
                                            wrapped_label(ui, &self.update_check_status);
                                            if self.update_unavailable > 0 {
                                                muted_label(
                                                    ui,
                                                    format!(
                                                        "{} set(s) are no longer available online and were skipped",
                                                        self.update_unavailable
                                                    ),
                                                );
                                            }
                                        }
                                        if !self.outdated_sets.is_empty() {
                                            ui.horizontal(|ui| {
                                                if ui
                                                    .add_enabled(
                                                        !self.is_updating
                                                            && !self.is_checking_updates,
                                                        egui::Button::new(format!(
                                                            "Update all ({})",
                                                            self.outdated_sets.len()
                                                        )),
                                                    )
                                                    .clicked()
                                                {
                                                    update_all_requested = true;
                                                }
                                                if self.is_updating {
                                                    ui.add(egui::Spinner::new());
                                                }
                                            });
                                            if self.is_updating {
                                                wrapped_label(ui, &self.update_progress);
                                            }
                                            if self.update_total > 0 {
                                                let progress = self.update_done as f32
                                                    / self.update_total as f32;
                                                ui.add(
                                                    egui::ProgressBar::new(progress).text(
                                                        format!(
                                                            "{}/{} complete | {} succeeded | {} failed",
                                                            self.update_done,
                                                            self.update_total,
                                                            self.update_successes,
                                                            self.update_failures
                                                        ),
                                                    ),
                                                );
                                            }
                                            ui.add_space(6.0);
                                            egui::ScrollArea::vertical()
                                                .id_source("outdated_sets")
                                                .max_height(280.0)
                                                .show(ui, |ui| {
                                                    for entry in &self.update_log {
                                                        status_log_label(
                                                            ui,
                                                            entry.status,
                                                            "updating",
                                                            "updated",
                                                            "failed",
                                                            entry.beatmapset_id,
                                                            &entry.message,
                                                        );
                                                    }
                                                    for set in &self.outdated_sets {
                                                        let beatmapset_id = set.beatmapset_id;
                                                        nested_frame(ui.style()).show(
                                                            ui,
                                                            |ui| {
                                                                fill_tile_width(ui);
                                                                ui.horizontal(|ui| {
                                                                    let mut header = format!(
                                                                        "{} (set {}): {} of {} diffs outdated",
                                                                        set.title,
                                                                        beatmapset_id,
                                                                        set.outdated_count(),
                                                                        set.total_checked()
                                                                    );
                                                                    if let Some(updated) =
                                                                        &set.remote_updated
                                                                    {
                                                                        header.push_str(&format!(
                                                                            " · online version {updated}"
                                                                        ));
                                                                    }
                                                                    wrapped_label(ui, header);
                                                                    if ui
                                                                        .add_enabled(
                                                                            !self.is_updating
                                                                                && !self.is_checking_updates,
                                                                            egui::Button::new(
                                                                                "Update",
                                                                            ),
                                                                        )
                                                                        .clicked()
                                                                    {
                                                                        update_single_requested =
                                                                            Some(beatmapset_id);
                                                                    }
                                                                });
                                                                for diff in &set.outdated {
                                                                    wrapped_label(
                                                                        ui,
                                                                        format!("  {}", diff.label),
                                                                    );
                                                                }
                                                                if set.unchecked > 0 {
                                                                    muted_label(
                                                                        ui,
                                                                        format!(
                                                                            "  {} diff(s) without an online id could not be checked",
                                                                            set.unchecked
                                                                        ),
                                                                    );
                                                                }
                                                            },
                                                        );
                                                    }
                                                });
                                        } else if !self.is_checking_updates
                                            && self.update_check_total > 0
                                        {
                                            muted_label(
                                                ui,
                                                "All checked beatmaps are up to date.",
                                            );
                                        }
                                    });
                                    ui.add_space(card_gap);

                                    let delete_selection = DeleteModeSelection {
                                        taiko: self.delete_taiko,
                                        catch: self.delete_catch,
                                        mania: self.delete_mania,
                                    };
                                    let delete_count = scan
                                        .maps
                                        .iter()
                                        .filter(|map| delete_selection.matches(map.mode))
                                        .count();
                                    let mode_counts = count_modes(&scan.maps);
                                    ui.add_space(card_gap);
                                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = card_item_spacing;
                                        fill_tile_width(ui);
                                        ui.heading("Delete non-std maps");
                                        muted_label(
                                            ui,
                                            format!(
                                                "Library modes: {} std · {} taiko · {} catch · {} mania{}",
                                                mode_counts.std,
                                                mode_counts.taiko,
                                                mode_counts.catch,
                                                mode_counts.mania,
                                                if mode_counts.unknown > 0 {
                                                    format!(
                                                        " · {} unrecognized",
                                                        mode_counts.unknown
                                                    )
                                                } else {
                                                    String::new()
                                                },
                                            ),
                                        );
                                        muted_label(
                                            ui,
                                            format!(
                                                "{} of {} scanned .osu files declare a Mode field",
                                                mode_counts.with_mode_field, mode_counts.total
                                            ),
                                        );
                                        if mode_counts.taiko
                                            + mode_counts.catch
                                            + mode_counts.mania
                                            == 0
                                        {
                                            muted_label(
                                                ui,
                                                "No non-std .osu files were detected. Converted maps share the original std file, so only natively mapped taiko/catch/mania files can appear here — rescan if you added some.",
                                            );
                                        }
                                        ui.horizontal_wrapped(|ui| {
                                            ui.checkbox(&mut self.delete_taiko, "Taiko");
                                            ui.checkbox(&mut self.delete_catch, "Catch");
                                            ui.checkbox(&mut self.delete_mania, "Mania");
                                        });
                                        muted_label(
                                            ui,
                                            format!(
                                                "{delete_count} scanned .osu file(s) match selected mode(s)"
                                            ),
                                        );
                                        if ui
                                            .add_enabled(
                                                !self.is_scanning
                                                    && !self.is_repairing
                                                    && delete_count > 0,
                                                egui::Button::new("Delete selected non-std maps"),
                                            )
                                            .clicked()
                                        {
                                            delete_non_std_requested = true;
                                        }
                                    });
                                } else {
                                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = card_item_spacing;
                                        fill_tile_width(ui);
                                        ui.heading("Repairs and delete");
                                        muted_label(ui, "No scan loaded.");
                                    });
                                }
                            });
                    }
                }
            });

        if repair_requested {
            self.start_repair_all();
        }
        if let Some(beatmapset_id) = repair_single_requested {
            self.start_repair_single(beatmapset_id);
        }
        if update_check_requested {
            self.start_update_check();
        }
        if update_all_requested {
            self.start_update_all();
        }
        if let Some(beatmapset_id) = update_single_requested {
            self.start_update_single(beatmapset_id);
        }
        if sign_in_requested {
            self.start_oauth_login();
        }
        if sign_out_requested {
            self.sign_out();
        }
        if delete_non_std_requested {
            let selection = DeleteModeSelection {
                taiko: self.delete_taiko,
                catch: self.delete_catch,
                mania: self.delete_mania,
            };
            let count = self
                .scan
                .as_ref()
                .map_or(0, |scan| {
                    scan.maps
                        .iter()
                        .filter(|map| selection.matches(map.mode))
                        .count()
                });
            self.delete_confirmation = Some(DeleteIntent::NonStdModes(count));
        }
        if load_collections_requested {
            self.load_collections();
        }
        if load_collection_selection_requested {
            self.load_selected_collection_into_selection();
        }
        if save_collection_requested {
            self.save_selection_to_collection();
        }
        if create_collection_requested {
            self.create_collection(&self.collection_name.clone());
        }
        if add_to_collection_requested {
            if let Some(name) = selected_collection_name.as_deref() {
                self.add_selected_to_collection(name);
            }
        }

        self.maybe_show_delete_confirmation(ctx);
    }
}

fn compact_map_details(map: &LocalBeatmap) -> String {
    let mut details = vec![format!("mapped by {}", map.creator), map.version.clone()];
    if let Some(stars) = map.stars {
        details.push(format!("{}★", format_number(stars)));
    }
    if let Some(bpm) = map.bpm {
        details.push(format!("{} BPM", format_number(bpm)));
    }
    if let Some(length) = map.length_seconds {
        details.push(format_duration(length));
    }
    details.join("  ·  ")
}

fn inspector_grid_value(ui: &mut egui::Ui, label: &str, value: impl std::fmt::Display) {
    ui.label(egui::RichText::new(label).color(egui::Color32::from_rgb(0x9f, 0x9a, 0x93)));
    ui.label(value.to_string());
}

fn optional_number(value: Option<f32>) -> String {
    value.map(format_number).unwrap_or_else(|| "—".to_owned())
}

fn mode_label(mode: Option<u8>) -> &'static str {
    // A missing Mode field means osu!std in the .osu format.
    match mode {
        None | Some(0) => "osu!std",
        Some(1) => "Taiko",
        Some(2) => "Catch",
        Some(3) => "Mania",
        _ => "Unknown",
    }
}

fn load_background_image(path: &Path) -> Result<egui::ColorImage> {
    if let Some(image) = load_background_jpeg_fast(path)? {
        return Ok(image);
    }

    let image = image::io::Reader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("detecting image format for {}", path.display()))?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?
        .thumbnail(1200, 675)
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        size,
        image.as_raw(),
    ))
}

fn load_background_jpeg_fast(path: &Path) -> Result<Option<egui::ColorImage>> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder = jpeg_decoder::Decoder::new(&mut file);
    decoder
        .read_info()
        .with_context(|| format!("reading jpeg header for {}", path.display()))?;

    let Some(info) = decoder.info() else {
        return Ok(None);
    };
    if !matches!(
        info.pixel_format,
        jpeg_decoder::PixelFormat::RGB24 | jpeg_decoder::PixelFormat::L8
    ) {
        return Ok(None);
    }

    // Decodes at a reduced IDCT size (1/8..1) and reports the actual output
    // dimensions, which can differ from the header size above.
    let (scaled_width, scaled_height) = decoder
        .scale(BACKGROUND_PREVIEW_WIDTH, BACKGROUND_PREVIEW_HEIGHT)
        .with_context(|| format!("configuring jpeg scaling for {}", path.display()))?;
    let pixels = decoder
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?;

    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => pixels,
        jpeg_decoder::PixelFormat::L8 => pixels
            .iter()
            .flat_map(|&luminance| [luminance, luminance, luminance])
            .collect(),
        _ => return Ok(None),
    };
    let decoded = image::RgbImage::from_raw(
        u32::from(scaled_width),
        u32::from(scaled_height),
        rgb,
    )
    .with_context(|| format!("decoding {}", path.display()))?;
    let thumbnail = image::DynamicImage::ImageRgb8(decoded)
        .thumbnail(
            u32::from(BACKGROUND_PREVIEW_WIDTH),
            u32::from(BACKGROUND_PREVIEW_HEIGHT),
        )
        .to_rgba8();
    let size = [thumbnail.width() as usize, thumbnail.height() as usize];
    Ok(Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        thumbnail.as_raw(),
    )))
}

fn ffmpeg_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("FFMPEG_PATH") {
        return PathBuf::from(path);
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(app_dir) = current_exe.parent()
    {
        let executable_name = if cfg!(target_os = "windows") {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        for candidate in [
            app_dir.join(executable_name),
            app_dir.join("tools").join(executable_name),
        ] {
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    PathBuf::from("ffmpeg")
}

fn open_with_default_app(path: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer.exe")
            .arg(path)
            .spawn()
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ModeCounts {
    std: usize,
    taiko: usize,
    catch: usize,
    mania: usize,
    unknown: usize,
    with_mode_field: usize,
    total: usize,
}

fn count_modes(maps: &[LocalBeatmap]) -> ModeCounts {
    let mut counts = ModeCounts {
        total: maps.len(),
        ..ModeCounts::default()
    };
    for map in maps {
        if map.has_mode_field {
            counts.with_mode_field += 1;
        }
        match map.mode {
            None | Some(0) => counts.std += 1,
            Some(1) => counts.taiko += 1,
            Some(2) => counts.catch += 1,
            Some(3) => counts.mania += 1,
            _ => counts.unknown += 1,
        }
    }
    counts
}

#[derive(Debug, Clone)]
struct DeleteModeSelection {
    taiko: bool,
    catch: bool,
    mania: bool,
}

impl DeleteModeSelection {
    fn any(&self) -> bool {
        self.taiko || self.catch || self.mania
    }

    fn matches(&self, mode: Option<u8>) -> bool {
        match mode {
            Some(1) => self.taiko,
            Some(2) => self.catch,
            Some(3) => self.mania,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct RepairJob {
    beatmapset_id: i64,
    labels: Vec<String>,
    folders: Vec<PathBuf>,
    issues: Vec<String>,
    /// Asset file names (e.g. `audio.mp3`, `bg.jpg`) that were reported
    /// missing on disk. The repair restores exactly these from the download.
    missing_files: Vec<String>,
    ignore_after_success: Vec<IgnoredRepairIssue>,
}

fn label_value_for_field(map: &LocalBeatmap, field: SearchField) -> Option<String> {
    match field {
        SearchField::StarRating => map.stars.map(|value| format!("*{}", format_number(value))),
        SearchField::ApproachRate => map.ar.map(|value| format!("AR {}", format_number(value))),
        SearchField::CircleSize => map.cs.map(|value| format!("CS {}", format_number(value))),
        SearchField::OverallDifficulty => {
            map.od.map(|value| format!("OD {}", format_number(value)))
        }
        SearchField::HpDrain => map.hp.map(|value| format!("HP {}", format_number(value))),
        SearchField::Bpm => map.bpm.map(|value| format!("{} BPM", format_number(value))),
        SearchField::Length => map
            .length_seconds
            .map(|value| format!("{} length", format_duration(value))),
        SearchField::Circles => Some(format!("{} circles", map.circles)),
        SearchField::Sliders => Some(format!("{} sliders", map.sliders)),
        SearchField::Keys => map.cs.map(|value| format!("{}K", format_number(value))),
        SearchField::Mode => map.mode.map(|value| format!("mode {}", value)),
        _ => None,
    }
}

fn matches_visible_filters(query: &BeatmapQuery, only_osu_std: bool, map: &LocalBeatmap) -> bool {
    (!only_osu_std || is_osu_std(map)) && query.matches_local(map)
}

fn scan_progress_status(scanned_maps: usize, matched_maps: usize) -> String {
    format!("Maps read: {scanned_maps:>6} | Current filters: {matched_maps:>6}")
}

fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let bg = egui::Color32::from_rgb(0x15, 0x16, 0x18);
    let panel = egui::Color32::from_rgb(0x1e, 0x1f, 0x22);
    let surface = egui::Color32::from_rgb(0x26, 0x27, 0x2b);
    let surface_hover = egui::Color32::from_rgb(0x30, 0x31, 0x35);
    let border = egui::Color32::from_rgb(0x3b, 0x3c, 0x41);
    let text = egui::Color32::from_rgb(0xe3, 0xdf, 0xd7);
    let muted = egui::Color32::from_rgb(0xb3, 0xad, 0xa5);
    let accent = egui::Color32::from_rgb(0x9a, 0x8f, 0x78);
    let accent_soft = egui::Color32::from_rgb(0x3a, 0x35, 0x2b);

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size = egui::vec2(80.0, 28.0);
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(text);
    style.visuals.panel_fill = bg;
    style.visuals.window_fill = panel;
    style.visuals.extreme_bg_color = bg;
    style.visuals.faint_bg_color = surface;
    style.visuals.code_bg_color = surface;
    style.visuals.hyperlink_color = accent;
    style.visuals.selection.bg_fill = accent_soft;
    style.visuals.selection.stroke = egui::Stroke::new(1.0, text);
    style.visuals.warn_fg_color = egui::Color32::from_rgb(0xc4, 0xa2, 0x6a);
    style.visuals.error_fg_color = egui::Color32::from_rgb(0xc2, 0x6b, 0x72);
    style.visuals.window_rounding = egui::Rounding::same(10.0);
    style.visuals.menu_rounding = egui::Rounding::same(8.0);
    style.visuals.window_stroke = egui::Stroke::new(1.0, border);
    style.visuals.widgets.noninteractive.bg_fill = surface;
    style.visuals.widgets.noninteractive.weak_bg_fill = surface;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, border);
    style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, muted);
    style.visuals.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.inactive.bg_fill = surface;
    style.visuals.widgets.inactive.weak_bg_fill = surface;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, border);
    style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, text);
    style.visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.hovered.bg_fill = surface_hover;
    style.visuals.widgets.hovered.weak_bg_fill = surface_hover;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, accent);
    style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, text);
    style.visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.active.bg_fill = accent_soft;
    style.visuals.widgets.active.weak_bg_fill = accent_soft;
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent);
    style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, text);
    style.visuals.widgets.active.rounding = egui::Rounding::same(8.0);
    ctx.set_style(style);
}

fn panel_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::side_top_panel(style)
        .inner_margin(egui::Margin::symmetric(14.0, 10.0))
        .fill(egui::Color32::from_rgb(0x18, 0x19, 0x1c))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgb(0x32, 0x33, 0x37),
        ))
}

fn section_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::group(style)
        .inner_margin(egui::Margin::same(12.0))
        .outer_margin(egui::Margin::same(0.0))
        .rounding(egui::Rounding::same(10.0))
        .fill(egui::Color32::from_rgb(0x20, 0x21, 0x24))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgb(0x3a, 0x3b, 0x40),
        ))
}

fn inspector_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::group(style)
        .inner_margin(egui::Margin::same(12.0))
        .outer_margin(egui::Margin::same(0.0))
        .rounding(egui::Rounding::same(8.0))
        .fill(egui::Color32::from_rgb(0x20, 0x21, 0x24))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgb(0x3a, 0x3b, 0x40),
        ))
}

fn nested_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::group(style)
        .inner_margin(egui::Margin::same(10.0))
        .rounding(egui::Rounding::same(8.0))
        .fill(egui::Color32::from_rgb(0x26, 0x27, 0x2b))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgb(0x3a, 0x3b, 0x40),
        ))
}

fn muted_label(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text.into()).color(egui::Color32::from_rgb(0xb3, 0xad, 0xa5)),
        )
        .wrap(true),
    );
}

fn scan_status_label(ui: &mut egui::Ui, prefix: &str, value: &str) {
    let text = format!("{prefix}: {value}");
    let width = ui.available_width().max(1.0);
    ui.add_sized([width, 18.0], egui::Label::new(text.clone()).truncate(true))
        .on_hover_text(text);
}

fn wrapped_label(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) {
    ui.add(egui::Label::new(text).wrap(true));
}

fn fix_ui_width(ui: &mut egui::Ui, width: f32) {
    let width = width.max(1.0);
    ui.set_width(width);
    ui.set_min_width(width);
    ui.set_max_width(width);
}

fn fill_tile_width(ui: &mut egui::Ui) {
    fix_ui_width(ui, ui.available_width());
}

fn status_log_label(
    ui: &mut egui::Ui,
    status: RepairLogStatus,
    in_progress: &'static str,
    success: &'static str,
    failed: &'static str,
    beatmapset_id: i64,
    message: &str,
) {
    let (label, color) = match status {
        RepairLogStatus::InProgress => (in_progress, None),
        RepairLogStatus::Success => (success, Some(egui::Color32::from_rgb(0x7f, 0xa6, 0x86))),
        RepairLogStatus::Failed => (failed, Some(egui::Color32::from_rgb(0xc2, 0x6b, 0x72))),
    };
    let number_color = egui::Color32::from_rgb(0xb0, 0x9d, 0x7d);

    ui.horizontal_wrapped(|ui| {
        let mut rich = egui::RichText::new(format!("{label}:")).strong();
        if let Some(color) = color {
            rich = rich.color(color);
        }
        ui.label(rich);
        ui.label("set");
        ui.label(
            egui::RichText::new(beatmapset_id.to_string())
                .color(number_color)
                .strong(),
        );
        ui.label("-");
        colored_number_text(ui, message, number_color);
    });
}

fn colored_number_text(ui: &mut egui::Ui, text: &str, number_color: egui::Color32) {
    let mut chunk = String::new();
    let mut chunk_is_number = false;

    for ch in text.chars() {
        let is_number = ch.is_ascii_digit();
        if !chunk.is_empty() && is_number != chunk_is_number {
            add_number_text_chunk(ui, &chunk, chunk_is_number, number_color);
            chunk.clear();
        }
        chunk.push(ch);
        chunk_is_number = is_number;
    }

    if !chunk.is_empty() {
        add_number_text_chunk(ui, &chunk, chunk_is_number, number_color);
    }
}

fn add_number_text_chunk(
    ui: &mut egui::Ui,
    text: &str,
    is_number: bool,
    number_color: egui::Color32,
) {
    if is_number {
        ui.label(egui::RichText::new(text).color(number_color).strong());
    } else {
        ui.label(text);
    }
}

fn is_osu_std(map: &LocalBeatmap) -> bool {
    map.mode.unwrap_or(0) == 0
}

fn build_sets_for_scan(maps: &[LocalBeatmap]) -> Vec<LocalBeatmapSet> {
    let mut grouped = BTreeMap::<(Option<i64>, PathBuf), Vec<LocalBeatmap>>::new();
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

fn format_number(value: f32) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    if (rounded.fract()).abs() < f32::EPSILON {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

fn format_duration(seconds: f32) -> String {
    let total = seconds.round().max(0.0) as u32;
    format!("{}:{:02}", total / 60, total % 60)
}

fn repair_jobs(scan: &LibraryScan) -> Vec<RepairJob> {
    let corrupted_paths: BTreeSet<_> = scan
        .problems
        .iter()
        .filter(|issue| issue.severity == RepairSeverity::MissingRequiredFile)
        .map(|issue| issue.beatmap.clone())
        .collect();
    let mut issue_messages = BTreeMap::<PathBuf, Vec<String>>::new();
    for issue in scan
        .problems
        .iter()
        .filter(|issue| issue.severity == RepairSeverity::MissingRequiredFile)
    {
        issue_messages
            .entry(issue.beatmap.clone())
            .or_default()
            .push(issue.message.clone());
    }

    let mut grouped = BTreeMap::<
        i64,
        (
            Vec<String>,
            BTreeSet<PathBuf>,
            Vec<String>,
            BTreeSet<String>,
            Vec<IgnoredRepairIssue>,
        ),
    >::new();

    for map in &scan.maps {
        if !corrupted_paths.contains(&map.path) {
            continue;
        }
        if let Some(beatmapset_id) = map.beatmapset_id {
            let entry = grouped.entry(beatmapset_id).or_default();
            entry.0.push(map.label());
            entry.1.insert(map.folder.clone());
            for missing in missing_asset_filenames(map) {
                entry.3.insert(missing);
            }
            for message in issue_messages.get(&map.path).into_iter().flatten() {
                let file = map
                    .path
                    .file_name()
                    .and_then(|file| file.to_str())
                    .unwrap_or("unknown .osu");
                entry.2.push(format!("{file}: {message}"));
                if message
                    .to_ascii_lowercase()
                    .contains("missing background file")
                {
                    entry.3.insert(
                        map.background_filename
                            .clone()
                            .unwrap_or_else(|| file.to_owned()),
                    );
                    entry.4.push(IgnoredRepairIssue {
                        beatmapset_id: map.beatmapset_id,
                        beatmap_id: map.beatmap_id,
                        beatmap_file: file.to_owned(),
                        message: message.clone(),
                    });
                }
            }
        }
    }

    grouped
        .into_iter()
        .map(
            |(beatmapset_id, (labels, folders, issues, missing_files, ignore_after_success))| {
                RepairJob {
                    beatmapset_id,
                    labels,
                    folders: folders.into_iter().collect(),
                    issues,
                    missing_files: missing_files.into_iter().collect(),
                    ignore_after_success,
                }
            },
        )
        .collect()
}

/// Asset file names the scanner flagged as missing for this map. Derived from
/// the map's own audio/background references (not by parsing message text) so
/// the repair knows exactly which files to restore from the download.
fn missing_asset_filenames(map: &LocalBeatmap) -> Vec<String> {
    let mut missing = Vec::new();
    if let Some(audio) = &map.audio_filename
        && !map.folder.join(audio).exists()
    {
        missing.push(audio.clone());
    }
    if let Some(background) = &map.background_filename
        && !map.folder.join(background).exists()
    {
        missing.push(background.clone());
    }
    missing
}

fn upsert_log(
    log: &mut Vec<RepairLogEntry>,
    beatmapset_id: i64,
    status: RepairLogStatus,
    message: String,
) {
    if let Some(entry) = log
        .iter_mut()
        .find(|entry| entry.beatmapset_id == beatmapset_id)
    {
        entry.status = status;
        entry.message = message;
    } else {
        log.push(RepairLogEntry {
            beatmapset_id,
            status,
            message,
        });
    }
}

fn run_repair_jobs(
    jobs: Vec<RepairJob>,
    backend_url: String,
    osu_root: String,
    mut oauth_session: Option<OauthSession>,
    tx: mpsc::Sender<RepairEvent>,
) {
    let total = jobs.len();
    let _ = tx.send(RepairEvent::Started { total });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    // Refresh the osu! token once up front so every download in this batch can
    // use the official osu! API. On failure the session is dropped and the
    // downloads fall back to the mirror instead of failing the whole batch.
    if oauth_session.is_some()
        && osu_oauth::ensure_access_token(&client, &backend_url, &mut oauth_session).is_ok()
        && let Some(session) = &oauth_session
    {
        let _ = osu_oauth::save_oauth_session(&osu_root, session);
    }

    for (index, job) in jobs.iter().enumerate() {
        let current = index + 1;
        if index > 0 {
            thread::sleep(BEATMAPSET_DOWNLOAD_DELAY);
        }
        let _ = tx.send(RepairEvent::Opening {
            beatmapset_id: job.beatmapset_id,
            index: current,
            total,
        });
        match repair_beatmapset(&client, job, &backend_url, oauth_session.as_ref()) {
            Ok(outcome) => {
                let _ = tx.send(RepairEvent::Repaired {
                    beatmapset_id: job.beatmapset_id,
                    folder_count: job.folders.len(),
                    restored_files: outcome.restored_files,
                    download_source: outcome.download_source,
                    ignored_after_success: job.ignore_after_success.clone(),
                });
            }
            Err(err) => {
                let _ = tx.send(RepairEvent::Failed {
                    beatmapset_id: job.beatmapset_id,
                    message: format!("{err:#}"),
                });
            }
        }
    }

    let _ = tx.send(RepairEvent::Finished);
}

fn run_update_check(
    targets: Vec<updates::CheckTarget>,
    uncheckable: usize,
    backend_url: String,
    tx: mpsc::Sender<UpdateCheckEvent>,
) {
    let total = targets.len();
    let _ = tx.send(UpdateCheckEvent::Started { total, uncheckable });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    let mut first_request = true;
    for (position, target) in targets.iter().enumerate() {
        let done = position + 1;
        if let Some(set_id) = target.beatmapset_id {
            pace_update_requests(&mut first_request);
            check_single_set(&client, &backend_url, set_id, &target.locals, &tx);
        } else {
            // No set id on file: resolve each beatmap id online, then check
            // the discovered sets.
            match updates::resolve_unknown_sets(&client, &backend_url, &target.locals) {
                Ok((grouped, _unresolved)) => {
                    for (set_id, resolved) in grouped {
                        pace_update_requests(&mut first_request);
                        check_single_set(&client, &backend_url, set_id, &resolved, &tx);
                    }
                }
                Err(err) => {
                    let _ = tx.send(UpdateCheckEvent::Unavailable {
                        beatmapset_id: None,
                        reason: format!("resolving beatmap ids failed: {err:#}"),
                    });
                }
            }
        }
        let _ = tx.send(UpdateCheckEvent::Checked { done, total });
    }

    let _ = tx.send(UpdateCheckEvent::Finished);
}

fn pace_update_requests(first_request: &mut bool) {
    if *first_request {
        *first_request = false;
    } else {
        thread::sleep(UPDATE_CHECK_DELAY);
    }
}

fn check_single_set(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    beatmapset_id: i64,
    locals: &[updates::LocalDiffRef],
    tx: &mpsc::Sender<UpdateCheckEvent>,
) {
    let folders = locals
        .iter()
        .map(|local| local.folder.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    match updates::fetch_set_meta_blocking(client, backend_url, beatmapset_id) {
        Err(err) => {
            let _ = tx.send(UpdateCheckEvent::Unavailable {
                beatmapset_id: Some(beatmapset_id),
                reason: format!("{err:#}"),
            });
        }
        Ok(None) => {
            let _ = tx.send(UpdateCheckEvent::Unavailable {
                beatmapset_id: Some(beatmapset_id),
                reason: "no longer available online".to_owned(),
            });
        }
        Ok(Some(remote)) => {
            let title = format!("{} - {}", remote.artist, remote.title);
            let set = updates::detect_outdated(
                beatmapset_id,
                title,
                remote.last_updated.clone(),
                locals,
                &remote,
                folders,
            );
            if set.outdated_count() > 0 {
                let _ = tx.send(UpdateCheckEvent::Found(set));
            }
        }
    }
}

fn run_update_jobs(
    jobs: Vec<updates::UpdateJob>,
    backend_url: String,
    osu_root: String,
    mut oauth_session: Option<OauthSession>,
    tx: mpsc::Sender<UpdateEvent>,
) {
    let total = jobs.len();
    let _ = tx.send(UpdateEvent::Started { total });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    if oauth_session.is_some()
        && osu_oauth::ensure_access_token(&client, &backend_url, &mut oauth_session).is_ok()
        && let Some(session) = &oauth_session
    {
        let _ = osu_oauth::save_oauth_session(&osu_root, session);
    }
    let access_token = oauth_session
        .as_ref()
        .map(|session| session.access_token.as_str());

    for (index, job) in jobs.iter().enumerate() {
        let current = index + 1;
        if index > 0 {
            thread::sleep(BEATMAPSET_DOWNLOAD_DELAY);
        }
        let _ = tx.send(UpdateEvent::Opening {
            beatmapset_id: job.beatmapset_id,
            index: current,
            total,
        });
        match updates::apply_update_blocking(&client, &backend_url, access_token, job) {
            Ok(outcome) => {
                let _ = tx.send(UpdateEvent::Updated {
                    beatmapset_id: job.beatmapset_id,
                    folders: job.folders.clone(),
                    written: outcome.written_files.len(),
                    removed: outcome.removed_files.len(),
                    download_source: outcome.download_source,
                });
            }
            Err(err) => {
                let _ = tx.send(UpdateEvent::Failed {
                    beatmapset_id: job.beatmapset_id,
                    message: format!("{err:#}"),
                });
            }
        }
    }

    let _ = tx.send(UpdateEvent::Finished);
}

struct RepairOutcome {
    restored_files: Vec<String>,
    download_source: String,
}

fn repair_beatmapset(
    client: &reqwest::blocking::Client,
    job: &RepairJob,
    backend_url: &str,
    oauth_session: Option<&OauthSession>,
) -> Result<RepairOutcome> {
    if backend_url.trim().is_empty() {
        anyhow::bail!("backend URL is required for automatic repair downloads");
    }

    let temp_path = std::env::temp_dir()
        .join("osu-map-manager-repairs")
        .join(format!("{}.osz", job.beatmapset_id));
    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let access_token = oauth_session.map(|session| session.access_token.as_str());
    let download_source = updates::download_beatmapset_file(
        client,
        backend_url,
        job.beatmapset_id,
        access_token,
        &temp_path,
    )?;
    updates::verify_osz(&temp_path)?;
    let mut restored = BTreeSet::new();
    for folder in &job.folders {
        let restored_here = restore_missing_from_osz(&temp_path, folder, &job.missing_files)
            .with_context(|| format!("extracting into {}", folder.display()))?;
        restored.extend(restored_here);
    }
    let restored_files = restored.into_iter().collect::<Vec<_>>();
    verify_repair(job, &restored_files)?;
    Ok(RepairOutcome {
        restored_files,
        download_source,
    })
}

/// Restores only the files flagged as missing (plus any other archive entry
/// whose destination is absent) instead of overwriting the whole song folder,
/// so local scores, edits, and skins are left untouched. Returns the restored
/// file names.
fn restore_missing_from_osz(
    osz_path: &Path,
    folder: &Path,
    missing_files: &[String],
) -> Result<Vec<String>> {
    let wanted = missing_files
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let file = fs::File::open(osz_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    fs::create_dir_all(folder)?;
    let mut restored = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let Some(enclosed_name) = entry.enclosed_name() else {
            continue;
        };
        let file_name = enclosed_name
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        if file_name.is_empty() {
            continue;
        }
        let output_path = folder.join(enclosed_name);
        let destination_missing = !output_path.exists();
        let explicitly_wanted = wanted.contains(&file_name.to_ascii_lowercase());
        if !explicitly_wanted && !destination_missing {
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Never clobber an existing file we were not asked to repair.
        if output_path.exists() && !explicitly_wanted {
            continue;
        }
        let mut output = fs::File::create(&output_path)?;
        io::copy(&mut entry, &mut output)?;
        restored.push(file_name.to_owned());
    }
    restored.sort();
    restored.dedup();
    Ok(restored)
}

/// Confirms every expected missing file now exists on disk after extraction.
fn verify_repair(job: &RepairJob, restored_files: &[String]) -> Result<()> {
    let restored = restored_files
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let mut still_missing = Vec::new();
    for missing in &job.missing_files {
        let present_on_disk = job
            .folders
            .iter()
            .any(|folder| folder.join(missing).exists());
        if !present_on_disk && !restored.contains(&missing.to_ascii_lowercase()) {
            still_missing.push(missing.clone());
        }
    }
    if still_missing.is_empty() {
        return Ok(());
    }
    // Missing entries that are only referenced by stale background ignores are
    // tolerated: the repair still fixed everything else.
    if job.missing_files.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "download did not contain the missing file(s): {}",
        still_missing.join(", ")
    )
}

fn load_repair_ignores(osu_root: &str) -> Result<RepairIgnoreStore> {
    let path = repair_ignore_path(osu_root);
    if !path.exists() {
        return Ok(RepairIgnoreStore::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn save_repair_ignores(osu_root: &str, store: &RepairIgnoreStore) -> Result<()> {
    let path = repair_ignore_path(osu_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(store)?;
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

fn repair_ignore_path(osu_root: &str) -> PathBuf {
    app_data_path(osu_root).join("repair_ignores.json")
}

fn load_scan_cache(osu_root: &str) -> Result<Option<LibraryScan>> {
    let path = scan_cache_path(osu_root);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cache = match serde_json::from_str::<ScanCacheFile>(&text) {
        Ok(cache) => cache,
        Err(_) => return Ok(None),
    };
    if cache.version != SCAN_CACHE_VERSION {
        return Ok(None);
    }
    Ok(Some(cache.scan))
}

fn save_scan_cache(osu_root: &str, scan: &LibraryScan) -> Result<()> {
    let path = scan_cache_path(osu_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(&ScanCacheFile {
        version: SCAN_CACHE_VERSION,
        scan: scan.clone(),
    })?;
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

fn scan_cache_path(osu_root: &str) -> PathBuf {
    app_data_path(osu_root).join("scan_cache.json")
}

fn app_data_path(osu_root: &str) -> PathBuf {
    let root = if osu_root.trim().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        expand_prefilled_path(osu_root)
    };
    root.join(".osu-map-manager")
}

fn default_osu_root() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        candidates.push(path.join("osu!"));
    }
    if let Some(path) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        candidates.push(path.join("AppData").join("Local").join("osu!"));
    }

    candidates.into_iter().find(|path| path.exists())
}

fn display_prefilled_path(path: &Path) -> String {
    if let Some(user_profile) = std::env::var_os("USERPROFILE").map(PathBuf::from)
        && let Ok(suffix) = path.strip_prefix(&user_profile)
    {
        let suffix = suffix.display().to_string();
        return if suffix.is_empty() {
            "%USERPROFILE%".to_owned()
        } else {
            format!("%USERPROFILE%\\{suffix}")
        };
    }

    path.display().to_string()
}

fn expand_prefilled_path(path: &str) -> PathBuf {
    let path = path.trim();
    if let Some(user_profile) = std::env::var_os("USERPROFILE")
        && let Some(suffix) = path.strip_prefix("%USERPROFILE%")
    {
        let suffix = suffix.trim_start_matches(['\\', '/']);
        return PathBuf::from(user_profile).join(suffix);
    }

    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_osz(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut writer, bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn repair_restores_only_missing_files_without_clobbering() {
        let root = unique_temp_dir("osu-repair-restore");
        let osz = root.join("123.osz");
        write_test_osz(
            &osz,
            &[
                ("audio.mp3", b"audio-bytes"),
                ("bg.jpg", b"bg-bytes"),
                ("existing.osu", b"new-osu-bytes"),
            ],
        );
        let folder = root.join("song-folder");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("existing.osu"), b"local-osu-bytes").unwrap();

        let restored =
            restore_missing_from_osz(&osz, &folder, &["audio.mp3".to_owned()]).unwrap();

        // The requested file plus any other archive entry absent on disk.
        assert_eq!(restored, vec!["audio.mp3".to_owned(), "bg.jpg".to_owned()]);
        assert_eq!(fs::read(folder.join("audio.mp3")).unwrap(), b"audio-bytes");
        assert_eq!(fs::read(folder.join("bg.jpg")).unwrap(), b"bg-bytes");
        // Present files are left untouched even when the archive has them.
        assert_eq!(
            fs::read(folder.join("existing.osu")).unwrap(),
            b"local-osu-bytes"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repair_rejects_non_archive_downloads() {
        let root = unique_temp_dir("osu-repair-verify");
        let bad = root.join("bad.osz");
        fs::write(&bad, b"not a zip file at all, just an error page...").unwrap();
        assert!(updates::verify_osz(&bad).is_err());

        let tiny = root.join("tiny.osz");
        fs::write(&tiny, b"PK").unwrap();
        assert!(updates::verify_osz(&tiny).is_err());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn jpeg_fast_path_matches_scaled_output_size() {
        use image::ImageEncoder;

        let root = unique_temp_dir("osu-bg-jpeg");
        // Large enough that the decoder's reduced IDCT size kicks in, which
        // used to mismatch the reported image dimensions.
        let (width, height) = (2000_u32, 1500_u32);
        let rgb = vec![128_u8; (width * height * 3) as usize];
        let path = root.join("bg.jpg");
        let file = fs::File::create(&path).unwrap();
        image::codecs::jpeg::JpegEncoder::new(file)
            .write_image(&rgb, width, height, image::ColorType::Rgb8)
            .unwrap();

        let image = load_background_image(&path).unwrap();
        assert!(image.width() <= BACKGROUND_PREVIEW_WIDTH as usize);
        assert!(image.height() <= BACKGROUND_PREVIEW_HEIGHT as usize);
        assert!(!image.pixels.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mode_counts_treat_missing_mode_as_std() {
        let map_with = |mode: Option<u8>| LocalBeatmap {
            path: Default::default(),
            folder: Default::default(),
            md5: Default::default(),
            beatmap_id: None,
            beatmapset_id: None,
            artist: String::new(),
            title: String::new(),
            source: String::new(),
            creator: String::new(),
            version: String::new(),
            tags: String::new(),
            audio_filename: None,
            background_filename: None,
            mode,
            has_mode_field: mode.is_some(),
            ar: None,
            cs: None,
            od: None,
            hp: None,
            stars: None,
            bpm: None,
            length_seconds: None,
            circles: 0,
            sliders: 0,
        };
        let maps = vec![
            map_with(None),
            map_with(Some(0)),
            map_with(Some(1)),
            map_with(Some(3)),
            map_with(Some(9)),
        ];
        let counts = count_modes(&maps);
        assert_eq!(counts.std, 2);
        assert_eq!(counts.taiko, 1);
        assert_eq!(counts.mania, 1);
        assert_eq!(counts.unknown, 1);
        assert_eq!(mode_label(None), "osu!std");
    }

    #[test]
    fn repair_reports_files_absent_from_download() {
        let root = unique_temp_dir("osu-repair-missing");
        let folder = root.join("song-folder");
        fs::create_dir_all(&folder).unwrap();
        let job = RepairJob {
            beatmapset_id: 42,
            labels: Vec::new(),
            folders: vec![folder],
            issues: Vec::new(),
            missing_files: vec!["gone.mp3".to_owned()],
            ignore_after_success: Vec::new(),
        };
        let err = verify_repair(&job, &[]).unwrap_err();
        assert!(err.to_string().contains("gone.mp3"));

        let _ = fs::remove_dir_all(root);
    }
}
