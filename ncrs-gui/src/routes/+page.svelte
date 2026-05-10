<script lang="ts">
    import '../app.css';

    import Icon from '../components/Icon.svelte';

    import { invoke } from "@tauri-apps/api/core";
    import { listen } from "@tauri-apps/api/event";

    import { mdiAlertCircleOutline, mdiInformationVariantCircleOutline,
        mdiMagnify, mdiFolder, mdiAppsBox,
        mdiPlus, mdiAccountCog,
        mdiMessageText,
        mdiChevronDown, mdiClose,
    } from '@mdi/js';

    import { onMount } from 'svelte';

    import SetStatusView from './SetStatusView.svelte';
    import SyncProgressView from './SyncProgressView.svelte';

    interface UserInfo {
        username: string;
        server_url: string;
        mount_point: string;
    }

    let userInfo = $state<UserInfo | null>(null);
    let syncState = $state<string>("idle");

    // Placeholder notifications — will be replaced by NC Notifications API in a future step.
    let notifications = $state([
        { id: 1, message: "New file uploaded!", description: "File 'report.pdf' has been successfully uploaded.", type: "info", icon: mdiInformationVariantCircleOutline},
        { id: 2, message: "Important system updates are available.", description: "Please update your system to the latest version.", type: "info", icon: mdiAlertCircleOutline, action: () => {}, cta: "View"},
        { id: 3, message: "Test user sent a message to ABCD", description: "Hello team! What are we working on today?", type: "info", icon: mdiMessageText, avatar: "https://avatars.githubusercontent.com/u/5474117?v=4"},
    ]);

    async function close() {
        await invoke("close_window");
    }

    async function openFolder() {
        await invoke("open_mount_folder");
    }

    async function loadInfo() {
        userInfo = await invoke<UserInfo | null>("get_user_info");
        syncState = await invoke<string>("get_sync_state");
    }

    onMount(() => {
        loadInfo();

        // React to state changes pushed from the tray menu.
        const unlisten = listen<string>("sync-state-changed", (e) => {
            syncState = e.payload;
        });

        const clickOutListener = (event: MouseEvent) => {
            const container = document.querySelector(".container");
            if (container && !container.contains(event.target as Node)) {
                close();
            }
        };
        document.addEventListener("click", clickOutListener);

        const escKeyListener = (event: KeyboardEvent) => {
            if (event.key === "Escape") close();
        };
        document.addEventListener("keydown", escKeyListener);

        return () => {
            unlisten.then(f => f());
            document.removeEventListener("click", clickOutListener);
            document.removeEventListener("keydown", escKeyListener);
        };
    });
</script>

<!-- Root is the entire computer window, window is the 'virtual' window we style to bypass wayland positioning limitations -->
<main class="window select-none">
    <div class="grid grid-rows-[6rem_auto] h-full w-full">
        <!-- Header -->
        <div class="grid grid-rows-2 grid-cols-[auto_auto] items-center justify-between p-4 bg-base-300 rounded-t-lg">
            
            <div class="flex items-center row-span-2">
                <div class="dropdown dropdown-hover dropdown-center cursor-pointer" title="Set status">
                    <div tabindex="0" role="button" class="avatar">
                        <div class="w-12 rounded-full">
                            <img src="https://avatars.githubusercontent.com/u/5474117?v=4" alt="Avatar" />
                        </div>
                        <!-- Overlay status circle bottom right -->
                        <div class="absolute bottom-0 right-0 w-4 h-4 bg-green-500 border-2 border-white rounded-full"></div>
                    </div>
                    
                    <!-- Set account status -->
                    <SetStatusView class="dropdown-content z-[1] menu shadow bg-base-100 rounded-box" />
                </div>

                <div class="dropdown dropdown-start">
                    <!-- svelte-ignore a11y_no_noninteractive_element_to_interactive_role -->
                    <h1 tabindex="0" role="button" class="ml-2 font-bold cursor-pointer">
                        {userInfo?.username ?? "…"} <Icon class="w-4 h-4 inline-block" path={mdiChevronDown} />
                    </h1>

                    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
                    <ul tabindex="0" class="dropdown-content menu bg-base-100 rounded-box z-1 w-52 p-2 shadow-sm">
                        <li><button onclick={() => false}>
                            <Icon class="w-4 h-4 inline-block mr-2 align-baseline" path={mdiAccountCog} /> {userInfo?.username ?? "—"}
                        </button></li>
                        <li class="text-xs text-gray-400 px-2 py-1 truncate">{userInfo?.server_url ?? ""}</li>
                        <li><button onclick={() => false}>
                            <Icon class="w-4 h-4 inline-block mr-2 align-baseline" path={mdiPlus} /> Add account
                        </button></li>
                    </ul>
                </div>
                
                <!-- <span class="text-gray-300">Status: connected</span> -->
            </div>
            
            <!-- Close button -->
            <button class="place-self-end self-start -mt-4 -mr-4 w-8 h-8 px-1 rounded-tr-[16px] rounded-bl-xl cursor-pointer bg-primary-content" onclick={close} aria-label="Close">
                <Icon class="w-6 h-6" path={mdiClose} />
            </button>
            
            <!-- Toolbar -->
            <div>
                <button class="btn btn-ghost btn-sm rounded-btn" aria-label="Search">
                    <Icon class="w-6 h-6" path={mdiMagnify} />
                </button>
                <button class="btn btn-ghost btn-sm rounded-btn" aria-label="Open Containing Folder" onclick={openFolder}>
                    <Icon class="w-6 h-6" path={mdiFolder} />
                </button>
                <button class="btn btn-ghost btn-sm rounded-btn" aria-label="Notifications">
                    <Icon class="w-6 h-6" path={mdiAppsBox} />
                </button>
            </div>
        </div>

        <!-- Sync status -->
        <SyncProgressView {syncState} />
    
        <!-- Notifications -->
        <div class="overflow-y-auto p-2 flex-grow">
            {#each notifications as notification (notification.id) }
            <div class="alert shadow-none rounded-none border-0 [:not(:last-child)]:border-b [:not(:last-child)]:mb-2 pb-4 border-gray-200 relative">
                <div class="flex items-center gap-1 justify-start">
                    {#if notification.avatar}
                    <div tabindex="0" role="button" class="avatar">
                        <div class="w-8 rounded-full">
                            <img src="https://avatars.githubusercontent.com/u/5474117?v=4" alt="Avatar" />
                        </div>

                        <!-- Overlay status circle bottom right -->
                        <!-- <div class="absolute bottom-0 right-0 w-4 h-4 bg-green-500 border-2 border-white rounded-full"></div> -->
                        <Icon class="absolute bottom-0 right-0 w-4 h-4 border-2 border-white bg-white" path={notification.icon} />
                    </div>
                    {:else}
                    <Icon class="inline-block w-8 h-8 mr-2" path={notification.icon} />
                    {/if}
                    <div>
                        <h3 class="font-bold">{notification.message}</h3>
                        <p>{notification.description}</p>
                    </div>
                </div>

                <div class="flex-none self-end justify-self-end">
                    <time class="text-xs text-gray-400 absolute top-2 right-4">1m ago</time>
                    {#if notification.action || notification.cta}
                    <button class="btn btn-sm" onclick={notification.action ?? (() => undefined)}>{notification.cta ?? "Open"}</button>
                    {/if}
                </div>
            </div>
            {/each}
        </div>
    </div>
</main>

<style>
:root {
    font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
    font-size: 16px;
    line-height: 24px;
    font-weight: 400;
    
    color: #0f0f0f;
    /* background-color: #f6f6f6; */
    background-color: transparent;
    
    font-synthesis: none;
    text-rendering: optimizeLegibility;
    -webkit-font-smoothing: antialiased;
    -moz-osx-font-smoothing: grayscale;
    -webkit-text-size-adjust: 100%;
}

.window {
    margin: 0;
    
    background-color: #f6f6f6;
    position: absolute;
    right: 20px;
    top: 20px;
    width: min(400px, 40vw);
    height: min(700px, 60vh);
    
    /* Round like a nice UX window, add border/shadow */
    border-radius: 16px;
    box-shadow: 0 2px 10px rgba(0, 0, 0, 0.1);
    border: 1px solid #eaeaea;
}
/* 
@media (prefers-color-scheme: dark) {
.window {
color: #f6f6f6;
background-color: #2f2f2f;
}
} */

</style>
