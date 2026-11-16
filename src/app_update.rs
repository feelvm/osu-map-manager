//! App self-update from GitHub Releases.
//!
//! Flow (one passive startup check + user-triggered download):
//! 1. At startup a silent check asks the GitHub API for the latest release;
//!    a newer tag only highlights the "↻ Update" button, errors are ignored.
//! 2. "Check for app update" repeats that check on demand and shows the
//!    result inside the update window.
//! 3. If a newer tag is found, "Download & restart" fetches the release
//!    zip, verifies it against the release's published `checksums.txt`,
//!    extracts the fresh `osu-map-manager.exe` and swaps it over the
//!    running executable via `self-replace`.
//! 4. The app respawns itself and exits, so the new version takes over.

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

pub const GITHUB_OWNER: &str = "feelvm";
pub const GITHUB_REPO: &str = "osu-map-manager";
/// Asset published by `.github/workflows/release.yml`.
pub const RELEASE_ASSET_ZIP: &str = "osu-map-manager-windows-x86_64.zip";
pub const RELEASE_EXE_NAME: &str = "osu-map-manager.exe";
/// SHA-256 list published by `.github/workflows/release.yml`; the updater
/// refuses to install from a release that does not carry it.
pub const RELEASE_CHECKSUMS_NAME: &str = "checksums.txt";

/// Upper bound for the downloaded release zip (generous, but a broken
/// redirect must not fill the disk).
pub const MAX_RELEASE_DOWNLOAD_BYTES: u64 = 500_000_000; // ~500 MiB

/// Upper bound for what a downloaded archive may expand to on disk.
pub const MAX_RELEASE_EXTRACTED_BYTES: u64 = 500_000_000; // ~500 MiB

#[derive(Debug, Clone)]
pub struct AppRelease {
    pub tag: String,
    pub notes: String,
    pub asset_url: String,
    pub asset_name: String,
    /// URL of the release's `checksums.txt`; empty when the release does
    /// not publish one (the download then refuses to install).
    pub checksums_url: String,
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

/// The release workflow stamps `Cargo.toml`'s version from the release tag
/// before building, so the displayed version always matches the tag.
pub fn current_version_text() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// `true` when `latest_tag` (e.g. `v0.7.0`) is strictly newer than `current`
/// (e.g. `v0.1.0` / `0.1.0`). Compares numeric `major.minor.patch` parts;
/// tags that do not parse as versions are never treated as updates, so a
/// weirdly-shaped release tag cannot trigger a downgrade.
pub fn is_newer_version(latest_tag: &str, current: &str) -> bool {
    let Some(latest_parts) = parse_version_parts(latest_tag) else {
        return false;
    };
    let Some(current_parts) = parse_version_parts(current) else {
        return false;
    };
    latest_parts > current_parts
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

/// Only accepts GitHub-hosted asset URLs (`github.com` serves release
/// downloads and redirects to GitHub's CDN), so a crafted release cannot
/// point the updater at an arbitrary host.
fn is_github_asset_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://github.com/") || url.starts_with("https://api.github.com/")
}

fn select_release_asset(release: &GithubRelease) -> Option<AppRelease> {
    if release.tag_name.trim().is_empty() {
        return None;
    }
    let usable = |asset: &GithubAsset| {
        !asset.browser_download_url.trim().is_empty()
            && is_github_asset_url(&asset.browser_download_url)
    };
    let asset = release
        .assets
        .iter()
        .find(|asset| usable(asset) && asset.name == RELEASE_ASSET_ZIP)
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|asset| usable(asset) && asset.name.ends_with(".zip"))
        })
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|asset| usable(asset) && asset.name.ends_with(".exe"))
        })?;
    let checksums = release
        .assets
        .iter()
        .find(|asset| usable(asset) && asset.name == RELEASE_CHECKSUMS_NAME);
    Some(AppRelease {
        tag: release.tag_name.clone(),
        notes: release.body.clone().unwrap_or_default(),
        asset_url: asset.browser_download_url.clone(),
        asset_name: asset.name.clone(),
        checksums_url: checksums
            .map(|asset| asset.browser_download_url.clone())
            .unwrap_or_default(),
    })
}

/// Downloads the release asset while reporting `(downloaded, total)` bytes,
/// verifies it against the release's published `checksums.txt` (refusing to
/// install from a release without one), and returns the staged file in the
/// temp dir.
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
    let destination = std::env::temp_dir().join(format!(
        "osu-map-manager-update-{}-{}",
        random_temp_suffix(),
        asset_file_name(&release.asset_name)
    ));
    let mut file = fs::File::create(&destination).context("staging the update download")?;
    let mut downloaded: u64 = 0;
    progress(0, total);
    loop {
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
        file.write_all(&chunk[..read])
            .context("staging the update download")?;
        progress(downloaded, total);
    }
    file.flush().context("staging the update download")?;
    file.sync_all().context("staging the update download")?;
    verify_staged_download(&client, &destination, release)?;
    Ok(destination)
}

/// Verifies the staged download's SHA-256 against the checksums published
/// with the release. Fails closed: an update without `checksums.txt` is
/// refused, so a compromised release cannot swap an unverified binary in.
fn verify_staged_download(
    client: &reqwest::blocking::Client,
    staged: &Path,
    release: &AppRelease,
) -> Result<()> {
    if release.checksums_url.trim().is_empty() {
        anyhow::bail!(
            "release {} does not publish {RELEASE_CHECKSUMS_NAME}; refusing to install an unverified update",
            release.tag
        );
    }
    let text = client
        .get(release.checksums_url.trim())
        .send()
        .context("downloading the update checksums")?
        .error_for_status()
        .context("downloading the update checksums")?
        .text()
        .context("downloading the update checksums")?;
    checksum_matches(staged, &text, &release.asset_name)
}

/// Checks the staged file's SHA-256 against the `sha256sum`-style line for
/// `asset_name` in `checksums_text`.
fn checksum_matches(staged: &Path, checksums_text: &str, asset_name: &str) -> Result<()> {
    let expected = find_checksum(checksums_text, asset_name)
        .with_context(|| format!("{RELEASE_CHECKSUMS_NAME} has no entry for {asset_name}"))?;
    let actual = sha256_hex(staged).context("hashing the downloaded update")?;
    if !actual.eq_ignore_ascii_case(&expected) {
        anyhow::bail!(
            "the downloaded update failed its checksum check (expected {expected}, got {actual}); refusing to install it"
        );
    }
    Ok(())
}

fn find_checksum(checksums_text: &str, asset_name: &str) -> Option<String> {
    for line in checksums_text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let hash = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        if name.eq_ignore_ascii_case(asset_name) {
            return Some(hash.to_owned());
        }
    }
    None
}

fn sha256_hex(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).context("hashing the downloaded update")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Random per-attempt suffix so concurrent update attempts never share a
/// staged download.
fn random_temp_suffix() -> String {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        bytes = nanos.to_le_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Keeps the staged name a plain file name even if a release asset name
/// ever contains a separator.
fn asset_file_name(asset_name: &str) -> String {
    asset_name.replace(['/', '\\'], "_")
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
    let mut extracted: u64 = 0;
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
        // Cap what this entry may contribute, including one probe byte
        // past the limit, so a decompression bomb cannot stream to disk
        // unchecked before the budget fires.
        let mut limited = (&mut entry).take(MAX_RELEASE_EXTRACTED_BYTES - extracted + 1);
        let written = io::copy(&mut limited, &mut out).context("staging the update files")?;
        drop(out);
        extracted += written;
        if extracted > MAX_RELEASE_EXTRACTED_BYTES {
            let _ = fs::remove_dir_all(&out_dir);
            anyhow::bail!("update archive expands beyond the allowed size; refusing to continue");
        }
    }
    // The release zip always carries the exe at its root under a fixed
    // name — no need to search for it.
    let direct = out_dir.join(RELEASE_EXE_NAME);
    if direct.is_file() {
        Ok(direct)
    } else {
        anyhow::bail!(
            "update archive did not contain {RELEASE_EXE_NAME} (asset {})",
            asset_name
        )
    }
}

/// Swaps the running executable with the fresh one, then respawns the app
/// with the same arguments and exits the old process.
pub fn install_and_restart(fresh_exe: &Path) -> Result<()> {
    self_replace::self_replace(fresh_exe).context("replacing the app executable")?;
    // The staged copy is only needed until the swap; drop our extraction
    // dir so update staging does not accumulate in %TEMP%.
    if let Some(parent) = fresh_exe.parent() {
        let name = parent
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
        if name
            .as_deref()
            .is_some_and(|name| name.ends_with(".extracted"))
        {
            let _ = fs::remove_dir_all(parent);
        }
    }
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
    fn unparseable_tags_are_not_updates() {
        assert!(!is_newer_version("test-build", "v0.8.4"));
        assert!(!is_newer_version("v0.9a", "v0.8.4"));
        assert!(!is_newer_version("", "v0.8.4"));
        assert!(!is_newer_version("v0.9.0", ""));
    }

    #[test]
    fn prefers_the_expected_zip_asset() {
        let release = GithubRelease {
            tag_name: "v0.7.0".to_owned(),
            body: Some("notes".to_owned()),
            assets: vec![
                GithubAsset {
                    name: RELEASE_CHECKSUMS_NAME.to_owned(),
                    browser_download_url: "https://github.com/feelvm/osu-map-manager/releases/download/v0.7.0/checksums.txt".to_owned(),
                },
                GithubAsset {
                    name: RELEASE_ASSET_ZIP.to_owned(),
                    browser_download_url: "https://github.com/feelvm/osu-map-manager/releases/download/v0.7.0/zip".to_owned(),
                },
            ],
        };
        let picked = select_release_asset(&release).expect("asset");
        assert_eq!(
            picked.asset_url,
            "https://github.com/feelvm/osu-map-manager/releases/download/v0.7.0/zip"
        );
        assert_eq!(
            picked.checksums_url,
            "https://github.com/feelvm/osu-map-manager/releases/download/v0.7.0/checksums.txt"
        );
        assert_eq!(picked.tag, "v0.7.0");
    }

    #[test]
    fn falls_back_to_any_zip_or_exe() {
        let release = GithubRelease {
            tag_name: "v0.7.0".to_owned(),
            body: None,
            assets: vec![GithubAsset {
                name: "app.exe".to_owned(),
                browser_download_url:
                    "https://github.com/feelvm/osu-map-manager/releases/download/v0.7.0/app.exe"
                        .to_owned(),
            }],
        };
        let picked = select_release_asset(&release).expect("asset");
        assert_eq!(picked.asset_name, "app.exe");
        assert!(picked.checksums_url.is_empty());
    }

    #[test]
    fn rejects_non_github_asset_urls() {
        let release = GithubRelease {
            tag_name: "v0.7.0".to_owned(),
            body: None,
            assets: vec![
                GithubAsset {
                    name: RELEASE_ASSET_ZIP.to_owned(),
                    browser_download_url: "https://evil.example/osu-map-manager-windows-x86_64.zip"
                        .to_owned(),
                },
                GithubAsset {
                    name: RELEASE_ASSET_ZIP.to_owned(),
                    browser_download_url:
                        "http://github.com/feelvm/osu-map-manager/releases/download/v0.7.0/zip"
                            .to_owned(),
                },
            ],
        };
        assert!(select_release_asset(&release).is_none());
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

    #[test]
    fn checksum_accepts_a_matching_file() {
        let dir = std::env::temp_dir().join(format!("omm-checksum-ok-{}", random_temp_suffix()));
        fs::create_dir_all(&dir).expect("temp dir");
        let staged = dir.join("app.zip");
        fs::write(&staged, b"fresh exe bytes").expect("staged file");
        let hash = sha256_hex(&staged).expect("hash");
        let text = format!("{hash}  osu-map-manager-windows-x86_64.zip\n");
        checksum_matches(&staged, &text, "osu-map-manager-windows-x86_64.zip")
            .expect("matching checksum");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn checksum_rejects_a_mismatching_file() {
        let dir = std::env::temp_dir().join(format!("omm-checksum-bad-{}", random_temp_suffix()));
        fs::create_dir_all(&dir).expect("temp dir");
        let staged = dir.join("app.zip");
        fs::write(&staged, b"tampered bytes").expect("staged file");
        let text = "0000000000000000000000000000000000000000000000000000000000000000  app.zip\n"
            .to_owned();
        assert!(checksum_matches(&staged, &text, "app.zip").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn checksum_requires_an_entry_for_the_asset() {
        let dir = std::env::temp_dir().join(format!("omm-checksum-miss-{}", random_temp_suffix()));
        fs::create_dir_all(&dir).expect("temp dir");
        let staged = dir.join("app.zip");
        fs::write(&staged, b"anything").expect("staged file");
        let text = "abcdef  other.zip\n".to_owned();
        assert!(checksum_matches(&staged, &text, "app.zip").is_err());
        assert!(checksum_matches(&staged, "", "app.zip").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_checksum_parses_sha256sum_lines() {
        let text = "# comment\n\
                    e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  app.zip\n\
                    ABCDEF  *other.zip\n";
        assert_eq!(
            find_checksum(text, "app.zip").as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(find_checksum(text, "other.zip").as_deref(), Some("ABCDEF"));
        assert_eq!(find_checksum(text, "missing.zip"), None);
    }

    #[test]
    fn asset_names_stay_plain_file_names() {
        assert_eq!(asset_file_name("a/b\\c.zip"), "a_b_c.zip");
    }
}
