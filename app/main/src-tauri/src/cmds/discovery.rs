use qrcode::render::svg;
use qrcode::QrCode;

use crate::AppState;

/// Start discovery, returning an SVG QR code to display.
///
/// A phone that scans it advertises itself **even while hidden**, so this is how
/// we reach a device that isn't set to "Everyone" visibility. Visible peers are
/// still reported exactly as before - the QR is an additional path, not a
/// replacement.
#[tauri::command]
pub async fn start_discovery(state: tauri::State<'_, AppState>) -> Result<String, String> {
    info!("start_discovery");

    let url = state
        .rqs
        .lock()
        .await
        .discovery_with_qr(state.dch_sender.clone())
        .map_err(|e| format!("unable to start discovery: {}", e))?;

    // Rendered here rather than in core_lib: the URL is protocol, the picture is
    // presentation.
    let code = QrCode::new(url.as_bytes()).map_err(|e| format!("unable to encode QR: {e}"))?;

    Ok(code
        .render::<svg::Color>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .build())
}

#[tauri::command]
pub async fn stop_discovery(state: tauri::State<'_, AppState>) -> Result<(), String> {
    info!("stop_discovery");

    state.rqs.lock().await.stop_discovery();

    Ok(())
}
