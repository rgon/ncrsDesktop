<script lang="ts">
    import '../app.css';

    import Icon from '../components/Icon.svelte';

    import { invoke } from "@tauri-apps/api/core";
    import { listen } from "@tauri-apps/api/event";

    import {
        mdiFolder, mdiAppsBox, mdiPlus, mdiAccountCog,
        mdiChevronDown, mdiClose, mdiMagnify, mdiBell,
        mdiBellOutline, mdiAlertCircleOutline, mdiSwapHorizontal,
    } from '@mdi/js';

    import { onMount } from 'svelte';

    import SetStatusView from './SetStatusView.svelte';
    import SyncProgressView from './SyncProgressView.svelte';
    import SearchView from '../components/SearchView.svelte';
    import ErrorsView from '../components/ErrorsView.svelte';
    import ConflictsView from '../components/ConflictsView.svelte';

    // ── Types ─────────────────────────────────────────────────────────────────

    interface UserInfo {
        username: string;
        server_url: string;
        mount_point: string;
        avatar_url: string;
    }

    interface NcAction {
        label: string;
        link: string;
        action_type: string;
        primary: boolean;
    }

    interface NcNotification {
        notification_id: number;
        app: string;
        user: string;
        datetime: string;
        object_type: string;
        object_id: string;
        subject: string;
        message: string;
        link: string;
        icon: string;
        actions: NcAction[];
    }

    interface SyncError {
        path: string;
        kind: string | { ServerError: number };
        message: string;
        timestamp_ms: number;
    }

    interface TransferProgress {
        path: string;
        direction: "Download" | "Upload";
        bytes_done: number;
        total_bytes: number;
    }

    interface ConflictRecord {
        id: number;
        kind: Record<string, unknown>;
        timestamp_ms: number;
        resolved: boolean;
    }

    // ── State ─────────────────────────────────────────────────────────────────

    type View = "notifications" | "search" | "errors" | "conflicts";

    let userInfo = $state<UserInfo | null>(null);
    let syncState = $state<string>("idle");
    let notifications = $state<NcNotification[]>([]);
    let errors = $state<SyncError[]>([]);
    let transfers = $state<TransferProgress[]>([]);
    let conflicts = $state<ConflictRecord[]>([]);
    let pendingMutations = $state(0);
    let avatarError = $state(false);
    let activeView = $state<View>("notifications");

    // ── Helpers ───────────────────────────────────────────────────────────────

    function relativeTime(datetime: string): string {
        const diffMin = Math.floor((Date.now() - new Date(datetime).getTime()) / 60_000);
        if (diffMin < 1) return "just now";
        if (diffMin < 60) return `${diffMin}m ago`;
        const diffHr = Math.floor(diffMin / 60);
        if (diffHr < 24) return `${diffHr}h ago`;
        return `${Math.floor(diffHr / 24)}d ago`;
    }

    function primaryAction(n: NcNotification): NcAction | null {
        return n.actions.find(a => a.primary) ?? n.actions[0] ?? null;
    }

    // ── Commands ──────────────────────────────────────────────────────────────

    async function close() {
        await invoke("close_window");
    }

    async function openFolder() {
        await invoke("open_mount_folder");
    }

    async function dismiss(id: number) {
        await invoke("dismiss_notification", { id });
        notifications = notifications.filter(n => n.notification_id !== id);
    }

    async function openLink(url: string) {
        if (url) await invoke("open_link", { url });
    }

    async function loadInfo() {
        userInfo = await invoke<UserInfo | null>("get_user_info");
        syncState = await invoke<string>("get_sync_state");
        notifications = await invoke<NcNotification[]>("get_notifications");
        errors = await invoke<SyncError[]>("get_errors");
        transfers = await invoke<TransferProgress[]>("get_transfers");
        conflicts = await invoke<ConflictRecord[]>("get_conflicts");
    }

    async function clearErrors() {
        await invoke("clear_errors");
        errors = [];
    }

    async function resolveConflict(id: number) {
        await invoke("resolve_conflict", { id });
        conflicts = conflicts.filter(c => c.id !== id);
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    onMount(() => {
        loadInfo();

        const unlistenSync = listen<string>("sync-state-changed", (e) => {
            syncState = e.payload;
        });

        const unlistenNotifs = listen<NcNotification[]>("notifications-updated", (e) => {
            notifications = e.payload;
        });

        const unlistenErrors = listen<SyncError[]>("sync-errors-updated", (e) => {
            errors = e.payload;
        });

        const unlistenTransfers = listen<TransferProgress[]>("transfers-updated", (e) => {
            transfers = e.payload;
        });

        const unlistenJournal = listen<number>("journal-updated", (e) => {
            pendingMutations = e.payload;
        });

        const unlistenConflicts = listen<ConflictRecord[]>("conflicts-updated", (e) => {
            conflicts = e.payload;
        });

        const clickOutListener = (event: MouseEvent) => {
            const container = document.querySelector(".window");
            if (container && !container.contains(event.target as Node)) close();
        };
        document.addEventListener("click", clickOutListener);

        const escKeyListener = (event: KeyboardEvent) => {
            if (event.key === "Escape" && activeView !== "search") close();
        };
        document.addEventListener("keydown", escKeyListener);

        return () => {
            unlistenSync.then(f => f());
            unlistenNotifs.then(f => f());
            unlistenErrors.then(f => f());
            unlistenTransfers.then(f => f());
            unlistenJournal.then(f => f());
            unlistenConflicts.then(f => f());
            document.removeEventListener("click", clickOutListener);
            document.removeEventListener("keydown", escKeyListener);
        };
    });
</script>

<!-- Root is the entire computer window; .window is the styled popup -->
<main class="window select-none">
    <div class="grid grid-rows-[6rem_auto] h-full w-full">

        <!-- Header -->
        <div class="grid grid-rows-2 grid-cols-[auto_auto] items-center justify-between p-4 bg-base-300 rounded-t-lg">

            <div class="flex items-center row-span-2">
                <!-- Avatar + status dropdown -->
                <div class="dropdown dropdown-hover dropdown-center cursor-pointer" title="Set status">
                    <div tabindex="0" role="button" class="avatar">
                        <div class="w-12 rounded-full">
                            {#if userInfo?.avatar_url && !avatarError}
                                <img
                                    src={userInfo.avatar_url}
                                    alt="Avatar"
                                    onerror={() => { avatarError = true; }}
                                />
                            {:else}
                                <!-- Initials fallback -->
                                <div class="w-12 h-12 rounded-full bg-primary flex items-center justify-center text-primary-content font-bold text-lg">
                                    {(userInfo?.username ?? "?")[0].toUpperCase()}
                                </div>
                            {/if}
                        </div>
                        <div class="absolute bottom-0 right-0 w-4 h-4 bg-green-500 border-2 border-white rounded-full"></div>
                    </div>
                    <SetStatusView class="dropdown-content z-[1] menu shadow bg-base-100 rounded-box" />
                </div>

                <!-- Username + account switcher dropdown -->
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
            </div>

            <!-- Close button -->
            <button
                class="place-self-end self-start -mt-4 -mr-4 w-8 h-8 px-1 rounded-tr-[16px] rounded-bl-xl cursor-pointer bg-primary-content"
                onclick={close}
                aria-label="Close"
            >
                <Icon class="w-6 h-6" path={mdiClose} />
            </button>

            <!-- Toolbar -->
            <div>
                <button
                    class="btn btn-ghost btn-sm rounded-btn"
                    class:btn-active={activeView === "search"}
                    aria-label="Search"
                    onclick={() => { activeView = activeView === "search" ? "notifications" : "search"; }}
                >
                    <Icon class="w-6 h-6" path={mdiMagnify} />
                </button>
                <button class="btn btn-ghost btn-sm rounded-btn" aria-label="Open Containing Folder" onclick={openFolder}>
                    <Icon class="w-6 h-6" path={mdiFolder} />
                </button>
                <button
                    class="btn btn-ghost btn-sm rounded-btn relative"
                    class:btn-active={activeView === "errors"}
                    aria-label="Sync Errors"
                    onclick={() => { activeView = activeView === "errors" ? "notifications" : "errors"; }}
                >
                    <Icon class="w-6 h-6" path={mdiAlertCircleOutline} />
                    {#if errors.length > 0}
                        <span class="badge badge-error badge-xs absolute top-0.5 right-0.5">{errors.length}</span>
                    {/if}
                </button>
                <button
                    class="btn btn-ghost btn-sm rounded-btn relative"
                    class:btn-active={activeView === "conflicts"}
                    aria-label="Conflicts"
                    onclick={() => { activeView = activeView === "conflicts" ? "notifications" : "conflicts"; }}
                >
                    <Icon class="w-6 h-6" path={mdiSwapHorizontal} />
                    {#if conflicts.length > 0}
                        <span class="badge badge-warning badge-xs absolute top-0.5 right-0.5">{conflicts.length}</span>
                    {/if}
                </button>
                <button
                    class="btn btn-ghost btn-sm rounded-btn relative"
                    class:btn-active={activeView === "notifications"}
                    aria-label="Notifications"
                    onclick={() => { activeView = "notifications"; }}
                >
                    <Icon class="w-6 h-6" path={notifications.length > 0 ? mdiBell : mdiBellOutline} />
                    {#if notifications.length > 0}
                        <span class="badge badge-error badge-xs absolute top-0.5 right-0.5">{notifications.length}</span>
                    {/if}
                </button>
            </div>
        </div>

        <!-- Sync status bar -->
        <SyncProgressView {syncState} {transfers} />

        <!-- Content area: search, errors, or notifications -->
        {#if activeView === "search"}
            <SearchView
                class="flex flex-col flex-grow overflow-hidden"
                onclose={() => { activeView = "notifications"; }}
            />
        {:else if activeView === "errors"}
            <ErrorsView {errors} onclear={clearErrors} />
        {:else if activeView === "conflicts"}
            <ConflictsView {conflicts} {pendingMutations} onresolve={resolveConflict} />
        {:else}
            <div class="overflow-y-auto p-2 flex-grow">
                {#if notifications.length === 0}
                    <div class="flex flex-col items-center justify-center h-24 text-gray-400 gap-2">
                        <Icon class="w-8 h-8 opacity-40" path={mdiBellOutline} />
                        <span class="text-sm">No new notifications</span>
                    </div>
                {:else}
                    {#each notifications as n (n.notification_id)}
                    {@const action = primaryAction(n)}
                    <div class="alert shadow-none rounded-none border-0 border-b border-gray-100 py-3 relative">
                        <div class="flex items-start gap-2 flex-1 min-w-0">
                            {#if n.icon}
                                <img
                                    src={n.icon}
                                    alt={n.app}
                                    class="w-8 h-8 flex-shrink-0 object-contain"
                                    onerror={(e) => { (e.currentTarget as HTMLImageElement).style.display = 'none'; }}
                                />
                            {/if}
                            <div class="min-w-0">
                                <p class="font-semibold text-sm leading-snug">{n.subject}</p>
                                {#if n.message}
                                    <p class="text-xs text-gray-500 truncate">{n.message}</p>
                                {/if}
                            </div>
                        </div>

                        <div class="flex flex-col items-end gap-1 flex-shrink-0 ml-2">
                            <div class="flex items-center gap-1">
                                <time class="text-xs text-gray-400">{relativeTime(n.datetime)}</time>
                                <button
                                    class="btn btn-ghost btn-xs p-0 w-5 h-5 min-h-0"
                                    onclick={() => dismiss(n.notification_id)}
                                    aria-label="Dismiss"
                                >
                                    <Icon class="w-3 h-3" path={mdiClose} />
                                </button>
                            </div>
                            {#if action}
                                <button
                                    class="btn btn-primary btn-xs"
                                    onclick={() => openLink(action!.link)}
                                >
                                    {action.label}
                                </button>
                            {:else if n.link}
                                <button class="btn btn-primary btn-xs" onclick={() => openLink(n.link)}>
                                    Open
                                </button>
                            {/if}
                        </div>
                    </div>
                    {/each}
                {/if}
            </div>
        {/if}
    </div>
</main>

<style>
:root {
    font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
    font-size: 16px;
    line-height: 24px;
    font-weight: 400;
    color: #0f0f0f;
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
    border-radius: 16px;
    box-shadow: 0 2px 10px rgba(0, 0, 0, 0.1);
    border: 1px solid #eaeaea;
}
</style>
