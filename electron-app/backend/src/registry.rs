use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    protocol::{ErrorCode, MAX_ACTIVE_REQUESTS, MAX_FRAME_BYTES, MAX_SESSION_REQUESTS, VERSION},
    runtime::{CancellationMode, RequestContext},
};

pub type BackendResult<T> = Result<T, ErrorCode>;
pub type BackendFuture<T> = Pin<Box<dyn Future<Output = BackendResult<T>> + Send + 'static>>;

pub trait CommandHandler: Send + Sync {
    fn cancellation_mode(&self) -> CancellationMode {
        CancellationMode::Immediate
    }

    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub command: String,
    pub owner: String,
    pub streaming: bool,
}

#[derive(Default)]
pub struct Registry {
    specs: BTreeMap<String, CommandSpec>,
    handlers: BTreeMap<String, Arc<dyn CommandHandler>>,
}

impl Registry {
    pub fn new(specs: Vec<CommandSpec>) -> Result<Self, &'static str> {
        let mut registry = Self::default();
        for spec in specs {
            if spec.command.is_empty()
                || registry.specs.insert(spec.command.clone(), spec).is_some()
            {
                return Err("invalid or duplicate command specification");
            }
        }
        Ok(registry)
    }

    pub fn foundation() -> Self {
        let specs = serde_json::from_str(include_str!("../../contracts/commands.json"))
            .expect("checked-in command inventory must be valid");
        Self::new(specs).expect("checked-in command inventory must be unique")
    }

    // Modules may only install handlers for their frozen ownership slots.
    pub fn install(
        &mut self,
        command: &str,
        owner: &str,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<(), &'static str> {
        let spec = self.specs.get(command).ok_or("command not inventoried")?;
        if spec.owner != owner || command == "health" || self.handlers.contains_key(command) {
            return Err("command ownership conflict");
        }
        self.handlers.insert(command.to_owned(), handler);
        Ok(())
    }

    pub fn handler(&self, command: &str) -> BackendResult<Arc<dyn CommandHandler>> {
        if !self.specs.contains_key(command) {
            return Err(ErrorCode::UnknownCommand);
        }
        self.handlers
            .get(command)
            .cloned()
            .ok_or(ErrorCode::Unavailable)
    }

    pub fn is_streaming(&self, command: &str) -> bool {
        self.specs.get(command).is_some_and(|spec| spec.streaming)
    }

    pub fn health(&self) -> Value {
        let commands: Vec<_> = self.specs.values().map(|spec| json!({
            "command": spec.command, "owner": spec.owner, "streaming": spec.streaming,
            "available": spec.command == "health" || self.handlers.contains_key(&spec.command)
        })).collect();
        json!({
            "ok": true, "protocol_version": VERSION, "backend": "rust",
            "stage": if self.handlers.contains_key("list_papers") { "paper_browsing" } else { "foundation" },
            "chat_mode": "unavailable", "search_enabled": self.handlers.contains_key("search"),
            "limits": {"frame_bytes": MAX_FRAME_BYTES, "active_requests": MAX_ACTIVE_REQUESTS,
                "session_requests": MAX_SESSION_REQUESTS},
            "commands": commands,
            "data_policy": "copy_history_rebuild_separate_indexes",
            "paid_operations_authorized": false
        })
    }
}
