#[tauri::command]
pub fn get_hostname() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or(String::from("Unknown"))
}
