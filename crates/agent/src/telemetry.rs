use std::{
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Instant,
};

use tokio::sync::Mutex;

pub(crate) struct Counters {
    pub(crate) load_stage_index: AtomicU32,
    pub(crate) desired_virtual_clients: AtomicU32,
    pub(crate) included_in_results: AtomicBool,
    pub(crate) attempted: AtomicU64,
    pub(crate) established: AtomicU64,
    pub(crate) failed: AtomicU64,
    active: StdMutex<ActiveWindow>,
    pub(crate) transactions: AtomicU64,
    pub(crate) transaction_errors: AtomicU64,
    pub(crate) timeout_errors: AtomicU64,
    pub(crate) reset_errors: AtomicU64,
    pub(crate) tls_handshake_errors: AtomicU64,
    pub(crate) proxy_connect_errors: AtomicU64,
    pub(crate) http_error_responses: AtomicU64,
    pub(crate) tx: AtomicU64,
    pub(crate) rx: AtomicU64,
    pub(crate) packets_tx: AtomicU64,
    pub(crate) packets_rx: AtomicU64,
    pub(crate) wire_tx_bytes: AtomicU64,
    pub(crate) wire_rx_bytes: AtomicU64,
    pub(crate) tcp_retransmissions: AtomicU64,
    pub(crate) tcp_connect_latencies_us: Mutex<Vec<u64>>,
    pub(crate) http_latencies_us: Mutex<Vec<u64>>,
}

struct ActiveWindow {
    current: u64,
    min: u64,
    max: u64,
    weighted_nanos: u128,
    window_started: Instant,
    last_change: Instant,
}

impl Default for ActiveWindow {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            current: 0,
            min: 0,
            max: 0,
            weighted_nanos: 0,
            window_started: now,
            last_change: now,
        }
    }
}

impl ActiveWindow {
    fn account_until(&mut self, now: Instant) {
        self.weighted_nanos +=
            now.saturating_duration_since(self.last_change).as_nanos() * self.current as u128;
        self.last_change = now;
    }
}

#[allow(clippy::derivable_impls)]
impl Default for Counters {
    fn default() -> Self {
        Self {
            load_stage_index: Default::default(),
            desired_virtual_clients: Default::default(),
            included_in_results: Default::default(),
            attempted: Default::default(),
            established: Default::default(),
            failed: Default::default(),
            active: Default::default(),
            transactions: Default::default(),
            transaction_errors: Default::default(),
            timeout_errors: Default::default(),
            reset_errors: Default::default(),
            tls_handshake_errors: Default::default(),
            proxy_connect_errors: Default::default(),
            http_error_responses: Default::default(),
            tx: Default::default(),
            rx: Default::default(),
            packets_tx: Default::default(),
            packets_rx: Default::default(),
            wire_tx_bytes: Default::default(),
            wire_rx_bytes: Default::default(),
            tcp_retransmissions: Default::default(),
            tcp_connect_latencies_us: Default::default(),
            http_latencies_us: Default::default(),
        }
    }
}

pub(crate) struct ActiveConnection<'a>(&'a Counters);

impl Counters {
    pub(crate) fn connection_established(&self) {
        self.established.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn transaction_completed(&self) {
        self.transactions.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_failure(&self, error: &anyhow::Error) {
        let message = format!("{error:#}").to_ascii_lowercase();
        if message.contains("deadline has elapsed") || message.contains("timed out") {
            self.timeout_errors.fetch_add(1, Ordering::Relaxed);
        }
        if message.contains("connection reset") || message.contains("forcibly closed") {
            self.reset_errors.fetch_add(1, Ordering::Relaxed);
        }
        if message.contains("tls handshake failed") {
            self.tls_handshake_errors.fetch_add(1, Ordering::Relaxed);
        }
        if message.contains("http connect failed") {
            self.proxy_connect_errors.fetch_add(1, Ordering::Relaxed);
        }
        if message.contains("http error response") {
            self.http_error_responses.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn connection_opened(&self) -> ActiveConnection<'_> {
        let now = Instant::now();
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.account_until(now);
        active.current += 1;
        active.min = active.min.min(active.current);
        active.max = active.max.max(active.current);
        ActiveConnection(self)
    }

    pub(crate) fn active_snapshot(&self) -> (u64, f64, u64, u64) {
        let now = Instant::now();
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.account_until(now);
        let elapsed = now
            .saturating_duration_since(active.window_started)
            .as_nanos()
            .max(1);
        let result = (
            active.current,
            active.weighted_nanos as f64 / elapsed as f64,
            active.min,
            active.max,
        );
        active.weighted_nanos = 0;
        active.window_started = now;
        active.min = active.current;
        active.max = active.current;
        result
    }
}

impl Drop for ActiveConnection<'_> {
    fn drop(&mut self) {
        let now = Instant::now();
        let mut active = self
            .0
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.account_until(now);
        active.current = active.current.saturating_sub(1);
        active.min = active.min.min(active.current);
    }
}
