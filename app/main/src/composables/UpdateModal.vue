<script setup lang="ts">
import { PropType } from 'vue';
import { TauriVM } from '../vue_lib/helper/ParamsHelper';
import { useModalDialog } from '../vue_lib/modalDialog';

const props = defineProps({
	vm: {
		type: Object as PropType<TauriVM>,
		required: true
	},
	openUrl: {
		type: Function as PropType<(url: string) => void>,
		required: true
	}
});

const emit = defineEmits(['close']);

const close = () => emit('close');
useModalDialog(() => props.vm.updateOpen, close);

// The newest release's page, where its installers are.
const latestUrl = () => props.vm.releaseNotes[0]?.url;

function openRelease() {
	const url = latestUrl();
	if (url) props.openUrl(url);
	close();
}
</script>

<template>
	<div
		v-if="vm.updateOpen" class="absolute z-10 w-full h-full flex justify-center items-center bg-black/25"
		@click.self="close()">
		<div
			ref="dialog" role="dialog" aria-modal="true" aria-labelledby="update-title"
			tabindex="-1"
			class="bg-white dark:bg-neutral-800 rounded-xl shadow-xl p-4 w-[26rem] max-h-[calc(100%-2rem)] flex flex-col focus:outline-hidden">
			<h3 id="update-title" class="font-medium text-xl">
				Update available
			</h3>
			<p class="text-sm opacity-70 mt-1">
				v{{ vm.version }} → v{{ vm.new_version }}
			</p>

			<div class="overflow-y-auto mt-4 flex flex-col gap-4">
				<section v-for="release in vm.releaseNotes" :key="release.version">
					<h4 class="font-medium">
						v{{ release.version }}
					</h4>
					<ul v-if="release.changes.length" class="list-disc pl-5 text-sm mt-1">
						<li v-for="(change, i) in release.changes" :key="i">
							<button
								v-if="change.url" type="button" class="text-left align-top hover:underline"
								:title="change.url" @click="openUrl(change.url)">
								{{ change.text }}
							</button>
							<span v-else>{{ change.text }}</span>
						</li>
					</ul>
					<p v-else class="text-sm opacity-70 mt-1">
						No notes for this release.
					</p>
				</section>
			</div>

			<div class="flex justify-end gap-2 mt-4">
				<button type="button" class="btn px-3 rounded-xl active:scale-95 transition duration-150 ease-in-out" @click="close()">
					Not now
				</button>
				<button type="button" class="btn px-3 rounded-xl active:scale-95 transition duration-150 ease-in-out" @click="openRelease()">
					Open release page
				</button>
			</div>
		</div>
	</div>
</template>
