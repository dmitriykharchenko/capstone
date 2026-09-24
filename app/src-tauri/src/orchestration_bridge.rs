//! Bounded, cancellable NDJSON transport. No raw stdout/stderr enters errors.
use crate::orchestration_wire::{parse, ERROR, MAX_BYTES};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct OwnedChild {
    child: Child,
    readers: Vec<JoinHandle<()>>,
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // Kill descendants too, including those retaining pipe handles after
            // their leader exited. This is our exclusively owned process group.
            unsafe {
                kill(-(self.child.id() as i32), 9);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(unix)]
        {
            extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // A child may fork while the first group signal is being delivered.
            // After reaping the leader it cannot create more descendants.
            unsafe {
                kill(-(self.child.id() as i32), 9);
            }
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

pub fn call<F>(root: &Path, request: &Value, on_line: F) -> Result<(), &'static str>
where
    F: FnMut(Value) -> Result<(), &'static str>,
{
    let executable = std::env::var_os("CAPSTONE_PYTHON")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "python3".into());
    call_with_executable(root, Path::new(&executable), request, on_line)
}

pub fn call_with_executable<F>(
    root: &Path,
    executable: &Path,
    request: &Value,
    mut on_line: F,
) -> Result<(), &'static str>
where
    F: FnMut(Value) -> Result<(), &'static str>,
{
    call_with_timeout(
        root,
        executable,
        request,
        &mut on_line,
        Duration::from_secs(120),
    )
}

fn call_with_timeout<F>(
    root: &Path,
    executable: &Path,
    request: &Value,
    mut on_line: F,
    timeout: Duration,
) -> Result<(), &'static str>
where
    F: FnMut(Value) -> Result<(), &'static str>,
{
    let payload = serde_json::to_vec(request).map_err(|_| ERROR)?;
    if payload.len() > 16_384 {
        return Err(ERROR);
    }
    let mut command = Command::new(executable);
    command
        .args(["-m", "src.cli"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut process = OwnedChild {
        child: command.spawn().map_err(|_| ERROR)?,
        readers: Vec::new(),
    };
    let mut stderr = process.child.stderr.take().ok_or(ERROR)?;
    process.readers.push(thread::spawn(move || {
        let mut buffer = [0; 8192];
        while matches!(stderr.read(&mut buffer), Ok(n) if n > 0) {}
    }));
    let stdout = process.child.stdout.take().ok_or(ERROR)?;
    let (tx, rx) = mpsc::sync_channel(2);
    process.readers.push(thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut bytes = Vec::new();
            let read = (&mut reader)
                .take((MAX_BYTES + 2) as u64)
                .read_until(b'\n', &mut bytes);
            if matches!(read, Ok(0)) {
                break;
            }
            let value =
                if read.is_err() || bytes.len() > MAX_BYTES + 1 || bytes.last() != Some(&b'\n') {
                    Err(ERROR)
                } else {
                    std::str::from_utf8(&bytes[..bytes.len() - 1])
                        .map_err(|_| ERROR)
                        .and_then(parse)
                };
            let failed = value.is_err();
            if tx.send(value).is_err() || failed {
                break;
            }
        }
    }));
    let result = (|| {
        let mut stdin = process.child.stdin.take().ok_or(ERROR)?;
        process.readers.push(thread::spawn(move || {
            let _ = stdin.write_all(&payload);
        }));
        loop {
            match rx.recv_timeout(timeout) {
                Ok(value) => on_line(value?)?,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(_) => return Err(ERROR),
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = process.child.try_wait().map_err(|_| ERROR)? {
                if !status.success() {
                    return Err(ERROR);
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err(ERROR);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    })();
    // Unblock a reader whose bounded queue is full before joining it on failure.
    drop(rx);
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new(body: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "orchestration-bridge-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            let file = root.join("fake-python");
            fs::write(
                &file,
                format!("#!/bin/sh\nprintf '%s' $$ > leader.pid\n{body}\n"),
            )
            .unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
            Self(root)
        }
        fn call(
            &self,
            callback: impl FnMut(Value) -> Result<(), &'static str>,
        ) -> Result<(), &'static str> {
            call_with_timeout(
                &self.0,
                &self.0.join("fake-python"),
                &serde_json::json!({}),
                callback,
                Duration::from_secs(2),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(pid) = fs::read_to_string(self.0.join("leader.pid")) {
                let pid: i32 = pid.parse().unwrap();
                extern "C" {
                    fn kill(pid: i32, signal: i32) -> i32;
                }
                assert_ne!(unsafe { kill(pid, 0) }, 0, "owned child was not reaped");
            }
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn drains_large_stderr_and_reads_bounded_json() {
        let fixture = Fixture::new(
            "dd if=/dev/zero bs=8192 count=256 >&2 2>/dev/null\nprintf '{\"ok\":true}\\n'",
        );
        let mut count = 0;
        fixture
            .call(|value| {
                assert_eq!(value["ok"], true);
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn malformed_oversized_partial_and_nonzero_are_sanitized() {
        for body in [
            "printf 'malformed\\n'",
            "printf '{\"x\":1,\"x\":2}\\n'",
            "printf '{\"x\":1}'",
            "dd if=/dev/zero bs=8192 count=130 2>/dev/null",
            "exit 3",
        ] {
            let fixture = Fixture::new(body);
            assert_eq!(fixture.call(|_| Ok(())), Err(ERROR));
        }
    }

    #[test]
    fn channel_close_and_stalled_pipes_kill_and_reap_group() {
        for body in [
            "printf '{}\\n'; sleep 10",
            "sleep 10 & printf '{}\\n'; wait",
            "exec 1>&-; sleep 10",
            "sleep 10 & exit 0",
        ] {
            let fixture = Fixture::new(body);
            let start = Instant::now();
            assert_eq!(fixture.call(|_| Err(ERROR)), Err(ERROR));
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "fixture {body} exceeded cleanup deadline: {:?}",
                start.elapsed()
            );
        }
    }
}
