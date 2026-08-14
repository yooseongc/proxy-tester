use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub(crate) mod http1;
pub(crate) mod http2;
pub(crate) mod tcp;

pub(crate) struct WorkerGate<'a> {
    pub(crate) running: &'a AtomicBool,
    pub(crate) paused: &'a AtomicBool,
    pub(crate) generating: &'a AtomicBool,
    pub(crate) desired_clients: &'a AtomicU32,
    pub(crate) worker_index: u32,
}

impl WorkerGate<'_> {
    pub(crate) fn enabled(&self) -> bool {
        self.running.load(Ordering::Relaxed)
            && self.generating.load(Ordering::Relaxed)
            && self.worker_index < self.desired_clients.load(Ordering::Relaxed)
    }
}
