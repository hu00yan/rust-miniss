#[cfg(test)]
mod buffered_overflow_tests {
    #[test]
    fn buffer_capacity_grows_without_length_overflow() {
        let mut buffer = rust_miniss::Buffer::with_capacity(4);

        buffer.resize(8, 0xaa);

        assert_eq!(buffer.len(), 8);
        assert!(buffer.capacity() >= 8);
        assert!(buffer.iter().all(|byte| *byte == 0xaa));
    }
}
