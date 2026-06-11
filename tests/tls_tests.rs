#[cfg(feature = "tls")]
mod tls_tests {
    #[test]
    fn test_tls_compiles() {
        fn _assert_send<T: Send>() {}
        _assert_send::<rust_miniss::tls::TlsStream>();
    }
}
