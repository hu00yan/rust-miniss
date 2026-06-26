//! Build script for rust-miniss
//!
//! This build script detects the target platform and kernel version to determine
//! which IO backend to enable. The selection logic is as follows:
//!
//! ## Platform-specific IO Backend Selection
//!
//! - **Linux with kernel 6.12+**: Enables `io_uring` backend
//! - **Linux with older kernels**: **Error** — kernel must be 6.12+
//! - **macOS**: Enables `kqueue` backend (development only, not optimized)
//! - **Other Unix systems**: Enables `epoll` backend (development only, not optimized)
//!
//! ## Assumptions
//!
//! - The compilation machine and runtime machine are the same (reasonable for most use cases)
//! - Kernel version 6.12+ is required for io_uring
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
                eprintln!("Detected kernel version: {version:?}");
                if version >= (6, 12, 0) {
                    eprintln!("Kernel 6.12+ detected: selecting io_uring backend.");
                    println!("cargo:rustc-cfg=has_io_uring");
                    println!("cargo:rustc-cfg=io_backend=\"io_uring\"");
                } else {
                    panic!(
                        "Linux kernel {}.{}.{} is too old. rust-miniss requires kernel 6.12+ for io_uring support.",
                        version.0, version.1, version.2
                    );
                }
            }
            Err(e) => {
                panic!(
                    "Failed to determine kernel version: {e}. rust-miniss requires Linux kernel 6.12+."
                );
            }
        }
    } else if cfg!(target_os = "macos") {
        eprintln!("Enabling kqueue backend (macOS development only)");
        println!("cargo:rustc-cfg=io_backend=\"kqueue\"");
    } else if cfg!(all(
        unix,
        not(target_os = "linux"),
        not(target_os = "macos")
    )) {
        eprintln!("Enabling epoll backend (other Unix, development only)");
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
