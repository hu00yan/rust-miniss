use signal_hook::consts::SIGTERM;
use signal_hook::iterator::Signals;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn signal_handling_sigterm_graceful() {
    let term_received = Arc::new(AtomicBool::new(false));
    let term_received_clone = term_received.clone();
    let ready = Arc::new(AtomicBool::new(false));
    let ready_clone = ready.clone();

    // Spawn a thread to install handler and listen for SIGTERM
    let handle = std::thread::spawn(move || {
        let mut signals = Signals::new([SIGTERM]).expect("create signals");
        // Mark that handler is installed
        ready_clone.store(true, Ordering::SeqCst);
        for sig in &mut signals {
            if sig == SIGTERM {
                term_received_clone.store(true, Ordering::SeqCst);
                break;
            }
        }
    });

    // Wait until the signal handler thread reports readiness to avoid race
    let start = std::time::Instant::now();
    while !ready.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(1) {
        std::thread::yield_now();
    }

    // Send SIGTERM to current process
    unsafe {
        libc::kill(libc::getpid(), libc::SIGTERM);
    }

    // Wait briefly to allow signal to be handled
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(term_received.load(Ordering::SeqCst));
    handle.join().unwrap();
}
