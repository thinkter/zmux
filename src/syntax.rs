use std::sync::Arc;

use language::{LanguageRegistry, LoadedLanguage};

/// Register built-in parsers for syntax highlighting without enabling LSPs.
pub fn register_builtin_languages(languages: &Arc<LanguageRegistry>) {
    let native_grammars = grammars::native_grammars();
    let language_names = native_grammars
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();
    languages.register_native_grammars(native_grammars);

    for name in language_names {
        let config = grammars::load_config(name);
        languages.register_language(
            config.name.clone(),
            config.grammar.clone(),
            config.matcher.clone(),
            config.hidden,
            None,
            Arc::new(move || {
                Ok(LoadedLanguage {
                    config: config.clone(),
                    queries: grammars::load_queries(name),
                    context_provider: None,
                    toolchain_provider: None,
                    manifest_name: None,
                })
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gpui::TestAppContext;

    use super::*;

    #[gpui::test]
    async fn detects_and_loads_registered_languages(cx: &mut TestAppContext) {
        let registry = Arc::new(LanguageRegistry::new(cx.background_executor.clone()));
        register_builtin_languages(&registry);

        assert_eq!(
            registry
                .language_for_file_path(Path::new("src/main.rs"))
                .map(|language| language.name().to_string())
                .as_deref(),
            Some("Rust")
        );

        let language = registry
            .load_language_for_file_path(Path::new("src/main.rs"))
            .await
            .expect("the registered Rust grammar should load");
        assert_eq!(language.name(), "Rust");
    }
}
