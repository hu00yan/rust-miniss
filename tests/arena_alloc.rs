#[cfg(all(test, feature = "arena"))]
mod arena_tests {
    use super::super::http::HttpRequestContext;
    use bumpalo::Bump;

    #[test]
    fn test_http_arena_headers() {
        let arena = Bump::new();
        let mut ctx = HttpRequestContext::new(&arena);

        ctx.parse_header("Host", "localhost:8080");
        ctx.parse_header("User-Agent", "curl/7.68.0");

        assert_eq!(ctx.headers.len(), 2);
        assert_eq!(ctx.headers[0].0, "Host");
        assert_eq!(ctx.headers[0].1, "localhost:8080");

        // Reset arena (no leak)
        arena.reset();
        assert_eq!(arena.chunk_capacity(), 0);  // Cleared
    }

    #[test]
    #[should_panic(expected = "bumpalo alloc after reset")]
    fn test_arena_leak_panic() {
        let arena = Bump::new();
        let mut ctx = HttpRequestContext::new(&arena);
        ctx.parse_header("Test", "value");
        arena.reset();
        ctx.parse_header("Leak", "test");  // Panic on alloc post-reset
    }
}

#[cfg(not(feature = "arena"))]
mod arena_tests {
    #[test]
    fn test_no_arena() {
        // Fallback Vec test (assume implemented)
        // ...
    }
}