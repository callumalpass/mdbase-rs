use std::sync::{Condvar, Mutex, MutexGuard};

use super::{OperationContext, ProviderError};

/// Cancellation-aware, writer-preferring provider gate.
pub(crate) struct RuntimeGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    active_readers: usize,
    writer_active: bool,
    waiting_writers: usize,
}

impl RuntimeGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn read(
        &self,
        context: &OperationContext,
    ) -> Result<RuntimeReadGuard<'_>, ProviderError> {
        let mut state = self.lock_state()?;
        loop {
            context.check()?;
            if !state.writer_active && state.waiting_writers == 0 {
                state.active_readers = state
                    .active_readers
                    .checked_add(1)
                    .ok_or(ProviderError::LockPoisoned)?;
                return Ok(RuntimeReadGuard { gate: self });
            }
            state = self.wait(state, context)?;
        }
    }

    pub(crate) fn write(
        &self,
        context: &OperationContext,
    ) -> Result<RuntimeWriteGuard<'_>, ProviderError> {
        let mut state = self.lock_state()?;
        state.waiting_writers = state
            .waiting_writers
            .checked_add(1)
            .ok_or(ProviderError::LockPoisoned)?;
        loop {
            if let Err(error) = context.check() {
                state.waiting_writers -= 1;
                self.changed.notify_all();
                return Err(error);
            }
            if !state.writer_active && state.active_readers == 0 {
                state.waiting_writers -= 1;
                state.writer_active = true;
                return Ok(RuntimeWriteGuard { gate: self });
            }
            match self.wait(state, context) {
                Ok(next) => state = next,
                Err(error) => {
                    if let Ok(mut recovered) = self.state.lock() {
                        recovered.waiting_writers = recovered.waiting_writers.saturating_sub(1);
                        self.changed.notify_all();
                    }
                    return Err(error);
                }
            }
        }
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, GateState>, ProviderError> {
        self.state.lock().map_err(|_| ProviderError::LockPoisoned)
    }

    fn wait<'a>(
        &self,
        state: MutexGuard<'a, GateState>,
        context: &OperationContext,
    ) -> Result<MutexGuard<'a, GateState>, ProviderError> {
        let duration = context.next_wait()?;
        self.changed
            .wait_timeout(state, duration)
            .map(|(state, _)| state)
            .map_err(|_| ProviderError::LockPoisoned)
    }

    fn release_read(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.active_readers = state.active_readers.saturating_sub(1);
        if state.active_readers == 0 {
            self.changed.notify_all();
        }
    }

    fn release_write(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.writer_active = false;
        self.changed.notify_all();
    }
}

pub(crate) struct RuntimeReadGuard<'a> {
    gate: &'a RuntimeGate,
}

impl Drop for RuntimeReadGuard<'_> {
    fn drop(&mut self) {
        self.gate.release_read();
    }
}

pub(crate) struct RuntimeWriteGuard<'a> {
    gate: &'a RuntimeGate,
}

impl Drop for RuntimeWriteGuard<'_> {
    fn drop(&mut self) {
        self.gate.release_write();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OperationCancellation, OperationStopReason};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn context(cancellation: &OperationCancellation, duration: Duration) -> OperationContext {
        OperationContext::new(
            cancellation,
            super::super::OperationDeadline::after(duration),
        )
    }

    #[test]
    fn waiting_writer_honors_explicit_cancellation() {
        let gate = Arc::new(RuntimeGate::new());
        let held_cancellation = OperationCancellation::new();
        let held_context = context(&held_cancellation, Duration::from_secs(1));
        let _held = gate.write(&held_context).unwrap();

        let cancellation = OperationCancellation::new();
        let waiting_context = context(&cancellation, Duration::from_secs(1));
        let waiting_gate = gate.clone();
        let handle = thread::spawn(move || waiting_gate.write(&waiting_context).map(drop));
        thread::sleep(Duration::from_millis(30));
        cancellation.cancel();

        assert!(matches!(
            handle.join().unwrap(),
            Err(ProviderError::OperationCancelled)
        ));
    }

    #[test]
    fn waiting_reader_honors_deadline() {
        let gate = Arc::new(RuntimeGate::new());
        let held_cancellation = OperationCancellation::new();
        let held_context = context(&held_cancellation, Duration::from_secs(1));
        let _held = gate.write(&held_context).unwrap();

        let cancellation = OperationCancellation::new();
        let waiting_context = context(&cancellation, Duration::from_millis(20));
        let started = Instant::now();
        assert!(matches!(
            gate.read(&waiting_context),
            Err(ProviderError::OperationDeadline)
        ));
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(
            waiting_context.cancellation().stop_reason(),
            Some(OperationStopReason::Deadline)
        );
    }

    #[test]
    fn waiting_writer_prevents_new_readers_from_starving_it() {
        let gate = Arc::new(RuntimeGate::new());
        let cancellation = OperationCancellation::new();
        let active_context = context(&cancellation, Duration::from_secs(1));
        let active_read = gate.read(&active_context).unwrap();

        let writer_gate = gate.clone();
        let writer = thread::spawn(move || {
            let cancellation = OperationCancellation::new();
            let context = context(&cancellation, Duration::from_secs(1));
            writer_gate.write(&context).map(|_guard| ())
        });
        thread::sleep(Duration::from_millis(30));

        let reader_cancellation = OperationCancellation::new();
        let reader_context = context(&reader_cancellation, Duration::from_millis(30));
        assert!(matches!(
            gate.read(&reader_context),
            Err(ProviderError::OperationDeadline)
        ));
        drop(active_read);
        assert!(writer.join().unwrap().is_ok());
    }
}
