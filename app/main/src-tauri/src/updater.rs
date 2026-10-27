//! In-app updates through `tauri-plugin-updater`.
//!
//! The Release workflow publishes a signed `latest.json` with every release.
//! The front end drives the steps (check, download, install, restart) so it can
//! hold the install until no transfer is running. Only release builds carry the
//! public key the downloads are verified against; see `release.yml`.
//!
//! The plugin's own semver comparison is kept: the `semver` crate orders build
//! metadata numerically, so 1.26.298+43 is already newer than 1.26.298+42.

use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Mutex;

use crate::stop_rqs;

/// The update the last check found, and its package once downloaded. The front
/// end only gets the version: the steps run here, where the install can stop
/// the RQS service first.
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Pending>>);

struct Pending {
    update: Update,
    package: Option<Vec<u8>>,
}

/// Download progress, shaped like the plugin's own JS events.
#[derive(Clone, Serialize)]
#[serde(tag = "event", content = "data")]
pub enum DownloadEvent {
    #[serde(rename_all = "camelCase")]
    Started {
        content_length: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    Progress {
        chunk_length: usize,
    },
    Finished,
}

/// Asks whether the latest release is newer than this one, and returns its
/// version if so.
#[tauri::command]
pub async fn check_update(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
) -> Result<Option<String>, String> {
    let stop_handle = app.clone();
    let update = app
        .updater_builder()
        // On Windows the plugin starts the installer and ends the process with
        // `process::exit`, which skips ExitRequested and so the usual
        // `kill_app`. Stop the service here instead, so BLE, mDNS and the
        // hotspot are torn down as on any other exit.
        .on_before_exit(move || {
            stop_rqs(&stop_handle);
            stop_handle.cleanup_before_exit();
        })
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let version = update.as_ref().map(|u| u.version.clone());
    *pending.0.lock().await = update.map(|update| Pending {
        update,
        package: None,
    });
    Ok(version)
}

/// Downloads the pending update and checks its signature, without installing
/// it.
#[tauri::command]
pub async fn download_update(
    pending: State<'_, PendingUpdate>,
    on_event: Channel<DownloadEvent>,
) -> Result<(), String> {
    let mut pending = pending.0.lock().await;
    let pending = pending.as_mut().ok_or("no update to download")?;
    let mut started = false;
    let package = pending
        .update
        .download(
            |chunk_length, content_length| {
                if !started {
                    started = true;
                    send(&on_event, DownloadEvent::Started { content_length });
                }
                send(&on_event, DownloadEvent::Progress { chunk_length });
            },
            || send(&on_event, DownloadEvent::Finished),
        )
        .await
        .map_err(|e| e.to_string())?;
    pending.package = Some(package);
    Ok(())
}

fn send(channel: &Channel<DownloadEvent>, event: DownloadEvent) {
    // Progress is only for show; the download carries on if the window is gone.
    if let Err(e) = channel.send(event) {
        debug!("update progress not delivered: {e}");
    }
}

/// Installs the downloaded update. On Windows this never returns: the installer
/// takes over and the plugin exits the app. On Linux and macOS the files are
/// replaced in place, and the front end calls [`restart_app`] once nothing would
/// be cut off.
#[tauri::command]
pub async fn install_update(pending: State<'_, PendingUpdate>) -> Result<(), String> {
    let pending = pending.0.lock().await;
    let pending = pending.as_ref().ok_or("no update to install")?;
    let package = pending.package.as_ref().ok_or("update not downloaded")?;
    // `install` blocks: on Linux it waits for pkexec and the package manager.
    tokio::task::block_in_place(|| pending.update.install(package)).map_err(|e| e.to_string())
}

/// Restarts into the version just installed.
#[tauri::command]
pub fn restart_app(app: AppHandle) {
    // Goes through ExitRequested, so the RQS service is stopped as on any
    // other exit before the new process starts.
    app.request_restart();
}
