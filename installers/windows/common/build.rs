use std::{env, fs, path::Path};

const VERSION_ENV: &str = "OSU_MAP_MANAGER_VERSION";

fn main() {
    println!("cargo:rerun-if-env-changed={VERSION_ENV}");

    // The release workflow stamps the app's root manifest before building, so
    // reading it from here keeps the setup, the uninstall entry and the app on
    // one version without a second place to bump.
    let root_manifest =
        Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../../Cargo.toml");
    println!("cargo:rerun-if-changed={}", root_manifest.display());

    let version = env::var(VERSION_ENV)
        .ok()
        .filter(|version| !version.is_empty())
        .or_else(|| {
            fs::read_to_string(&root_manifest)
                .ok()
                .and_then(|text| package_version(&text))
        })
        .unwrap_or_else(|| "0.0.0".to_owned());

    println!("cargo:rustc-env={VERSION_ENV}={version}");
}

/// Pulls the first `version = "..."` out of the `[package]` table.
fn package_version(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = line.strip_prefix("version") else {
            continue;
        };
        let rest = rest.trim_start().strip_prefix('=')?.trim();
        let rest = rest.strip_prefix('"')?.strip_suffix('"')?;
        return Some(rest.to_owned());
    }
    None
}
