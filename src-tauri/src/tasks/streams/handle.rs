use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::task::{Context, Waker};
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
      log::error!("Failed to persist task {} state: {}", self.desc.task_id, error);
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
    self.persist();
    self.reporter.report_progress(
      self.desc.task_id,
      self.desc.task_group.as_deref(),
      self.desc.current,
    );
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
