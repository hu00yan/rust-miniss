use crate::multicore::MultiCoreRuntime;
use crate::timer::SleepFuture;
use hyper::rt::{Executor, Sleep, Timer};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct MinissHyperExecutor {
    runtime: Arc<MultiCoreRuntime>,
}

impl MinissHyperExecutor {
    pub fn new(runtime: Arc<MultiCoreRuntime>) -> Self {
        Self { runtime }
    }
}

impl<F> Executor<F> for MinissHyperExecutor
where
    F: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, fut: F) {
        self.runtime
            .spawn(fut)
            .expect("hyper background task spawn failed");
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MinissHyperTimer;

impl Timer for MinissHyperTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>> {
        Box::pin(SleepFuture::new(duration))
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Sleep>> {
        Box::pin(SleepFuture::new(
            deadline.saturating_duration_since(Instant::now()),
        ))
    }

    fn reset(&self, sleep: &mut Pin<Box<dyn Sleep>>, new_deadline: Instant) {
        *sleep = self.sleep_until(new_deadline);
    }
}

impl Sleep for SleepFuture {}
