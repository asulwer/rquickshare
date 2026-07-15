use crate::AppState;

#[tauri::command]
pub async fn start_discovery(state: tauri::State<'_, AppState>) -> Result<(), String> {
    info!("start_discovery");

    state
        .rqs
        .lock()
        .await
        .discovery(state.dch_sender.clone())
        .map_err(|e| format!("unable to start discovery: {}", e))
}

#[tauri::command]
pub async fn stop_discovery(state: tauri::State<'_, AppState>) -> Result<(), String> {
    info!("stop_discovery");

    state.rqs.lock().await.stop_discovery();

    Ok(())
}
