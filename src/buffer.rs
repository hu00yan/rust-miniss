// Simplified buffer module - just use Vec<u8> directly
// As per docs/BUFFER_SIMPLIFICATION.md, removed complex buffer pool

pub type Buffer = Vec<u8>;
