//! One shared circuit for the finance service. Do not hold the lock across network I/O.
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct Circuit(Arc<Mutex<State>>);

struct State {
    phase: Phase,
    failures: u32,
    threshold: u32,
    cooldown: Duration,
    generation: u64,
}

enum Phase {
    Closed,
    Open(Instant),
    HalfOpen,
}

impl State {
    fn label(&self) -> &'static str {
        match self.phase {
            Phase::Closed => "closed",
            Phase::Open(_) => "open",
            Phase::HalfOpen => "half_open",
        }
    }

    fn change(&mut self, phase: Phase) {
        let previous = self.label();
        self.generation = self.generation.wrapping_add(1);
        self.phase = phase;
        tracing::info!(previous, circuit = self.label(), "finance circuit changed");
    }
}

impl Circuit {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self(Arc::new(Mutex::new(State {
            phase: Phase::Closed,
            failures: 0,
            threshold,
            cooldown,
            generation: 0,
        })))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn label(&self) -> &'static str {
        self.state().label()
    }

    pub fn admit(&self) -> Option<Permit> {
        let mut state = self.state();
        match state.phase {
            Phase::Closed => {}
            Phase::Open(since) if since.elapsed() >= state.cooldown => {
                state.change(Phase::HalfOpen)
            }
            _ => return None,
        }
        Some(Permit {
            circuit: self.clone(),
            generation: state.generation,
            is_trial: matches!(state.phase, Phase::HalfOpen),
            completed: false,
        })
    }
}

pub(crate) struct Permit {
    circuit: Circuit,
    generation: u64,
    pub is_trial: bool,
    completed: bool,
}

impl Permit {
    pub fn active(&self) -> bool {
        self.circuit.state().generation == self.generation
    }

    pub fn finish(mut self, reachable: bool) {
        self.completed = true;
        let mut state = self.circuit.state();
        if state.generation != self.generation {
            return; // A different request already moved the circuit to a new generation.
        }
        if reachable {
            state.failures = 0;
            if self.is_trial {
                state.change(Phase::Closed);
            }
        } else {
            state.failures = state.failures.saturating_add(1);
            if self.is_trial || state.failures >= state.threshold {
                state.change(Phase::Open(Instant::now()));
            }
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        // A disconnected caller or the outer web deadline can cancel a half-open attempt.
        if self.is_trial && !self.completed {
            let mut state = self.circuit.state();
            if state.generation == self.generation {
                state.change(Phase::Open(Instant::now()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_requests_cannot_close_a_new_circuit_and_abandoned_trial_releases_the_slot() {
        let circuit = Circuit::new(1, Duration::ZERO);
        let late_completion = circuit.admit().unwrap();
        circuit.admit().unwrap().finish(false);
        assert_eq!(circuit.label(), "open");
        late_completion.finish(true);
        assert_eq!(circuit.label(), "open");
        let trial = circuit.admit().unwrap();
        assert!(trial.is_trial && circuit.admit().is_none());
        drop(trial);
        assert_eq!(circuit.label(), "open");
        circuit.admit().unwrap().finish(true);
        assert_eq!(circuit.label(), "closed");
    }
}
