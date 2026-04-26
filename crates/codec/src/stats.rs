use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Default)]
pub struct CodecStats {
    processed: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
    backpressure: Arc<AtomicU64>,
    last_nanos: Arc<AtomicU64>,
    window: Arc<Mutex<WindowState>>,
}

#[derive(Debug, Clone)]
struct WindowState {
    samples: VecDeque<(Instant, u64)>,
    max: usize,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
            max: DEFAULT_WINDOW,
        }
    }
}

const DEFAULT_WINDOW: usize = 120;

impl CodecStats {
    pub fn inc_processed(&self) {
        self.processed.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_errors(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_backpressure(&self) {
        self.backpressure.fetch_add(1, Ordering::Relaxed);
    }
    pub fn processed(&self) -> u64 {
        self.processed.load(Ordering::Relaxed)
    }
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
    pub fn backpressure(&self) -> u64 {
        self.backpressure.load(Ordering::Relaxed)
    }
    pub fn samples(&self) -> u64 {
        self.window
            .lock()
            .map(|w| w.samples.len() as u64)
            .unwrap_or(0)
    }
    pub fn record_duration(&self, dur: Duration) {
        let nanos = dur.as_nanos().min(u64::MAX as u128) as u64;
        self.last_nanos.store(nanos, Ordering::Relaxed);
        if let Ok(mut win) = self.window.lock() {
            if win.max == 0 {
                win.max = DEFAULT_WINDOW;
            }
            win.samples.push_back((Instant::now(), nanos));
            while win.samples.len() > win.max {
                win.samples.pop_front();
            }
        }
    }
    pub fn set_window_size(&self, window: usize) {
        let window = window.max(1);
        if let Ok(mut win) = self.window.lock() {
            win.max = window;
            while win.samples.len() > win.max {
                win.samples.pop_front();
            }
        }
    }
    pub fn avg_millis(&self) -> Option<f64> {
        self.window.lock().ok().and_then(|w| {
            let count = w.samples.len();
            if count == 0 {
                return None;
            }
            let total: u128 = w.samples.iter().map(|(_, n)| *n as u128).sum();
            Some(total as f64 / 1_000_000.0 / count as f64)
        })
    }
    pub fn last_millis(&self) -> Option<f64> {
        let last = self.last_nanos.load(Ordering::Relaxed);
        if last == 0 {
            None
        } else {
            Some(last as f64 / 1_000_000.0)
        }
    }
    pub fn fps(&self) -> Option<f64> {
        self.window.lock().ok().and_then(|w| {
            if w.samples.len() < 2 {
                return None;
            }
            let first = w.samples.front()?.0;
            let last = w.samples.back()?.0;
            let span = last.saturating_duration_since(first).as_secs_f64();
            if span > 0.0 {
                Some(w.samples.len() as f64 / span)
            } else {
                None
            }
        })
    }
}
