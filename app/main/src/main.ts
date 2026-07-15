import { devtools } from '@vue/devtools'
import { createApp } from 'vue'
import { createPinia } from 'pinia'

import './vue_lib/assets/main.postcss'

import App from './App.vue'

// Only dial the standalone Vue devtools when it was actually started. `pnpm dev`
// launches it and sets this flag; `pnpm tauri dev` does not, and without the
// guard socket.io retries forever and floods the console with connection errors.
if (process.env.NODE_ENV === 'development' && import.meta.env.VITE_VUE_DEVTOOLS === '1') {
	devtools.connect('http://localhost', 8098)
}

const pinia = createPinia();
const app = createApp(App)

app.use(pinia);

app.mount('#app')
