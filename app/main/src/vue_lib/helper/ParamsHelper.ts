import type { Store } from '@tauri-apps/plugin-store';
import type { InvokeArgs } from '@tauri-apps/api/core';
import type { UnlistenFn } from '@tauri-apps/api/event';

import type { DisplayedItem } from '../types';

import type { EndpointInfo } from '@martichou/core_lib/bindings/EndpointInfo';
import type { Visibility } from '@martichou/core_lib/bindings/Visibility';
import type { OutboundPayload } from '@martichou/core_lib/bindings/OutboundPayload';
import type { ChannelMessage } from '@martichou/core_lib/bindings/ChannelMessage';

export interface TauriVM {
	store: Store;
    discoveryRunning: boolean;
    qrSvg: string | undefined;
    qrAutoSent: boolean;
    debugLevel: string;
    isDragHovering: boolean;
    requests: ChannelMessage[];
    // Ids of the devices a send is connecting to.
    sending: Set<string>;
    endpointsInfo: EndpointInfo[];
    outboundPayload: OutboundPayload | undefined;
    unlisten: Array<UnlistenFn>;
    version: string | null;
    autostart: boolean;
    realclose: boolean;
    startminimized: boolean;
    nearbyAlerts: boolean;
    clipboardAutosync: boolean;
    visibility: Visibility;
    downloadPath: string | undefined;
    hostname: string | undefined;
    // The user's chosen name, or undefined when following the hostname. Kept
    // apart from `hostname` (the effective advertised name) so the settings
    // field can show the hostname as a placeholder rather than as typed text.
    deviceNameOverride: string | undefined;
    hostnameDefault: string | undefined;
    settingsOpen: boolean;
    new_version: string | null;
    // 'installing' from the click until the app restarts, 'waiting' while a
    // transfer holds up the install or the restart, 'failed' if the download or
    // install didn't finish.
    updateState: 'idle' | 'installing' | 'waiting' | 'failed';
    // Percent downloaded, or null while the size is unknown.
    updateProgress: number | null;
    // The last check's outcome, for Settings. null until one has run.
    updateCheck: 'checking' | 'current' | 'available' | 'failed' | null;
    // Install updates without asking, once no transfer is running.
    autoUpdate: boolean;
    enable: () => Promise<void>;
    disable: () => Promise<void>;
    invoke: (cmd: string, args?: InvokeArgs) => Promise<unknown>
    setVisibility: (vm: TauriVM, visibility: Visibility) => Promise<void>;

    displayedIsEmpty: boolean;
    displayedItems: DisplayedItem[];

    // Remapped function for compatibility with Tauri v1 and v2
    dialogOpen: (options?: {
        title: string,
        directory: boolean,
        multiple: boolean,
    }) => Promise<unknown>;
}