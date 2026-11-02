//! Wake the UI for input or worker results without redrawing on a polling loop.
use crossterm::event::{self, Event};
use std::{
    io,
    sync::{Arc, Condvar, Mutex, mpsc::Sender},
    thread,
    time::Duration,
};

pub enum Wake {
    Terminal(io::Result<Event>),
    Worker,
}

#[derive(Default)]
struct State {
    paused: bool,
    active: bool,
    stopped: bool,
}

pub struct EventReader {
    state: Arc<(Mutex<State>, Condvar)>,
    handle: Option<thread::JoinHandle<()>>,
}

impl EventReader {
    pub fn new(send: Sender<Wake>) -> Self {
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let shared = state.clone();
        let handle = thread::spawn(move || {
            let (lock, changed) = &*shared;
            loop {
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                while state.paused && !state.stopped {
                    state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                if state.stopped {
                    break;
                }
                state.active = true;
                drop(state);
                // The bounded poll lets Drop join this thread without injecting
                // terminal input. It does not cause rendering or backend reads.
                let input = match event::poll(Duration::from_millis(100)) {
                    Ok(true) => Some(event::read()),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                };
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                state.active = false;
                changed.notify_all();
                drop(state);
                if let Some(input) = input {
                    let failed = input.is_err();
                    if send.send(Wake::Terminal(input)).is_err() || failed {
                        break;
                    }
                }
            }
        });
        Self {
            state,
            handle: Some(handle),
        }
    }

    /// Wait for any read to finish before giving stdin to an external editor.
    pub fn pause(&self) -> Paused<'_> {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.paused = true;
        while state.active {
            state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        Paused(self)
    }
}

pub struct Paused<'a>(&'a EventReader);
impl Drop for Paused<'_> {
    fn drop(&mut self) {
        let (lock, changed) = &*self.0.state;
        lock.lock().unwrap_or_else(|e| e.into_inner()).paused = false;
        changed.notify_all();
    }
}

impl Drop for EventReader {
    fn drop(&mut self) {
        let (lock, changed) = &*self.state;
        lock.lock().unwrap_or_else(|e| e.into_inner()).stopped = true;
        changed.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
