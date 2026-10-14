# Configuration reference

[Index](README.md) · [Getting started](getting-started.md) · [Storage](data-storage.md) · [Native desktop](native-desktop.md)

**Source inspection: October 14, 2026.** This reference documents variable names and source defaults, not the values present in any user's environment. No credential file or live credential was read to write it.

## Configuration loading

In source mode, the Python CLI searches for the nearest `.env` at or above the repository root and best-effort loads it without overriding existing environment variables. Frozen backends do not load `.env`; their writable defaults are under `~/Library/Application Support/Capstone` unless overridden. Missing credentials select a deterministic mock model and local embedder; mock behavior does not establish retrieval quality or provider connectivity. There is no general renderer settings API.

Sources: [Python configuration](../src/cli/config.py) and [Python construction](../src/cli/models.py).

### Tauri shell versus Python dotenv

The Rust Tauri shell itself resolves `CAPSTONE_PYTHON`, `CAPSTONE_PROJECT_ROOT`, and its native paper root from inherited environment. Source-mode Python subsequently loads dotenv inside its own process. Setting `CAPSTONE_PAPERS_DIR` only in that file can therefore make Python indexing and Rust paper browsing see different roots. For a nondefault corpus, supply the same intended value to the parent desktop process.

Likewise, `CAPSTONE_PROJECT_ROOT` changes the shell's working/source-root selection; it is not a replacement for all Python data-path overrides. In source mode, Python computes its default paths from the imported `src/cli/config.py` location.

### App-selected research library

In **Papers**, **Open folder** persists an absolute canonical research-library root. The native host passes that root only to each backend child as `CAPSTONE_LIBRARY_DIR`; it does not mutate the parent process environment. In this selected mode the root is authoritative for top-level PDF discovery, and every writable library artifact is derived beneath `<root>/.research-assistant/`. Legacy `CAPSTONE_DATA_DIR`, `CAPSTONE_PAPERS_DIR`, database, Chroma, and log overrides do not redirect a selected library to mixed locations.

No selection keeps the defaults and overrides below. A selected folder without `.research-assistant/` starts fresh; an existing directory is reused in place. Selection does not migrate, replace, or delete data, and it does not automatically index papers or call a provider.

## Provider settings

| Name | Default/selection | Behavior |
| --- | --- | --- |
| `OPENAI_API_KEY` | Absent | Preferred credential when selected/present; never print its value |
| `OPENROUTER_API_KEY` | Absent | Used when the OpenAI credential is not selected |
| `CAPSTONE_CHAT_BASE_URL` | `https://api.openai.com/v1`, or `https://openrouter.ai/api/v1` for OpenRouter selection | Explicit value wins over provider-derived default |
| `CAPSTONE_CHAT_MODEL` | `gpt-4o-mini`, or `openai/gpt-5.4-mini` for OpenRouter selection | Explicit value overrides the selected provider default |
| `CAPSTONE_EMBED_BASE_URL` | Resolved chat base URL | A distinct compatible embedding endpoint can be specified |
| `CAPSTONE_EMBED_MODEL` | `text-embedding-3-small` | Embedding identity must match stored vectors |
| `CAPSTONE_CHAT_MAX_TOKENS` | Unset | Optional positive integer; no app-specified output cap by default, but providers still impose limits |
| `CAPSTONE_SYSTEM_PROMPT` | Unset | Optional backend prompt override; treat its content as sensitive |
| `CAPSTONE_INDEX_MAX_WORKERS` | `4` | Requires a positive integer |

Python chooses `OPENAI_API_KEY or OPENROUTER_API_KEY` before its separate `has_api_key` whitespace check. Avoid setting unused blank/whitespace credential variables: a whitespace OpenAI value can interfere with the intended fallback.

When both real credentials are present, OpenAI takes precedence; an explicit endpoint override can still target a different endpoint. The selected single credential is used for chat and embeddings. There is no separate `CAPSTONE_EMBED_API_KEY` in the current CLI contract. Python delegates provider validation to its adapters/SDK; do not embed credentials in endpoint URLs.

## Tauri/Python paths and launcher settings

| Name | Default | Consumer |
| --- | --- | --- |
| `CAPSTONE_PYTHON` | Package script attempts Conda `ragcourse`; bridge fallback `python3` | Tauri interpreter selection |
| `CAPSTONE_PROJECT_ROOT` | Two levels above Tauri Cargo manifest directory | Tauri command working/root directory |
| `CAPSTONE_DATA_DIR` | Source: repository `data/`; frozen macOS: `~/Library/Application Support/Capstone` | Default root for writable backend data |
| `CAPSTONE_PAPERS_DIR` | Source: repository `ResearchPapers/`; frozen macOS: `CAPSTONE_DATA_DIR/ResearchPapers` | Native Tauri browsing and Python indexing, subject to loading distinction above |
| `CAPSTONE_DB_PATH` | `CAPSTONE_DATA_DIR/chat_history.sqlite3` | Canonical history and paper-status store |
| `CAPSTONE_CHROMA_DIR` | `CAPSTONE_DATA_DIR/chroma/` | Paper/history collections and lexical index directory |
| `CAPSTONE_LOG_PATH` | `CAPSTONE_DATA_DIR/chat_logs.txt` | Chat diagnostic output |
| `CAPSTONE_RETRIEVAL_LOG_PATH` | `CAPSTONE_DATA_DIR/retrieval_logs.txt` | Retrieval/context diagnostic output |
| `TAURI_DEV_HOST` | Unset | Vite host override; when set, configured websocket HMR uses port 1421 |

Relative Python overrides are interpreted by the process; prefer deliberate absolute paths for isolated profiles. Do not assume changing only the history path also redirects Chroma or every artifact.

`CAPSTONE_LIBRARY_DIR` is reserved for the desktop host's per-child selection contract. Prefer **Open folder** instead of setting it manually. When it is present, the selected-root rules above take precedence over the legacy path rows in this table.

Cozo's Python `paper_graph.db` is derived from the parent of the configured Chroma directory. Durable `chat_orchestration.sqlite3` is derived beside the history database. Evaluation artifact configuration is a separate code-level path/template rather than another documented `CAPSTONE_*` environment switch; see [Evaluation](evaluation.md).

Log files can contain questions, responses, sources, and full retrieved context. They are user data, not safe configuration summaries to paste into bug reports.

## Changing configuration safely

- **Model/endpoint/dimension changes:** use an intentionally rebuilt derived index; do not query existing vectors with an unrelated embedding space. Preserve canonical history and PDFs.
- **Moving a corpus:** align shell and backend paper roots and understand filename-derived identities/status reuse before reindexing.
- **Moving stores:** preserve the separate history, Chroma, graph, orchestration, and artifact paths; changing one does not relocate the rest.
- **Changing secrets:** restart the owning app through the trusted launcher. Verify presence/behavior without printing values.
- **Testing:** use injected providers and disposable roots; never depend on a repository or ancestor dotenv for an allegedly offline test.

Configuration presence does not verify billing, model access, supported structured-output behavior, or current provider availability. Those require separate intentional checks.
