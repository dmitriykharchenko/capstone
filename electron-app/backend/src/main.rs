use std::time::Duration;

use research_backend::{composition::browsing_registry, runtime::serve};

fn main() {
    std::panic::set_hook(Box::new(|_| eprintln!("backend: internal failure")));
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("backend: runtime unavailable");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(serve(
        tokio::io::stdin(),
        tokio::io::stdout(),
        browsing_registry(|key| std::env::var(key).ok()),
    ));
    // Tokio stdin uses a blocking read. A closed stdout must not keep the child
    // alive forever waiting for an owner that no longer reads protocol output.
    runtime.shutdown_timeout(Duration::from_millis(100));
    if result.is_err() {
        eprintln!("backend: transport closed");
        std::process::exit(1);
    }
}
