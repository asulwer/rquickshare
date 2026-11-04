/// Whether bluetoothd runs with experimental features, so the side menu can
/// explain the pairing request they cause (KNOWN_ISSUES.md, D). The window
/// asks on load, since the flag may have been set before it was listening.
#[tauri::command]
pub fn get_bluez_experimental() -> bool {
    #[cfg(target_os = "linux")]
    return *rqs_lib::bluez_experimental().borrow();
    #[cfg(not(target_os = "linux"))]
    false
}
