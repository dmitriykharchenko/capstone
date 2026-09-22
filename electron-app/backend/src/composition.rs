//! Production browsing composition. Only trusted startup environment supplies
//! roots; never infer them from cwd or initialize provider/history/index stores.
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
