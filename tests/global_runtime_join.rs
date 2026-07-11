use rust_miniss::multicore::{init_runtime, shutdown_global_with_timeout, spawn};
use std::time::Duration;

#[test]
fn global_runtime_shutdown_joins_all_workers() {
    init_runtime(Some(1)).expect("global runtime init");
    spawn(async {}).expect("global task admission");
    shutdown_global_with_timeout(Duration::from_secs(1)).expect("global worker join");
}
