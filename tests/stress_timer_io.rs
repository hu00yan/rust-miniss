use std::{
    fs::OpenOptions,
    io::Write,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

// Long-lived stress test exercising timers + I/O for 10 minutes.
// Run manually: `cargo test --test stress_timer_io -- --ignored --nocapture`.
// This test is ignored in CI to prevent timeouts.
#[test]
#[ignore = "Long running stress test - run manually"]
fn stress_timer_io() {
    const RUN_SECS: u64 = 600; // 10 minutes

    let start = Instant::now();
    let tmp_path = std::env::temp_dir().join("stress_timer_io.tmp");
    let file = Arc::new(Mutex::new(
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .unwrap(),
    ));

    let writer = {
        let file = Arc::clone(&file);
        thread::spawn(move || {
            let mut counter: u64 = 0;
            while start.elapsed() < Duration::from_secs(RUN_SECS) {
                {
                    let mut f = file.lock().unwrap();
                    writeln!(f, "counter {}", counter).unwrap();
                }
                counter += 1;
                // Use yield instead of sleep for stress testing
                thread::yield_now();
            }
        })
    };

    let timer = thread::spawn(move || {
        while start.elapsed() < Duration::from_secs(RUN_SECS) {
            // Use yield instead of sleep for stress testing
            thread::yield_now();
        }
    });

    writer.join().unwrap();
    timer.join().unwrap();
}
