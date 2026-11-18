//! The application icon. One `app.ico` at the repository root feeds the
//! executable resource (`build.rs`), the native window icon and the logo drawn
//! in the top bar, so all three can never disagree.

/// Same file the Windows installer uses as its setup icon.
pub const ICO: &[u8] = include_bytes!("../app.ico");

/// Side of the logo texture uploaded to egui. About four times the ~22 points
/// it is painted at, which keeps it crisp when Windows display scaling raises
/// the points-per-pixel.
const LOGO_TEXTURE_SIDE: u32 = 96;

/// Decodes the largest layer of the embedded `.ico`.
fn decode() -> Option<image::RgbaImage> {
    image::load_from_memory_with_format(ICO, image::ImageFormat::Ico)
        .ok()
        .map(|icon| icon.to_rgba8())
}

/// Native window and taskbar icon. `None` only if the committed file is
/// corrupt, in which case the app still opens with the platform default.
pub fn window_icon() -> Option<egui::IconData> {
    let icon = decode()?;
    let (width, height) = icon.dimensions();
    Some(egui::IconData {
        rgba: icon.into_raw(),
        width,
        height,
    })
}

/// Header logo texture, downscaled once at startup.
pub fn header_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let icon = decode()?;
    let logo = image::imageops::resize(
        &icon,
        LOGO_TEXTURE_SIDE,
        LOGO_TEXTURE_SIDE,
        image::imageops::FilterType::Lanczos3,
    );
    let image =
        egui::ColorImage::from_rgba_unmultiplied([LOGO_TEXTURE_SIDE as usize; 2], logo.as_raw());
    Some(ctx.load_texture("app_logo", image, egui::TextureOptions::LINEAR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icon_decodes_to_a_large_square_layer() {
        let icon = decode().expect("app.ico decodes");
        assert_eq!(icon.width(), icon.height());
        assert!(icon.width() >= 256, "largest layer is {}px", icon.width());
    }

    #[test]
    fn window_icon_rgba_matches_its_dimensions() {
        let icon = window_icon().expect("window icon");
        assert_eq!(
            icon.rgba.len(),
            (icon.width * icon.height * 4) as usize,
            "RGBA data does not cover {}x{}",
            icon.width,
            icon.height
        );
    }
}
