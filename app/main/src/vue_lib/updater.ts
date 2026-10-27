import { invoke } from '@tauri-apps/api/core';
import { check, type DownloadEvent, type Update } from '@tauri-apps/plugin-updater';
import { TauriVM } from './helper/ParamsHelper';
import { displayVersion, isReleaseBuild } from './version';

// The update `check` found, waiting for the user to install it. Kept out of the
// component's data: Vue would wrap it in a proxy, and the plugin's resource
// handle is a private field that a proxy can't reach.
let pending: Update | null = null;

// Asks the updater plugin whether the latest release is newer than this one.
// The plugin reads the release's latest.json and compares versions in Rust
// (updater.rs).
export async function checkForUpdate(vm: TauriVM) {
	// A build that isn't a release has no release to be behind, and no key to
	// verify one with.
	if (!vm.version || !isReleaseBuild(vm.version)) return;
	try {
		pending = await check();
		vm.new_version = pending ? displayVersion(pending.version) : null;
	} catch (err) {
		console.error('update check failed', err);
	}
}

// Downloads and installs the pending update, then restarts into it. On
// Windows the installer closes the app itself, so this never returns there.
export async function installUpdate(vm: TauriVM) {
	if (!pending || vm.updateState === 'installing') return;
	vm.updateState = 'installing';
	vm.updateProgress = null;
	const progress = progressTracker();
	try {
		await pending.downloadAndInstall((event: DownloadEvent) => {
			vm.updateProgress = progress(event);
		});
		await invoke('restart_app');
	} catch (err) {
		console.error('update failed', err);
		vm.updateState = 'failed';
	}
}

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
