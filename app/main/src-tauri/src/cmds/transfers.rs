use rqs_lib::channel::ChannelMessage;

use crate::AppState;

/// Every transfer the window would be showing, oldest first.
///
/// Closing to the tray destroys the webview, so a window opened later has
/// missed the `rs2js_channelmessage` events sent while it was gone. It loads
/// this on mount to rebuild its cards.
#[tauri::command]
pub fn get_transfers(state: tauri::State<'_, AppState>) -> Vec<ChannelMessage> {
    state
        .transfers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The user cleared a transfer's card: drop it, so a recreated window doesn't
/// bring it back, along with the paths its Open buttons used.
#[tauri::command]
pub fn forget_transfer(id: String, state: tauri::State<'_, AppState>) {
    state
        .transfers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|t| t.id != id);
    state
        .received
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
}

/// Fold a message bound for the front end into the snapshot, the same way the
/// window merges it into its list: later messages may leave `state` or `meta`
/// out, and the last known value stands.
pub fn remember_transfer(transfers: &mut Vec<ChannelMessage>, msg: &ChannelMessage) {
    match transfers.iter_mut().find(|t| t.id == msg.id) {
        Some(prev) => {
            let state = msg.state.clone().or_else(|| prev.state.take());
            let meta = msg.meta.clone().or_else(|| prev.meta.take());
            *prev = ChannelMessage {
                state,
                meta,
                ..msg.clone()
            };
        }
        None => transfers.push(msg.clone()),
    }
}

#[cfg(test)]
mod tests {
    use rqs_lib::channel::{ChannelDirection, TransferType};
    use rqs_lib::State;

    use super::*;

    fn msg(id: &str, state: Option<State>) -> ChannelMessage {
        ChannelMessage {
            id: id.into(),
            direction: ChannelDirection::LibToFront,
            rtype: Some(TransferType::Inbound),
            state,
            ..Default::default()
        }
    }

    #[test]
    fn keeps_last_known_state_and_meta() {
        let mut t = Vec::new();
        // TransferMetadata isn't exported by name; reach it through the field.
        let mut first = msg("a", Some(State::WaitingForUserConsent));
        first.meta = Some(Default::default());
        if let Some(meta) = first.meta.as_mut() {
            meta.pin_code = Some("1234".into());
        }
        remember_transfer(&mut t, &first);
        remember_transfer(&mut t, &msg("a", Some(State::ReceivingFiles)));
        remember_transfer(&mut t, &msg("a", None));

        assert_eq!(t.len(), 1);
        assert_eq!(t[0].state, Some(State::ReceivingFiles));
        assert_eq!(
            t[0].meta.as_ref().and_then(|m| m.pin_code.as_deref()),
            Some("1234")
        );
    }

    #[test]
    fn keeps_arrival_order() {
        let mut t = Vec::new();
        remember_transfer(&mut t, &msg("a", Some(State::Initial)));
        remember_transfer(&mut t, &msg("b", Some(State::Initial)));
        remember_transfer(&mut t, &msg("a", Some(State::Finished)));

        let ids: Vec<_> = t.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }
}
