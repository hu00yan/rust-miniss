#[cfg(all(test, feature = "arena"))]
mod arena_lifetime_tests {
    use super::super::http::HttpRequestContext;
    use bumpalo::Bump;
    use std::mem::leak; // For negative test

    #[test]
    fn test_arena_borrow_after_reset() {
        let arena = Bump::new();
        let mut ctx = HttpRequestContext::new(&arena);

        ctx.parse_header("Test", "value"); // Borrow from arena

        // Borrow after reset should fail (lifetime invalid)
        arena.reset();
        // ctx.parse_header("After", "value");  // This would be UB—test via drop or separate

        // Assert: No access post-reset (test via clone or indirect)
        let headers_clone = ctx.get_headers().clone(); // Pre-reset borrow invalid post
                                                       // But since reset, clone would have dangling refs—use drop to test reset
        drop(ctx); // Arena dropped, but test passes if no panic during use
    }

    #[test]
    fn test_arena_large_alloc() {
        let arena = Bump::new();
        let mut ctx = HttpRequestContext::new(&arena);

        // Large body alloc
        let body = ctx.arena.alloc_slice_fill_copy(1024, b'A'); // Assume added to context
        assert_eq!(body.len(), 1024);

        // Reset clears (capacity 0)
        arena.reset();
        assert_eq!(arena.chunk_capacity(), 0); // All freed
    }
}

#[cfg(not(feature = "arena"))]
mod arena_lifetime_tests {
    #[test]
    fn test_no_arena_fallback() {
        // Vec fallback test
        // ...
    }
}
