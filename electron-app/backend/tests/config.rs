use research_backend::config::{AppConfig, ConfigError, ConfigNotice, Configuration};
use std::{collections::HashMap, fs, path::PathBuf};

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let resources = temp.path().join("resources");
    fs::create_dir_all(resources.join("ResearchPapers")).unwrap();
    let data = temp.path().join("new-app");
    (temp, data, resources)
}
fn config(
    data: &std::path::Path,
    resources: &std::path::Path,
    vars: &[(&str, &str)],
) -> Result<AppConfig, ConfigError> {
    let vars = vars.iter().copied().collect::<HashMap<_, _>>();
    AppConfig::from_lookup(data, resources, |name| {
        vars.get(name).map(|s| s.to_string())
    })
}

#[test]
fn missing_credentials_have_safe_defaults_without_any_writes() {
    let (_temp, data, resources) = fixture();
    let c = config(&data, &resources, &[]).unwrap();
    assert!(!data.exists());
    assert!(c.providers.api_key.is_none());
    assert_eq!(c.providers.chat_model, "gpt-4o-mini");
    assert_eq!(c.providers.embedding_model, "text-embedding-3-small");
    assert_eq!(c.providers.index_max_workers, 4);
    assert_eq!(c.providers.request_timeout.as_secs(), 30);
    assert!(
        c.paths()
            .history_database
            .starts_with(c.paths().data_root.clone())
    );
}

#[test]
fn provider_precedence_overrides_and_secret_redaction() {
    let (_temp, data, resources) = fixture();
    let c = config(
        &data,
        &resources,
        &[
            ("OPENROUTER_API_KEY", "synthetic-router"),
            ("CAPSTONE_CHAT_MAX_TOKENS", "500"),
            ("CAPSTONE_INDEX_MAX_WORKERS", "2"),
            ("CAPSTONE_SYSTEM_PROMPT", "synthetic-private"),
        ],
    )
    .unwrap();
    assert_eq!(c.providers.chat_model, "openai/gpt-5.4-mini");
    assert_eq!(c.providers.chat_base_url, "https://openrouter.ai/api/v1");
    assert_eq!(c.providers.embedding_base_url, c.providers.chat_base_url);
    assert_eq!(c.providers.chat_max_tokens, Some(500));
    assert_eq!(c.providers.index_max_workers, 2);
    assert!(!format!("{c:?}").contains("synthetic-"));
    let c = config(
        &data,
        &resources,
        &[
            ("OPENAI_API_KEY", "synthetic-openai"),
            ("OPENROUTER_API_KEY", "synthetic-router"),
            ("CAPSTONE_CHAT_MODEL", "chosen"),
            ("CAPSTONE_EMBED_BASE_URL", "http://127.0.0.1:8123/v1"),
        ],
    )
    .unwrap();
    assert_eq!(c.providers.chat_base_url, "https://api.openai.com/v1");
    assert_eq!(c.providers.chat_model, "chosen");
    assert_eq!(
        c.providers.api_key.unwrap().expose_secret(),
        "synthetic-openai"
    );
}

#[test]
fn legacy_storage_overrides_are_never_new_app_writes() {
    let (temp, data, resources) = fixture();
    let legacy = temp.path().join("legacy.sqlite3");
    fs::write(&legacy, b"unchanged").unwrap();
    let c = config(
        &data,
        &resources,
        &[
            ("CAPSTONE_DB_PATH", legacy.to_str().unwrap()),
            ("CAPSTONE_CHROMA_DIR", "legacy-chroma"),
            ("CAPSTONE_LOG_PATH", "legacy.log"),
        ],
    )
    .unwrap();
    assert_eq!(c.legacy_history_source, Some(legacy.clone()));
    assert_eq!(
        c.notices,
        vec![
            ConfigNotice::LegacyHistoryRequiresImport,
            ConfigNotice::LegacyDerivedStorageIgnored,
            ConfigNotice::LegacyLogPathIgnored
        ]
    );
    assert_ne!(c.paths().history_database, legacy);
    assert_eq!(fs::read(legacy).unwrap(), b"unchanged");
    assert!(!data.exists());
}

#[test]
fn invalid_numeric_model_endpoint_and_credentials_fail_safely() {
    let (_temp, data, resources) = fixture();
    for key in [
        "CAPSTONE_CHAT_MAX_TOKENS",
        "CAPSTONE_INDEX_MAX_WORKERS",
        "CAPSTONE_REQUEST_TIMEOUT_SECONDS",
    ] {
        for value in ["0", "-1", "oops", "4294967296"] {
            assert_eq!(
                config(&data, &resources, &[(key, value)]).unwrap_err(),
                ConfigError::InvalidInteger
            );
        }
    }
    for value in [
        "http://example.com/v1",
        "file:///private",
        "https://user:password@example.com",
        "https://example.com?key=synthetic",
        "https://example.com/#fragment",
    ] {
        assert_eq!(
            config(&data, &resources, &[("CAPSTONE_CHAT_BASE_URL", value)]).unwrap_err(),
            ConfigError::InvalidEndpoint
        );
    }
    assert_eq!(
        config(&data, &resources, &[("CAPSTONE_CHAT_MODEL", " ")]).unwrap_err(),
        ConfigError::InvalidModel
    );
    assert_eq!(
        config(
            &data,
            &resources,
            &[("OPENAI_API_KEY", "synthetic\ninvalid")]
        )
        .unwrap_err(),
        ConfigError::InvalidCredential
    );
    let c = config(
        &data,
        &resources,
        &[
            ("OPENAI_API_KEY", "   "),
            ("OPENROUTER_API_KEY", "synthetic"),
        ],
    )
    .unwrap();
    assert!(c.providers.chat_base_url.contains("openrouter"));
}

#[test]
fn writable_overrides_must_be_separate_direct_children() {
    let (temp, data, resources) = fixture();
    let outside = temp.path().join("other.sqlite3");
    assert!(
        config(
            &data,
            &resources,
            &[("CAPSTONE_APP_DB_PATH", outside.to_str().unwrap())]
        )
        .is_err()
    );
    let valid = data.join("custom.sqlite3");
    assert_eq!(
        config(
            &data,
            &resources,
            &[("CAPSTONE_APP_DB_PATH", valid.to_str().unwrap())]
        )
        .unwrap()
        .paths()
        .history_database
        .file_name()
        .unwrap(),
        "custom.sqlite3"
    );
    assert!(config(&resources, &resources, &[]).is_err());
    assert!(
        config(
            &data,
            &resources,
            &[(
                "CAPSTONE_APP_DB_PATH",
                data.join("vectors.sqlite3").to_str().unwrap()
            )]
        )
        .is_err()
    );
}

#[test]
#[cfg(unix)]
fn symlinks_hardlinks_and_traversal_cannot_alias_legacy_writes() {
    use std::os::unix::fs::symlink;
    let (temp, data, resources) = fixture();
    fs::create_dir(&data).unwrap();
    let legacy = temp.path().join("legacy.db");
    fs::write(&legacy, b"legacy").unwrap();
    symlink(&legacy, data.join("chat_history.sqlite3")).unwrap();
    assert!(config(&data, &resources, &[]).is_err());
    let linked = data.join("hard.sqlite3");
    fs::hard_link(&legacy, &linked).unwrap();
    assert!(
        config(
            &data,
            &resources,
            &[("CAPSTONE_APP_DB_PATH", linked.to_str().unwrap())]
        )
        .is_err()
    );
    assert!(config(&data.join(".."), &resources, &[]).is_err());
    assert_eq!(fs::read(legacy).unwrap(), b"legacy");
}

#[test]
fn reserved_outputs_and_nonexistent_legacy_aliases_are_rejected() {
    let (_temp, data, resources) = fixture();
    for name in [
        "chat.log",
        "retrieval.log",
        "evaluations",
        "vectors.sqlite3",
        "graph.sqlite3",
        "bm25.json",
    ] {
        assert!(
            config(
                &data,
                &resources,
                &[("CAPSTONE_APP_DB_PATH", data.join(name).to_str().unwrap())]
            )
            .is_err()
        );
    }
    assert!(
        config(
            &data,
            &resources,
            &[(
                "CAPSTONE_DB_PATH",
                data.join("chat_history.sqlite3").to_str().unwrap()
            )]
        )
        .is_err()
    );
    assert!(
        config(
            &data,
            &resources,
            &[("CAPSTONE_DB_PATH", "relative.sqlite3")]
        )
        .is_err()
    );
    assert!(!data.exists());
}
