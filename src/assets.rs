//! Resolves embedded Zmux assets first, then falls back to Zed's embedded
//! assets for Project Panel and file-icon paths.

use gpui::{AssetSource, Result, SharedString};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "fonts/**/*"]
#[include = "icons/**/*"]
#[include = "images/**/*"]
#[include = "themes/**/*"]
#[exclude = "*.DS_Store"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(file) = Self::get(path) {
            return Ok(Some(file.data));
        }
        zed_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut assets: Vec<_> = Self::iter()
            .filter(|candidate| candidate.starts_with(path))
            .map(SharedString::from)
            .collect();
        assets.extend(zed_assets::Assets.list(path)?);
        assets.sort();
        assets.dedup();
        Ok(assets)
    }
}
