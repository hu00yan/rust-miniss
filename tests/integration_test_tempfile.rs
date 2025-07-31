#[cfg(test)]
mod integration_tests {
    use tempfile::tempfile;
    use std::io::{Read, Write, Seek, SeekFrom};
    use crc::{Crc, CRC_32_ISO_HDLC};

    #[test]
    fn test_tempfile_read_write_crc32() {
        let mut temp_file = tempfile().unwrap();

        let data = b"Hello, async world!";
        temp_file.write_all(data).unwrap();
        
        // Verify crc32
        let crc = Crc::<u32>::new(&CRC_32_ISO_HDLC);
        let expected_crc32 = crc.checksum(data);

        // Read from the beginning
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        let mut buffer = Vec::new();
        temp_file.read_to_end(&mut buffer).unwrap();

        assert_eq!(buffer, data);
        assert_eq!(crc.checksum(&buffer), expected_crc32);
    }
}

