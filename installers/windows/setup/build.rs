use anyhow::{Context, Result};
use flate2::{Compression, write::DeflateEncoder};
use std::{env, fs, io::Write, path::PathBuf};

fn main() -> Result<()> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let out = PathBuf::from(env::var("OUT_DIR")?);

    let payload = env::var_os("OSU_MAP_MANAGER_PAYLOAD")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../../target/release/osu-map-manager.exe"));
    println!("cargo:rerun-if-env-changed=OSU_MAP_MANAGER_PAYLOAD");
    println!("cargo:rerun-if-changed={}", payload.display());

    let bytes = fs::read(&payload).with_context(|| {
        format!(
            "reading the app payload from {}; build it with `cargo build --release` first, \
             or point OSU_MAP_MANAGER_PAYLOAD at the exe",
            payload.display()
        )
    })?;
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&bytes)?;
    fs::write(out.join("payload.deflate"), encoder.finish()?)?;

    let uninstaller = env::var_os("OSU_MAP_MANAGER_UNINSTALLER")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../target/release/osu-map-manager-uninstaller.exe"));
    println!("cargo:rerun-if-env-changed=OSU_MAP_MANAGER_UNINSTALLER");
    println!("cargo:rerun-if-changed={}", uninstaller.display());

    fs::copy(&uninstaller, out.join("uninstaller.exe")).with_context(|| {
        format!(
            "copying the uninstaller from {}; build it with \
             `cargo build --release -p osu-map-manager-uninstaller` first",
            uninstaller.display()
        )
    })?;

    println!("cargo:rerun-if-changed=setup.rc");
    println!("cargo:rerun-if-changed=../installer.manifest");
    println!("cargo:rerun-if-changed=../../../app.ico");
    if let Err(err) = embed_resource::compile("setup.rc", embed_resource::NONE).manifest_optional()
    {
        panic!("embedding the Windows manifest failed: {err:?}");
    }

    Ok(())
}
