use rqs_lib::Visibility;

use crate::AppState;

#[tauri::command]
pub async fn change_visibility(
    message: Visibility,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    info!("change_visibility: {message:?}");

    state.rqs.lock().await.change_visibility(message);

    Ok(())
}
