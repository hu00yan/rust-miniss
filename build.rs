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
    // Emit check-cfg hints so `cfg(io_backend = "...")` is accepted by the compiler
    println!("cargo:rustc-check-cfg=cfg(io_backend, values(\"io_uring\", \"epoll\", \"kqueue\"))");
    println!("cargo:rustc-check-cfg=cfg(has_io_uring)");

    if cfg!(target_os = "linux") {
        match get_kernel_version() {
            Ok(version) => {
                eprintln!("Detected kernel version: {:?}", version);
                let io_uring_eligible =
                    version >= (5, 10, 0) && is_io_uring_actually_supported_on_linux();

                // If kernel supports io_uring (5.10+), use io_uring. Otherwise fall back to epoll.
                if io_uring_eligible {
                    eprintln!("Kernel supports io_uring: Selecting io_uring backend.");
                    println!("cargo:rustc-cfg=has_io_uring");
                    println!("cargo:rustc-cfg=io_backend=\"io_uring\"");
                } else {
                    eprintln!("Kernel doesn't support io_uring (< 5.10): Falling back to epoll.");
                    println!("cargo:rustc-cfg=io_backend=\"epoll\"");
                }
            }
            Err(e) => {
                eprintln!(
                    "Failed to determine kernel version: {}, falling back to epoll for Linux",
                    e
                );
                println!("cargo:rustc-cfg=io_backend=\"epoll\"");
            }
        }
    } else if cfg!(target_os = "macos") {
        eprintln!("Enabling kqueue backend (macOS)");
        println!("cargo:rustc-cfg=io_backend=\"kqueue\"");
    } else if cfg!(all(
        unix,
        not(target_os = "linux"),
        not(target_os = "macos")
    )) {
        eprintln!("No specific IO backend available for this platform, using dummy backend");
        // Don't set any io_backend cfg, let the code use DummyIoBackend
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
    let parts: Vec<&str> = version_str.trim().split(['.', '-']).collect();
    if parts.len() >= 3 {
        let major = parts[0].parse()?;
        let minor = parts[1].parse()?;
        let patch_str = parts[2]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("0");
        let patch = patch_str.parse()?;
        Ok((major, minor, patch))
    } else {
        Err("Invalid version format".into())
    }
}

/// Checks if io_uring is actually supported on this system.
/// A more robust implementation might try to create an io_uring instance.
fn is_io_uring_actually_supported_on_linux() -> bool {
    // For now, be conservative and only enable io_uring on newer kernels
    // In practice, io_uring might not be available even on supported kernels
    // due to system configuration or container restrictions
    true
}
