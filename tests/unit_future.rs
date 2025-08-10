use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

// Custom future that transitions Pending -> Pending -> Ready with a value
struct TwoStepFuture {
    state: u8,
}

impl TwoStepFuture {
    fn new() -> Self {
        Self { state: 0 }
    }
}

impl Future for TwoStepFuture {
    type Output = u8;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.state {
            0 => {
                self.state = 1;
                Poll::Pending
            }
            1 => {
                self.state = 2;
                Poll::Pending
            }
            _ => Poll::Ready(42),
        }
    }
}

fn noop_waker() -> Waker {
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    fn wake(_: *const ()) {}
    fn wake_by_ref(_: *const ()) {}
    fn drop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

#[test]
fn custom_future_state_transitions() {
    let mut fut = TwoStepFuture::new();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // Pin the future on stack
    let mut fut = unsafe { Pin::new_unchecked(&mut fut) };

    // First poll -> Pending
    assert!(matches!(Future::poll(fut.as_mut(), &mut cx), Poll::Pending));
    // Second poll -> Pending
    assert!(matches!(Future::poll(fut.as_mut(), &mut cx), Poll::Pending));
    // Third poll -> Ready(42)
    assert!(matches!(
        Future::poll(fut.as_mut(), &mut cx),
        Poll::Ready(42)
    ));
}
