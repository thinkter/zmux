//! Registers the fonts bundled under `assets/fonts/` with GPUI's text system,
//! mirroring Zed's `load_embedded_fonts`. GPUI resolves the `.ZedMono` and
//! `.ZedSans` family aliases to these embedded families (Lilex and IBM Plex
//! Sans), so font resolution succeeds even on systems with no suitable fonts
//! installed.

use gpui::App;

pub fn load_embedded_fonts(cx: &App) -> anyhow::Result<()> {
    let asset_source = cx.asset_source();
    let mut embedded_fonts = Vec::new();
    for font_path in asset_source.list("fonts")? {
        if !font_path.ends_with(".ttf") {
            continue;
        }
        if let Some(font_bytes) = asset_source.load(&font_path)? {
            embedded_fonts.push(font_bytes);
        }
    }
    cx.text_system().add_fonts(embedded_fonts)
}
