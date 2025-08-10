use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// Multicore task distribution: use tokio multi-threaded test runtime with 4 threads
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multicore_task_distribution() {
    let counters = Arc::new([
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ]);

    let tasks: Vec<_> = (0..1000)
        .map(|i| {
            let counters = counters.clone();
            tokio::spawn(async move {
                // hash the thread id to an index 0..=3
                let tid = std::thread::current().id();
                // There is no stable way to map ThreadId->usize; hash its Debug repr
                let mut hasher = DefaultHasher::new();
                format!("{:?}", tid).hash(&mut hasher);
                let idx = (hasher.finish() % 4) as usize;
                counters[idx].fetch_add(1, Ordering::Relaxed);
                i
            })
        })
        .collect();

    let mut _sum = 0usize;
    for t in tasks {
        _sum += t.await.unwrap();
    }

    let counts: Vec<_> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    // Assert at least two worker threads observed work; OS scheduler may keep tasks on fewer threads.
    let active = counts.iter().filter(|&&c| c > 0).count();
    assert!(
        active >= 2,
        "expected >=2 active workers, got {} with counts {:?}",
        active,
        counts
    );
}
