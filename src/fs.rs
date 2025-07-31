use std::os::unix::io::RawFd;
use std::path::Path;
use std::result::Result;
use std::sync::Arc;

use crate::cpu::Cpu;
use crate::io::{IoFuture, IoBackend, IoError};
use crate::error::Result as RuntimeResult;

pub struct File {
    fd: RawFd,
    cpu: usize, // CPU ID where the file is managed
}

impl File {
    pub async fn open(path: &Path, flags: i32) -> RuntimeResult<File> {
        let path_str = path.to_str().unwrap();
        let cpu_id = crate::multicore::runtime().next_cpu();

        let cpu = crate::multicore::runtime().cpu_handles.as_ref().unwrap()[cpu_id].clone();
        let backend = Arc::new(crate::io::DummyIoBackend::new()); // This should be your actual IoBackend

        let fd = IoFuture::open(backend, path_str, flags, 0o644).await?;

        Ok(File { fd, cpu: cpu_id })
    }

    pub async fn read(&self, buf: &mut [u8], offset: u64) -> RuntimeResult<usize> {
        let backend = Arc::new(crate::io::DummyIoBackend::new()); // Replace with your actual IoBackend

        IoFuture::read_at(backend, self.fd, buf, offset).await.map_err(IoError::from)
    }

    pub async fn write(&self, buf: &[u8], offset: u64) -> RuntimeResult<usize> {
        let backend = Arc::new(crate::io::DummyIoBackend::new()); // Replace with your actual IoBackend

        IoFuture::write_at(backend, self.fd, buf, offset).await.map_err(IoError::from)
    }

    pub async fn close(self) -> RuntimeResult<()> {
        let backend = Arc::new(crate::io::DummyIoBackend::new()); // Replace with your actual IoBackend

        IoFuture::close(backend, self.fd).await.map(|_| ()).map_err(IoError::from)
    }
}

use std::os::unix::io::RawFd;
use std::path::Path;
use std::result::Result;

// Assuming IoFuture and CpuId types exist, these would be defined elsewhere
//use some_module::{IoFuture, CpuId};

pub struct File {
    fd: RawFd,
    cpu: CpuId,
}

impl File {
    pub async fn open(path: &Path, flags: i32) -> Result<File> {
        // Implementation using IoFuture
    }

    pub async fn read(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        // Implementation using IoFuture
    }

    pub async fn write(&self, buf: &[u8], offset: u64) -> Result<usize> {
        // Implementation using IoFuture
    }

    pub async fn close(self) -> Result<()> {
        // Implementation using IoFuture
    }
}
