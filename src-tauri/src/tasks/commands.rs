use sjmcl_types::error::BGUMCLResult;
use std::collections::VecDeque;
use std::pin::Pin;
use std::time::Duration;
use tauri::{AppHandle, Manager};

use crate::tasks::download::DownloadTask;
use crate::tasks::events::GEventStatus;
use crate::tasks::monitor::TaskMonitor;
use crate::tasks::{BGUMCLFutureDesc, PTaskGroupDesc, PTaskParam, THandle};
use crate::utils::fs::extract_filename;

fn interleave_download_origins(params: Vec<PTaskParam>) -> Vec<PTaskParam> {
  let total = params.len();
  let mut buckets: Vec<(String, VecDeque<PTaskParam>)> = Vec::new();
  for param in params {
    let PTaskParam::Download(download) = &param;
    let origin = download
      .src
      .host_str()
      .unwrap_or("local-or-unknown")
      .to_string();
    if let Some((_, bucket)) = buckets.iter_mut().find(|(host, _)| host == &origin) {
      bucket.push_back(param);
    } else {
      buckets.push((origin, VecDeque::from([param])));
    }
  }

  let mut result = Vec::with_capacity(total);
  while result.len() < total {
    for (_, bucket) in &mut buckets {
      if let Some(param) = bucket.pop_front() {
        result.push(param);
      }
    }
  }
  result
}

#[tauri::command]
pub async fn schedule_progressive_task_group(
  app: AppHandle,
  task_group: String,
  params: Vec<PTaskParam>,
  with_timestamp: bool,
) -> BGUMCLResult<PTaskGroupDesc> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  let mut task_descs = Vec::new();
  let mut future_descs = Vec::new();
  let task_group = if with_timestamp {
    // If with_timestamp is true, append a timestamp to the task group name
    // to ensure uniqueness and avoid conflicts.
    let timestamp = chrono::Utc::now().timestamp_millis();
    format!("{task_group}@{timestamp}")
  } else {
    task_group.clone()
  };

  for param in interleave_download_origins(params) {
    let task_id = monitor.get_new_id();
    match param {
      PTaskParam::Download(mut param) => {
        if param.filename.is_none() {
          param.filename = Some(extract_filename(
            param.dest.to_str().unwrap_or_default(),
            true,
          ));
        }
        let task = DownloadTask::new(
          app.clone(),
          task_id,
          Some(task_group.clone()),
          param,
          Duration::from_secs(1),
        );
        let (f, h) = task
          .future(
            app.clone(),
            monitor.download_rate_limiter.clone(),
            monitor.download_connections.clone(),
            monitor.download_request_gate.clone(),
            monitor.download_context(Some(&task_group)),
          )
          .await?;
        let task_desc = h.read().unwrap().desc.clone();
        let future_desc = BGUMCLFutureDesc {
          task_id,
          f: Box::pin(f),
          h: h.clone(),
        };
        task_descs.push(task_desc);
        future_descs.push(future_desc);
      }
    }
  }
  monitor
    .enqueue_task_group(task_group.clone(), future_descs)
    .await;
  Ok(PTaskGroupDesc {
    task_group,
    task_descs,
    status: GEventStatus::Started,
  })
}

#[tauri::command]
pub fn create_transient_task(app: AppHandle, desc: THandle) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.create_transient_task(app.clone(), desc);
  Ok(())
}

#[tauri::command]
pub fn set_transient_task_state(app: AppHandle, task_id: u32, state: String) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.set_transient_task(app.clone(), task_id, state);
  Ok(())
}

#[tauri::command]
pub fn cancel_transient_task(app: AppHandle, task_id: u32) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.cancel_transient_task(task_id);
  Ok(())
}

#[tauri::command]
pub fn get_transient_task(app: AppHandle, task_id: u32) -> BGUMCLResult<Option<THandle>> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  Ok(monitor.get_transient_task(task_id))
}

#[tauri::command]
pub fn cancel_progressive_task(app: AppHandle, task_id: u32) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.cancel_progress(task_id);
  Ok(())
}

#[tauri::command]
pub fn resume_progressive_task(app: AppHandle, task_id: u32) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.resume_progress(task_id);
  Ok(())
}

#[tauri::command]
pub async fn restart_progressive_task(app: AppHandle, task_id: u32) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.restart_progress(task_id).await;
  Ok(())
}

#[tauri::command]
pub fn stop_progressive_task(app: AppHandle, task_id: u32) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.stop_progress(task_id);
  Ok(())
}

#[tauri::command]
pub async fn cancel_progressive_task_group(app: AppHandle, task_group: String) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  // Cancel and abort the task handles first.  Cleaning the instance directory
  // before aborting leaves a race where the still-running download can create
  // the partial instance files again after cleanup.
  monitor
    .cancel_progressive_task_group(task_group.clone())
    .await;
  // If cancelling an instance-creation download, remove the leftover
  // instance directory after all task handles have been aborted so the same
  // name cannot be rediscovered and restarted by an instance refresh.
  crate::instance::helpers::misc::cleanup_cancelled_instance_creation(&app, &task_group);
  Ok(())
}

#[tauri::command]
pub fn stop_progressive_task_group(app: AppHandle, task_group: String) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.stop_progressive_task_group(task_group);
  Ok(())
}

#[tauri::command]
pub async fn resume_progressive_task_group(app: AppHandle, task_group: String) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.resume_progressive_task_group(task_group).await;
  Ok(())
}

#[tauri::command]
pub fn delete_progressive_task_group(app: AppHandle, task_group: String) -> BGUMCLResult<()> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.delete_progressive_task_group(task_group);
  Ok(())
}

#[tauri::command]
pub fn retrieve_progressive_task_list(app: AppHandle) -> Vec<PTaskGroupDesc> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  monitor.state_list()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tasks::download::DownloadParam;
  use std::path::PathBuf;
  use tauri::Url;

  fn download(host: &str, name: &str) -> PTaskParam {
    PTaskParam::Download(DownloadParam {
      src: Url::parse(&format!("https://{host}/{name}")).unwrap(),
      dest: PathBuf::from(name),
      filename: Some(name.to_string()),
      sha1: None,
    })
  }

  #[test]
  fn task_group_interleaves_origins_instead_of_leaving_modrinth_at_the_tail() {
    let reordered = interleave_download_origins(vec![
      download("mediafilez.forgecdn.net", "a"),
      download("mediafilez.forgecdn.net", "b"),
      download("mediafilez.forgecdn.net", "c"),
      download("cdn.modrinth.com", "d"),
      download("cdn.modrinth.com", "e"),
    ]);
    let hosts = reordered
      .iter()
      .map(|param| {
        let PTaskParam::Download(download) = param;
        download.src.host_str().unwrap().to_string()
      })
      .collect::<Vec<_>>();
    assert_eq!(
      hosts,
      vec![
        "mediafilez.forgecdn.net",
        "cdn.modrinth.com",
        "mediafilez.forgecdn.net",
        "cdn.modrinth.com",
        "mediafilez.forgecdn.net",
      ]
    );
  }
}
