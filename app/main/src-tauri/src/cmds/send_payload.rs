use std::path::Path;

use rqs_lib::{OutboundPayload, SendInfo};

use crate::AppState;

#[tauri::command]
pub async fn send_payload(
    message: SendInfo,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    info!("send_payload: {message:?}");

    // Quick Share sends a flat list of files - it has no notion of a folder on
    // the wire. Dropping a folder used to slip through here, get silently
    // skipped deeper in the pipeline, and surface as a bogus "Unexpected
    // disconnection". Reject it up front with a message the UI can show as a
    // toast instead. (The file picker already sets `directory: false`, so this
    // only ever catches drag-and-dropped folders.)
    if let OutboundPayload::Files(files) = &message.ob {
        if files.iter().any(|f| Path::new(f).is_dir()) {
            return Err("Folders aren't supported - send the individual files instead".to_owned());
        }
    }

    state
        .sender_file
        .send(message)
        .await
        .map_err(|e| format!("couldn't send payload: {e}"))
}
