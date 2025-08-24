use crate::executor::Executor;
use std::cell::RefCell;

thread_local! {
    pub(crate) static EXECUTOR: RefCell<Option<*const Executor>> = RefCell::new(None);
}

pub fn with_executor<F, R>(f: F) -> R
where
    F: FnOnce(&Executor) -> R,
{
    EXECUTOR.with(|executor| {
        let executor = executor.borrow();
        let executor = executor.as_ref().expect("Not in a runtime context");
        f(unsafe { &**executor })
    })
}
