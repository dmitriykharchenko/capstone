//! Trusted startup composition. Browsing is read-only; explicitly opted-in native
//! indexing may initialize/recover derived stores, but never calls providers here.
use std::{path::PathBuf, sync::Arc};

use crate::{
    config::PaperBrowsingConfig,
    papers::{NativePaperRepository, PaperCommand, PaperCommandHandler},
    registry::Registry,
};

pub fn browsing_registry(env: impl Fn(&str) -> Option<String>) -> Registry {
    let mut registry = Registry::foundation();
    let configured = || {
        let data = PathBuf::from(env("CAPSTONE_APP_DATA_ROOT")?);
        let resources = PathBuf::from(env("CAPSTONE_RESOURCES_DIR")?);
        let papers = PathBuf::from(env("CAPSTONE_PAPERS_DIR")?);
        let config = PaperBrowsingConfig::new(&data, &resources, &papers).ok()?;
        NativePaperRepository::new(config.papers).ok()
    };
    // Invalid/missing roots fail closed, but health remains usable. Nothing is
    // silently treated as an empty library or redirected to a different root.
    if let Some(repository) = configured() {
        for (name, command) in [
            ("list_papers", PaperCommand::List),
            ("get_paper", PaperCommand::Get),
            ("read_pdf", PaperCommand::ReadPdf),
        ] {
            registry
                .install(
                    name,
                    "papers",
                    Arc::new(PaperCommandHandler::new(repository.clone(), command)),
                )
                .expect("checked-in paper ownership slots");
        }
    }
    registry
}

/// Explicit opt-in. Provider construction validates configuration but sends no
/// requests. Never infer a dimension using a paid probe or fall back to a mock.
pub async fn native_registry(env: impl Fn(&str) -> Option<String>) -> Registry {
    use crate::{
        config::{AppConfig, Configuration},
        graph::PaperMetadataExtractor,
        indexing::{CoordinatedIndex, IndexError, NativeIndexer, install_commands},
        providers::{EmbeddingConfig, HttpEmbeddingClient, HttpModelClient, ModelConfig},
        retrieval::EmbeddingIdentity,
    };
    let mut registry = browsing_registry(&env);
    if env("CAPSTONE_NATIVE_INDEXING").as_deref() != Some("1") {
        return registry;
    }
    let configured = || {
        let root = PathBuf::from(env("CAPSTONE_APP_DATA_ROOT")?);
        let resources = PathBuf::from(env("CAPSTONE_RESOURCES_DIR")?);
        for key in [
            "CAPSTONE_PAPERS_DIR",
            "CAPSTONE_EMBED_MODEL",
            "CAPSTONE_EMBED_BASE_URL",
            "CAPSTONE_CHAT_MODEL",
            "CAPSTONE_CHAT_BASE_URL",
        ] {
            if env(key)?.trim().is_empty() {
                return None;
            }
        }
        let config = AppConfig::from_lookup(&root, &resources, &env).ok()?;
        let dimension = env("CAPSTONE_EMBED_DIMENSIONS")?.parse::<usize>().ok()?;
        if !(1..=32).contains(&config.providers.index_max_workers) {
            return None;
        }
        let settings = &config.providers;
        let key = settings.api_key.as_ref()?.expose_secret();
        let identity = EmbeddingIdentity {
            // Bind endpoint/model and the actual native embedding mode, without
            // disclosing this private configuration in any wire response.
            model: format!(
                "native-http-v1:{}:{}",
                settings.embedding_base_url, settings.embedding_model
            ),
            dimension,
        };
        identity.validate().ok()?;
        let mut embedding = EmbeddingConfig::new(
            settings.embedding_model.clone(),
            settings.embedding_base_url.clone(),
            key.into(),
        );
        embedding.expected_dimensions = Some(dimension);
        embedding.policy.timeout = settings.request_timeout;
        let mut model = ModelConfig::new(
            settings.chat_model.clone(),
            settings.chat_base_url.clone(),
            key.into(),
        );
        model.max_tokens = settings.chat_max_tokens;
        model.policy.timeout = settings.request_timeout;
        let embeddings = HttpEmbeddingClient::new(embedding).ok()?;
        let metadata = PaperMetadataExtractor::from_http(HttpModelClient::new(model).ok()?);
        Some((config, identity, embeddings, metadata))
    };
    let Some((config, identity, embeddings, metadata)) = configured() else {
        install_commands(&mut registry, Err("unavailable"));
        return registry;
    };
    let result = async {
        let index = CoordinatedIndex::open(config.paths(), identity).await?;
        index.recover().await?;
        NativeIndexer::new(
            index,
            Arc::new(embeddings),
            Arc::new(metadata),
            config.providers.index_max_workers,
        )
        .map_err(IndexError::from)
    }
    .await;
    install_commands(
        &mut registry,
        result.map_err(|error| {
            if error == IndexError::RebuildRequired {
                "rebuild_required"
            } else {
                "recovery_required"
            }
        }),
    );
    registry
}
