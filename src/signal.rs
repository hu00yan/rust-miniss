//! Signal handling module
//!
//! This module provides signal handling for graceful shutdown using the signal-hook crate.
//! It spawns a dedicated thread that listens for SIGINT, SIGTERM, and SIGHUP signals.
//! On the first signal:
//! - The shutdown flag is set to true
//! - CrossCpuMessage::Shutdown is broadcast to all CPUs
//! - User-defined signal callbacks are executed

use crossbeam_channel::Sender;
use signal_hook::{consts::signal::*, iterator::Signals};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crate::cpu::{CpuHandle, CrossCpuMessage};

/// Type for user-defined signal callbacks
pub type SignalCallback = Box<dyn Fn(i32) + Send + Sync>;

/// Signal handler that manages graceful shutdown on signals
pub struct SignalHandler {
    shutdown_flag: Arc<AtomicBool>,
    cpu_handles: Option<Vec<Sender<CrossCpuMessage>>>,
    callbacks: HashMap<i32, Vec<SignalCallback>>,
}

impl SignalHandler {
    /// Create a new SignalHandler with just a shutdown flag
    pub fn new(shutdown_flag: Arc<AtomicBool>) -> Self {
        Self {
            shutdown_flag,
            cpu_handles: None,
            callbacks: HashMap::new(),
        }
    }

    /// Create a new SignalHandler with CPU handles for broadcasting shutdown
    pub fn with_cpu_handles(shutdown_flag: Arc<AtomicBool>, cpu_handles: &[CpuHandle]) -> Self {
        let senders: Vec<Sender<CrossCpuMessage>> = cpu_handles
            .iter()
            .map(|handle| handle.sender().clone())
            .collect();

        Self {
            shutdown_flag,
            cpu_handles: Some(senders),
            callbacks: HashMap::new(),
        }
    }

    /// Register a callback for a specific signal
    pub fn register_callback<F>(&mut self, signal: i32, callback: F)
    where
        F: Fn(i32) + Send + Sync + 'static,
    {
        self.callbacks
            .entry(signal)
            .or_default()
            .push(Box::new(callback));
    }

    /// Start listening for shutdown signals in a dedicated thread
    pub fn start(&self) {
        let shutdown_flag = self.shutdown_flag.clone();
        let cpu_handles = self.cpu_handles.clone();

        thread::spawn(move || {
            let mut signals =
                Signals::new([SIGINT, SIGTERM, SIGHUP]).expect("Failed to setup signal handler");

            for signal in signals.forever() {
                match signal {
                    SIGINT | SIGTERM | SIGHUP => {
                        tracing::info!("Received signal {} for graceful shutdown", signal);

                        // On first signal, initiate shutdown
                        if !shutdown_flag.load(Ordering::SeqCst) {
                            // Set shutdown flag
                            shutdown_flag.store(true, Ordering::SeqCst);

                            // Broadcast CrossCpuMessage::Shutdown to all CPUs
                            if let Some(ref handles) = cpu_handles {
                                for (cpu_id, sender) in handles.iter().enumerate() {
                                    if let Err(e) = sender.send(CrossCpuMessage::Shutdown) {
                                        tracing::warn!(
                                            "Failed to send shutdown signal to CPU {}: {}",
                                            cpu_id,
                                            e
                                        );
                                    }
                                }
                            }

                            tracing::info!("Graceful shutdown initiated");
                        } else {
                            tracing::warn!("Received second signal {}, forcing exit", signal);
                            std::process::exit(1);
                        }
                    }
                    _ => unreachable!(),
                }
            }
        });
    }
}
