use crate::store::{get_device_name, set_device_name};
use crate::AppState;

/// The name currently advertised to peers.
///
/// This is the *effective* name - the user's override if they set one, else the
/// hostname - so the UI can show exactly what nearby devices will display.
#[tauri::command]
pub fn get_advertised_name() -> String {
    rqs_lib::device_name()
}

/// The user's override, if any. `None` means "following the hostname", which
/// the settings field shows as a placeholder rather than as typed-in text.
#[tauri::command]
pub fn get_device_name_override(app_handle: tauri::AppHandle) -> Option<String> {
    get_device_name(&app_handle)
}

/// Rename the device, or pass an empty/absent name to go back to the hostname.
///
/// Unlike the other settings, persistence lives here rather than in the front
/// end: the name has to be trimmed and clamped to the 255 bytes the wire format
/// allows, and doing that in one place keeps the stored value, the advertised
/// value and the value shown in the header from drifting apart. Returns the
/// effective name so the caller can display what actually went on air.
#[tauri::command]
pub async fn change_device_name(
    name: Option<String>,
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    info!("change_device_name: {name:?}");

    // Normalise first so the store and the library agree on what was set.
    let normalized = name.and_then(|n| {
        let trimmed = n.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(rqs_lib::truncate_device_name(trimmed))
        }
    });

    // Applying to the library re-announces over mDNS, so a phone already
    // listing us picks the new name up without a restart.
    state.rqs.lock().await.set_device_name(normalized.clone());
    set_device_name(&app_handle, normalized);

    Ok(rqs_lib::device_name())
}
