//! Build script for rust-miniss
//!
//! This build script detects the target platform and kernel version to determine 
//! which IO backend to enable. The selection logic is as follows:
//!
//! ## Platform-specific IO Backend Selection
//!
//! - **Linux with kernel 5.10+**: Enables `io_uring` backend for optimal performance
//! - **Linux with older kernels**: Falls back to `epoll` backend  
//! - **macOS**: Enables `kqueue` backend
//! - **Other Unix systems**: Enables `epoll` backend
//!
//! ## Assumptions
//!
//! - The compilation machine and runtime machine are the same (reasonable for most use cases)
//! - Kernel version 5.10+ is considered stable and feature-complete for io_uring
//!
//! ## Configuration Flags
//!
//! This build script sets the `io_backend` configuration flag which is used by
//! conditional compilation attributes throughout the codebase.

use std::process::Command;

fn main() {
    // Declare custom cfg conditions to avoid warnings
    println!("cargo::rustc-check-cfg=cfg(io_backend, values(\"io_uring\", \"epoll\", \"kqueue\"))");
    
    // Only run on Linux targets
    if cfg!(target_os = "linux") {
        match get_kernel_version() {
            Ok(version) => {
                eprintln!("Detected kernel version: {:?}", version);
                // io_uring is considered stable and feature-complete from kernel 5.10+
                // This is a LTS version with good production usage reports
                if version >= (5, 10, 0) {
                    // Check if io_uring is actually available in the kernel
                    if has_io_uring_support() {
                        eprintln!("Enabling io_uring backend");
                        println!("cargo:rustc-cfg=has_io_uring");
                        println!("cargo:rustc-cfg=io_backend=\"io_uring\"");
                    } else {
                        // Fallback to epoll if io_uring is not available despite kernel version
                        eprintln!("Falling back to epoll backend (io_uring support check failed)");
                        println!("cargo:rustc-cfg=io_backend=\"epoll\"");
                    }
                } else {
                    // For older kernels, use epoll
                    eprintln!("Falling back to epoll backend (kernel version < 5.10)");
                    println!("cargo:rustc-cfg=io_backend=\"epoll\"");
                }
            }
            Err(e) => {
                // If we can't determine the kernel version, default to epoll for safety
                eprintln!("Failed to determine kernel version: {}, falling back to epoll", e);
                println!("cargo:rustc-cfg=io_backend=\"epoll\"");
            }
        }
    }
    
    // For macOS, always use kqueue
    #[cfg(target_os = "macos")]
    {
        eprintln!("Enabling kqueue backend (macOS)");
        println!("cargo:rustc-cfg=io_backend=\"kqueue\"");
    }
    
    // For other Unix systems, use epoll as fallback
    #[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
    {
        eprintln!("Enabling epoll backend (other Unix)");
        println!("cargo:rustc-cfg=io_backend=\"epoll\"");
    }
}

/// Gets the Linux kernel version as a tuple of (major, minor, patch)
fn get_kernel_version() -> Result<(u32, u32, u32), Box<dyn std::error::Error>> {
    let output = Command::new("uname").arg("-r").output()?;
    let version_str = String::from_utf8(output.stdout)?;
    parse_kernel_version(&version_str)
}

/// Parses a kernel version string like "5.10.0-8-generic" into (5, 10, 0)
fn parse_kernel_version(version_str: &str) -> Result<(u32, u32, u32), Box<dyn std::error::Error>> {
    let parts: Vec<&str> = version_str.trim().split(|c| c == '.' || c == '-').collect();
    if parts.len() >= 3 {
        let major = parts[0].parse()?;
        let minor = parts[1].parse()?;
        // The patch version might have trailing non-numeric characters, so we extract only digits
        let patch_str = parts[2].split(|c: char| !c.is_ascii_digit()).next().unwrap_or("0");
        let patch = patch_str.parse()?;
        Ok((major, minor, patch))
    } else {
        Err("Invalid version format".into())
    }
}

/// Checks if io_uring is actually supported on this system
/// This is a simple check - in practice, we might want to do a more thorough check
/// by attempting to create an io_uring instance
fn has_io_uring_support() -> bool {
    // For now, we'll assume that if we're on kernel 5.10+, io_uring is available
    // A more robust implementation might actually try to create an io_uring instance
    true
}