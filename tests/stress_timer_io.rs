use std::{fs::OpenOptions, io::Write, sync::{Arc, Mutex}, thread, time::{Duration, Instant}};

// Long-lived stress test exercising timers + I/O for 10 minutes.
// Run manually or in CI dedicated job: `cargo test --test stress_timer_io -- --ignored`.
#[test]
#[ignore]
fn stress_timer_io() {
    const RUN_SECS: u64 = 600; // 10 minutes

    let start = Instant::now();
    let tmp_path = std::env::temp_dir().join("stress_timer_io.tmp");
    let file = Arc::new(Mutex::new(OpenOptions::new().create(true).write(true).open(&tmp_path).unwrap()));

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
            }
        })
    };

    let timer = thread::spawn(move || {
        while start.elapsed() < Duration::from_secs(RUN_SECS) {
            thread::sleep(Duration::from_millis(1));
        }
    });

    writer.join().unwrap();
    timer.join().unwrap();
}
