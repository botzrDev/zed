use anyhow::{Context as _, Result};
use client::{Client, telemetry::MINIDUMP_ENDPOINT};
use feature_flags::FeatureFlagAppExt;
use futures::{AsyncReadExt, TryStreamExt};
use gpui::{
    ActionStatistics, App, AppContext, AsyncApp, FutureExt, ResolvedActionStatistics,
    SerializedThreadTaskTimings, TasksIncluded, profiler,
};
use http_client::{self, AsyncBody, HttpClient, Request};
use log::info;
use project::Project;
use proto::{CrashReport, GetCrashFilesResponse};
use reqwest::{
    Method,
    multipart::{Form, Part},
};
use serde::Deserialize;
use smol::{
    channel::{Receiver, Sender},
    stream::StreamExt,
};
use std::{
    ffi::OsStr,
    fmt::Write,
    fs,
    path::PathBuf,
    sync::Arc,
    thread::{self, ThreadId},
    time::Duration,
};
use sysinfo::{MemoryRefreshKind, RefreshKind, System};
use util::ResultExt;

use crate::STARTUP_TIME;

gpui::actions!(
    dev,
    [
        /// Causes a performance hang to test performance monitoring
        HangAction,
        /// Causes a performance hang to test performance monitoring
        HangBackground,
        /// Causes a performance hang to test performance monitoring
        HangForeground,
    ]
);

pub fn init(client: Arc<Client>, cx: &mut App) {
    // if cfg!(debug_assertions) {
    //     log::info!("Debug assertions enabled, skipping hang monitoring");
    // } else {
    start_hang_detection(cx);
    // }

    cx.on_flags_ready({
        let client = client.clone();
        move |flags_ready, cx| {
            if flags_ready.is_staff {
                let client = client.clone();
                cx.background_spawn(async move {
                    upload_build_timings(client).await.warn_on_err();
                })
                .detach();
            }
        }
    })
    .detach();

    if client.telemetry().diagnostics_enabled() {
        let client = client.clone();
        cx.background_spawn(async move {
            upload_previous_minidumps(client).await.warn_on_err();
        })
        .detach()
    }

    cx.on_action(move |_: &HangAction, _| {
        log::warn!(
            "Hanging the foreground for 5 seconds by blocking in an action.
            Zed will be unresponsive for that time. This should trigger a report in the log"
        );
        std::thread::sleep(Duration::from_secs(5));
        log::warn!("Hang ended");
    });
    cx.on_action(move |_: &HangBackground, cx| {
        cx.background_spawn(async {
            log::warn!(
                "Hanging a background executor for 5 seconds! This should trigger a report in the log"
            );
            std::thread::sleep(Duration::from_secs(5));
            log::warn!("Hang ended");
        }).detach();
    });
    cx.on_action(move |_: &HangForeground, cx| {
        cx.spawn(async |_| {
            log::warn!(
                "Hanging the foreground executor for 5 seconds to test performance monitoring! \
            Zed will be unresponsive for that time. This should trigger a report in the log"
            );
            std::thread::sleep(Duration::from_secs(5));
            log::warn!("Hang ended");
        })
        .detach();
    });

    cx.observe_new(move |project: &mut Project, _, cx| {
        let client = client.clone();

        let Some(remote_client) = project.remote_client() else {
            return;
        };
        remote_client.update(cx, |remote_client, cx| {
            if !client.telemetry().diagnostics_enabled() {
                return;
            }
            let request = remote_client
                .proto_client()
                .request(proto::GetCrashFiles {});
            cx.background_spawn(async move {
                let GetCrashFilesResponse { crashes } = request.await?;

                let Some(endpoint) = MINIDUMP_ENDPOINT.as_ref() else {
                    return Ok(());
                };
                for CrashReport {
                    metadata,
                    minidump_contents,
                } in crashes
                {
                    if let Some(metadata) = serde_json::from_str(&metadata).log_err() {
                        upload_minidump(client.clone(), endpoint, minidump_contents, &metadata)
                            .await
                            .log_err();
                    }
                }

                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        })
    })
    .detach();
}

fn start_hang_detection(cx: &App) {
    let foreground_thread = std::thread::current().id();
    // TODO!(yara) use the CX in a way that makes it so this must be run on the
    // foregrund exec

    let background_executor = cx.background_executor().clone();

    // need to run on the foreground to access the actions registry
    // so we have a this little dance to get the data back out
    //
    // TODO!(yara) remove all this complexity and just have a copy of the action
    // registry aroung (Arc::weak?)
    let (request_tx, rx) = smol::channel::bounded(1);
    let (tx, response_rx) = smol::channel::bounded(1);
    async fn action_resolver(
        rx: smol::channel::Receiver<ActionStatistics>,
        tx: smol::channel::Sender<ResolvedActionStatistics>,
        cx: AsyncApp,
    ) {
        while let Ok(stats) = rx.recv().await {
            let resolved = cx.update(|cx| stats.resolve(cx));
            if tx.try_send(resolved).is_err() {
                log::error!("profiler action resolver lagging behind")
            }
        }
    }
    cx.spawn(async move |cx| action_resolver(rx, tx, cx.clone()))
        .detach();

    // an OS thread to insulate detection and reporting from hangs on the fore
    // or background.
    thread::Builder::new()
        .name("HangDetection".to_string())
        .spawn(move || {
            // TODO!(yara) make this recently reported and bound the size of the collection

            loop {
                thread::sleep(Duration::from_secs(1));
                let task_stats = background_executor
                    .dispatcher()
                    .get_all_stats(TasksIncluded::CompletedAndRunning);

                // TODO!(yara) make these objects and only report new-ish issues
                report_hanging_foreground_tasks(&task_stats, foreground_thread);
                report_hanging_background_tasks(&task_stats, foreground_thread);
                report_hanging_actions(&request_tx, &response_rx, &background_executor);

                // TODO!(yara) save a trace again
            }
        })
        .expect("App can always spawn threads");
}

fn report_hanging_foreground_tasks(
    task_stats: &[gpui::ThreadTaskStatistics],
    foreground_thread: ThreadId,
) {
    let foreground = task_stats
        .iter()
        .find(|t| t.thread_id == foreground_thread)
        .expect("main thread should be in all statistics");

    if foreground
        .stats
        .longest_poll_times
        .iter()
        .any(|task| task.until_yielded() > Duration::from_millis(600))
    {
        info!("Foreground hang detected:\n\t{}", foreground.stats);
    }
}

fn report_hanging_background_tasks(
    task_stats: &[gpui::ThreadTaskStatistics],
    foreground_thread: ThreadId,
) {
    let background = task_stats
        .iter()
        .filter(|t| t.thread_id != foreground_thread);

    for worker in background {
        if worker
            .stats
            .longest_poll_times
            .iter()
            .any(|stat| stat.until_yielded() > Duration::from_millis(600))
        {
            info!(
                "Background hang detected on worker {}:\n\t{}",
                worker.thread_name.as_deref().unwrap_or_else(|| "Unknown"),
                worker.stats
            );
        }
    }
}

fn report_hanging_actions(
    request_tx: &Sender<ActionStatistics>,
    response_rx: &Receiver<ResolvedActionStatistics>,
    background_executor: &gpui::BackgroundExecutor,
) {
    let Some(stats) = action_statistics(request_tx, response_rx, background_executor) else {
        return;
    };

    if stats
        .0
        .iter()
        .any(|s| s.runtime() > Duration::from_millis(600))
    {
        info!("Action hang detected:\n\t{}", stats);
    }
}

fn action_statistics(
    request_tx: &Sender<ActionStatistics>,
    response_rx: &Receiver<ResolvedActionStatistics>,
    background_executor: &gpui::BackgroundExecutor,
) -> Option<ResolvedActionStatistics> {
    if request_tx
        .send_blocking(profiler::collect_action_stats())
        .is_err()
    {
        return None; // app closing
    }
    let Ok(stats) = smol::block_on(
        response_rx
            .recv()
            // during extreme lag we may need to wait a fair bit
            // before we get to get things from the foreground
            .with_timeout(Duration::from_secs(30), &background_executor),
    )
    .unwrap_or_else(|_| {
        log::error!("Extreme hang, could not get foreground info within 30s");
        Ok(ResolvedActionStatistics::empty())
    }) else {
        return None; // app closing
    };
    Some(stats)
}

fn cleanup_old_hang_traces() {
    if let Ok(entries) = std::fs::read_dir(paths::hang_traces_dir()) {
        let mut files: Vec<_> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "json" || ext == "miniprof")
            })
            .collect();

        const MAX_HANG_TRACES: usize = 3;
        if files.len() > MAX_HANG_TRACES {
            files.sort_by_key(|entry| entry.file_name());
            for entry in files.iter().take(files.len() - MAX_HANG_TRACES) {
                std::fs::remove_file(entry.path()).log_err();
            }
        }
    }
}

fn format_task_statistics(stats: &[gpui::ThreadTaskStatistics]) -> Option<String> {
    let mut res = String::new();
    for gpui::ThreadTaskStatistics {
        thread_name,
        thread_id,
        stats,
    } in stats
    {
        let name = thread_name
            .clone()
            .unwrap_or_else(|| format!("{:?}", thread_id));
        res.write_fmt(format_args!("thread: {name}")).ok()?;
        res.write_fmt(format_args!("{}", stats)).ok()?;
    }
    Some(res)
}

fn save_traces(
    background_executor: &gpui::BackgroundExecutor,
    main_thread_id: ThreadId,
) -> Option<PathBuf> {
    let thread_timings = background_executor
        .dispatcher()
        .get_all_timings(TasksIncluded::CompletedAndRunning);

    let thread_timings = thread_timings
        .into_iter()
        .map(|mut timings| {
            if timings.thread_id == main_thread_id {
                timings.thread_name = Some("main".to_string());
            }

            SerializedThreadTaskTimings::convert(*STARTUP_TIME.get().unwrap(), timings)
        })
        .collect::<Vec<_>>();

    let Some(timings) = serde_json::to_string(&thread_timings)
        .context("hang timings serialization")
        .log_err()
    else {
        return None;
    };

    if profiler::trace_enabled() {
        None
    } else {
        cleanup_old_hang_traces();
        let trace_path = paths::hang_traces_dir().join(&format!(
            "hang-{}.miniprof.json",
            chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
        ));
        std::fs::write(&trace_path, timings)
            .context("hang trace file writing")
            .log_err();
        Some(trace_path)
    }
}

pub async fn upload_previous_minidumps(client: Arc<Client>) -> anyhow::Result<()> {
    let Some(minidump_endpoint) = MINIDUMP_ENDPOINT.as_ref() else {
        log::warn!("Minidump endpoint not set");
        return Ok(());
    };

    let mut children = smol::fs::read_dir(paths::logs_dir()).await?;
    while let Some(child) = children.next().await {
        let child = child?;
        let child_path = child.path();
        if child_path.extension() != Some(OsStr::new("dmp")) {
            continue;
        }
        let mut json_path = child_path.clone();
        json_path.set_extension("json");
        let Ok(metadata) = smol::fs::read(&json_path)
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .and_then(|data| serde_json::from_slice(&data).map_err(|e| anyhow::anyhow!(e)))
        else {
            continue;
        };
        if upload_minidump(
            client.clone(),
            minidump_endpoint,
            smol::fs::read(&child_path)
                .await
                .context("Failed to read minidump")?,
            &metadata,
        )
        .await
        .log_err()
        .is_some()
        {
            fs::remove_file(child_path).ok();
            fs::remove_file(json_path).ok();
        }
    }
    Ok(())
}

async fn upload_minidump(
    client: Arc<Client>,
    endpoint: &str,
    minidump: Vec<u8>,
    metadata: &crashes::CrashInfo,
) -> Result<()> {
    let mut form = Form::new()
        .part(
            "upload_file_minidump",
            Part::bytes(minidump)
                .file_name("minidump.dmp")
                .mime_str("application/octet-stream")?,
        )
        .text(
            "sentry[tags][channel]",
            metadata.init.release_channel.clone(),
        )
        .text("sentry[tags][version]", metadata.init.zed_version.clone())
        .text("sentry[tags][binary]", metadata.init.binary.clone())
        .text("sentry[release]", metadata.init.commit_sha.clone())
        .text("platform", "rust");
    let mut panic_message = "".to_owned();
    if let Some(panic_info) = metadata.panic.as_ref() {
        panic_message = panic_info.message.clone();
        form = form
            .text("sentry[logentry][formatted]", panic_info.message.clone())
            .text("span", panic_info.span.clone());
    }
    if let Some(minidump_error) = metadata.minidump_error.clone() {
        form = form.text("minidump_error", minidump_error);
    }

    if let Some(is_staff) = &metadata
        .user_info
        .as_ref()
        .and_then(|user_info| user_info.is_staff)
    {
        form = form.text(
            "sentry[user][is_staff]",
            if *is_staff { "true" } else { "false" },
        );
    }

    if let Some(metrics_id) = metadata
        .user_info
        .as_ref()
        .and_then(|user_info| user_info.metrics_id.as_ref())
    {
        form = form.text("sentry[user][id]", metrics_id.clone());
    } else if let Some(id) = client.telemetry().installation_id() {
        form = form.text("sentry[user][id]", format!("installation-{}", id))
    }

    ::telemetry::event!(
        "Minidump Uploaded",
        panic_message = panic_message,
        crashed_version = metadata.init.zed_version.clone(),
        commit_sha = metadata.init.commit_sha.clone(),
    );

    let gpu_count = metadata.gpus.len();
    for (index, gpu) in metadata.gpus.iter().cloned().enumerate() {
        let system_specs::GpuInfo {
            device_name,
            device_pci_id,
            vendor_name,
            vendor_pci_id,
            driver_version,
            driver_name,
        } = gpu;
        let num = if gpu_count == 1 && metadata.active_gpu.is_none() {
            String::new()
        } else {
            index.to_string()
        };
        let name = format!("gpu{num}");
        let root = format!("sentry[contexts][{name}]");
        form = form
            .text(
                format!("{root}[Description]"),
                "A GPU found on the users system. May or may not be the GPU Zed is running on",
            )
            .text(format!("{root}[type]"), "gpu")
            .text(format!("{root}[name]"), device_name.unwrap_or(name))
            .text(format!("{root}[id]"), format!("{:#06x}", device_pci_id))
            .text(
                format!("{root}[vendor_id]"),
                format!("{:#06x}", vendor_pci_id),
            )
            .text_if_some(format!("{root}[vendor_name]"), vendor_name)
            .text_if_some(format!("{root}[driver_version]"), driver_version)
            .text_if_some(format!("{root}[driver_name]"), driver_name);
    }
    if let Some(active_gpu) = metadata.active_gpu.clone() {
        form = form
            .text(
                "sentry[contexts][Active_GPU][Description]",
                "The GPU Zed is running on",
            )
            .text("sentry[contexts][Active_GPU][type]", "gpu")
            .text("sentry[contexts][Active_GPU][name]", active_gpu.device_name)
            .text(
                "sentry[contexts][Active_GPU][driver_version]",
                active_gpu.driver_info,
            )
            .text(
                "sentry[contexts][Active_GPU][driver_name]",
                active_gpu.driver_name,
            )
            .text(
                "sentry[contexts][Active_GPU][is_software_emulated]",
                active_gpu.is_software_emulated.to_string(),
            );
    }

    // TODO: feature-flag-context, and more of device-context like screen resolution, available ram, device model, etc

    let content_type = format!("multipart/form-data; boundary={}", form.boundary());
    let mut body_bytes = Vec::new();
    let mut stream = form
        .into_stream()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        .into_async_read();
    stream.read_to_end(&mut body_bytes).await?;
    let req = Request::builder()
        .method(Method::POST)
        .uri(endpoint)
        .header("Content-Type", content_type)
        .body(AsyncBody::from(body_bytes))?;
    let mut response_text = String::new();
    let mut response = client.http_client().send(req).await?;
    response
        .body_mut()
        .read_to_string(&mut response_text)
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("failed to upload minidump: {response_text}");
    }
    log::info!("Uploaded minidump. event id: {response_text}");
    Ok(())
}

#[derive(Debug, Deserialize)]
struct BuildTiming {
    started_at: chrono::DateTime<chrono::Utc>,
    duration_ms: f32,
    first_crate: String,
    target: String,
    blocked_ms: f32,
    command: String,
}

// NOTE: this is a bit of a hack. We want to be able to have internal
// metrics around build times, but we don't have an easy way to authenticate
// users - except - we know internal users use Zed.
// So, we have it upload the timings on their behalf, it'd be better to do
// this more directly in ./script/cargo-timing-info.js.
async fn upload_build_timings(_client: Arc<Client>) -> Result<()> {
    let build_timings_dir = paths::data_dir().join("build_timings");

    if !build_timings_dir.exists() {
        return Ok(());
    }

    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let system = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
    );
    let ram_size_gb = (system.total_memory() as f64) / (1024.0 * 1024.0 * 1024.0);

    let mut entries = smol::fs::read_dir(&build_timings_dir).await?;
    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let path = entry.path();

        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }

        let contents = match smol::fs::read_to_string(&path).await {
            Ok(contents) => contents,
            Err(err) => {
                log::warn!("Failed to read build timing file {:?}: {}", path, err);
                continue;
            }
        };

        let timing: BuildTiming = match serde_json::from_str(&contents) {
            Ok(timing) => timing,
            Err(err) => {
                log::warn!("Failed to parse build timing file {:?}: {}", path, err);
                continue;
            }
        };

        telemetry::event!(
            "Build Timing: Cargo Build",
            started_at = timing.started_at.to_rfc3339(),
            duration_ms = timing.duration_ms,
            first_crate = timing.first_crate,
            target = timing.target,
            blocked_ms = timing.blocked_ms,
            command = timing.command,
            cpu_count = cpu_count,
            ram_size_gb = ram_size_gb
        );

        if let Err(err) = smol::fs::remove_file(&path).await {
            log::warn!("Failed to delete build timing file {:?}: {}", path, err);
        }
    }

    Ok(())
}

trait FormExt {
    fn text_if_some(
        self,
        label: impl Into<std::borrow::Cow<'static, str>>,
        value: Option<impl Into<std::borrow::Cow<'static, str>>>,
    ) -> Self;
}

impl FormExt for Form {
    fn text_if_some(
        self,
        label: impl Into<std::borrow::Cow<'static, str>>,
        value: Option<impl Into<std::borrow::Cow<'static, str>>>,
    ) -> Self {
        match value {
            Some(value) => self.text(label.into(), value.into()),
            None => self,
        }
    }
}
