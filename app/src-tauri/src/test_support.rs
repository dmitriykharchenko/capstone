//! Shared helpers for tests that run commands against a fake Python bridge.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tauri::ipc::{Channel, InvokeResponseBody, IpcResponse};

static PYTHON_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Run `invoke` with `CAPSTONE_PYTHON` pointing at a shell script built from
/// `script`, returning its result and every JSON frame sent to the channel.
pub fn run_with_fake_python<T: IpcResponse, R>(
    script: &str,
    invoke: impl FnOnce(Channel<T>) -> R,
) -> (R, Vec<Value>) {
    let _lock = PYTHON_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("capstone-fake-python-{suffix}.sh"));
    fs::write(&path, script).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();

    let previous = std::env::var_os("CAPSTONE_PYTHON");
    std::env::set_var("CAPSTONE_PYTHON", &path);
    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let sink = Arc::clone(&received);
    let channel = Channel::new(move |body| {
        if let InvokeResponseBody::Json(json) = body {
            sink.lock()
                .unwrap()
                .push(serde_json::from_str(&json).unwrap());
        }
        Ok(())
    });
    let result = invoke(channel);
    match previous {
        Some(value) => std::env::set_var("CAPSTONE_PYTHON", value),
        None => std::env::remove_var("CAPSTONE_PYTHON"),
    }
    let _ = fs::remove_file(path);
    let events = received.lock().unwrap().clone();
    (result, events)
}
