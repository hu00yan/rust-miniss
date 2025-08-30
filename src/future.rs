//! Custom Future/Promise implementation
//!
//! This module provides the core Future and Promise types that form
//! the foundation of our async runtime.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// A future that can be completed by its corresponding Promise
pub struct Future<T> {
    shared: Arc<Mutex<SharedState<T>>>,
}

/// A promise that can complete a Future
pub struct Promise<T> {
    shared: Arc<Mutex<SharedState<T>>>,
}

/// Shared state between Future and Promise
struct SharedState<T> {
    completed: bool,
    result: Option<T>,
    waker: Option<Waker>,
}

impl<T> Future<T> {
    /// Create a new Future/Promise pair
    pub fn new() -> (Future<T>, Promise<T>) {
        let shared = Arc::new(Mutex::new(SharedState {
            completed: false,
            result: None,
            waker: None,
        }));

        let future = Future {
            shared: shared.clone(),
        };

        let promise = Promise { shared };

        (future, promise)
    }

    /// Check if the future is ready without polling
    pub fn is_ready(&self) -> bool {
        self.shared.lock().unwrap().completed
    }
}

impl<T> std::future::Future for Future<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut shared = self.shared.lock().unwrap();

        if shared.completed {
            // Take the result (this can only happen once)
            let result = shared
                .result
                .take()
                .expect("Future polled after completion");
            Poll::Ready(result)
        } else {
            // Store the waker for later notification
            shared.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl<T> Promise<T> {
    /// Complete the future with a value
    pub fn complete(self, value: T) {
        let mut shared = self.shared.lock().unwrap();

        if shared.completed {
            panic!("Promise already completed");
        }

        shared.completed = true;
        shared.result = Some(value);

        // Wake the future if it's waiting
        if let Some(waker) = shared.waker.take() {
            waker.wake();
        }
    }

    /// Check if the promise has been completed
    pub fn is_completed(&self) -> bool {
        self.shared.lock().unwrap().completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future as StdFuture;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};

    // Helper to create a dummy waker
    fn dummy_waker() -> Waker {
        use std::task::RawWaker;
        use std::task::RawWakerVTable;

        fn dummy_clone(_: *const ()) -> RawWaker {
            dummy_raw_waker()
        }
        fn dummy_wake(_: *const ()) {}
        fn dummy_wake_by_ref(_: *const ()) {}
        fn dummy_drop(_: *const ()) {}

        fn dummy_raw_waker() -> RawWaker {
            RawWaker::new(
                std::ptr::null(),
                &RawWakerVTable::new(dummy_clone, dummy_wake, dummy_wake_by_ref, dummy_drop),
            )
        }

        unsafe { Waker::from_raw(dummy_raw_waker()) }
    }

    #[test]
    fn test_future_promise_basic() {
        let (mut future, promise) = Future::new();

        // Future should not be ready initially
        assert!(!future.is_ready());

        // Poll should return Pending
        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Pending
        ));

        // Complete the promise
        promise.complete(42);

        // Now future should be ready
        assert!(future.is_ready());

        // Poll should return Ready with the value
        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Ready(42)
        );
    }

    #[test]
    fn test_waker_notification() {
        let (mut future, promise) = Future::new();
        let woken = Arc::new(AtomicBool::new(false));

        // Create a waker that sets a flag when woken
        let woken_clone = woken.clone();
        let waker = {
            use std::task::RawWaker;
            use std::task::RawWakerVTable;

            fn wake_clone(data: *const ()) -> RawWaker {
                let woken = unsafe { Arc::from_raw(data as *const AtomicBool) };
                let cloned = woken.clone();
                std::mem::forget(woken); // Don't drop the original
                RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
            }

            fn wake(data: *const ()) {
                let woken = unsafe { Arc::from_raw(data as *const AtomicBool) };
                woken.store(true, Ordering::SeqCst);
            }

            fn wake_by_ref(data: *const ()) {
                let woken = unsafe { &*(data as *const AtomicBool) };
                woken.store(true, Ordering::SeqCst);
            }

            fn drop_fn(data: *const ()) {
                unsafe { Arc::from_raw(data as *const AtomicBool) };
            }

            static VTABLE: RawWakerVTable =
                RawWakerVTable::new(wake_clone, wake, wake_by_ref, drop_fn);

            let raw = RawWaker::new(Arc::into_raw(woken_clone) as *const (), &VTABLE);
            unsafe { Waker::from_raw(raw) }
        };

        // Poll future (should register waker)
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Pending
        ));

        // Waker should not be called yet
        assert!(!woken.load(Ordering::SeqCst));

        // Complete the promise
        promise.complete(100);

        // Waker should now be called
        assert!(woken.load(Ordering::SeqCst));
    }

    #[test]
    fn test_double_completion_protection() {
        // Since Promise moves on complete, we can't test double completion directly
        // But we can test that a promise can only be completed once by design
        let (_future, promise) = Future::<i32>::new();
        assert!(!promise.is_completed());
        promise.complete(42);
        // Promise is consumed/moved, so this test just verifies the API design
    }

    #[test]
    fn test_promise_completion_status() {
        let (_future, promise) = Future::<i32>::new();
        assert!(!promise.is_completed());
        promise.complete(42);
        // Can't check after completion since promise is moved
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use std::future::Future as StdFuture;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Waker};
    use std::thread;

    // Helper to create a dummy waker
    fn dummy_waker() -> Waker {
        use std::task::RawWaker;
        use std::task::RawWakerVTable;

        fn dummy_clone(_: *const ()) -> RawWaker {
            dummy_raw_waker()
        }
        fn dummy_wake(_: *const ()) {}
        fn dummy_wake_by_ref(_: *const ()) {}
        fn dummy_drop(_: *const ()) {}

        fn dummy_raw_waker() -> RawWaker {
            RawWaker::new(
                std::ptr::null(),
                &RawWakerVTable::new(dummy_clone, dummy_wake, dummy_wake_by_ref, dummy_drop),
            )
        }

        unsafe { Waker::from_raw(dummy_raw_waker()) }
    }

    #[test]
    fn test_concurrent_complete() {
        let (mut future, promise) = Future::<i32>::new();
        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        let poller = thread::spawn(move || {
            let waker = dummy_waker();
            let mut cx = Context::from_waker(&waker);

            // Poll until the future is ready
            loop {
                match StdFuture::poll(Pin::new(&mut future), &mut cx) {
                    Poll::Ready(val) => {
                        assert_eq!(val, 99);
                        completed_clone.store(true, Ordering::SeqCst);
                        break;
                    }
                    Poll::Pending => thread::yield_now(),
                }
            }
        });

        let completer = thread::spawn(move || {
            // Wait a bit to ensure the poller has started polling
            thread::yield_now();
            promise.complete(99);
        });

        completer.join().unwrap();
        poller.join().unwrap();

        // Verify that the future was completed
        assert!(completed.load(Ordering::SeqCst));
    }

    #[test]
    fn test_dropped_promise() {
        let (mut future, promise) = Future::<i32>::new();

        // Drop the promise
        drop(promise);

        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);

        // The future should remain pending forever
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Pending
        ));

        // Polling again should not deadlock or panic
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut future), &mut cx),
            Poll::Pending
        ));
    }
}
