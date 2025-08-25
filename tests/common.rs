use std::sync::Once;

static INIT: Once = Once::new();

/// Sets up the tracing subscriber for tests, ensuring it's only initialized once.
pub fn setup_tracing() {
    INIT.call_once(|| {
        tracing_subscriber::fmt::init();
    });
}
