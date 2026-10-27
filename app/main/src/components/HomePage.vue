<template>
	<div class="flex flex-col w-full h-full bg-green-50 dark:bg-neutral-900 max-w-full max-h-full overflow-hidden">
		<ToastNotification />
		<SettingsModal :vm="vm" @close="settingsOpen = false" />

		<Heading :vm="vm" :open-url="openUrl" @open-settings="settingsOpen = true" @install-update="installUpdate(vm)" />

		<div class="flex-1 flex flex-row">
			<SideMenu :vm="vm" @invert-visibility="invertVisibility(vm)" @clear-sending="stopSending()" />

			<div class="flex-1 flex flex-col bg-white dark:bg-neutral-800 w-full max-w-full min-w-0 min-h-full rounded-tl-[3rem] p-12 h-1 overflow-y-scroll">
				<ContentStatus :vm="vm" @outbound-payload="(el: OutboundPayload) => outboundPayload = el" @discovery-running="startDiscovery" />

				<!-- Scanning this makes a phone advertise itself to us even when
				     it isn't set to "Everyone" visibility - the scan is the
				     authorization, so no settings dive is needed. -->
				<div
					v-if="qrDataUri"
					class="w-full rounded-3xl flex flex-col items-center gap-3 p-4 mb-4 bg-green-100 dark:bg-neutral-700">
					<p class="text-sm text-center">
						Device not showing up? Scan this with it - no need to switch it to "Everyone".
					</p>
					<img :src="qrDataUri" alt="Quick Share QR code" class="w-44 h-44 rounded-xl bg-white p-2">
				</div>

				<div
					v-for="item in displayedItems" :key="item.id" class="w-full rounded-3xl flex flex-row gap-6 p-4 mb-4 bg-green-100 dark:bg-neutral-700"
					:class="{
						'pressable hover:bg-green-200 dark:hover:bg-neutral-600': item.endpoint && !sending.has(item.id),
						'opacity-60 cursor-wait pointer-events-none': sending.has(item.id)
					}"
					:aria-disabled="sending.has(item.id)" :aria-busy="sending.has(item.id)"
					@click="item.endpoint && startSend(item.id)">
					<!-- Loader and image of the device type & pin_code -->
					<ItemSide :item="item" :busy="item.endpoint === true && sending.has(item.id)" />

					<!-- Content and state of the transfer -->
					<div class="flex-1 flex flex-col text-sm min-w-0" :class="{'justify-center': item.state === undefined}">
						<h4 class="text-base font-medium">
							{{ item.name }}
						</h4>

						<!-- Connecting to a device takes a while - seconds on the LAN,
							 up to a minute over BLE - and until the first state comes
							 back this is still an endpoint card, so without this the
							 click left no trace at all and the user kept clicking. The
							 spinner itself is the ring around the device icon. -->
						<p v-if="item.endpoint && sending.has(item.id)" class="mt-2">
							Connecting...
						</p>

						<div v-if="item.state === 'WaitingForUserConsent'" class="flex-1 flex flex-col justify-between">
							<div class="mt-2 min-w-0">
								<template v-if="item.files?.length">
									<p>Wants to share {{ pluralize(item.files.length, 'file') }}</p>
									<FileList :files="item.files" />
								</template>
								<p v-else>
									Wants to share {{ item.text_description ?? 'some text' }}
								</p>
							</div>
							<div class="flex flex-row justify-end items-center gap-4 mt-1">
								<!-- Once answered, the buttons are replaced rather than just
									 styled as disabled. A second click starts a second transfer
									 setup, and the backend can sit on this state for several
									 seconds, which is exactly when a click looks ignored. -->
								<div v-if="responding.has(item.id)" class="flex flex-row items-center gap-2 px-3 py-2 opacity-70">
									<span
										class="w-4 h-4 rounded-full border-2 border-current border-t-transparent animate-spin"
										aria-hidden="true" />
									<p class="text-sm">
										{{ responding.get(item.id) === 'AcceptTransfer' ? 'Accepting…' : 'Declining…' }}
									</p>
								</div>
								<template v-else>
									<p
										@click.stop="respondToTransfer(item.id, 'AcceptTransfer')" class="btn px-3
										rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
										Accept
									</p>
									<p
										@click.stop="respondToTransfer(item.id, 'RejectTransfer')" class="btn px-3
										rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
										Decline
									</p>
								</template>
							</div>
						</div>

						<div v-else-if="['SentIntroduction', 'SendingFiles', 'ReceivingFiles'].includes(item.state ?? 'Initial')">
							<p class="mt-2" v-if="['SentIntroduction', 'SendingFiles'].includes(item.state ?? 'Initial')">
								Sending...
							</p>
							<p class="mt-2" v-else>
								Receiving...
							</p>
							<FileList v-if="item.files?.length" :files="item.files" />
							<div class="flex flex-row justify-end gap-4 mt-1">
								<p
									@click.stop="sendCmd(vm, item.id, 'CancelTransfer')" class="btn px-3
									rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
									Cancel
								</p>
							</div>
						</div>

						<div v-else-if="item.state === 'Finished'">
							<p class="mt-2">
								{{ item.transferType === 'Outbound' ? 'Sent' : 'Received' }}
								<span v-if="item.text_type">text</span>
								<span v-else-if="item.files?.length">{{ pluralize(item.files.length, 'file') }}</span>
							</p>

							<!-- If files -->
							<FileList v-if="item.files?.length" :files="item.files" />
							<p v-if="item.files && item.destination" class="mt-2 overflow-hidden whitespace-nowrap text-ellipsis">
								Saved to {{ item.destination }}
							</p>

							<!-- If text -->
							<p v-if="item.text_type" class="!select-text cursor-text overflow-hidden whitespace-nowrap text-ellipsis">
								{{ item.text_payload }}
							</p>

							<div class="flex flex-row justify-end gap-4 mt-1">
								<!-- Multi-file "Open" already falls back to the folder, so only
								     offer it when it does something "Open folder" doesn't. -->
								<p
									v-if="(item.destination && item.files?.length === 1) || (item.text_type === 'Url' && item.text_payload)"
									@click.stop="openTransfer(item)"
									class="btn px-3 rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
									Open
								</p>
								<p
									v-if="item.destination && item.files?.length"
									@click.stop="openFolder(item)"
									class="btn px-3 rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
									Open folder
								</p>
								<p
									v-if="item.text_type && item.text_payload" @click.stop="writeToClipboard(item.text_payload)"
									class="btn px-3 rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
									Copy
								</p>
								<p
									@click.stop="removeRequest(vm, item.id)"
									class="btn px-3 rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
									Clear
								</p>
							</div>
						</div>

						<div v-else-if="item.state === 'Cancelled'">
							<p class="mt-2">
								Transfer cancelled
							</p>
							<div class="flex flex-row justify-end gap-4 mt-1">
								<p
									@click.stop="removeRequest(vm, item.id)" class="btn px-3
									rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
									Clear
								</p>
							</div>
						</div>

						<div v-else-if="item.state === 'Rejected'">
							<p class="mt-2">
								Transfer rejected
							</p>
							<div class="flex flex-row justify-end gap-4 mt-1">
								<p
									@click.stop="removeRequest(vm, item.id)" class="btn px-3
									rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
									Clear
								</p>
							</div>
						</div>

						<div v-else-if="item.state === 'Disconnected'">
							<p class="mt-2">
								Unexpected disconnection
							</p>
							<div class="flex flex-row justify-end gap-4 mt-1">
								<p
									@click.stop="removeRequest(vm, item.id)" class="btn px-3
									rounded-xl active:scale-95 transition duration-150 ease-in-out shadow-none">
									Clear
								</p>
							</div>
						</div>
					</div>
				</div>
			</div>
		</div>
	</div>
</template>

<script lang="ts">
import { ref, nextTick } from 'vue'
import { UnlistenFn, listen } from '@tauri-apps/api/event'
import { invoke } from '@tauri-apps/api/core'
import { getVersion } from '@tauri-apps/api/app';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { getStore } from "@tauri-apps/plugin-store";
import { isPermissionGranted, requestPermission } from '@tauri-apps/plugin-notification';
import { disable, enable } from '@tauri-apps/plugin-autostart';
import { open as tauriDialog } from '@tauri-apps/plugin-dialog';
import { openUrl as openExternalUrl } from '@tauri-apps/plugin-opener';
import { writeText, readText } from '@tauri-apps/plugin-clipboard-manager';

import { ChannelMessage } from '@martichou/core_lib/bindings/ChannelMessage';
import { ChannelAction } from '@martichou/core_lib/bindings/ChannelAction';
import { EndpointInfo } from '@martichou/core_lib/bindings/EndpointInfo';
import { OutboundPayload } from '@martichou/core_lib/bindings/OutboundPayload';
import { Visibility } from '@martichou/core_lib/bindings/Visibility';

import { ToastNotification, stateToDisplay, autostartKey, DisplayedItem, useToastStore, opt, ToastType, utils } from '../vue_lib';
import { transferInProgress } from '../vue_lib/updater';

import SettingsModal from '../composables/SettingsModal.vue';
import Heading from '../composables/Heading.vue';
import SideMenu from '../composables/SideMenu.vue';
import ContentStatus from '../composables/ContentStatus.vue';
import ItemSide from '../composables/ItemSide.vue';
import FileList from '../composables/FileList.vue';

export default {
	name: "HomePage",

	components: {
		ToastNotification,
		SettingsModal,
		Heading,
		SideMenu,
		ContentStatus,
		ItemSide,
		FileList
	},

	async setup() {
		const store = (await getStore(".settings.json"))!;
		const toastStore = useToastStore();

		const dialogOpen = tauriDialog;

		return {
			stateToDisplay,
			store,
			toastStore,
			invoke,
			getVersion,
			enable,
			disable,
			dialogOpen,
			...utils
		};
	},

	data() {
		return {
			discoveryRunning: ref(false),
			isDragHovering: ref(false),

			requests: ref<ChannelMessage[]>([]),
			endpointsInfo: ref<EndpointInfo[]>([]),
			outboundPayload: ref<OutboundPayload | undefined>(),
			// SVG QR code returned by start_discovery. Scanning it makes a
			// phone advertise itself to us even when it isn't set to
			// "Everyone" visibility.
			qrSvg: ref<string | undefined>(undefined),
			// Guards against firing a second transfer: a resolved service is
			// re-announced repeatedly.
			qrAutoSent: ref<boolean>(false),

			// Transfer id -> the action already sent for it. Guards against
			// double-clicking Accept, which would otherwise run the whole
			// transfer setup twice.
			responding: ref<Map<string, ChannelAction>>(new Map()),

			// Endpoint ids we've asked the backend to send to and not yet heard
			// back about. Same purpose as `responding`, for the outbound side.
			sending: ref<Set<string>>(new Set()),

			unlisten: Array<UnlistenFn>(),

			version: opt<string>(),

			autostart: ref<boolean>(true),
			realclose: ref<boolean>(false),
			startminimized: ref<boolean>(false),
			nearbyAlerts: ref<boolean>(true),
			clipboardAutosync: ref<boolean>(false),
			debugLevel: ref<string>('info'),
			lastClipboard: ref<string | undefined>(undefined),
			visibility: ref<Visibility>('Visible'),
			downloadPath: ref<string | undefined>(),

			hostname: ref<string>(),
			// The override the user typed, and the hostname we fall back to.
			deviceNameOverride: ref<string | undefined>(undefined),
			hostnameDefault: ref<string | undefined>(undefined),

			settingsOpen: ref<boolean>(false),

			new_version: opt<string>(),
			updateState: ref<'idle' | 'installing' | 'failed'>('idle'),
			updateProgress: ref<number | null>(null),
			updateCheck: ref<'checking' | 'current' | 'available' | 'failed' | null>(null),
			autoUpdate: ref<boolean>(false),
		};
	},

	mounted: function () {
		nextTick(async () => {
			try {
				await this.getDeviceName(this);
				this.version = this.displayVersion(await getVersion());

				await this.getVisibility(this);

				try {
					if (!await this.store.has(autostartKey)) {
						await this.setAutoStart(this, true);
					} else {
						await this.applyAutoStart(this);
					}
				} catch (autostartErr) {
					console.warn('[rqs] autostart unavailable (expected under `tauri dev`):', autostartErr);
				}

				await this.getLoggingLevel(this);
				await this.getRealclose(this);
				await this.getStartMinimized(this);
				await this.getNearbyAlerts(this);
				await this.getAutoUpdate(this);
				await this.getClipboardAutosync(this);
				await this.getDownloadPath(this);
			} catch (e) {
				console.error('[rqs] startup settings init failed (continuing to register listeners):', e);
			}

			// Check permission for notification
			if (!(await isPermissionGranted())) {
				await requestPermission();
			}

			this.unlisten.push(
				await listen('rs2js_channelmessage', async (event) => {
					const cm = event.payload as ChannelMessage;
					const idx = this.requests.findIndex((el) => el.id === cm.id);

					// The card has moved on (or died), so let the buttons work again
					// if this id ever comes back for another transfer.
					if (cm.state && cm.state !== "WaitingForUserConsent") {
						this.responding.delete(cm.id);
					}

					// Drop the "Connecting..." spinner once the transfer has a card
					// of its own to report through. The intermediate handshake
					// states aren't displayed, so waiting for one of those would
					// blank the card out mid-connect.
					if (cm.state && this.stateToDisplay.includes(cm.state)) {
						this.sending.delete(cm.id);
					}

					// TODO - Automatically open || copy to clipboard + toast

					if (idx !== -1) {
						const prev = this.requests.at(idx);
						// Update the existing message at index 'idx'
						this.requests.splice(idx, 1, {
							...cm,
							state: cm.state ?? prev!.state,
							meta: cm.meta ?? prev!.meta,
						});
					} else {
						// Push the new message if not found
						this.requests.push(cm);
					}

					return;
				})
			);

			this.unlisten.push(
				await listen('rs2js_endpointinfo', async (event) => {
					const ei = event.payload as EndpointInfo;
					const idx = this.endpointsInfo.findIndex((el) => el.id === ei.id);

					if (!ei.present) {
						if (idx !== -1) {
							this.endpointsInfo.splice(idx, 1);
						}

						return;
					}

					if (idx !== -1) {
						this.endpointsInfo.splice(idx, 1, ei);
					} else {
						this.endpointsInfo.push(ei);
					}

					// Scanning our QR is the user already choosing this device,
					// so send straight away rather than making them pick it from
					// the list.
					if (ei.qr_match && this.outboundPayload !== undefined && !this.qrAutoSent) {
						this.qrAutoSent = true;
						this.qrSvg = undefined;
						await this.startSend(ei.id);
					}
				})
			);

			this.unlisten.push(
				await listen('visibility_updated', async () => {
					console.log("Visibility changed");
					await this.getVisibility(this);
				})
			);

			this.unlisten.push(
				await getCurrentWindow().onDragDropEvent(async (event) => {
					if (event.payload.type === 'over') {
						this.isDragHovering = true;
					} else if (event.payload.type === 'drop') {
						console.log("Dropped");
						this.isDragHovering = false;
						this.outboundPayload = {
							Files: event.payload.paths
						} as OutboundPayload;
						await this.startDiscovery();
					} else {
						this.isDragHovering = false;
					}
				})
			);

			// Ctrl/Cmd+V pastes clipboard text and stages it to send.
			const onPaste = async (e: KeyboardEvent) => {
				if (!(e.ctrlKey || e.metaKey) || (e.key !== 'v' && e.key !== 'V')) return;

				// Ignore paste while typing in an input field.
				const target = e.target as HTMLElement | null;
				if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable)) {
					return;
				}

				let text: string;
				try {
					text = await readText();
				} catch {
					return;
				}
				if (!text || text.trim().length === 0) return;

				this.lastClipboard = text;
				this.outboundPayload = { Text: text } as OutboundPayload;
				await this.startDiscovery();
				this.toastStore.addToast("Text ready to send - pick a device", ToastType.Success);
			};
			window.addEventListener('keydown', onPaste);
			this.unlisten.push(() => window.removeEventListener('keydown', onPaste));

			// Optional clipboard auto-sync: when enabled, stage newly copied text
			// so you only have to pick a device. Checked when the window gains
			// focus - the moment someone who just copied something elsewhere
			// comes back to send it - rather than by polling the clipboard.
			const stageClipboard = async () => {
				if (!this.clipboardAutosync) return;
				if (this.outboundPayload !== undefined) return;

				let text: string;
				try {
					text = await readText();
				} catch {
					return;
				}
				if (!text || text.trim().length === 0) return;
				if (text === this.lastClipboard) return;

				this.lastClipboard = text;
				this.outboundPayload = { Text: text } as OutboundPayload;
				await this.startDiscovery();
				this.toastStore.addToast("Clipboard text staged to send - pick a device", ToastType.Success);
			};
			this.unlisten.push(await getCurrentWindow().onFocusChanged(async ({ payload: focused }) => {
				if (focused) await stageClipboard();
			}));

			await this.checkForUpdate(this);
		});
	},

	unmounted: function() {
		this.unlisten.forEach((el) => el());
	},

	computed: {
		vm() {
			return this;
		},
		displayedIsEmpty(): boolean {
			return this.displayedItems.length == 0
		},
		displayedItems(): Array<DisplayedItem> {
			return this.computeDisplayedItems(this);
		},
		// With automatic updates on, a found update installs as soon as nothing
		// would be cut off by the restart. Watched below, so it fires when the
		// last transfer ends rather than being polled for.
		autoInstallReady(): boolean {
			return this.autoUpdate && !!this.new_version && this.updateState === 'idle'
				&& !transferInProgress(this.requests, this.sending.size);
		},
		// Rendered as an <img> rather than v-html: no raw HTML injection.
		qrDataUri(): string | undefined {
			if (!this.qrSvg) return undefined;

			return `data:image/svg+xml;utf8,${encodeURIComponent(this.qrSvg)}`;
		}
	},

	watch: {
		autoInstallReady(ready: boolean) {
			if (ready) this.installUpdate(this);
		}
	},

	methods: {
		// Discovery is what makes us visible and produces the pairing QR. It is
		// idempotent from the caller's side: several UI paths stage a payload and
		// all of them need discovery up, but only the first should actually start
		// it. Kept here rather than in the children so `qrSvg` has exactly one
		// writer - children ask for discovery by emitting, they don't reach into
		// the shared vm to set it.
		startDiscovery: async function() {
			if (this.discoveryRunning) return;
			// Set before the await: a paste and a drop in quick succession
			// otherwise both saw it false and started discovery twice.
			this.discoveryRunning = true;
			try {
				this.qrSvg = await invoke<string>('start_discovery');
			} catch (e) {
				this.discoveryRunning = false;
				throw e;
			}
		},
		pluralize: function(n: number, noun: string): string {
			return `${n} ${n === 1 ? noun : `${noun}s`}`;
		},
		writeToClipboard: async function(text: string) {
			try {
				await writeText(text);
				this.toastStore.addToast("Copied to clipboard", ToastType.Success);
			} catch (e) {
				this.toastStore.addToast("Unknown error while copying text", ToastType.Error);
				console.error("Error copying text", e);
			}
		},
		// Cancelling the whole send drops the staged payload and the device list,
		// so any spinner still up belongs to a device that is about to vanish.
		stopSending: async function() {
			this.sending.clear();
			await this.clearSending(this);
		},
		// Clicking a device only queues the transfer; the backend then has to
		// connect and get through the handshake before anything about it is
		// displayable. Hold a spinner over that gap, and refuse a second click
		// for the same device while it's open - the backend ignores duplicates
		// anyway, so a second click could only ever be a wasted one.
		startSend: async function(id: string) {
			if (this.sending.has(id)) return;
			this.sending.add(id);

			// The spinner ends with the transfer's first displayable state, or
			// with Disconnected, which the backend sends when it cannot connect.
			// Cancel clears it too.
			try {
				await this.sendInfo(this, id);
			} catch (e) {
				this.sending.delete(id);
				// A rejected Tauri command surfaces its `Err(String)` as a raw
				// string, not an Error, so handle both before falling back.
				let msg = "Could not start the transfer";
				if (typeof e === 'string') {
					msg = e;
				} else if (e instanceof Error) {
					msg = e.message;
				}
				this.toastStore.addToast(msg, ToastType.Error);
				console.error("Error sending to endpoint", e);
			}
		},
		respondToTransfer: async function(id: string, action: ChannelAction) {
			// The state only changes once the backend answers, so without this
			// guard an impatient second click sends AcceptTransfer twice.
			if (this.responding.has(id)) return;
			this.responding.set(id, action);

			try {
				await this.sendCmd(this, id, action);
			} catch (e) {
				// Let them try again rather than stranding the card on a spinner.
				this.responding.delete(id);
				this.toastStore.addToast("Could not answer the transfer", ToastType.Error);
				console.error("Error responding to transfer", e);
			}
		},
		// Web links only. `destination` is a filesystem path and must not come
		// through here - see openTransfer.
		openUrl: async function(url: string) {
			try {
				await openExternalUrl(url);
			} catch (e) {
				this.toastStore.addToast("Error opening URL, it may not be a valid URI", ToastType.Error);
				console.error("Error opening URL", e);
			}
		},
		// "Open" on a received transfer. A shared URL payload is a web link and
		// goes to openUrl. Files go through the backend, which knows what this
		// transfer saved: it opens a single file, shows several in their
		// folder, and shows rather than runs anything executable, since the
		// sender could be anyone nearby.
		openTransfer: async function(item: DisplayedItem) {
			if (item.text_type === 'Url' && item.text_payload) {
				await this.openUrl(item.text_payload);
				return;
			}

			try {
				const outcome = await invoke<string>('open_received', { id: item.id });
				if (outcome === 'revealed' && (item.files ?? []).length === 1) {
					this.toastStore.addToast("This file could run code, so it's shown in its folder instead", ToastType.Info);
				}
			} catch (e) {
				this.toastStore.addToast("Could not open the received file", ToastType.Error);
				console.error("Error opening transfer", e);
			}
		},
		// "Open folder" on a received transfer: its files, selected in the file
		// manager.
		openFolder: async function(item: DisplayedItem) {
			try {
				await invoke('reveal_received', { id: item.id });
			} catch (e) {
				this.toastStore.addToast("Could not open the download folder", ToastType.Error);
				console.error("Error opening folder", e);
			}
		},
	},
}
</script>