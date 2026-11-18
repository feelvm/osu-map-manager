//! Embeds `app.ico` in the executable so Explorer, the taskbar and shortcuts
//! show the app's logo before it ever starts. The running app loads the same
//! file for its window icon (see `src/app_icon.rs`).

fn main() {
    println!("cargo:rerun-if-changed=app.ico");

    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("app.ico");
        resource
            .compile()
            .expect("failed to embed app.ico into the executable");
    }
}
