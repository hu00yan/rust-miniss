//! Signal handling module
//!
//! This module provides signal handling for graceful shutdown.
//! It uses nix for signal handling (already a dependency).
//!
//! Sends `CoreMessage::Shutdown` to each CPU core's message inbox.

use crossbeam_channel::Sender;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use nix::sys::signal::Signal;

use crate::multicore::CoreMessage;

pub type SignalCallback = Arc<dyn Fn(i32) + Send + Sync>;

/// Signal handler that manages graceful shutdown on signals
pub struct SignalHandler {
    shutdown_flag: Arc<AtomicBool>,
    cpu_senders: Vec<Sender<CoreMessage>>,
    core_shutdowns: Vec<Arc<AtomicBool>>,
    callbacks: HashMap<i32, Vec<SignalCallback>>,
}

impl SignalHandler {
    pub fn new(shutdown_flag: Arc<AtomicBool>) -> Self {
        Self {
            shutdown_flag,
            cpu_senders: Vec::new(),
            core_shutdowns: Vec::new(),
            callbacks: HashMap::new(),
        }
    }

    pub fn with_cpu_handles(
        shutdown_flag: Arc<AtomicBool>,
        cpu_senders: Vec<Sender<CoreMessage>>,
        core_shutdowns: Vec<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            shutdown_flag,
            cpu_senders,
            core_shutdowns,
            callbacks: HashMap::new(),
        }
    }

    pub fn register_callback<F>(&mut self, signal: i32, callback: F)
    where
        F: Fn(i32) + Send + Sync + 'static,
    {
        self.callbacks
            .entry(signal)
            .or_default()
            .push(Arc::new(callback));
    }

    pub fn start(&self) {
        let shutdown_flag = self.shutdown_flag.clone();
        let cpu_senders = self.cpu_senders.clone();
        let core_shutdowns = self.core_shutdowns.clone();
        let callbacks = self.callbacks.clone();

        thread::spawn(move || {
            let mut signals = nix::sys::signal::SigSet::empty();
            signals.add(Signal::SIGINT);
            signals.add(Signal::SIGTERM);
            signals.add(Signal::SIGHUP);
            signals.thread_block().unwrap();

            loop {
                match signals.wait() {
                    Ok(signal) => {
                        let sig = signal as i32;
                        tracing::info!("Received signal {} for graceful shutdown", sig);

                        if !shutdown_flag.load(Ordering::Acquire) {
                            shutdown_flag.store(true, Ordering::Release);

                            for flag in &core_shutdowns {
                                flag.store(true, Ordering::Release);
                            }
                            for sender in &cpu_senders {
                                let _ = sender.try_send(CoreMessage::Shutdown);
                            }

                            if let Some(signal_callbacks) = callbacks.get(&sig) {
                                for callback in signal_callbacks {
                                    callback(sig);
                                }
                            }

                            tracing::info!("Graceful shutdown initiated");
                        } else {
                            tracing::warn!("Received second signal, forcing exit");
                            std::process::exit(1);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Signal wait error: {:?}", e);
                        break;
                    }
                }
            }
        });
    }
}
