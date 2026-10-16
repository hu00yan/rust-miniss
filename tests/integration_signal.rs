#![cfg(feature = "signal")]

use signal_hook::consts::SIGTERM;
use signal_hook::iterator::Signals;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn signal_handling_sigterm_graceful() {
    let term_received = Arc::new(AtomicBool::new(false));
    let term_received_clone = term_received.clone();
    let ready = Arc::new(AtomicBool::new(false));
    let ready_clone = ready.clone();

    let handle = std::thread::spawn(move || {
        let mut signals = Signals::new([SIGTERM]).expect("create signals");
        ready_clone.store(true, Ordering::Release);
        for sig in &mut signals {
            if sig == SIGTERM {
                term_received_clone.store(true, Ordering::Release);
                break;
            }
        }
    });

    let start = std::time::Instant::now();
    while !ready.load(Ordering::Acquire) && start.elapsed() < Duration::from_secs(1) {
        std::thread::yield_now();
    }

    let runtime = rust_miniss::Runtime::new();
    runtime.block_on(async {
        // SAFETY: Sending SIGTERM to the current process is the behavior under
        // test; libc::kill is called with a valid pid from getpid().
        unsafe {
            libc::kill(libc::getpid(), libc::SIGTERM);
        }
        rust_miniss::timer::sleep(Duration::from_millis(100)).await;
    });

    assert!(term_received.load(Ordering::Acquire));
    handle.join().unwrap();
}
