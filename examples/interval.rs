use rust_miniss::timer::Interval;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let mut interval = Interval::new(Duration::from_secs(1));
    for _ in 0..5 {
        interval.tick().await;
        println!("Tick");
    }
}
