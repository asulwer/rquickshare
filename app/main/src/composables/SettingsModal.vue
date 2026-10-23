<script setup lang="ts">
import { loggingLevels, utils } from '../vue_lib';
import { PropType, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { TauriVM } from '../vue_lib/helper/ParamsHelper';

const props = defineProps({
	vm: {
		type: Object as PropType<TauriVM>,
		required: true
	}
});

const emit = defineEmits(['close']);

// Edited locally and committed on blur or Enter, rather than on every
// keystroke: each commit re-announces over mDNS, and doing that per character
// would spam the network and make peers flicker through half-typed names.
const nameDraft = ref(props.vm.deviceNameOverride ?? '');

watch(() => props.vm.deviceNameOverride, (v) => {
	nameDraft.value = v ?? '';
});

async function commitDeviceName() {
	const next = nameDraft.value.trim();
	if (next === (props.vm.deviceNameOverride ?? '')) {
		return;
	}

	await utils.setDeviceName(props.vm, next);
}

async function resetDeviceName() {
	nameDraft.value = '';
	await utils.setDeviceName(props.vm, undefined);
}

function openDownloadPicker() {
	props.vm.dialogOpen({
		title: "Select the destination for files",
		directory: true,
		multiple: false,
	}).then(async (el) => {
		if (el === null) {
			return;
		}

		await utils.setDownloadPath(props.vm, el as string);
	});
}

const checked = (e: Event) => (e.target as HTMLInputElement).checked;

const dialog = ref<HTMLElement | null>(null);
// Whatever had focus when the dialog opened (the gear button), so closing
// hands focus back instead of dropping it on <body>.
let returnFocus: HTMLElement | null = null;

watch(() => props.vm.settingsOpen, async (open) => {
	if (open) {
		returnFocus = document.activeElement as HTMLElement | null;
		await nextTick();
		dialog.value?.focus();
	} else {
		returnFocus?.focus();
		returnFocus = null;
	}
});

// Every way out goes through here. Closing removes the name field without
// blurring it, so a name typed and then dismissed with Escape would otherwise
// be lost; the Close button only kept it because clicking it blurred the field.
async function close() {
	await commitDeviceName();
	emit('close');
}

function onKeydown(e: KeyboardEvent) {
	if (!props.vm.settingsOpen) return;

	if (e.key === 'Escape') {
		e.preventDefault();
		close();
		return;
	}

	// Keep Tab inside the dialog: the page behind it is covered and can't be used.
	if (e.key === 'Tab' && dialog.value) {
		const focusable = Array.from(dialog.value.querySelectorAll<HTMLElement>('button, input, select'));
		if (focusable.length === 0) return;

		const first = focusable[0];
		const last = focusable[focusable.length - 1];
		const active = document.activeElement;
		if (e.shiftKey && (active === first || active === dialog.value)) {
			e.preventDefault();
			last.focus();
		} else if (!e.shiftKey && active === last) {
			e.preventDefault();
			first.focus();
		}
	}
}

onMounted(() => window.addEventListener('keydown', onKeydown));
onBeforeUnmount(() => window.removeEventListener('keydown', onKeydown));
</script>

<template>
	<div
		v-if="vm.settingsOpen" class="absolute z-10 w-full h-full flex justify-center items-center bg-black bg-opacity-25"
		@click.self="close()">
		<div
			ref="dialog" role="dialog" aria-modal="true" aria-labelledby="settings-title"
			tabindex="-1"
			class="bg-white dark:bg-neutral-800 rounded-xl shadow-xl p-4 w-[26rem] max-h-[calc(100%-2rem)] overflow-y-auto focus:outline-none">
			<div class="flex flex-row justify-between items-center">
				<h3 id="settings-title" class="font-medium text-xl">
					Settings
				</h3>
				<button type="button" class="btn px-3 rounded-xl active:scale-95 transition duration-150 ease-in-out" @click="close()">
					Close
				</button>
			</div>

			<h4 class="settings-section">
				Device
			</h4>
			<div class="settings-row !flex-col !items-start !gap-1">
				<span class="flex flex-row justify-between items-center w-full">
					<label for="settings-device-name">Device name</label>
					<button
						v-if="vm.deviceNameOverride" type="button" class="text-xs underline opacity-70
						transition duration-150 ease-in-out hover:opacity-100 active:opacity-40"
						@click="resetDeviceName()">
						Reset
					</button>
				</span>
				<input
					id="settings-device-name" v-model="nameDraft" type="text" :placeholder="vm.hostnameDefault ?? 'This device'"
					maxlength="64" aria-describedby="settings-device-name-hint"
					class="settings-field w-full"
					@blur="commitDeviceName()" @keyup.enter="commitDeviceName()">
				<span id="settings-device-name-hint" class="text-xs opacity-70">
					What nearby devices show. Leave empty to use the hostname.
				</span>
			</div>
			<div class="settings-row">
				<span class="flex flex-col min-w-0">
					<span>Save received files to</span>
					<span class="text-xs opacity-70 overflow-hidden whitespace-nowrap text-ellipsis" :title="vm.downloadPath">
						{{ vm.downloadPath ?? 'Your Downloads folder' }}
					</span>
				</span>
				<button type="button" class="btn !h-9 !min-h-9 px-3 shrink-0" @click="openDownloadPicker()">
					Change…
				</button>
			</div>

			<h4 class="settings-section">
				Startup
			</h4>
			<label class="settings-row cursor-pointer">
				<span>Start on boot</span>
				<input
					type="checkbox" role="switch" class="switch" :checked="vm.autostart"
					@change="utils.setAutoStart(vm, checked($event))">
			</label>
			<label class="settings-row cursor-pointer">
				<span>Start minimized</span>
				<input
					type="checkbox" role="switch" class="switch" :checked="vm.startminimized"
					@change="utils.setStartMinimized(vm, checked($event))">
			</label>
			<label class="settings-row cursor-pointer">
				<span class="flex flex-col">
					<span>Keep running when closed</span>
					<span class="text-xs opacity-70">Closing the window leaves RQuickShare in the tray, ready to receive.</span>
				</span>
				<input
					type="checkbox" role="switch" class="switch" :checked="!vm.realclose"
					@change="utils.setRealClose(vm, !checked($event))">
			</label>

			<h4 class="settings-section">
				Advanced
			</h4>
			<label class="settings-row cursor-pointer">
				<span class="flex flex-col">
					<span>Auto-stage clipboard text</span>
					<span class="text-xs opacity-70">New text you copy is staged to send, so you only pick a device.</span>
				</span>
				<input
					type="checkbox" role="switch" class="switch" :checked="vm.clipboardAutosync"
					@change="utils.setClipboardAutosync(vm, checked($event))">
			</label>
			<label class="settings-row cursor-pointer">
				<span class="flex flex-col">
					<span>Notify me when a nearby device is sharing</span>
					<span class="text-xs opacity-70">
						Also shows this device right away to a phone that starts sharing; with it off, that can take up to a minute.
						On macOS, and on Linux without BlueZ's experimental features, it can stop Bluetooth mice and keyboards from reconnecting.
					</span>
				</span>
				<input
					type="checkbox" role="switch" class="switch" :checked="vm.nearbyAlerts"
					@change="utils.setNearbyAlerts(vm, checked($event))">
			</label>
			<label class="settings-row">
				<span class="flex flex-col">
					<span>Logging level</span>
					<span class="text-xs opacity-70">Applies immediately. Use "trace" to capture a problem.</span>
				</span>
				<select
					class="settings-field cursor-pointer"
					:value="vm.debugLevel"
					@change="utils.setLoggingLevel(vm, ($event.target as HTMLSelectElement).value)">
					<!-- The popup is drawn by the OS, so the options need their own
					     colours - inheriting leaves black-on-dark in dark mode. -->
					<option
						v-for="level in loggingLevels" :key="level" :value="level"
						class="bg-white text-gray-900 dark:bg-neutral-700 dark:text-gray-200">
						{{ level }}
					</option>
				</select>
			</label>
		</div>
	</div>
</template>
