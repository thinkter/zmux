//! Zed extension integration.
//!
//! The stock Zed extension host, store, and gallery UI are used directly. Its
//! language and theme bridges register extension contributions with zmux's
//! existing registries.

use std::sync::Arc;

use extension::ExtensionHostProxy;
use gpui::App;
use workspace::AppState;

/// Initialize Zed's extension infrastructure against zmux's language registry.
///
/// `paths` is configured to zmux's data directory during bootstrap, so Zed's
/// extension store keeps its state under `<zmux data dir>/extensions`, never in
/// a Zed installation's extension directory.
pub fn init(app_state: &Arc<AppState>, cx: &mut App) {
    extension::init(cx);
    let extension_host = ExtensionHostProxy::default_global(cx);
    theme_extension::init(
        extension_host.clone(),
        theme::ThemeRegistry::global(cx),
        cx.background_executor().clone(),
    );
    language_extension::init(
        language_extension::LspAccess::Noop,
        extension_host.clone(),
        app_state.languages.clone(),
    );
    extension_host::init(
        extension_host,
        app_state.fs.clone(),
        app_state.client.clone(),
        app_state.node_runtime.clone(),
        cx,
    );
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use extension::{ExtensionLanguageProxy as _, ExtensionThemeProxy as _};
    use fs::FakeFs;
    use gpui::TestAppContext;
    use language::{LanguageMatcher, LanguageName, LanguageRegistry};
    use theme::ThemeRegistry;

    use super::*;

    #[gpui::test]
    async fn language_extension_bridge_registers_matching_languages(cx: &mut TestAppContext) {
        let registry = Arc::new(LanguageRegistry::new(cx.background_executor.clone()));
        let host = Arc::new(ExtensionHostProxy::new());
        language_extension::init(
            language_extension::LspAccess::Noop,
            host.clone(),
            registry.clone(),
        );

        host.register_language(
            LanguageName::new("Fixture"),
            None,
            LanguageMatcher {
                path_suffixes: vec!["fixture".into()],
                ..Default::default()
            },
            false,
            Arc::new(|| anyhow::bail!("fixture loading is not exercised here")),
        );

        assert_eq!(
            registry
                .language_for_file_path(Path::new("example.fixture"))
                .map(|language| language.name().to_string())
                .as_deref(),
            Some("Fixture")
        );
    }

    #[gpui::test]
    async fn theme_extension_bridge_registers_extension_themes(cx: &mut TestAppContext) {
        let host = Arc::new(ExtensionHostProxy::new());
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/extension",
            serde_json::json!({
                "theme.json": include_str!("../assets/themes/vercel-theme.json"),
            }),
        )
        .await;

        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            theme_extension::init(
                host.clone(),
                ThemeRegistry::global(cx),
                cx.background_executor().clone(),
            );
        });

        host.load_user_theme(PathBuf::from("/extension/theme.json"), fs)
            .await
            .expect("extension theme should load");

        cx.update(|cx| {
            let names = ThemeRegistry::global(cx).list_names();
            assert!(names.iter().any(|name| name == "Vercel Dark"));
            assert!(names.iter().any(|name| name == "Vercel Light"));
        });
    }
}
