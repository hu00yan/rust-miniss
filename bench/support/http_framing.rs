const HEADER_END: &[u8; 4] = b"\r\n\r\n";
const MAX_PENDING_REQUEST_BYTES: usize = 64 * 1024;

pub struct RequestFramer {
    pending: Vec<u8>,
    pending_start: usize,
}

impl RequestFramer {
    pub fn new() -> Self {
        Self {
            pending: Vec::with_capacity(4096),
            pending_start: 0,
        }
    }

    fn pending_len(&self) -> usize {
        self.pending.len().saturating_sub(self.pending_start)
    }

    fn prepare_append(&mut self, additional: usize) -> Result<(), ()> {
        if self.pending_len().saturating_add(additional) > MAX_PENDING_REQUEST_BYTES {
            return Err(());
        }
        if self.pending.capacity().saturating_sub(self.pending.len()) < additional
            && self.pending_start != 0
        {
            let pending_len = self.pending_len();
            self.pending.copy_within(self.pending_start.., 0);
            self.pending.truncate(pending_len);
            self.pending_start = 0;
        }
        Ok(())
    }

    /// Append one transport read and return the number of complete HTTP
    /// headers. Bodies are intentionally unsupported by this echo workload.
    pub fn push(&mut self, bytes: &[u8]) -> Result<usize, ()> {
        if self.pending_len() == 0 {
            self.pending.clear();
            self.pending_start = 0;
            let mut complete = 0;
            let mut consumed = 0;
            while let Some(relative_end) = bytes[consumed..]
                .windows(HEADER_END.len())
                .position(|window| window == HEADER_END)
            {
                consumed += relative_end + HEADER_END.len();
                complete += 1;
            }
            let trailing = &bytes[consumed..];
            if trailing.len() > MAX_PENDING_REQUEST_BYTES {
                return Err(());
            }
            self.pending.extend_from_slice(trailing);
            return Ok(complete);
        }
        self.prepare_append(bytes.len())?;
        self.pending.extend_from_slice(bytes);

        let mut complete = 0;
        let mut consumed = self.pending_start;
        while let Some(relative_end) = self.pending[consumed..]
            .windows(HEADER_END.len())
            .position(|window| window == HEADER_END)
        {
            consumed += relative_end + HEADER_END.len();
            complete += 1;
        }
        if consumed == self.pending.len() {
            self.pending.clear();
            self.pending_start = 0;
        } else {
            self.pending_start = consumed;
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

    #[test]
    fn consumed_prefix_is_not_copied_before_a_fragmented_trailer() {
        let mut framer = RequestFramer::new();
        assert_eq!(framer.push(b"GET / HTTP/1.1\r\n\r\npartial"), Ok(1));
        assert_eq!(framer.push(b"-request"), Ok(0));
        assert_eq!(framer.push(b"\r\n\r\n"), Ok(1));
    }
}
