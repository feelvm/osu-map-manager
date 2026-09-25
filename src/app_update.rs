//! Manual app self-update from GitHub Releases.
//!
//! Flow (all user-triggered, no background checks):
//! 1. "Check for app update" asks the GitHub API for the latest release.
//! 2. If a newer tag is found, "Download & restart" fetches the release
//!    zip, extracts the fresh `osu-map-manager.exe` and swaps it over the
//!    running executable via `self-replace`.
//! 3. The app respawns itself and exits, so the new version takes over.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub const GITHUB_OWNER: &str = "feelvm";
pub const GITHUB_REPO: &str = "osu-map-manager";
/// Asset published by `.github/workflows/release.yml`.
pub const RELEASE_ASSET_ZIP: &str = "osu-map-manager-windows-x86_64.zip";
pub const RELEASE_EXE_NAME: &str = "osu-map-manager.exe";

/// Upper bound for the downloaded release zip (generous, but a broken
/// redirect must not fill the disk).
pub const MAX_RELEASE_DOWNLOAD_BYTES: u64 = 500_000_000; // ~500 MiB

#[derive(Debug, Clone)]
pub struct AppRelease {
    pub tag: String,
    pub notes: String,
    pub asset_url: String,
    pub asset_name: String,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

pub fn current_version_text() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// `true` when `latest_tag` (e.g. `v0.7.0`) is newer than `current`
/// (e.g. `v0.1.0` / `0.1.0`). Compares numeric `major.minor.patch`
/// parts; unknown suffixes fall back to plain inequality so a new
/// differently-shaped tag still surfaces.
pub fn is_newer_version(latest_tag: &str, current: &str) -> bool {
    let latest_parts = parse_version_parts(latest_tag);
    let current_parts = parse_version_parts(current);
    if let (Some(latest_parts), Some(current_parts)) = (latest_parts, current_parts) {
        return latest_parts > current_parts;
    }
    normalize_tag(latest_tag) != normalize_tag(current)
}

fn normalize_tag(tag: &str) -> String {
    let tag = tag.trim();
    tag.strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
        .to_owned()
}

fn parse_version_parts(tag: &str) -> Option<Vec<u64>> {
    let normalized = normalize_tag(tag);
    let core = normalized.split(['-', '+']).next().unwrap_or(&normalized);
    if core.is_empty() {
        return None;
    }
    core.split('.')
        .map(|part| part.trim().parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()
}

/// Asks the GitHub API for the latest release and picks the Windows asset.
pub fn fetch_latest_release() -> Result<Option<AppRelease>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("osu-map-manager-updater")
        .build()
        .context("building the update client")?;
    let url = format!("https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest");
    let response = client
        .get(&url)
        .send()
        .context("checking the latest release")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // No releases published yet.
        return Ok(None);
    }
    let release: GithubRelease = response
        .error_for_status()
        .context("checking the latest release")?
        .json()
        .context("decoding the latest release")?;
    Ok(select_release_asset(&release))
}

fn select_release_asset(release: &GithubRelease) -> Option<AppRelease> {
    if release.tag_name.trim().is_empty() {
        return None;
    }
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == RELEASE_ASSET_ZIP)
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|asset| asset.name.ends_with(".zip"))
        })
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|asset| asset.name.ends_with(".exe"))
        })?;
    if asset.browser_download_url.trim().is_empty() {
        return None;
    }
    Some(AppRelease {
        tag: release.tag_name.clone(),
        notes: release.body.clone().unwrap_or_default(),
        asset_url: asset.browser_download_url.clone(),
        asset_name: asset.name.clone(),
    })
}

/// Downloads the release asset while reporting `(downloaded, total)` bytes.
/// Returns the downloaded file in the temp dir.
pub fn download_release_asset(
    release: &AppRelease,
    progress: &dyn Fn(u64, Option<u64>),
) -> Result<PathBuf> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .user_agent("osu-map-manager-updater")
        .build()
        .context("building the download client")?;
    let mut response = client
        .get(&release.asset_url)
        .send()
        .context("downloading the update")?
        .error_for_status()
        .context("downloading the update")?;
    let total = response.content_length();
    let destination =
        std::env::temp_dir().join(format!("osu-map-manager-update-{}", release.asset_name));
    let mut file = fs::File::create(&destination).context("staging the update download")?;
    let mut downloaded: u64 = 0;
    progress(0, total);
    loop {
        use io::Read;
        let mut chunk = [0_u8; 65536];
        let read = response
            .read(&mut chunk)
            .context("downloading the update")?;
        if read == 0 {
            break;
        }
        downloaded += read as u64;
        if downloaded > MAX_RELEASE_DOWNLOAD_BYTES {
            drop(file);
            let _ = fs::remove_file(&destination);
            anyhow::bail!("update download is unexpectedly large; refusing to continue");
        }
        use io::Write;
        file.write_all(&chunk[..read])
            .context("staging the update download")?;
        progress(downloaded, total);
    }
    file.flush().ok();
    Ok(destination)
}

/// Extracts the fresh exe from the downloaded `.zip` (or uses the file
/// directly when the asset itself is an `.exe`).
pub fn extract_fresh_exe(staged: &Path, asset_name: &str) -> Result<PathBuf> {
    if asset_name.ends_with(".exe") {
        return Ok(staged.to_owned());
    }
    let file = fs::File::open(staged).context("reading the update archive")?;
    let mut archive = zip::ZipArchive::new(file).context("reading the update archive")?;
    let out_dir = staged.with_extension("extracted");
    let _ = fs::remove_dir_all(&out_dir);
    fs::create_dir_all(&out_dir).context("staging the update files")?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("reading the update archive")?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let target = out_dir.join(name);
        if entry.is_dir() {
            fs::create_dir_all(&target).ok();
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).ok();
        }
        let mut out = fs::File::create(&target).context("staging the update files")?;
        io::copy(&mut entry, &mut out).context("staging the update files")?;
    }
    find_exe(&out_dir).with_context(|| {
        format!(
            "update archive did not contain {RELEASE_EXE_NAME} (asset {})",
            asset_name
        )
    })
}

fn find_exe(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join(RELEASE_EXE_NAME);
    if direct.is_file() {
        return Some(direct);
    }
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_exe(&path) {
                return Some(found);
            }
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(RELEASE_EXE_NAME))
        {
            return Some(path);
        }
    }
    None
}

/// Swaps the running executable with the fresh one, then respawns the app
/// with the same arguments and exits the old process.
pub fn install_and_restart(fresh_exe: &Path) -> Result<()> {
    self_replace::self_replace(fresh_exe).context("replacing the app executable")?;
    let current = std::env::current_exe().context("locating the app executable for restart")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut command = std::process::Command::new(current);
    command.args(args);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0010);
    }
    command.spawn().context("restarting the updated app")?;
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_counts_as_update() {
        assert!(is_newer_version("v0.2.0", "v0.1.0"));
        assert!(is_newer_version("v0.1.1", "0.1.0"));
        assert!(!is_newer_version("v0.1.0", "v0.1.0"));
        assert!(!is_newer_version("v0.1.0", "v0.2.0"));
    }

    #[test]
    fn prefers_the_expected_zip_asset() {
        let release = GithubRelease {
            tag_name: "v0.7.0".to_owned(),
            body: Some("notes".to_owned()),
            assets: vec![
                GithubAsset {
                    name: "checksums.txt".to_owned(),
                    browser_download_url: "https://example.com/c".to_owned(),
                },
                GithubAsset {
                    name: RELEASE_ASSET_ZIP.to_owned(),
                    browser_download_url: "https://example.com/z".to_owned(),
                },
            ],
        };
        let picked = select_release_asset(&release).expect("asset");
        assert_eq!(picked.asset_url, "https://example.com/z");
        assert_eq!(picked.tag, "v0.7.0");
    }

    #[test]
    fn falls_back_to_any_zip_or_exe() {
        let release = GithubRelease {
            tag_name: "v0.7.0".to_owned(),
            body: None,
            assets: vec![GithubAsset {
                name: "app.exe".to_owned(),
                browser_download_url: "https://example.com/e".to_owned(),
            }],
        };
        let picked = select_release_asset(&release).expect("asset");
        assert_eq!(picked.asset_name, "app.exe");
    }

    #[test]
    fn no_releases_or_assets_yields_none() {
        let release = GithubRelease {
            tag_name: String::new(),
            body: None,
            assets: vec![],
        };
        assert!(select_release_asset(&release).is_none());
    }
}
