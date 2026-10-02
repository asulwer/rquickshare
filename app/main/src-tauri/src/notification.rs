#[cfg(target_os = "linux")]
use notify_rust::Notification;
use rqs_lib::TextPayloadType;
#[cfg(target_os = "linux")]
use rqs_lib::{
    channel::{ChannelAction, ChannelDirection, ChannelMessage},
    Visibility,
};
use tauri::AppHandle;
#[cfg(target_os = "linux")]
use tauri::Manager;
#[cfg(not(target_os = "linux"))]
use tauri_plugin_notification::NotificationExt;

#[cfg(target_os = "linux")]
use tauri_plugin_clipboard_manager::ClipboardExt;
#[cfg(target_os = "linux")]
use tauri_plugin_opener::OpenerExt;

#[cfg(target_os = "linux")]
use crate::cmds;

/// Longest slice of a received text shown in its notification body.
const TEXT_PREVIEW_CHARS: usize = 200;

pub fn send_request_notification(name: String, id: String, app_handle: &AppHandle) {
    // Is not used in macos, get rid of warning
    let _ = id;

    let body = format!("{name} want to initiate a transfer");

    #[cfg(not(target_os = "linux"))]
    let _ = app_handle
        .notification()
        .builder()
        .title("RQuickShare")
        .body(&body)
        .show();

    #[cfg(target_os = "linux")]
    match Notification::new()
        .summary("RQuickShare")
        .body(&body)
        .action("accept", "Accept")
        .action("reject", "Reject")
        .show()
    {
        Ok(n) => {
            let capp_handle = app_handle.clone();
            // TODO - Meh, untracked, unwaited tasks...
            #[cfg(target_os = "linux")]
            tokio::task::spawn(async move {
                n.wait_for_action(|action| match action {
                    "accept" => {
                        let _ = cmds::send_to_rs(
                            ChannelMessage {
                                id,
                                direction: ChannelDirection::FrontToLib,
                                action: Some(ChannelAction::AcceptTransfer),
                                ..Default::default()
                            },
                            capp_handle.state(),
                        );
                    }
                    "reject" => {
                        let _ = cmds::send_to_rs(
                            ChannelMessage {
                                id,
                                direction: ChannelDirection::FrontToLib,
                                action: Some(ChannelAction::RejectTransfer),
                                ..Default::default()
                            },
                            capp_handle.state(),
                        );
                    }
                    _ => (),
                });
            });
        }
        Err(e) => {
            error!("Couldn't show notification: {}", e);
        }
    }
}

pub fn send_temporarily_notification(app_handle: &AppHandle) {
    let body = "RQuickShare is temporarily hidden".to_string();

    #[cfg(not(target_os = "linux"))]
    let _ = app_handle
        .notification()
        .builder()
        .title("RQuickShare")
        .body(&body)
        .show();

    #[cfg(target_os = "linux")]
    match Notification::new()
        .summary("RQuickShare")
        .body(&body)
        .action("visible", "Be visible (1m)")
        .action("ignore", "Ignore")
        .id(1919)
        .show()
    {
        Ok(n) => {
            let capp_handle = app_handle.clone();
            // TODO - Meh, untracked, unwaited tasks...
            #[cfg(target_os = "linux")]
            tokio::task::spawn(async move {
                // wait_for_action blocks until the user clicks or the
                // notification expires, so it can't sit on an async worker.
                // It also takes a sync closure, which is why the action is
                // carried back out rather than acted on in place.
                let chosen = tokio::task::spawn_blocking(move || {
                    let mut chosen: Option<String> = None;
                    n.wait_for_action(|action| chosen = Some(action.to_owned()));
                    chosen
                })
                .await;

                if let Ok(Some(action)) = chosen {
                    if action == "visible" {
                        if let Err(e) =
                            cmds::change_visibility(Visibility::Temporarily, capp_handle.state())
                                .await
                        {
                            error!("Couldn't make the device visible: {e}");
                        }
                    }
                }
            });
        }
        Err(e) => {
            error!("Couldn't show notification: {}", e);
        }
    }
}

/// Tell the user a text/link share landed, with Copy (and Open, for links)
/// right on the notification so they don't have to dig the window out.
pub fn send_text_notification(
    name: String,
    text_type: TextPayloadType,
    text: String,
    app_handle: &AppHandle,
) {
    let mut body: String = text.chars().take(TEXT_PREVIEW_CHARS).collect();
    if body.len() < text.len() {
        body.push('…');
    }
    let summary = match text_type {
        TextPayloadType::Url => format!("{name} shared a link"),
        TextPayloadType::Wifi => format!("{name} shared a Wi-Fi network"),
        TextPayloadType::Text => format!("{name} shared text"),
    };

    #[cfg(not(target_os = "linux"))]
    let _ = app_handle
        .notification()
        .builder()
        .title(&summary)
        .body(&body)
        .show();

    #[cfg(target_os = "linux")]
    {
        // The Rust-side opener isn't bound by the capability scope, so only
        // offer Open for plain web links - not file:// or app-specific schemes
        // a sender could slip in.
        let openable = matches!(text_type, TextPayloadType::Url)
            && (text.starts_with("https://") || text.starts_with("http://"));

        let mut notification = Notification::new();
        notification.summary(&summary).body(&body);
        if openable {
            notification.action("open", "Open");
        }
        notification.action("copy", "Copy");

        match notification.show() {
            Ok(n) => {
                let capp_handle = app_handle.clone();
                tokio::task::spawn(async move {
                    // Same as send_temporarily_notification: wait_for_action
                    // blocks, so keep it off the async workers.
                    let chosen = tokio::task::spawn_blocking(move || {
                        let mut chosen: Option<String> = None;
                        n.wait_for_action(|action| chosen = Some(action.to_owned()));
                        chosen
                    })
                    .await;

                    match chosen.ok().flatten().as_deref() {
                        Some("open") if openable => {
                            if let Err(e) = capp_handle.opener().open_url(&text, None::<&str>) {
                                error!("Couldn't open the received link: {e}");
                            }
                        }
                        Some("copy") => {
                            if let Err(e) = capp_handle.clipboard().write_text(text) {
                                error!("Couldn't copy the received text: {e}");
                            }
                        }
                        _ => (),
                    }
                });
            }
            Err(e) => {
                error!("Couldn't show notification: {}", e);
            }
        }
    }
}
