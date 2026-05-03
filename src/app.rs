use crate::{
    collection,
    local::{self, LibraryScan, LocalBeatmap, LocalBeatmapSet, RepairSeverity, ScanEvent},
    query::{BeatmapQuery, Operator, QueryClause, SearchField},
};
use anyhow::{Context, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
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

const UPDATE_CHECK_DELAY: Duration = Duration::from_millis(25);
const BEATMAPSET_DOWNLOAD_DELAY: Duration = Duration::from_millis(750);

pub struct MapManagerApp {
    query: BeatmapQuery,
    songs_dir: String,
    osu_root: String,
    collection_name: String,
    repair_backend_url: String,
    selected_maps: Vec<LocalBeatmap>,
    selected_md5s: BTreeSet<String>,
    filtered_map_indexes: Vec<usize>,
    filtered_cache_key: String,
    repair_jobs_cache: Vec<RepairJob>,
    repair_jobs_cache_key: String,
    scan: Option<LibraryScan>,
    is_scanning: bool,
    is_repairing: bool,
    is_checking_updates: bool,
    is_updating_maps: bool,
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
    update_jobs: Vec<UpdateJob>,
    update_progress: String,
    update_total: usize,
    update_done: usize,
    update_successes: usize,
    update_failures: usize,
    update_log: Vec<UpdateLogEntry>,
    status: String,
    scan_rx: Option<Receiver<ScanEvent>>,
    scan_cancel: Option<Arc<AtomicBool>>,
    repair_rx: Option<Receiver<RepairEvent>>,
    update_rx: Option<Receiver<UpdateEvent>>,
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
        ignored_after_success: Vec<IgnoredRepairIssue>,
    },
    Failed {
        beatmapset_id: i64,
        message: String,
    },
    Finished,
}

#[derive(Debug)]
enum UpdateEvent {
    CheckStarted {
        total: usize,
    },
    Checking {
        beatmapset_id: i64,
        index: usize,
        total: usize,
    },
    CheckFailed {
        beatmapset_id: i64,
        message: String,
    },
    CheckFinished {
        jobs: Vec<UpdateJob>,
        failures: usize,
    },
    UpdateStarted {
        total: usize,
    },
    Updating {
        beatmapset_id: i64,
        index: usize,
        total: usize,
    },
    Updated {
        beatmapset_id: i64,
        folder_count: usize,
    },
    UpdateFailed {
        beatmapset_id: i64,
        message: String,
    },
    UpdateFinished,
}

#[derive(Debug, Clone)]
struct RepairLogEntry {
    beatmapset_id: i64,
    status: RepairLogStatus,
    message: String,
}

#[derive(Debug, Clone)]
struct UpdateLogEntry {
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
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let osu_root = default_osu_root()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let songs_dir = if osu_root.is_empty() {
            String::new()
        } else {
            PathBuf::from(&osu_root).join("Songs").display().to_string()
        };
        let repair_ignores = load_repair_ignores(&osu_root).unwrap_or_default();

        Self {
            query: BeatmapQuery {
                clauses: vec![
                    QueryClause {
                        field: SearchField::ApproachRate,
                        operator: Operator::Ge,
                        value: "9".to_owned(),
                        enabled: true,
                    },
                    QueryClause {
                        field: SearchField::Length,
                        operator: Operator::Le,
                        value: "180".to_owned(),
                        enabled: true,
                    },
                ],
            },
            songs_dir,
            osu_root,
            collection_name: "osu-map-manager".to_owned(),
            repair_backend_url: "https://osu-map-manager.stanislavberman.workers.dev".to_owned(),
            selected_maps: Vec::new(),
            selected_md5s: BTreeSet::new(),
            filtered_map_indexes: Vec::new(),
            filtered_cache_key: String::new(),
            repair_jobs_cache: Vec::new(),
            repair_jobs_cache_key: String::new(),
            scan: None,
            is_scanning: false,
            is_repairing: false,
            is_checking_updates: false,
            is_updating_maps: false,
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
            update_jobs: Vec::new(),
            update_progress: String::new(),
            update_total: 0,
            update_done: 0,
            update_successes: 0,
            update_failures: 0,
            update_log: Vec::new(),
            status: "Ready".to_owned(),
            scan_rx: None,
            scan_cancel: None,
            repair_rx: None,
            update_rx: None,
        }
    }

    fn poll_background(&mut self) {
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
                        songs_dir,
                        star_ratings_loaded,
                        star_parse_error,
                    } => {
                        self.scan = Some(LibraryScan::default());
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
                        self.status = format!("Scanning {}", songs_dir.display());
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
                        self.status = format!(
                            "Scanning folder {}: {}",
                            self.scanned_folders, self.current_folder
                        );
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
                        self.status = format!(
                            "Scanning... {} maps read, {} match current filters",
                            self.scanned_maps, self.matched_maps
                        );
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
                        ignored_after_success,
                    } => {
                        self.repair_done += 1;
                        self.repair_successes += 1;
                        let ignored_count = self.add_repair_ignores(ignored_after_success);
                        self.upsert_repair_log(
                            beatmapset_id,
                            RepairLogStatus::Success,
                            format!(
                                "Repaired {folder_count} folder(s); ignored {ignored_count} missing background issue(s)"
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
                    UpdateEvent::CheckStarted { total } => {
                        self.is_checking_updates = true;
                        self.update_jobs.clear();
                        self.update_log.clear();
                        self.update_total = total;
                        self.update_done = 0;
                        self.update_failures = 0;
                        self.update_successes = 0;
                        self.update_progress =
                            format!("Checking {total} beatmapset(s) for updates");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Checking {
                        beatmapset_id,
                        index,
                        total,
                    } => {
                        self.update_done = index;
                        self.update_progress =
                            format!("Checking updates {index}/{total}: set {beatmapset_id}");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::CheckFailed {
                        beatmapset_id,
                        message,
                    } => {
                        self.update_failures += 1;
                        self.upsert_update_log(beatmapset_id, RepairLogStatus::Failed, message);
                    }
                    UpdateEvent::CheckFinished { jobs, failures } => {
                        self.is_checking_updates = false;
                        self.update_jobs = jobs;
                        self.update_failures = failures;
                        self.update_progress = format!(
                            "Update check finished: {} beatmapset(s) need update, {} check(s) failed",
                            self.update_jobs.len(),
                            self.update_failures
                        );
                        self.status = self.update_progress.clone();
                        keep_rx = false;
                    }
                    UpdateEvent::UpdateStarted { total } => {
                        self.is_updating_maps = true;
                        self.update_total = total;
                        self.update_done = 0;
                        self.update_successes = 0;
                        self.update_failures = 0;
                        self.update_log.clear();
                        self.update_progress = format!("Updating {total} beatmapset(s)");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Updating {
                        beatmapset_id,
                        index,
                        total,
                    } => {
                        self.upsert_update_log(
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
                        folder_count,
                    } => {
                        self.update_done += 1;
                        self.update_successes += 1;
                        self.upsert_update_log(
                            beatmapset_id,
                            RepairLogStatus::Success,
                            format!("Updated {folder_count} folder(s)"),
                        );
                        self.update_progress =
                            format!("Updated set {beatmapset_id} in {folder_count} folder(s)");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::UpdateFailed {
                        beatmapset_id,
                        message,
                    } => {
                        self.update_done += 1;
                        self.update_failures += 1;
                        self.upsert_update_log(beatmapset_id, RepairLogStatus::Failed, message);
                    }
                    UpdateEvent::UpdateFinished => {
                        self.is_updating_maps = false;
                        self.update_jobs.clear();
                        self.update_progress = format!(
                            "Map update finished: {} succeeded, {} failed out of {}. Rescan to refresh local data.",
                            self.update_successes, self.update_failures, self.update_total
                        );
                        self.status = self.update_progress.clone();
                        keep_rx = false;
                    }
                }
            }

            if keep_rx {
                if disconnected {
                    self.is_checking_updates = false;
                    self.is_updating_maps = false;
                    self.status = "Update worker disconnected".to_owned();
                } else {
                    self.update_rx = Some(rx);
                }
            }
        }
    }

    fn start_scan(&mut self) {
        if self.is_scanning {
            self.status = "A scan is already running".to_owned();
            return;
        }

        let songs_dir = PathBuf::from(self.songs_dir.trim());
        let osu_root =
            (!self.osu_root.trim().is_empty()).then(|| PathBuf::from(self.osu_root.trim()));
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
        self.status = format!("Scanning {}", songs_dir.display());
        std::thread::spawn(move || {
            local::scan_songs_dir_streaming(
                songs_dir,
                osu_root,
                worker_cancel,
                skip_issue_kinds,
                tx,
            );
        });
        self.scan_cancel = Some(cancel);
        self.scan_rx = Some(rx);
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

    fn export_collection(&mut self) {
        if self.selected_maps.is_empty() {
            self.status = "Select at least one local map before exporting a collection".to_owned();
            return;
        }

        let path = if self.osu_root.trim().is_empty() {
            PathBuf::from("collection.db")
        } else {
            PathBuf::from(self.osu_root.trim()).join("collection.db")
        };

        match collection::write_collection_db(&path, &self.collection_name, &self.selected_maps) {
            Ok(()) => self.status = format!("Wrote collection to {}", path.display()),
            Err(err) => self.status = format!("Collection export failed: {err:#}"),
        }
    }

    fn export_manifest(&mut self) {
        let path = PathBuf::from("selected_maps.tsv");
        match collection::write_manifest(&path, &self.selected_maps) {
            Ok(()) => self.status = format!("Wrote {}", path.display()),
            Err(err) => self.status = format!("Manifest export failed: {err:#}"),
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

        let (tx, rx) = mpsc::channel();
        self.status = format!("Starting repair for {} beatmapset(s)", jobs.len());
        let backend_url = self.repair_backend_url.trim().to_owned();
        std::thread::spawn(move || run_repair_jobs(jobs, backend_url, tx));
        self.repair_rx = Some(rx);
    }

    fn start_update_check(&mut self) {
        if self.is_checking_updates || self.is_updating_maps {
            self.status = "An update job is already running".to_owned();
            return;
        }

        let Some(scan) = &self.scan else {
            self.status = "Scan your Songs directory before checking for map updates".to_owned();
            return;
        };

        let sets = update_candidates(scan);
        if sets.is_empty() {
            self.status = "No beatmapsets with usable IDs found for update checking".to_owned();
            return;
        }

        let backend_url = self.repair_backend_url.trim().to_owned();
        let (tx, rx) = mpsc::channel();
        self.status = format!("Checking {} beatmapset(s) for updates", sets.len());
        thread::spawn(move || check_update_jobs(sets, backend_url, tx));
        self.update_rx = Some(rx);
    }

    fn start_update_all(&mut self) {
        if self.is_checking_updates || self.is_updating_maps {
            self.status = "An update job is already running".to_owned();
            return;
        }
        if self.update_jobs.is_empty() {
            self.status = "Check for map updates before updating".to_owned();
            return;
        }

        let jobs = self.update_jobs.clone();
        let backend_url = self.repair_backend_url.trim().to_owned();
        let (tx, rx) = mpsc::channel();
        self.status = format!("Updating {} beatmapset(s)", jobs.len());
        thread::spawn(move || run_update_jobs(jobs, backend_url, tx));
        self.update_rx = Some(rx);
    }

    fn upsert_repair_log(&mut self, beatmapset_id: i64, status: RepairLogStatus, message: String) {
        if let Some(entry) = self
            .repair_log
            .iter_mut()
            .find(|entry| entry.beatmapset_id == beatmapset_id)
        {
            entry.status = status;
            entry.message = message;
        } else {
            self.repair_log.push(RepairLogEntry {
                beatmapset_id,
                status,
                message,
            });
        }
    }

    fn upsert_update_log(&mut self, beatmapset_id: i64, status: RepairLogStatus, message: String) {
        if let Some(entry) = self
            .update_log
            .iter_mut()
            .find(|entry| entry.beatmapset_id == beatmapset_id)
        {
            entry.status = status;
            entry.message = message;
        } else {
            self.update_log.push(UpdateLogEntry {
                beatmapset_id,
                status,
                message,
            });
        }
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
}

impl eframe::App for MapManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background();
        if self.is_scanning
            || self.is_repairing
            || self.is_checking_updates
            || self.is_updating_maps
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("osu! Map Manager");
                ui.separator();
                ui.label(&self.status);
            });
        });

        egui::SidePanel::left("filters")
            .resizable(true)
            .default_width(410.0)
            .show(ctx, |ui| {
                ui.heading("Local filters");
                ui.label("Rows are combined with AND against scanned maps in your Songs directory.");
                ui.add_space(8.0);

                for clause in &mut self.query.clauses {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut clause.enabled, "");
                        egui::ComboBox::from_id_source(("field", clause as *const _ as usize))
                            .selected_text(clause.field.label())
                            .show_ui(ui, |ui| {
                                for field in SearchField::SORTED {
                                    ui.selectable_value(&mut clause.field, field, field.label());
                                }
                            });
                        egui::ComboBox::from_id_source(("op", clause as *const _ as usize))
                            .selected_text(clause.operator.as_str())
                            .width(52.0)
                            .show_ui(ui, |ui| {
                                for operator in Operator::ALL {
                                    ui.selectable_value(
                                        &mut clause.operator,
                                        operator,
                                        operator.as_str(),
                                    );
                                }
                            });
                        ui.text_edit_singleline(&mut clause.value);
                    });
                }

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
                ui.label("Scan issue handling");
                ui.checkbox(&mut self.skip_parse_timeouts, "Skip map parse timeouts");
                ui.checkbox(&mut self.skip_parse_errors, "Skip map parse errors");
                ui.separator();
                ui.label("Equivalent query text");
                let mut query_text = self.query.to_osu_search();
                ui.text_edit_multiline(&mut query_text);
                ui.label("Some osu!web-only fields, such as ranked status and favourites, require database/API metadata and will not match local .osu files yet.");
            });

        self.refresh_filtered_maps();
        self.refresh_repair_jobs();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.columns(2, |columns| {
                columns[0].heading("Local library");
                egui::Grid::new("library_paths")
                    .num_columns(3)
                    .spacing([8.0, 6.0])
                    .show(&mut columns[0], |ui| {
                        ui.label("osu! root");
                        ui.add_sized([260.0, 22.0], egui::TextEdit::singleline(&mut self.osu_root));
                        if ui.button("Pick").clicked() {
                            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                self.osu_root = path.display().to_string();
                                self.songs_dir = path.join("Songs").display().to_string();
                                self.repair_ignores =
                                    load_repair_ignores(&self.osu_root).unwrap_or_default();
                            }
                        }
                        ui.end_row();
                        ui.label("Songs");
                        ui.add_sized([260.0, 22.0], egui::TextEdit::singleline(&mut self.songs_dir));
                        if self.is_scanning {
                            if ui.button("Stop").clicked() {
                                self.stop_scan();
                            }
                        } else if ui.button("Scan").clicked() {
                            self.start_scan();
                        }
                        ui.end_row();
                    });

                if self.is_scanning {
                    columns[0].add(egui::Spinner::new());
                    columns[0].label(format!(
                        "Reading folder {} | {} maps scanned | {} matches",
                        self.scanned_folders, self.scanned_maps, self.matched_maps
                    ));
                    columns[0].label(format!(
                        "Star ratings: {} db entries, {} matched scanned maps",
                        self.star_ratings_loaded, self.maps_with_stars
                    ));
                    if let Some(err) = &self.star_parse_error {
                        columns[0].label(format!("osu!.db fallback parser active: {err}"));
                    }
                    if !self.current_folder.is_empty() {
                        scan_status_label(
                            &mut columns[0],
                            "Current",
                            &self.current_folder,
                        );
                    }
                    if !self.current_map.is_empty() {
                        scan_status_label(&mut columns[0], "Parsing", &self.current_map);
                    }
                }

                if self.scan.is_some() {
                    let (scanned_maps, scanned_sets, repair_issues) =
                        self.scan.as_ref().map_or((0, 0, 0), |scan| {
                            (scan.maps.len(), scan.sets.len(), scan.problems.len())
                        });
                    columns[0].horizontal(|ui| {
                        if ui.button("Select all filtered").clicked() {
                            let maps = self
                                .scan
                                .as_ref()
                                .map(|scan| {
                                    self.filtered_map_indexes
                                        .iter()
                                        .filter_map(|&index| scan.maps.get(index).cloned())
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
                    columns[0].label(format!("{} selected map(s)", self.selected_maps.len()));
                    columns[0].horizontal(|ui| {
                        ui.label("Collection");
                        ui.text_edit_singleline(&mut self.collection_name);
                    });
                    columns[0].horizontal(|ui| {
                        if ui.button("Write collection.db").clicked() {
                            self.export_collection();
                        }
                        if ui.button("Write TSV manifest").clicked() {
                            self.export_manifest();
                        }
                    });
                    columns[0].add_space(8.0);
                    columns[0].label(format!(
                        "{} matching maps from {} scanned maps, {} sets, {} repair issue(s)",
                        self.filtered_map_indexes.len(),
                        scanned_maps,
                        scanned_sets,
                        repair_issues
                    ));
                    egui::ScrollArea::vertical().id_source("local_maps").show_rows(
                        &mut columns[0],
                        24.0,
                        self.filtered_map_indexes.len(),
                        |ui, row_range| {
                            for row in row_range {
                                let Some(map) = self.scan.as_ref().and_then(|scan| {
                                    self.filtered_map_indexes
                                        .get(row)
                                        .and_then(|&index| scan.maps.get(index))
                                        .cloned()
                                }) else {
                                    continue;
                                };
                                let mut selected = self.selected_md5s.contains(&map.md5);
                                if ui
                                    .checkbox(&mut selected, self.map_result_label(&map))
                                    .changed()
                                {
                                    if selected {
                                        self.select_map(&map);
                                    } else {
                                        self.deselect_md5(&map.md5);
                                    }
                                }
                            }
                        },
                    );
                }

                columns[1].heading("Results and actions");
                let mut repair_requested = false;
                let mut delete_non_std_requested = false;
                let mut update_check_requested = false;
                let mut update_all_requested = false;
                if let Some(scan) = &self.scan {
                    let jobs = &self.repair_jobs_cache;
                    let missing_file_issues = scan
                        .problems
                        .iter()
                        .filter(|issue| issue.severity == RepairSeverity::MissingRequiredFile)
                        .count();
                    columns[1].group(|ui| {
                        ui.heading("Repair corrupted beatmaps");
                        ui.label(format!(
                            "{} missing-file issue(s), {} downloadable beatmapset(s)",
                            missing_file_issues,
                            jobs.len()
                        ));
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !self.is_repairing && !jobs.is_empty(),
                                    egui::Button::new("Repair"),
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
                            ui.label("These findings do not have a usable beatmapset ID. Rescan after this build; the app now infers IDs from osu! song folder names.");
                        }
                        if self.is_repairing {
                            ui.label(&self.repair_progress);
                        }
                        if self.repair_total > 0 {
                            let progress = self.repair_done as f32 / self.repair_total as f32;
                            ui.add(egui::ProgressBar::new(progress).text(format!(
                                "{}/{} complete | {} succeeded | {} failed",
                                self.repair_done,
                                self.repair_total,
                                self.repair_successes,
                                self.repair_failures
                            )));
                        }
                    });
                    columns[1].separator();
                }

                if self.scan.is_some() {
                    columns[1].group(|ui| {
                        ui.heading("Update outdated maps");
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !self.is_scanning
                                        && !self.is_repairing
                                        && !self.is_checking_updates
                                        && !self.is_updating_maps,
                                    egui::Button::new("Check updates"),
                                )
                                .clicked()
                            {
                                update_check_requested = true;
                            }
                            if ui
                                .add_enabled(
                                    !self.is_scanning
                                        && !self.is_repairing
                                        && !self.is_checking_updates
                                        && !self.is_updating_maps
                                        && !self.update_jobs.is_empty(),
                                    egui::Button::new("Update all"),
                                )
                                .clicked()
                            {
                                update_all_requested = true;
                            }
                            if self.is_checking_updates || self.is_updating_maps {
                                ui.add(egui::Spinner::new());
                            }
                        });
                        ui.label(format!(
                            "{} beatmapset(s) require update",
                            self.update_jobs.len()
                        ));
                        if self.is_checking_updates || self.is_updating_maps {
                            ui.label(&self.update_progress);
                        }
                        if self.update_total > 0
                            && (self.is_checking_updates || self.is_updating_maps)
                        {
                            let progress =
                                self.update_done as f32 / self.update_total.max(1) as f32;
                            ui.add(egui::ProgressBar::new(progress).text(format!(
                                "{}/{} complete | {} succeeded | {} failed",
                                self.update_done,
                                self.update_total,
                                self.update_successes,
                                self.update_failures
                            )));
                        }
                    });
                    columns[1].separator();
                }

                if let Some(scan) = &self.scan {
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
                    columns[1].group(|ui| {
                        ui.heading("Delete non-std maps");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.delete_taiko, "Taiko");
                            ui.checkbox(&mut self.delete_catch, "Catch");
                            ui.checkbox(&mut self.delete_mania, "Mania");
                        });
                        ui.label(format!("{delete_count} scanned .osu file(s) match selected mode(s)"));
                        if ui
                            .add_enabled(
                                !self.is_scanning && !self.is_repairing && delete_count > 0,
                                egui::Button::new("Delete selected non-std maps"),
                            )
                            .clicked()
                        {
                            delete_non_std_requested = true;
                        }
                    });
                    columns[1].separator();
                }

                if !self.update_jobs.is_empty() || !self.update_log.is_empty() {
                    columns[1].label("Update findings");
                    egui::ScrollArea::vertical()
                        .id_source("update_findings")
                        .max_height(180.0)
                        .show(&mut columns[1], |ui| {
                            ui.set_width(ui.available_width());
                            for job in &self.update_jobs {
                                ui.group(|ui| {
                                    ui.label(format!(
                                        "Set {}: {} outdated map(s)",
                                        job.beatmapset_id,
                                        job.reasons.len()
                                    ));
                                    for reason in &job.reasons {
                                        wrapped_label(ui, format!("  {reason}"));
                                    }
                                });
                            }
                            for entry in &self.update_log {
                                let status = match entry.status {
                                    RepairLogStatus::InProgress => "updating",
                                    RepairLogStatus::Success => "updated",
                                    RepairLogStatus::Failed => "failed",
                                };
                                wrapped_label(ui, format!(
                                    "{status}: set {} - {}",
                                    entry.beatmapset_id, entry.message
                                ));
                            }
                        });
                    columns[1].separator();
                }

                if let Some(scan) = &self.scan {
                    columns[1].label("Repair findings");
                    let jobs = &self.repair_jobs_cache;
                    if !jobs.is_empty() {
                        egui::ScrollArea::vertical()
                            .id_source("repairable_sets")
                            .max_height(220.0)
                            .show(&mut columns[1], |ui| {
                                ui.set_width(ui.available_width());
                                for job in jobs {
                                    ui.group(|ui| {
                                        ui.label(format!(
                                            "Set {}: {} corrupted map(s)",
                                            job.beatmapset_id,
                                            job.labels.len()
                                        ));
                                        for issue in &job.issues {
                                            wrapped_label(ui, format!("  {issue}"));
                                        }
                                    });
                                }
                            });
                    } else {
                        egui::ScrollArea::vertical()
                            .id_source("repair")
                            .max_height(180.0)
                            .show(&mut columns[1], |ui| {
                                ui.set_width(ui.available_width());
                                for issue in &scan.problems {
                                    let severity = match issue.severity {
                                        RepairSeverity::MissingRequiredFile => "missing",
                                        RepairSeverity::ParseWarning => "parse",
                                    };
                                    wrapped_label(ui, format!(
                                        "{severity}: {} ({})",
                                        issue.message,
                                        issue.beatmap.display()
                                ));
                            }
                            });
                    }
                }
                if repair_requested {
                    self.start_repair_all();
                }
                if delete_non_std_requested {
                    self.delete_selected_non_std_modes();
                }
                if update_check_requested {
                    self.start_update_check();
                }
                if update_all_requested {
                    self.start_update_all();
                }
            });
        });
    }
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
    ignore_after_success: Vec<IgnoredRepairIssue>,
}

#[derive(Debug, Clone)]
struct UpdateCandidate {
    beatmapset_id: i64,
    maps: Vec<LocalBeatmap>,
    folders: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct UpdateJob {
    beatmapset_id: i64,
    folders: Vec<PathBuf>,
    local_osu_paths: Vec<PathBuf>,
    reasons: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteBeatmapset {
    #[serde(default)]
    beatmaps: Vec<RemoteBeatmap>,
}

#[derive(Debug, Deserialize)]
struct RemoteBeatmap {
    id: i64,
    #[serde(default)]
    checksum: Option<String>,
    #[serde(default)]
    version: String,
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

fn scan_status_label(ui: &mut egui::Ui, prefix: &str, value: &str) {
    let text = format!("{prefix}: {value}");
    let width = ui.available_width().max(160.0);
    ui.add_sized([width, 18.0], egui::Label::new(text.clone()).truncate(true))
        .on_hover_text(text);
}

fn wrapped_label(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) {
    ui.add(egui::Label::new(text).wrap(true));
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

fn update_candidates(scan: &LibraryScan) -> Vec<UpdateCandidate> {
    let mut grouped = BTreeMap::<i64, (Vec<LocalBeatmap>, BTreeSet<PathBuf>)>::new();
    for map in &scan.maps {
        let Some(beatmapset_id) = map.beatmapset_id else {
            continue;
        };
        let entry = grouped.entry(beatmapset_id).or_default();
        entry.0.push(map.clone());
        entry.1.insert(map.folder.clone());
    }

    grouped
        .into_iter()
        .map(|(beatmapset_id, (maps, folders))| UpdateCandidate {
            beatmapset_id,
            maps,
            folders: folders.into_iter().collect(),
        })
        .collect()
}

fn check_update_jobs(
    candidates: Vec<UpdateCandidate>,
    backend_url: String,
    tx: mpsc::Sender<UpdateEvent>,
) {
    let total = candidates.len();
    let _ = tx.send(UpdateEvent::CheckStarted { total });
    let mut jobs = Vec::new();
    let mut failures = 0;
    let client = reqwest::blocking::Client::new();

    for (index, candidate) in candidates.iter().enumerate() {
        let current = index + 1;
        let _ = tx.send(UpdateEvent::Checking {
            beatmapset_id: candidate.beatmapset_id,
            index: current,
            total,
        });

        match fetch_remote_beatmapset(&client, candidate.beatmapset_id, &backend_url).and_then(
            |remote| match remote {
                Some(remote) => update_job_from_remote(candidate, &remote),
                None => Ok(None),
            },
        ) {
            Ok(Some(job)) => jobs.push(job),
            Ok(None) => {}
            Err(err) => {
                failures += 1;
                let _ = tx.send(UpdateEvent::CheckFailed {
                    beatmapset_id: candidate.beatmapset_id,
                    message: format!("{err:#}"),
                });
            }
        }

        thread::sleep(UPDATE_CHECK_DELAY);
    }

    let _ = tx.send(UpdateEvent::CheckFinished { jobs, failures });
}

fn fetch_remote_beatmapset(
    client: &reqwest::blocking::Client,
    beatmapset_id: i64,
    backend_url: &str,
) -> Result<Option<RemoteBeatmapset>> {
    if backend_url.trim().is_empty() {
        anyhow::bail!("backend URL is required for update checks");
    }

    let url = format!(
        "{}/beatmapsets/{}",
        backend_url.trim().trim_end_matches('/'),
        beatmapset_id
    );
    let response = client.get(&url).send()?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        anyhow::bail!("HTTP {status} while fetching update metadata");
    }
    let remote = response
        .json::<RemoteBeatmapset>()
        .with_context(|| format!("fetching update metadata for set {beatmapset_id}"))?;
    Ok(Some(remote))
}

fn update_job_from_remote(
    candidate: &UpdateCandidate,
    remote: &RemoteBeatmapset,
) -> Result<Option<UpdateJob>> {
    let remote_by_id = remote
        .beatmaps
        .iter()
        .map(|beatmap| (beatmap.id, beatmap))
        .collect::<BTreeMap<_, _>>();
    let remote_by_version = remote
        .beatmaps
        .iter()
        .filter_map(|beatmap| {
            let version = normalized_version(&beatmap.version);
            (!version.is_empty()).then_some((version, beatmap))
        })
        .collect::<BTreeMap<_, _>>();
    let local_ids = candidate
        .maps
        .iter()
        .filter_map(|map| map.beatmap_id)
        .collect::<BTreeSet<_>>();
    let local_versions = candidate
        .maps
        .iter()
        .map(|map| normalized_version(&map.version))
        .filter(|version| !version.is_empty())
        .collect::<BTreeSet<_>>();
    let mut reasons = Vec::new();
    let mut local_osu_paths = BTreeSet::new();

    for map in &candidate.maps {
        let remote_map = map
            .beatmap_id
            .and_then(|beatmap_id| remote_by_id.get(&beatmap_id).copied())
            .or_else(|| {
                remote_by_version
                    .get(&normalized_version(&map.version))
                    .copied()
            });

        let Some(remote_map) = remote_map else {
            if map.beatmap_id.is_some() {
                reasons.push(format!(
                    "{} is no longer present in the latest set",
                    map.label()
                ));
                local_osu_paths.insert(map.path.clone());
            }
            continue;
        };

        if let Some(remote_checksum) = remote_map.checksum.as_deref()
            && !remote_checksum.eq_ignore_ascii_case(&map.md5)
        {
            reasons.push(format!(
                "{} [{}] checksum changed",
                map.artist,
                if remote_map.version.is_empty() {
                    map.version.as_str()
                } else {
                    remote_map.version.as_str()
                }
            ));
            local_osu_paths.insert(map.path.clone());
        }
    }

    for remote_map in &remote.beatmaps {
        if local_ids.contains(&remote_map.id)
            || local_versions.contains(&normalized_version(&remote_map.version))
        {
            continue;
        }
        reasons.push(format!(
            "New difficulty available: [{}]",
            remote_map.version
        ));
    }

    if reasons.is_empty() {
        return Ok(None);
    }

    Ok(Some(UpdateJob {
        beatmapset_id: candidate.beatmapset_id,
        folders: candidate.folders.clone(),
        local_osu_paths: local_osu_paths.into_iter().collect(),
        reasons,
    }))
}

fn normalized_version(version: &str) -> String {
    version.trim().to_ascii_lowercase()
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
                    entry.3.push(IgnoredRepairIssue {
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
            |(beatmapset_id, (labels, folders, issues, ignore_after_success))| RepairJob {
                beatmapset_id,
                labels,
                folders: folders.into_iter().collect(),
                issues,
                ignore_after_success,
            },
        )
        .collect()
}

fn run_repair_jobs(jobs: Vec<RepairJob>, backend_url: String, tx: mpsc::Sender<RepairEvent>) {
    let total = jobs.len();
    let _ = tx.send(RepairEvent::Started { total });
    let client = reqwest::blocking::Client::new();

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
        if let Err(err) = repair_beatmapset(&client, job, &backend_url) {
            let _ = tx.send(RepairEvent::Failed {
                beatmapset_id: job.beatmapset_id,
                message: format!("{err:#}"),
            });
            continue;
        }

        let _ = tx.send(RepairEvent::Repaired {
            beatmapset_id: job.beatmapset_id,
            folder_count: job.folders.len(),
            ignored_after_success: job.ignore_after_success.clone(),
        });
    }

    let _ = tx.send(RepairEvent::Finished);
}

fn run_update_jobs(jobs: Vec<UpdateJob>, backend_url: String, tx: mpsc::Sender<UpdateEvent>) {
    let total = jobs.len();
    let _ = tx.send(UpdateEvent::UpdateStarted { total });
    let client = reqwest::blocking::Client::new();

    for (index, job) in jobs.iter().enumerate() {
        let current = index + 1;
        if index > 0 {
            thread::sleep(BEATMAPSET_DOWNLOAD_DELAY);
        }
        let _ = tx.send(UpdateEvent::Updating {
            beatmapset_id: job.beatmapset_id,
            index: current,
            total,
        });

        if let Err(err) = update_beatmapset(&client, job, &backend_url) {
            let _ = tx.send(UpdateEvent::UpdateFailed {
                beatmapset_id: job.beatmapset_id,
                message: format!("{err:#}"),
            });
            continue;
        }

        let _ = tx.send(UpdateEvent::Updated {
            beatmapset_id: job.beatmapset_id,
            folder_count: job.folders.len(),
        });
    }

    let _ = tx.send(UpdateEvent::UpdateFinished);
}

fn repair_beatmapset(
    client: &reqwest::blocking::Client,
    job: &RepairJob,
    backend_url: &str,
) -> Result<()> {
    if backend_url.trim().is_empty() {
        anyhow::bail!("backend URL is required for automatic repair downloads");
    }

    let url = format!(
        "{}/beatmapsets/{}/download",
        backend_url.trim().trim_end_matches('/'),
        job.beatmapset_id
    );
    let temp_path = std::env::temp_dir()
        .join("osu-map-manager-repairs")
        .join(format!("{}.osz", job.beatmapset_id));
    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent)?;
    }

    download_file(client, &url, &temp_path).with_context(|| format!("downloading {url}"))?;
    for folder in &job.folders {
        extract_osz_into_folder(&temp_path, folder)
            .with_context(|| format!("extracting into {}", folder.display()))?;
    }
    Ok(())
}

fn update_beatmapset(
    client: &reqwest::blocking::Client,
    job: &UpdateJob,
    backend_url: &str,
) -> Result<()> {
    if backend_url.trim().is_empty() {
        anyhow::bail!("backend URL is required for automatic update downloads");
    }

    let url = format!(
        "{}/beatmapsets/{}/download",
        backend_url.trim().trim_end_matches('/'),
        job.beatmapset_id
    );
    let temp_path = std::env::temp_dir()
        .join("osu-map-manager-updates")
        .join(format!("{}.osz", job.beatmapset_id));
    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent)?;
    }

    download_file(client, &url, &temp_path).with_context(|| format!("downloading {url}"))?;
    for path in &job.local_osu_paths {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("removing old map file {}", path.display()))?;
        }
    }
    for folder in &job.folders {
        extract_osz_into_folder(&temp_path, folder)
            .with_context(|| format!("extracting into {}", folder.display()))?;
    }
    Ok(())
}

fn download_file(client: &reqwest::blocking::Client, url: &str, destination: &Path) -> Result<()> {
    let mut response = client.get(url).send()?.error_for_status()?;
    let mut file = fs::File::create(destination)?;
    io::copy(&mut response, &mut file)?;
    Ok(())
}

fn extract_osz_into_folder(osz_path: &Path, folder: &Path) -> Result<()> {
    let file = fs::File::open(osz_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    fs::create_dir_all(folder)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let Some(enclosed_name) = entry.enclosed_name() else {
            continue;
        };
        let output_path = folder.join(enclosed_name);
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::File::create(output_path)?;
        io::copy(&mut entry, &mut output)?;
    }
    Ok(())
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
    let root = if osu_root.trim().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(osu_root.trim())
    };
    root.join(".osu-map-manager").join("repair_ignores.json")
}

fn default_osu_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("osu!"))
        .filter(|path| path.exists())
}
