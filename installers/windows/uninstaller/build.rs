fn main() {
    println!("cargo:rerun-if-changed=uninstaller.rc");
    println!("cargo:rerun-if-changed=../installer.manifest");
    println!("cargo:rerun-if-changed=../../../app.ico");
    if let Err(err) =
        embed_resource::compile("uninstaller.rc", embed_resource::NONE).manifest_optional()
    {
        panic!("embedding the Windows manifest failed: {err:?}");
    }
}
