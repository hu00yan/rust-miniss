use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::thread;

// SPSC queue correctness under stress
#[test]
fn spsc_stress() {
    let q = Arc::new(ArrayQueue::new(1024));
    let iterations = 200_000;

    let producer = {
        let q = q.clone();
        thread::spawn(move || {
            for i in 0..iterations {
                loop {
                    if q.push(i).is_ok() {
                        break;
                    }
                    // busy-wait with small yield
                    std::thread::yield_now();
                }
            }
        })
    };

    let consumer = {
        let q = q.clone();
        thread::spawn(move || {
            let mut count = 0usize;
            let mut expected = 0usize;
            while count < iterations {
                if let Some(v) = q.pop() {
                    assert_eq!(v, expected);
                    expected += 1;
                    count += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        })
    };

    producer.join().unwrap();
    consumer.join().unwrap();

    // Ensure queue is empty
    assert!(q.is_empty());
}
