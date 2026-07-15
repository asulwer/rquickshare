use std::str::FromStr;

use log::LevelFilter;

/// Change the log level of the running app.
///
/// Takes effect immediately: `set_up_logging` lets fern pass everything and
/// gates on the global max level instead, so this needs no restart. That
/// matters because the interesting case - an app started at boot rather than
/// from a shell - can't be given an `RQS_LOG` environment variable.
#[tauri::command]
pub async fn change_logging_level(message: String) -> Result<(), String> {
    let level = LevelFilter::from_str(&message)
        .map_err(|_| format!("unknown logging level: {message}"))?;

    log::set_max_level(level);
    info!("change_logging_level: now {level:?}");

    Ok(())
}
