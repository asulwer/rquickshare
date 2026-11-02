import { ChannelMessage } from '@martichou/core_lib/bindings/ChannelMessage';

// Add the backend's transfers that the window doesn't know about yet.
//
// Closing to the tray destroys the window, so one opened later missed every
// event sent while it was gone; the backend keeps the latest of each transfer
// for it. Anything already in `requests` came from a live event received after
// the listener was registered, which is at least as new, so it stays.
export function addMissingTransfers(requests: ChannelMessage[], snapshot: ChannelMessage[]): void {
	for (const t of snapshot) {
		if (!requests.some((r) => r.id === t.id)) {
			requests.push(t);
		}
	}
}
