use crate::AppState;

#[tauri::command]
pub async fn change_nearby_alerts(
    message: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    info!("change_nearby_alerts: {message}");

    state.rqs.lock().await.set_nearby_alerts(message);

    Ok(())
}
