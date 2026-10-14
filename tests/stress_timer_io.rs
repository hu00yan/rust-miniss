use rust_miniss::{fs::AsyncFile, timer, Runtime};
use std::time::Duration;

#[test]
fn stress_timer_io() {
    let runtime = Runtime::new();
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let path = temp_dir.path().join("timer-io.bin");

    runtime.block_on(async {
        let file = AsyncFile::create(&path).expect("create async file");
        for i in 0..32u64 {
            let data = i.to_le_bytes();
            assert_eq!(
                file.write_at(i * 8, &data).await.expect("write"),
                data.len()
            );
            timer::sleep(Duration::from_millis(1)).await;
        }
        file.sync_all().await.expect("sync");
    });

    assert_eq!(std::fs::metadata(path).expect("metadata").len(), 32 * 8);
}
