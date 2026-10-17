//! Build script for rust-miniss
//!
//! This build script detects the target platform and kernel version to determine
//! which IO backend to enable. The selection logic is as follows:
//!
//! ## Platform-specific IO Backend Selection
//!
//! - **Linux with kernel 6.12+**: Enables the `io_uring` backend.
//! - **Linux with an older kernel**: Falls back to the `epoll` backend. The
//!   io_uring features the runtime depends on (multi-shot accept/recv, provided
//!   buffer ring increments, min-timeout) do not exist below 6.12, so the
//!   fallback is the supported choice rather than a build failure.
//! - **macOS**: Enables the `kqueue` backend (development only, not optimized).
//!   `epoll` and `kqueue` are the same backend source (`src/io/mio_poll.rs`).
//! - **Other Unix systems**: Unsupported (the runtime targets Linux and macOS).
//!
//! ## Overriding the probe
//!
//! Set `MINISS_IO_BACKEND` to `io_uring`, `epoll` or `kqueue` to build a specific
//! backend on a host whose kernel would otherwise select another one. This is
//! what makes the fallback backend testable on a modern kernel: without it,
//! `epoll` could never be built on a Linux host, and its code would go
//! unverified. The requested backend must be valid for the target OS.
//!
//! ## Assumptions
//!
//! - The compilation machine and runtime machine are the same (reasonable for
//!   most use cases), which is also what makes the kernel probe meaningful.
//!
//! ## Configuration Flags
//!
//! This build script sets the `io_backend` configuration flag which is used by
//! conditional compilation attributes throughout the codebase.

use std::process::Command;

/// The I/O backend this build compiles.
enum Backend {
    IoUring,
    Epoll,
    Kqueue,
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::IoUring => "io_uring",
            Backend::Epoll => "epoll",
            Backend::Kqueue => "kqueue",
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=MINISS_LOOM");
    println!("cargo:rerun-if-env-changed=MINISS_IO_BACKEND");
    // Emit check-cfg hints so `cfg(io_backend = "...")` is accepted by the compiler
    println!("cargo:rustc-check-cfg=cfg(io_backend, values(\"io_uring\", \"epoll\", \"kqueue\"))");
    println!("cargo:rustc-check-cfg=cfg(has_io_uring)");
    println!("cargo:rustc-check-cfg=cfg(loom)");
    if std::env::var_os("MINISS_LOOM").is_some() {
        // This cfg is emitted only for rust-miniss.  RUSTFLAGS would also
        // enable it in dependencies such as Tokio, changing their APIs.
        println!("cargo:rustc-cfg=loom");
    }

    let backend = match std::env::var("MINISS_IO_BACKEND") {
        Ok(requested) if !requested.is_empty() => requested_backend(&requested),
        _ => probed_backend(),
    };

    if matches!(backend, Backend::IoUring) {
        println!("cargo:rustc-cfg=has_io_uring");
    }
    println!("cargo:rustc-cfg=io_backend=\"{}\"", backend.name());
    eprintln!("Selected IO backend: {}", backend.name());
}

/// The backend named by `MINISS_IO_BACKEND`, validated against the target OS.
fn requested_backend(requested: &str) -> Backend {
    match requested {
        "io_uring" if cfg!(target_os = "linux") => Backend::IoUring,
        "epoll" if cfg!(target_os = "linux") => Backend::Epoll,
        "kqueue" if cfg!(target_os = "macos") => Backend::Kqueue,
        "io_uring" | "epoll" => panic!(
            "MINISS_IO_BACKEND={requested} is Linux-only; the io_uring and epoll backends do not \
             exist on this target"
        ),
        "kqueue" => panic!("MINISS_IO_BACKEND=kqueue is macOS-only"),
        other => {
            panic!("unknown MINISS_IO_BACKEND={other}; expected io_uring, epoll or kqueue")
        }
    }
}

/// The backend selected by probing the platform.
fn probed_backend() -> Backend {
    if cfg!(target_os = "linux") {
        return match get_kernel_version() {
            Ok(version) => {
                eprintln!("Detected kernel version: {version:?}");
                if version >= (6, 12, 0) {
                    Backend::IoUring
                } else {
                    eprintln!(
                        "Linux kernel {}.{}.{} predates 6.12: selecting the epoll backend. \
                         io_uring support starts at 6.12.",
                        version.0, version.1, version.2
                    );
                    Backend::Epoll
                }
            }
            Err(e) => {
                eprintln!(
                    "Failed to determine kernel version: {e}. Selecting the epoll backend; \
                     io_uring requires Linux kernel 6.12+."
                );
                Backend::Epoll
            }
        };
    }

    if cfg!(target_os = "macos") {
        eprintln!("Enabling kqueue backend (macOS development only)");
        return Backend::Kqueue;
    }

    if cfg!(unix) {
        panic!("unsupported Unix target: rust-miniss currently supports Linux and macOS only");
    }

    panic!("unsupported target: rust-miniss currently supports Linux and macOS only");
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
