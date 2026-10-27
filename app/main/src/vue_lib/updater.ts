import { Channel, invoke } from '@tauri-apps/api/core';
import { watch } from 'vue';
import type { ChannelMessage } from '@martichou/core_lib/bindings/ChannelMessage';
import type { State } from '@martichou/core_lib/bindings/State';
import { TauriVM } from './helper/ParamsHelper';
import { autoUpdateKey } from './types';
import { displayVersion, isReleaseBuild } from './version';

// Download progress from the download_update command (updater.rs).
export type DownloadEvent =
	| { event: 'Started'; data: { contentLength: number | null } }
	| { event: 'Progress'; data: { chunkLength: number } }
	| { event: 'Finished' };

// Asks whether the latest release is newer than this one. The check, and the
// update it finds, stay in Rust (updater.rs). Runs at startup and from
// Settings > Check now.
export async function checkForUpdate(vm: TauriVM) {
	// A build that isn't a release has no release to be behind, and no key to
	// verify one with.
	if (!vm.version || !isReleaseBuild(vm.version)) return;
	// A check replaces the update an install is working on.
	if (vm.updateCheck === 'checking' || updateBusy(vm)) return;
	vm.updateCheck = 'checking';
	try {
		const version = await invoke<string | null>('check_update');
		vm.new_version = version ? displayVersion(version) : null;
		vm.updateCheck = version ? 'available' : 'current';
		// The update a failed install was for is gone; start over from this one.
		vm.updateState = 'idle';
	} catch (err) {
		console.error('update check failed', err);
		vm.updateCheck = 'failed';
	}
}

// Downloads, installs and restarts into the update the last check found. Each
// step that would cut off a transfer waits for it to end first: a transfer can
// start while the download runs, or while Linux asks for the password.
// Retrying after a failure starts over.
export async function installUpdate(vm: TauriVM) {
	if (!vm.new_version || vm.updateCheck === 'checking' || updateBusy(vm)) return;
	vm.updateState = 'installing';
	vm.updateProgress = null;
	const progress = progressTracker();
	const onEvent = new Channel<DownloadEvent>();
	onEvent.onmessage = (event) => {
		vm.updateProgress = progress(event);
	};
	try {
		await invoke('download_update', { onEvent });
		await untilNoTransfer(vm);
		// On Windows the installer takes over here and the app exits.
		await invoke('install_update');
		await untilNoTransfer(vm);
		await invoke('restart_app');
	} catch (err) {
		console.error('update failed', err);
		vm.updateState = 'failed';
	}
}

function updateBusy(vm: TauriVM): boolean {
	return vm.updateState === 'installing' || vm.updateState === 'waiting';
}

// Resolves once no transfer is running, showing 'waiting' until then. Watches
// the transfer list rather than polling it.
async function untilNoTransfer(vm: TauriVM) {
	const busy = () => transferInProgress(vm.requests, vm.sending.size);
	if (!busy()) return;
	vm.updateState = 'waiting';
	await new Promise<void>((resolve) => {
		const stop = watch(busy, (running) => {
			if (running) return;
			stop();
			resolve();
		});
	});
	vm.updateState = 'installing';
}

// Whether a transfer is under way: connecting, waiting for the user to accept,
// or moving files. Installing restarts the app, which would cut it off, so an
// automatic update waits until this turns false.
export function transferInProgress(requests: ChannelMessage[], connecting: number): boolean {
	return connecting > 0 || requests.some((r) => !r.state || !FINAL_STATES.includes(r.state));
}

const FINAL_STATES: State[] = ['Finished', 'Cancelled', 'Rejected', 'Disconnected'];

async function setAutoUpdate(vm: TauriVM, autoUpdate: boolean) {
	await vm.store.set(autoUpdateKey, autoUpdate);
	await vm.store.save();
	vm.autoUpdate = autoUpdate;
}

async function getAutoUpdate(vm: TauriVM) {
	vm.autoUpdate = await vm.store.get(autoUpdateKey) ?? false;
}

export const updaterUtils = {
	checkForUpdate,
	installUpdate,
	setAutoUpdate,
	getAutoUpdate,
};

// Turns the download's events into the percent done, or null while the size
// is unknown (the server sent no Content-Length).
export function progressTracker(): (event: DownloadEvent) => number | null {
	let total: number | undefined;
	let received = 0;
	return (event) => {
		switch (event.event) {
		case 'Started':
			total = event.data.contentLength || undefined;
			return total ? 0 : null;
		case 'Progress':
			received += event.data.chunkLength;
			return total ? Math.min(100, Math.floor(received * 100 / total)) : null;
		case 'Finished':
			return 100;
		}
	};
}
