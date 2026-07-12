#[cfg(feature = "tls")]
mod tls_tests {
    #[test]
    fn test_tls_compiles() {
        static_assertions::assert_not_impl_any!(rust_miniss::tls::TlsStream: Send, Sync);
    }
}
