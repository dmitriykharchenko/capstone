use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

mod paths;
pub use paths::{validate_database_path, validate_root};

// Config/history owner validates these before opening anything. Paths are not
// accepted from renderer requests; legacy history is a read-only import source.
#[derive(Clone, Debug)]
pub struct StoragePaths {
    pub data_root: PathBuf,
    pub history_database: PathBuf,
    pub vector_database: PathBuf,
    pub graph_database: PathBuf,
    pub index_state_database: PathBuf,
    pub bm25_index: PathBuf,
    pub evaluation_results: PathBuf,
    pub resources: PathBuf,
    pub papers: PathBuf,
}

pub trait Configuration: Send + Sync {
    fn paths(&self) -> &StoragePaths;
}

#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Clone, Debug)]
pub struct ProviderSettings {
    pub chat_model: String,
    pub chat_base_url: String,
    pub embedding_model: String,
    pub embedding_base_url: String,
    pub api_key: Option<SecretString>,
    pub chat_max_tokens: Option<u32>,
    pub request_timeout: Duration,
    pub index_max_workers: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidPath,
    InvalidEndpoint,
    InvalidModel,
    InvalidInteger,
    InvalidCredential,
    InvalidDotenv,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid backend configuration ({self:?}).")
    }
}
impl std::error::Error for ConfigError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigNotice {
    LegacyHistoryRequiresImport,
    LegacyDerivedStorageIgnored,
    LegacyLogPathIgnored,
}

/// Only trusted desktop startup may supply these roots. Construction is read-only.
/// Legacy CAPSTONE_DB_PATH is an import *candidate*, never a writable override.
#[derive(Clone, Debug)]
pub struct AppConfig {
    paths: StoragePaths,
    pub providers: ProviderSettings,
    pub system_prompt: Option<SecretString>,
    pub legacy_history_source: Option<PathBuf>,
    pub log_path: PathBuf,
    pub retrieval_log_path: PathBuf,
    pub notices: Vec<ConfigNotice>,
}

impl Configuration for AppConfig {
    fn paths(&self) -> &StoragePaths {
        &self.paths
    }
}

/// The read-only subset of configuration: no provider, dotenv, history or index
/// settings are consulted just to browse. The same root separation applies.
pub struct PaperBrowsingConfig {
    pub data_root: PathBuf,
    pub resources: PathBuf,
    pub papers: PathBuf,
}

impl PaperBrowsingConfig {
    pub fn new(data: &Path, resources: &Path, papers: &Path) -> Result<Self, ConfigError> {
        let data_root = validate_root(data)?;
        let resources = read_only_root(resources)?;
        let papers = read_only_root(papers)?;
        if data_root.starts_with(&resources)
            || resources.starts_with(&data_root)
            || papers.starts_with(&data_root)
            || data_root.starts_with(&papers)
        {
            return Err(ConfigError::InvalidPath);
        }
        Ok(Self {
            data_root,
            resources,
            papers,
        })
    }
}

fn read_only_root(path: &Path) -> Result<PathBuf, ConfigError> {
    if !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(ConfigError::InvalidPath);
    }
    let path = path.canonicalize().map_err(|_| ConfigError::InvalidPath)?;
    if !path.is_dir() {
        return Err(ConfigError::InvalidPath);
    }
    Ok(path)
}

impl AppConfig {
    pub fn from_env(app_data_root: &Path, resources: &Path) -> Result<Self, ConfigError> {
        Self::from_lookup(app_data_root, resources, |key| std::env::var(key).ok())
    }

    /// Explicit dotenv only: never searches a repository or parent directories,
    /// never mutates process environment, and existing environment always wins.
    pub fn from_dotenv(
        app_data_root: &Path,
        resources: &Path,
        file: &Path,
    ) -> Result<Self, ConfigError> {
        let values: std::collections::HashMap<_, _> = dotenvy::from_path_iter(file)
            .map_err(|_| ConfigError::InvalidDotenv)?
            .collect::<Result<_, _>>()
            .map_err(|_| ConfigError::InvalidDotenv)?;
        Self::from_lookup(app_data_root, resources, |key| {
            std::env::var(key).ok().or_else(|| values.get(key).cloned())
        })
    }

    pub fn from_lookup(
        app_data_root: &Path,
        resources: &Path,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let papers = env("CAPSTONE_PAPERS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| resources.join("ResearchPapers"));
        let PaperBrowsingConfig {
            data_root: root,
            resources,
            papers,
        } = PaperBrowsingConfig::new(app_data_root, resources, &papers)?;
        let history = env("CAPSTONE_APP_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("chat_history.sqlite3"));
        let history = validate_database_path(&root, &history)?;
        let mut notices = Vec::new();
        let legacy_history_source = env("CAPSTONE_DB_PATH").map(PathBuf::from);
        if let Some(source) = &legacy_history_source {
            if !source.is_absolute()
                || source.starts_with(&root)
                || source
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(ConfigError::InvalidPath);
            }
            let resolved = source.canonicalize().or_else(|_| {
                let parent = source.parent().ok_or(ConfigError::InvalidPath)?;
                Ok::<_, ConfigError>(
                    validate_root(parent)?
                        .join(source.file_name().ok_or(ConfigError::InvalidPath)?),
                )
            })?;
            if resolved.starts_with(&root) {
                return Err(ConfigError::InvalidPath);
            }
            notices.push(ConfigNotice::LegacyHistoryRequiresImport);
        }
        if env("CAPSTONE_CHROMA_DIR").is_some() {
            notices.push(ConfigNotice::LegacyDerivedStorageIgnored);
        }
        if env("CAPSTONE_LOG_PATH").is_some() || env("CAPSTONE_RETRIEVAL_LOG_PATH").is_some() {
            notices.push(ConfigNotice::LegacyLogPathIgnored);
        }
        let openai = credential(env("OPENAI_API_KEY"))?;
        let openrouter = credential(env("OPENROUTER_API_KEY"))?;
        let router = openai.is_none() && openrouter.is_some();
        let chat_base_url = env("CAPSTONE_CHAT_BASE_URL").unwrap_or_else(|| {
            if router {
                "https://openrouter.ai/api/v1"
            } else {
                "https://api.openai.com/v1"
            }
            .into()
        });
        let embedding_base_url =
            env("CAPSTONE_EMBED_BASE_URL").unwrap_or_else(|| chat_base_url.clone());
        validate_endpoint(&chat_base_url)?;
        validate_endpoint(&embedding_base_url)?;
        let chat_model = env("CAPSTONE_CHAT_MODEL").unwrap_or_else(|| {
            if router {
                "openai/gpt-5.4-mini"
            } else {
                "gpt-4o-mini"
            }
            .into()
        });
        let embedding_model =
            env("CAPSTONE_EMBED_MODEL").unwrap_or_else(|| "text-embedding-3-small".into());
        for model in [&chat_model, &embedding_model] {
            if model.trim().is_empty() || model.chars().any(char::is_control) {
                return Err(ConfigError::InvalidModel);
            }
        }
        let providers = ProviderSettings {
            chat_model,
            chat_base_url,
            embedding_model,
            embedding_base_url,
            api_key: openai.or(openrouter),
            chat_max_tokens: positive(&env, "CAPSTONE_CHAT_MAX_TOKENS")?,
            request_timeout: Duration::from_secs(
                positive(&env, "CAPSTONE_REQUEST_TIMEOUT_SECONDS")?
                    .unwrap_or(30)
                    .into(),
            ),
            index_max_workers: positive(&env, "CAPSTONE_INDEX_MAX_WORKERS")?.unwrap_or(4) as usize,
        };
        let paths = StoragePaths {
            history_database: history,
            vector_database: root.join("vectors.sqlite3"),
            graph_database: root.join("graph.sqlite3"),
            index_state_database: root.join("index-state.sqlite3"),
            bm25_index: root.join("bm25.json"),
            evaluation_results: root.join("evaluations"),
            resources,
            papers,
            data_root: root.clone(),
        };
        for path in [
            &paths.vector_database,
            &paths.graph_database,
            &paths.index_state_database,
            &paths.bm25_index,
        ] {
            validate_database_path(&root, path)?;
        }
        let log_path = validate_database_path(&root, &root.join("chat.log"))?;
        let retrieval_log_path = validate_database_path(&root, &root.join("retrieval.log"))?;
        validate_root(&paths.evaluation_results)?;
        if paths.history_database == paths.vector_database
            || paths.history_database == paths.graph_database
            || paths.history_database == paths.index_state_database
            || paths.history_database == paths.index_state_database.with_extension("sqlite3.lock")
            || ["-journal", "-wal", "-shm"].iter().any(|suffix| {
                paths.history_database
                    == Path::new(&format!("{}{suffix}", paths.index_state_database.display()))
            })
            || paths.history_database == paths.bm25_index
            || paths.history_database == paths.evaluation_results
            || paths.history_database == log_path
            || paths.history_database == retrieval_log_path
        {
            return Err(ConfigError::InvalidPath);
        }
        Ok(Self {
            paths,
            providers,
            system_prompt: env("CAPSTONE_SYSTEM_PROMPT")
                .filter(|s| !s.trim().is_empty())
                .map(SecretString),
            legacy_history_source,
            log_path,
            retrieval_log_path,
            notices,
        })
    }
}

fn credential(value: Option<String>) -> Result<Option<SecretString>, ConfigError> {
    match value {
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) if value.chars().any(char::is_control) => Err(ConfigError::InvalidCredential),
        value => Ok(value.map(SecretString)),
    }
}

fn positive(env: &impl Fn(&str) -> Option<String>, name: &str) -> Result<Option<u32>, ConfigError> {
    env(name)
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.trim()
                .parse::<u32>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or(ConfigError::InvalidInteger)
        })
        .transpose()
}

fn validate_endpoint(value: &str) -> Result<(), ConfigError> {
    let url = reqwest::Url::parse(value).map_err(|_| ConfigError::InvalidEndpoint)?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.host_str().is_none()
        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidEndpoint);
    }
    Ok(())
}
