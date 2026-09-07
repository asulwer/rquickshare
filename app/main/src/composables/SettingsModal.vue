<script setup lang="ts">
import { loggingLevels, utils } from '../vue_lib';
import { PropType, ref, watch } from 'vue';
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
</script>

<template>
	<div v-if="vm.settingsOpen" class="absolute z-10 w-full h-full flex justify-center items-center bg-black bg-opacity-25">
		<div class="bg-white dark:bg-neutral-800 rounded-xl shadow-xl p-4 w-[24rem]">
			<div class="flex flex-row justify-between items-center">
				<h3 class="font-medium text-xl">
					Settings
				</h3>
				<div class="btn px-3 rounded-xl active:scale-95 transition duration-150 ease-in-out" @click="emit('close')">
					Close
				</div>
			</div>
			<div class="py-4 flex flex-col">
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="flex flex-col items-start gap-1">
						<span class="flex flex-row justify-between items-center w-full">
							<span class="label-text">Device name</span>
							<span
								v-if="vm.deviceNameOverride" class="text-xs cursor-pointer underline opacity-70"
								@click="resetDeviceName()">
								Reset
							</span>
						</span>
						<input
							v-model="nameDraft" type="text" :placeholder="vm.hostnameDefault ?? 'This device'"
							maxlength="64"
							class="w-full rounded-xl bg-white dark:bg-neutral-700 border border-gray-500 border-opacity-30 px-2 py-1 text-sm focus:outline-none"
							@blur="commitDeviceName()" @keyup.enter="commitDeviceName()">
						<span class="text-xs opacity-70">
							What nearby devices show. Empty follows the hostname.
						</span>
					</label>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="cursor-pointer flex flex-row justify-between items-center" @click="utils.setAutoStart(vm, !vm.autostart)">
						<span class="label-text">Start on boot</span>
						<input type="checkbox" :checked="vm.autostart" class="checkbox focus:outline-none">
					</label>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="cursor-pointer flex flex-row justify-between items-center" @click="utils.setRealClose(vm, !vm.realclose)">
						<span class="label-text">Keep running on close</span>
						<input type="checkbox" :checked="!vm.realclose" class="checkbox focus:outline-none">
					</label>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="cursor-pointer flex flex-row justify-between items-center" @click="utils.setStartMinimized(vm, !vm.startminimized)">
						<span class="label-text">Start minimized</span>
						<input type="checkbox" :checked="vm.startminimized" class="checkbox focus:outline-none">
					</label>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="cursor-pointer flex flex-row justify-between items-center" @click="utils.setClipboardAutosync(vm, !vm.clipboardAutosync)">
						<span class="label-text">Auto-stage clipboard text</span>
						<input type="checkbox" :checked="vm.clipboardAutosync" class="checkbox focus:outline-none">
					</label>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="flex flex-row justify-between items-center gap-3">
						<span class="flex flex-col items-start">
							<span class="label-text">Logging level</span>
							<span class="text-xs opacity-70">Applies immediately. Use "trace" to capture a problem.</span>
						</span>
						<select
							class="rounded-xl bg-transparent border border-gray-500 border-opacity-30 px-2 py-1 text-sm cursor-pointer focus:outline-none"
							:value="vm.debugLevel"
							@change="utils.setLoggingLevel(vm, ($event.target as HTMLSelectElement).value)">
							<option v-for="level in loggingLevels" :key="level" :value="level" class="text-black">
								{{ level }}
							</option>
						</select>
					</label>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="cursor-pointer flex flex-col items-start" @click="openDownloadPicker()">
						<span class="">Change download folder</span>
						<span class="overflow-hidden whitespace-nowrap text-ellipsis text-xs max-w-80">
							> {{ vm.downloadPath ?? 'OS User\'s download folder' }}
						</span>
					</label>
				</div>
			</div>
		</div>
	</div>
</template>