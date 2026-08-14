//! Zed extension integration.
//!
//! The stock Zed extension host, store, and gallery UI are used directly. Its
//! language bridge registers extension grammars and language metadata with zmux's
//! existing `LanguageRegistry`, so normal and diff buffers share highlighting.

use std::sync::Arc;

use extension::ExtensionHostProxy;
use gpui::App;
use workspace::AppState;


/// Initialize Zed's extension infrastructure against zmux's language registry.
///
/// `paths` is configured to zmux's data directory during bootstrap, so the stock
/// extension store keeps its state under `<zmux data dir>/extensions`, never in a
/// Zed installation's extension directory.
pub fn init(app_state: &Arc<AppState>, cx: &mut App) {
    extension::init(cx);
    let extension_host = ExtensionHostProxy::default_global(cx);
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
    use std::path::Path;

    use extension::ExtensionLanguageProxy as _;
    use gpui::TestAppContext;
    use language::{LanguageMatcher, LanguageName, LanguageRegistry};

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
            Arc::new(|| anyhow::bail!("fixture language loading is not exercised here")),
        );

        assert_eq!(
            registry
                .language_for_file_path(Path::new("example.fixture"))
                .map(|language| language.name().to_string())
                .as_deref(),
            Some("Fixture")
        );
    }
}

