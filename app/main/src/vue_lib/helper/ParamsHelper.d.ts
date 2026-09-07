import { Store } from '@tauri-apps/plugin-store';
import { UnlistenFn } from '@tauri-apps/api/event';

import { ToDelete } from '../types';

import { EndpointInfo } from '@martichou/core_lib/bindings/EndpointInfo';
import { Visibility } from '@martichou/core_lib/bindings/Visibility';
import { OutboundPayload } from '@martichou/core_lib/bindings/OutboundPayload';
import { ChannelMessage } from '@martichou/core_lib/bindings/ChannelMessage';

export interface TauriVM {
	store: Store;
    isAppInForeground: boolean;
    discoveryRunning: boolean;
    qrSvg: string | undefined;
    qrAutoSent: boolean;
    debugLevel: string;
    isDragHovering: boolean;
    requests: ChannelMessage[];
    endpointsInfo: EndpointInfo[];
    toDelete: ToDelete[];
    outboundPayload: OutboundPayload | undefined;
    unlisten: Array<UnlistenFn>;
    version: string | null;
    autostart: boolean;
    realclose: boolean;
    startminimized: boolean;
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