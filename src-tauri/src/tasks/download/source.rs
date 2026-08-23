use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceFailureKind {
  Connection,
  Timeout,
  Http,
  RateLimited,
  Range,
  Transfer,
  Integrity,
}

#[derive(Clone, Copy, Debug)]
struct CandidateHealth {
  failures: u32,
  cooldown_until: Option<Instant>,
  latency_ms: f64,
  bytes_per_second: f64,
}

impl Default for CandidateHealth {
  fn default() -> Self {
    Self {
      failures: 0,
      cooldown_until: None,
      latency_ms: 0.0,
      bytes_per_second: 0.0,
    }
  }
}

/// File-local source state.
///
/// A tracker belongs to exactly one target file. This is deliberate: a burst
/// of responses for one MOD must never reset the speed sample or preferred
/// source of another MOD in the same task group.
pub(crate) struct FileSourceTracker {
  candidates: Vec<String>,
  health: HashMap<String, CandidateHealth>,
  preferred: Option<String>,
  cursor: usize,
}

impl FileSourceTracker {
  pub(crate) fn new(candidates: Vec<String>) -> Self {
    let mut seen = HashSet::new();
    let candidates = candidates
      .into_iter()
      .filter(|candidate| seen.insert(candidate.clone()))
      .collect();
    Self {
      candidates,
      health: HashMap::new(),
      preferred: None,
      cursor: 0,
    }
  }

  pub(crate) fn ordered_candidates(&self, offset: usize) -> Vec<String> {
    if self.candidates.is_empty() {
      return Vec::new();
    }

    let now = Instant::now();
    let start = (self.cursor + offset) % self.candidates.len();
    let mut ordered = (0..self.candidates.len())
      .map(|step| self.candidates[(start + step) % self.candidates.len()].clone())
      .filter(|candidate| {
        self
          .health
          .get(candidate)
          .and_then(|health| health.cooldown_until)
          .is_none_or(|until| until <= now)
      })
      .collect::<Vec<_>>();

    // When every candidate is cooling down, retry them in cursor order rather
    // than clearing all health state and returning to the same failed URL.
    if ordered.is_empty() {
      ordered = (0..self.candidates.len())
        .map(|step| self.candidates[(start + step) % self.candidates.len()].clone())
        .collect();
      ordered.sort_by_key(|candidate| {
        self
          .health
          .get(candidate)
          .and_then(|health| health.cooldown_until)
      });
    }

    if offset == 0
      && let Some(preferred) = self.preferred.as_ref()
      && let Some(index) = ordered.iter().position(|candidate| candidate == preferred)
    {
      ordered.swap(0, index);
    }
    ordered
  }

  pub(crate) fn wait_before_retry(&self, candidate: &str) -> Duration {
    self
      .health
      .get(candidate)
      .and_then(|health| health.cooldown_until)
      .and_then(|until| until.checked_duration_since(Instant::now()))
      .unwrap_or_default()
  }

  pub(crate) fn record_response(&mut self, candidate: &str, latency: Duration) {
    let health = self.health.entry(candidate.to_string()).or_default();
    let latency_ms = latency.as_secs_f64() * 1000.0;
    health.latency_ms = if health.latency_ms == 0.0 {
      latency_ms
    } else {
      health.latency_ms * 0.7 + latency_ms * 0.3
    };
    health.failures = health.failures.saturating_sub(1);
    health.cooldown_until = None;
    self.preferred = Some(candidate.to_string());
    if let Some(index) = self.candidates.iter().position(|value| value == candidate) {
      self.cursor = index;
    }
  }

  pub(crate) fn record_transfer(&mut self, candidate: &str, bytes: i64, elapsed: Duration) {
    if bytes <= 0 || elapsed.is_zero() {
      return;
    }
    let health = self.health.entry(candidate.to_string()).or_default();
    let speed = bytes as f64 / elapsed.as_secs_f64();
    health.bytes_per_second = if health.bytes_per_second == 0.0 {
      speed
    } else {
      health.bytes_per_second * 0.7 + speed * 0.3
    };
  }

  pub(crate) fn record_failure(&mut self, candidate: &str, kind: SourceFailureKind) {
    let health = self.health.entry(candidate.to_string()).or_default();
    health.failures = health.failures.saturating_add(1);
    let base = match kind {
      SourceFailureKind::Connection | SourceFailureKind::Transfer => Duration::from_secs(2),
      SourceFailureKind::Timeout => Duration::from_secs(4),
      SourceFailureKind::Http => Duration::from_secs(6),
      SourceFailureKind::RateLimited => Duration::from_secs(20),
      SourceFailureKind::Range => Duration::from_secs(30),
      SourceFailureKind::Integrity => Duration::from_secs(60),
    };
    let multiplier = 1u32 << health.failures.saturating_sub(1).min(3);
    health.cooldown_until = Some(Instant::now() + base.saturating_mul(multiplier));

    if self.preferred.as_deref() == Some(candidate) {
      self.preferred = None;
    }
    if let Some(index) = self.candidates.iter().position(|value| value == candidate) {
      self.cursor = (index + 1) % self.candidates.len().max(1);
    }
  }

  pub(crate) fn reset_for_recovery(&mut self) {
    self.preferred = None;
    self.cursor = 0;
    for health in self.health.values_mut() {
      health.failures = 0;
      health.cooldown_until = None;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn failure_advances_to_a_different_candidate() {
    let mut tracker = FileSourceTracker::new(vec!["a".into(), "b".into(), "c".into()]);
    assert_eq!(tracker.ordered_candidates(0)[0], "a");
    tracker.record_failure("a", SourceFailureKind::Timeout);
    assert_eq!(tracker.ordered_candidates(0)[0], "b");
  }

  #[test]
  fn successful_candidate_stays_preferred_until_it_fails() {
    let mut tracker = FileSourceTracker::new(vec!["a".into(), "b".into()]);
    tracker.record_response("b", Duration::from_millis(40));
    assert_eq!(tracker.ordered_candidates(0)[0], "b");
    tracker.record_failure("b", SourceFailureKind::Transfer);
    assert_eq!(tracker.ordered_candidates(0)[0], "a");
  }

  #[test]
  fn duplicate_candidates_are_removed_without_reordering() {
    let tracker = FileSourceTracker::new(vec!["a".into(), "b".into(), "a".into()]);
    assert_eq!(tracker.ordered_candidates(0), vec!["a", "b"]);
  }
}
