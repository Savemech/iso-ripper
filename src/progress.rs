use std::sync::atomic::{AtomicU64, Ordering};

pub struct Progress {
    pub total: u64,
    completed: AtomicU64,
    failed: AtomicU64,
}

impl Progress {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }

    pub fn report_ok(&self, name: &str, detail: &str) {
        let n = self.completed.fetch_add(1, Ordering::Relaxed) + 1;
        let f = self.failed.load(Ordering::Relaxed);
        eprintln!("[{}/{}] OK  {} {}", n + f, self.total, name, detail);
    }

    pub fn report_err(&self, name: &str, err: &anyhow::Error) {
        let f = self.failed.fetch_add(1, Ordering::Relaxed) + 1;
        let n = self.completed.load(Ordering::Relaxed);
        eprintln!("[{}/{}] ERR {} -- {:#}", n + f, self.total, name, err);
    }

    pub fn summary(&self) -> (u64, u64) {
        (
            self.completed.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }
}
