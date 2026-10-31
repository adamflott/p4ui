//! A shared, cancellation-aware bound on live P4 child processes.
use anyhow::{Result, ensure};
use std::{
    num::NonZeroUsize,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Debug)]
struct State {
    active: usize,
    maximum: usize,
}

#[derive(Debug)]
pub(crate) struct ProcessLimit {
    state: Mutex<State>,
    available: Condvar,
}

impl Default for ProcessLimit {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                active: 0,
                maximum: 4,
            }),
            available: Condvar::new(),
        }
    }
}

impl ProcessLimit {
    pub(crate) fn set_maximum(&self, maximum: NonZeroUsize) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).maximum = maximum.get();
        self.available.notify_all();
    }
    pub(crate) fn acquire(&self, cancelled: &AtomicBool) -> Result<Permit<'_>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            ensure!(
                !cancelled.load(Ordering::Relaxed),
                "Perforce request cancelled"
            );
            if state.active < state.maximum {
                state.active += 1;
                return Ok(Permit(self));
            }
            // A tab switch can cancel a queued request without waiting for a
            // running command to finish and notify the condition variable.
            state = self
                .available
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

pub(crate) struct Permit<'a>(&'a ProcessLimit);
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
        self.0.available.notify_one();
    }
}
