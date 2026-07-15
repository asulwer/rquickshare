use std::path::PathBuf;

use crate::AppState;

#[tauri::command]
pub async fn change_download_path(
    message: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    info!("change_download_path: {message:?}");

    state
        .rqs
        .lock()
        .await
        .set_download_path(message.map(PathBuf::from));

    Ok(())
}
