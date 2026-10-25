use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::task::{Context, Waker};
use std::time::Instant;
use tokio::time::{Duration, Interval, interval};

use crate::tasks::streams::desc::{PDesc, PStatus};
use crate::tasks::streams::reporter::{Reporter, Sink};

pub struct PHandle<S, P>
where
  S: Sink,
  P: Clone + Serialize + for<'de> Deserialize<'de>,
{
  pub interval: Interval,
  pub desc: PDesc<P>,
  pub path: PathBuf,
  pub reporter: Reporter<S>,
  pub waker: Option<Waker>,
  last_direct_report: Instant,
  last_direct_persist: Instant,
}

impl<S, P> PHandle<S, P>
where
  S: Sink,
  P: Clone + Serialize + for<'de> Deserialize<'de>,
{
  pub fn new(desc: PDesc<P>, duration: Duration, path: PathBuf, reporter: Reporter<S>) -> Self {
    Self {
      interval: interval(duration),
      desc,
      path,
      reporter,
      waker: None,
      last_direct_report: Instant::now(),
      last_direct_persist: Instant::now(),
    }
  }

  pub fn mark_stopped(&mut self) {
    self.desc.stop();
    self.persist();
    self
      .reporter
      .report_stopped(self.desc.task_id, self.desc.task_group.as_deref());
  }

  pub fn mark_resumed(&mut self) {
    self.desc.resume();
    self.persist();
    self.reporter.report_started(
      self.desc.task_id,
      self.desc.task_group.as_deref(),
      self.desc.total,
    );

    // Wake up any waiting poll_next calls
    if let Some(waker) = self.waker.take() {
      waker.wake();
    }
  }

  pub fn mark_cancelled(&mut self) {
    if self.desc.status.is_cancelled() {
      return;
    }
    self.desc.cancel();
    self.persist();
    // Terminal tasks no longer need to be restored on restart; remove the
    // saved descriptor so stale entries do not pile up in the cache.
    let _ = std::fs::remove_file(&self.path);
    self
      .reporter
      .report_cancelled(self.desc.task_id, self.desc.task_group.as_deref());
  }

  pub fn mark_completed(&mut self) {
    self.desc.complete();
    self.persist();
    // Terminal tasks no longer need to be restored on restart; remove the
    // saved descriptor so stale entries do not pile up in the cache.
    let _ = std::fs::remove_file(&self.path);
    self
      .reporter
      .report_completion(self.desc.task_id, self.desc.task_group.as_deref());
  }

  pub fn mark_started(&mut self) {
    self.desc.start();
    self.persist();
    self.reporter.report_started(
      self.desc.task_id,
      self.desc.task_group.as_deref(),
      self.desc.total,
    );
  }

  pub fn mark_failed(&mut self, reason: String) {
    if self.desc.status.is_cancelled() || self.desc.status.is_completed() {
      return;
    }
    self.desc.fail();
    self.persist();
    // Terminal tasks no longer need to be restored on restart; remove the
    // saved descriptor so stale entries do not pile up in the cache.
    let _ = std::fs::remove_file(&self.path);
    self
      .reporter
      .report_failed(self.desc.task_id, self.desc.task_group.as_deref(), reason);
  }

  pub fn status(&self) -> &PStatus {
    &self.desc.status
  }

  fn persist(&self) {
    if let Err(error) = self.desc.save(&self.path) {
      log::error!(
        "Failed to persist task {} state: {}",
        self.desc.task_id,
        error
      );
    }
  }

  pub fn store_waker(&mut self, waker: Waker) {
    self.waker = Some(waker);
  }

  pub fn set_total(&mut self, total: i64) {
    if total > self.desc.total {
      self.desc.total = total;
      self.persist();
      self.reporter.set_total(total);
      // The response headers can reveal the size after the task has started.
      // Refresh the frontend's total before subsequent byte-progress events.
      if self.desc.status.is_in_progress() {
        self
          .reporter
          .report_started(self.desc.task_id, self.desc.task_group.as_deref(), total);
      }
    }
  }

  pub fn report_progress(&mut self, cx: &mut Context<'_>, incr: i64) {
    self.desc.increment_progress(incr);
    if self.interval.poll_tick(cx).is_ready() {
      self.persist();
      self.reporter.report_progress(
        self.desc.task_id,
        self.desc.task_group.as_deref(),
        self.desc.current,
      );
    }
  }

  /// Report progress from code that is not polling a `ProgressStream`.
  /// Segmented downloads have several response streams, so the first
  /// completed segment must not mark the whole task as completed.
  pub fn report_progress_now(&mut self, incr: i64) {
    if incr <= 0 {
      return;
    }
    self.desc.increment_progress(incr);
    if self.last_direct_persist.elapsed() >= Duration::from_secs(2) {
      self.persist();
      self.last_direct_persist = Instant::now();
    }
    if self.last_direct_report.elapsed() >= Duration::from_millis(250) {
      self.reporter.report_progress(
        self.desc.task_id,
        self.desc.task_group.as_deref(),
        self.desc.current,
      );
      self.last_direct_report = Instant::now();
    }
  }

  /// Align persisted progress with the bytes already present in a segmented
  /// download cache. This prevents a restarted task from reporting progress
  /// from an earlier strategy (for example, single-stream resume) twice.
  pub fn set_current(&mut self, current: i64) {
    self.desc.current = current.max(0);
    self.persist();
    self.reporter.report_progress(
      self.desc.task_id,
      self.desc.task_group.as_deref(),
      self.desc.current,
    );
  }
}

#[cfg(test)]
mod tests {
  use crate::tasks::streams::desc::{PDesc, PStatus};
  use crate::tasks::streams::handle::PHandle;
  use crate::tasks::streams::reporter::{Reporter, Sink};
  use std::sync::{Arc, Mutex};
  use std::time::Duration;

  struct RecordingSink(Arc<Mutex<Vec<(&'static str, i64)>>>);

  impl Sink for RecordingSink {
    fn report_progress(
      &self,
      _: u32,
      _: Option<&str>,
      _: i64,
      total: i64,
      _: u32,
      _: Option<f64>,
      _: f64,
    ) {
      self.0.lock().unwrap().push(("progress", total));
    }
    fn report_started(&self, _: u32, _: Option<&str>, total: i64) {
      self.0.lock().unwrap().push(("started", total));
    }
    fn report_completion(&self, _: u32, _: Option<&str>) {}
    fn report_stopped(&self, _: u32, _: Option<&str>) {}
    fn report_cancelled(&self, _: u32, _: Option<&str>) {}
    fn report_failed(&self, _: u32, _: Option<&str>, _: String) {}
  }

  #[tokio::test]
  async fn response_size_is_reported_before_byte_progress_without_resuming_paused_tasks() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let desc = PDesc::new(1, Some("test".to_string()), -1, (), PStatus::Waiting);
    let duration = Duration::from_millis(250);
    let reporter = Reporter::new(-1, duration, RecordingSink(events.clone()));
    let path = std::env::temp_dir().join(format!("bgumcl-progress-{}.json", uuid::Uuid::new_v4()));
    let mut handle = PHandle::new(desc, duration, path.clone(), reporter);
    handle.mark_started();
    handle.set_total(1_000);
    handle.set_total(1_000);
    handle.set_current(250);
    assert_eq!(
      *events.lock().unwrap(),
      vec![("started", -1), ("started", 1_000), ("progress", 1_000)]
    );
    handle.mark_stopped();
    handle.set_total(2_000);
    assert_eq!(events.lock().unwrap().len(), 3);
    handle.mark_resumed();
    assert_eq!(events.lock().unwrap().last(), Some(&("started", 2_000)));
    std::fs::remove_file(path).unwrap();
  }
}
