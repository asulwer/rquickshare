use rqs_lib::channel::ChannelMessage;
use rqs_lib::{OutboundPayload, State};
use serde::{Deserialize, Serialize};

use crate::AppState;

/// The most transfers the snapshot holds. A nearby peer can start as many as
/// it likes, and an app left in the tray never has its cards cleared.
pub const MAX_TRANSFERS: usize = 100;

/// What the page had staged when the window closed to the tray, so the next
/// window picks up where it left off.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageState {
    /// Files or text waiting for the user to pick a device.
    pub outbound_payload: Option<OutboundPayload>,
    /// The clipboard text last staged, so auto-sync doesn't stage it again.
    pub last_clipboard: Option<String>,
}

#[tauri::command]
pub fn get_page_state(state: tauri::State<'_, AppState>) -> PageState {
    state.page.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[tauri::command]
pub fn set_page_state(page: PageState, state: tauri::State<'_, AppState>) {
    *state.page.lock().unwrap_or_else(|e| e.into_inner()) = page;
}

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
///
/// Past [`MAX_TRANSFERS`] the oldest entry goes, preferring one that is over.
/// Returns its id so whatever else is kept per transfer can go with it.
pub fn remember_transfer(
    transfers: &mut Vec<ChannelMessage>,
    msg: &ChannelMessage,
) -> Option<String> {
    match transfers.iter_mut().find(|t| t.id == msg.id) {
        Some(prev) => {
            let state = msg.state.clone().or_else(|| prev.state.take());
            let meta = msg.meta.clone().or_else(|| prev.meta.take());
            *prev = ChannelMessage {
                state,
                meta,
                ..msg.clone()
            };
            None
        }
        None => {
            transfers.push(msg.clone());
            if transfers.len() <= MAX_TRANSFERS {
                return None;
            }
            let oldest = transfers.iter().position(is_over).unwrap_or(0);
            Some(transfers.remove(oldest).id)
        }
    }
}

fn is_over(t: &ChannelMessage) -> bool {
    matches!(
        t.state,
        Some(State::Finished | State::Rejected | State::Cancelled | State::Disconnected)
    )
}

#[cfg(test)]
mod tests {
    use rqs_lib::channel::{ChannelDirection, TransferType};

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

    #[test]
    fn evicts_oldest_finished_first() {
        let mut t = Vec::new();
        remember_transfer(&mut t, &msg("waiting", Some(State::WaitingForUserConsent)));
        remember_transfer(&mut t, &msg("done", Some(State::Finished)));
        for i in 2..MAX_TRANSFERS {
            remember_transfer(&mut t, &msg(&i.to_string(), Some(State::Initial)));
        }
        assert_eq!(t.len(), MAX_TRANSFERS);

        let evicted = remember_transfer(&mut t, &msg("new", Some(State::Initial)));
        assert_eq!(evicted.as_deref(), Some("done"));
        assert_eq!(t.len(), MAX_TRANSFERS);

        // Nothing is over any more, so the oldest goes.
        let evicted = remember_transfer(&mut t, &msg("newer", Some(State::Initial)));
        assert_eq!(evicted.as_deref(), Some("waiting"));
    }
}
