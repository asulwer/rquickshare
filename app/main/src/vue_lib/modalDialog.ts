import { nextTick, onBeforeUnmount, onMounted, useTemplateRef, watch } from 'vue';

// Focus handling shared by the app's modal dialogs: focus moves into the
// dialog when it opens and back when it closes, Escape closes it, and Tab stays
// inside it. The dialog element is the one with ref="dialog".
export function useModalDialog(isOpen: () => boolean, close: () => void) {
	const dialog = useTemplateRef<HTMLElement>('dialog');
	// Whatever had focus when the dialog opened (the button that opened it), so
	// closing hands focus back instead of dropping it on <body>.
	let returnFocus: HTMLElement | null = null;

	watch(isOpen, async (open) => {
		if (open) {
			returnFocus = document.activeElement as HTMLElement | null;
			await nextTick();
			dialog.value?.focus();
		} else {
			returnFocus?.focus();
			returnFocus = null;
		}
	});

	function onKeydown(e: KeyboardEvent) {
		if (!isOpen()) return;

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
}
