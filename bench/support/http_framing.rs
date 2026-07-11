const HEADER_END: &[u8; 4] = b"\r\n\r\n";
const MAX_PENDING_REQUEST_BYTES: usize = 64 * 1024;

pub struct RequestFramer {
    pending: Vec<u8>,
}

impl RequestFramer {
    pub fn new() -> Self {
        Self {
            pending: Vec::with_capacity(4096),
        }
    }

    /// Append one transport read and return the number of complete HTTP
    /// headers. Bodies are intentionally unsupported by this echo workload.
    pub fn push(&mut self, bytes: &[u8]) -> Result<usize, ()> {
        if self.pending.len().saturating_add(bytes.len()) > MAX_PENDING_REQUEST_BYTES {
            return Err(());
        }
        self.pending.extend_from_slice(bytes);

        let mut complete = 0;
        let mut consumed = 0;
        while let Some(relative_end) = self.pending[consumed..]
            .windows(HEADER_END.len())
            .position(|window| window == HEADER_END)
        {
            consumed += relative_end + HEADER_END.len();
            complete += 1;
        }
        if consumed != 0 {
            self.pending.drain(..consumed);
        }
        Ok(complete)
    }
}

#[cfg(test)]
mod tests {
    use super::RequestFramer;

    #[test]
    fn fragmented_header_is_counted_only_after_terminator_arrives() {
        let mut framer = RequestFramer::new();
        assert_eq!(framer.push(b"GET / HTTP/1.1\r\nHost: local").unwrap(), 0);
        assert_eq!(framer.push(b"host\r\n\r\n").unwrap(), 1);
    }

    #[test]
    fn coalesced_headers_are_counted_individually() {
        let mut framer = RequestFramer::new();
        assert_eq!(
            framer
                .push(b"GET / HTTP/1.1\r\n\r\nGET / HTTP/1.1\r\n\r\n")
                .unwrap(),
            2
        );
    }
}
