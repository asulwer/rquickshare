#[cfg(target_os = "linux")]
use notify_rust::Notification;
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
use crate::cmds;

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
