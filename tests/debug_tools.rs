#[cfg(all(test, feature = "debug"))]
mod debug_tests {
    use super::super::lib::submit_debug;  // Assume
    use tracing_test::traced_test;

    #[traced_test]
    #[tokio::test]
    async fn test_tracing_spans() {
        // Submit op, assert span logged
        let op = Op::Read { fd: 3, offset: 0, len: 1024 };
        submit_debug(op);
        // Assert trace output contains "io_submit"
    }
}

// Valgrind test (manual, CI)
#[test]
fn valgrind_suppressions() {
    // Run with valgrind --suppressions=valgrind.supp cargo test
    // Assert no leaks/races
}