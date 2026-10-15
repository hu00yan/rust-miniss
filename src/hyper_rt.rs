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
        // Called from the shard that owns the connection. `spawn` would cross
        // cores and then block this shard for up to 1s waiting for the
        // admission ack.
        if crate::cpu::current_core_id().is_some() {
            self.runtime
                .spawn_local(fut)
                .expect("hyper local task admission queue exhausted");
            return;
        }
        // Not on a worker shard (hyper driven from `block_on` on the main
        // thread): the cross-core path is the only option there.
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
