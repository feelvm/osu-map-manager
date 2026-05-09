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
    fs,
    io,
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
    status: String,
    scan_rx: Option<Receiver<ScanEvent>>,
    scan_cancel: Option<Arc<AtomicBool>>,
    repair_rx: Option<Receiver<RepairEvent>>,
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

        Self {
            query: BeatmapQuery {
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
            status: "Ready".to_owned(),
            scan_rx: None,
            scan_cancel: None,
            repair_rx: None,
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

    }

    fn start_scan(&mut self) {
        if self.is_scanning {
            self.status = "A scan is already running".to_owned();
            return;
        }

        let songs_dir = expand_prefilled_path(&self.songs_dir);
        let osu_root =
            (!self.osu_root.trim().is_empty()).then(|| expand_prefilled_path(&self.osu_root));
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
            expand_prefilled_path(&self.osu_root).join("collection.db")
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

    fn restore_collection_backup(&mut self) {
        let path = if self.osu_root.trim().is_empty() {
            PathBuf::from("collection.db")
        } else {
            expand_prefilled_path(&self.osu_root).join("collection.db")
        };

        match collection::restore_collection_backup(&path) {
            Ok(()) => {
                self.status = format!(
                    "Restored {} from {}",
                    path.display(),
                    path.with_extension("db.bak").display()
                )
            }
            Err(err) => self.status = format!("Collection restore failed: {err:#}"),
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
        if self.is_scanning || self.is_repairing {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        egui::TopBottomPanel::top("top")
            .frame(panel_frame(ctx.style().as_ref()))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("osu! Map Manager");
                    ui.separator();
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

        egui::SidePanel::left("filters")
            .resizable(false)
            .exact_width(400.0)
            .frame(panel_frame(ctx.style().as_ref()))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
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

        self.refresh_filtered_maps();
        self.refresh_repair_jobs();

        let mut repair_requested = false;
        let mut delete_non_std_requested = false;

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ctx.style().as_ref()))
            .show(ctx, |ui| {
                let rect = ui.available_rect_before_wrap();
                let gap = 8.0;
                let library_width = 430.0_f32.min(rect.width());
                let library_content_width = (library_width - 48.0).max(1.0);
                let actions_left = rect.left() + library_content_width + gap;
                let actions_width = (rect.right() - actions_left).clamp(0.0, 330.0);
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
                library_ui.set_clip_rect(library_rect);
                library_ui.set_width(library_width);
                library_ui.set_max_width(library_width);
                let middle_width = library_ui.available_width();
                egui::ScrollArea::vertical()
                    .id_source("library_pane")
                    .auto_shrink([false, false])
                    .show(&mut library_ui, |ui| {
                        let content_width = library_content_width.min(middle_width).max(1.0);
                        let card_item_spacing = ui.spacing().item_spacing;
                        let card_gap = gap;
                        ui.spacing_mut().item_spacing.y = 0.0;
                        fix_ui_width(ui, content_width);
                        section_frame(ctx.style().as_ref()).show(ui, |ui| {
                            ui.spacing_mut().item_spacing = card_item_spacing;
                            fill_tile_width(ui);
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
                        });

                        if self.is_scanning {
                            ui.add_space(card_gap);
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
                            ui.add_space(card_gap);
                            fix_ui_width(ui, content_width);
                            let results_card_height = ui.available_height().max(0.0);
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.set_min_height((results_card_height - 24.0).max(0.0));
                                ui.horizontal_wrapped(|ui| {
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
                                ui.label(format!("{} selected map(s)", self.selected_maps.len()));
                                ui.horizontal(|ui| {
                                    ui.label("Collection");
                                    let input_width = (ui.available_width() - 8.0).max(80.0);
                                    ui.add_sized(
                                        [input_width, 28.0],
                                        egui::TextEdit::singleline(&mut self.collection_name)
                                            .vertical_align(egui::Align::Center),
                                    );
                                });
                                ui.horizontal_wrapped(|ui| {
                                    if ui.button("Write collection").clicked() {
                                        self.export_collection();
                                    }
                                    if ui.button("Write TSV manifest").clicked() {
                                        self.export_manifest();
                                    }
                                    ui.add_enabled_ui(true, |ui| {
                                        if ui.button("Restore backup").clicked() {
                                            self.restore_collection_backup();
                                        }
                                    })
                                    .response
                                    .on_hover_text("Restore collection.db from collection.db.bak");
                                });
                                ui.add_space(8.0);
                                wrapped_label(
                                    ui,
                                    format!(
                                        "{} matching maps from {} scanned maps, {} sets, {} repair issue(s)",
                                        self.filtered_map_indexes.len(),
                                        scanned_maps,
                                        scanned_sets,
                                        repair_issues
                                    ),
                                );
                                fix_ui_width(ui, ui.available_width());
                                let list_height = ui.available_height().max(120.0);
                                egui::ScrollArea::vertical()
                                    .id_source("local_maps")
                                    .max_height(list_height)
                                    .auto_shrink([false, false])
                                    .show_rows(
                                        ui,
                                        30.0,
                                        self.filtered_map_indexes.len(),
                                        |ui, row_range| {
                                            let row_width = ui.available_width().max(1.0);
                                            fix_ui_width(ui, row_width);
                                            let row_height = 30.0;
                                            let visible_rows = row_range.len() as f32;
                                            let (list_rect, _) = ui.allocate_exact_size(
                                                egui::vec2(row_width, visible_rows * row_height),
                                                egui::Sense::hover(),
                                            );
                                            for (visible_index, row) in row_range.enumerate() {
                                                let Some(map) =
                                                    self.scan.as_ref().and_then(|scan| {
                                                        self.filtered_map_indexes
                                                            .get(row)
                                                            .and_then(|&index| scan.maps.get(index))
                                                            .cloned()
                                                    })
                                                else {
                                                    continue;
                                                };
                                                let mut selected =
                                                    self.selected_md5s.contains(&map.md5);
                                                let label = self.map_result_label(&map);
                                                let row_rect = egui::Rect::from_min_size(
                                                    egui::pos2(
                                                        list_rect.left(),
                                                        list_rect.top()
                                                            + visible_index as f32 * row_height,
                                                    ),
                                                    egui::vec2(row_width, row_height),
                                                );
                                                let checkbox_rect = egui::Rect::from_min_size(
                                                    egui::pos2(
                                                        row_rect.left(),
                                                        row_rect.center().y - 9.0,
                                                    ),
                                                    egui::vec2(18.0, 18.0),
                                                );
                                                let response = ui
                                                    .put(
                                                        checkbox_rect,
                                                        egui::Checkbox::new(&mut selected, ""),
                                                    )
                                                    .changed();
                                                let label_rect = egui::Rect::from_min_max(
                                                    egui::pos2(row_rect.left() + 28.0, row_rect.top()),
                                                    row_rect.right_bottom(),
                                                );
                                                ui.painter().with_clip_rect(label_rect).text(
                                                    label_rect.left_center(),
                                                    egui::Align2::LEFT_CENTER,
                                                    label.clone(),
                                                    egui::TextStyle::Body.resolve(ui.style()),
                                                    ui.visuals().text_color(),
                                                );
                                                ui.interact(
                                                    label_rect,
                                                    ui.id().with(("map_row_label", row)),
                                                    egui::Sense::hover(),
                                                )
                                                .on_hover_text(label);
                                                if response {
                                                    if selected {
                                                        self.select_map(&map);
                                                    } else {
                                                        self.deselect_md5(&map.md5);
                                                    }
                                                }
                                            }
                                        },
                                    );
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
                            let actions_content_width = (actions_width - 12.0).max(1.0);
                            let card_item_spacing = ui.spacing().item_spacing;
                            let card_gap = gap;
                            ui.spacing_mut().item_spacing.y = 0.0;
                            fix_ui_width(ui, actions_content_width);
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.heading("Results and actions");
                            });

                        if let Some(scan) = &self.scan {
                            let jobs = &self.repair_jobs_cache;
                            let missing_file_issues = scan
                                .problems
                                .iter()
                                .filter(|issue| {
                                    issue.severity == RepairSeverity::MissingRequiredFile
                                })
                                .count();
                            ui.add_space(card_gap);
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
                            });
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
                            ui.add_space(card_gap);
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.heading("Delete non-std maps");
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
                                        !self.is_scanning && !self.is_repairing && delete_count > 0,
                                        egui::Button::new("Delete selected non-std maps"),
                                    )
                                    .clicked()
                                {
                                    delete_non_std_requested = true;
                                }
                            });
                        }

                        if let Some(scan) = &self.scan {
                            ui.add_space(card_gap);
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.heading("Repair findings");
                                let jobs = &self.repair_jobs_cache;
                                if !jobs.is_empty() || !self.repair_log.is_empty() {
                                    egui::ScrollArea::vertical()
                                        .id_source("repairable_sets")
                                        .max_height(220.0)
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
                                                nested_frame(ui.style()).show(ui, |ui| {
                                                    fill_tile_width(ui);
                                                    wrapped_label(
                                                        ui,
                                                        format!(
                                                            "Set {}: {} corrupted map(s)",
                                                            job.beatmapset_id,
                                                            job.labels.len()
                                                        ),
                                                    );
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
                                        .show(ui, |ui| {
                                            for issue in &scan.problems {
                                                let severity = match issue.severity {
                                                    RepairSeverity::MissingRequiredFile => "missing",
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
                        }
                    });
                }
            });

        if repair_requested {
            self.start_repair_all();
        }
        if delete_non_std_requested {
            self.delete_selected_non_std_modes();
        }
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
    app_data_path(osu_root).join("repair_ignores.json")
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
