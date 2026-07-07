<script lang="ts">
    import '../app.css';

    import Icon from '../components/Icon.svelte';

    import { invoke } from "@tauri-apps/api/core";
    import { listen } from "@tauri-apps/api/event";

    import {
        mdiFolder, mdiAppsBox, mdiPlus, mdiAccountCog,
        mdiChevronDown, mdiClose, mdiMagnify, mdiBell,
        mdiBellOutline, mdiAlertCircleOutline,
    } from '@mdi/js';

    import { onMount } from 'svelte';

    import SetStatusView from './SetStatusView.svelte';
    import SyncProgressView from './SyncProgressView.svelte';
    import SearchView from '../components/SearchView.svelte';
    import IssuesView from '../components/IssuesView.svelte';
    import PluginsView from '../components/PluginsView.svelte';
    import LoginView from '../components/LoginView.svelte';
    import { getPluginComponent } from '../plugins/registry';

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

    interface StorageStats {
        kept_bytes: number;
        cached_bytes: number;
        remote_used: number;
        remote_total: number;
    }

    // ── State ─────────────────────────────────────────────────────────────────

    type View = "login" | "notifications" | "search" | "issues" | "plugins" | `plugin:${string}`;

    let userInfo = $state<UserInfo | null>(null);
    let syncState = $state<string>("idle");
    let notifications = $state<NcNotification[]>([]);
    let errors = $state<SyncError[]>([]);
    let transfers = $state<TransferProgress[]>([]);
    let conflicts = $state<ConflictRecord[]>([]);
    let storage = $state<StorageStats>({ kept_bytes: 0, cached_bytes: 0, remote_used: 0, remote_total: 0 });
    let pendingMutations = $state(0);
    let avatarError = $state(false);
    let activeView = $state<View>("notifications");
    let needsLogin = $state(false);
    let configServerUrl = $state("");

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

    function stripProtocol(url: string): string {
        return url.replace(/^https?:\/\//, "");
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

    interface ConfigStatus { needs_login: boolean; server_url: string | null; }

    async function loadInfo() {
        const status = await invoke<ConfigStatus>("get_config_status");
        if (status.needs_login) {
            needsLogin = true;
            configServerUrl = status.server_url ?? "";
            activeView = "login";
            return;
        }
        needsLogin = false;
        userInfo = await invoke<UserInfo | null>("get_user_info");
        syncState = await invoke<string>("get_sync_state");
        notifications = await invoke<NcNotification[]>("get_notifications");
        errors = await invoke<SyncError[]>("get_errors");
        transfers = await invoke<TransferProgress[]>("get_transfers");
        conflicts = await invoke<ConflictRecord[]>("get_conflicts");
        invoke<StorageStats>("get_storage_stats").then(s => { storage = s; }).catch(() => {});
        invoke<{ color: string; color_text: string } | null>("get_nc_theme").then(theme => {
            if (theme) {
                document.documentElement.style.setProperty("--nc-accent", theme.color);
            }
        }).catch(() => {});
    }

    async function clearErrors() {
        await invoke("clear_errors");
        errors = [];
    }

    async function resolveConflict(id: number) {
        await invoke("resolve_conflict", { id });
        conflicts = conflicts.filter(c => c.id !== id);
    }

    async function handleRemount() {
        await invoke("remount");
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

        const unlistenPluginNav = listen<string>("navigate-plugin", (e) => {
            activeView = `plugin:${e.payload}`;
        });

        const unlistenLoginComplete = listen<{ server: string; login_name: string }>("login-complete", () => {
            needsLogin = false;
            activeView = "notifications";
            loadInfo();
        });

        const storageInterval = setInterval(() => {
            invoke<StorageStats>("get_storage_stats").then(s => { storage = s; }).catch(() => {});
        }, 30_000);

        const clickOutListener = (event: MouseEvent) => {
            const container = document.querySelector(".window");
            if (container && !event.composedPath().includes(container)) close();
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
            unlistenPluginNav.then(f => f());
            unlistenLoginComplete.then(f => f());
            clearInterval(storageInterval);
            document.removeEventListener("click", clickOutListener);
            document.removeEventListener("keydown", escKeyListener);
        };
    });
</script>

<main class="window select-none">
    <div class="nc-shell">

        <!-- ── Header ──────────────────────────────────────────────────── -->
        <header class="nc-header">

            <!-- Avatar + status dropdown -->
            <div class="dropdown dropdown-hover dropdown-center" title="Set status">
                <div tabindex="0" role="button" class="nc-avatar-wrap">
                    {#if userInfo?.avatar_url && !avatarError}
                        <img
                            src={userInfo.avatar_url}
                            alt="Avatar"
                            class="nc-avatar"
                            onerror={() => { avatarError = true; }}
                        />
                    {:else}
                        <div class="nc-avatar nc-avatar-initials">
                            {(userInfo?.username ?? "?")[0].toUpperCase()}
                        </div>
                    {/if}
                    <span class="nc-status-orb"></span>
                </div>
                <SetStatusView class="dropdown-content z-[1] menu shadow bg-base-100 rounded-box" />
            </div>

            <!-- Username + server info -->
            <div class="nc-user-block">
                <div class="dropdown dropdown-start">
                    <!-- svelte-ignore a11y_no_noninteractive_element_to_interactive_role -->
                    <h1 tabindex="0" role="button" class="nc-username">
                        {userInfo?.username ?? "—"}
                        <Icon class="nc-chevron-icon" path={mdiChevronDown} />
                    </h1>
                    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
                    <ul tabindex="0" class="dropdown-content menu bg-base-100 rounded-box z-1 w-52 p-2 shadow-sm">
                        <li><button onclick={() => false}>
                            <Icon class="w-4 h-4 mr-2" path={mdiAccountCog} /> {userInfo?.username ?? "—"}
                        </button></li>
                        <li class="text-xs text-gray-400 px-2 py-1 truncate">{userInfo?.server_url ?? ""}</li>
                        <li><button onclick={() => false}>
                            <Icon class="w-4 h-4 mr-2" path={mdiPlus} /> Add account
                        </button></li>
                    </ul>
                </div>
                {#if userInfo?.server_url}
                    <p class="nc-server-label">{stripProtocol(userInfo.server_url)}</p>
                {/if}
            </div>

            <!-- Spacer -->
            <div style="flex: 1;"></div>

            <!-- Toolbar -->
            <div class="nc-toolbar">
                <button
                    class="nc-icon-btn"
                    class:nc-active={activeView === "plugins" || activeView.startsWith("plugin:")}
                    aria-label="Apps"
                    onclick={() => { activeView = activeView === "plugins" || activeView.startsWith("plugin:") ? "notifications" : "plugins"; }}
                >
                    <Icon class="nc-icon" path={mdiAppsBox} />
                </button>
                <button
                    class="nc-icon-btn"
                    class:nc-active={activeView === "search"}
                    aria-label="Search"
                    onclick={() => { activeView = activeView === "search" ? "notifications" : "search"; }}
                >
                    <Icon class="nc-icon" path={mdiMagnify} />
                </button>
                <button class="nc-icon-btn" aria-label="Open sync folder" onclick={openFolder}>
                    <Icon class="nc-icon" path={mdiFolder} />
                </button>
                <button
                    class="nc-icon-btn"
                    class:nc-active={activeView === "issues"}
                    aria-label="Issues"
                    onclick={() => { activeView = activeView === "issues" ? "notifications" : "issues"; }}
                >
                    <Icon class="nc-icon" path={mdiAlertCircleOutline} />
                    {#if errors.length > 0}
                        <span class="nc-badge nc-badge-error">{errors.length + conflicts.length}</span>
                    {:else if conflicts.length > 0}
                        <span class="nc-badge nc-badge-warn">{conflicts.length}</span>
                    {/if}
                </button>
                <button
                    class="nc-icon-btn"
                    class:nc-active={activeView === "notifications"}
                    aria-label="Notifications"
                    onclick={() => { activeView = "notifications"; }}
                >
                    <Icon class="nc-icon" path={notifications.length > 0 ? mdiBell : mdiBellOutline} />
                    {#if notifications.length > 0}
                        <span class="nc-badge nc-badge-info">{notifications.length}</span>
                    {/if}
                </button>
            </div>

            <!-- Close -->
            <button class="nc-icon-btn nc-close" onclick={close} aria-label="Close" style="margin-left: 6px;">
                <Icon class="nc-icon" path={mdiClose} />
            </button>
        </header>

        <!-- ── Sync status strip ────────────────────────────────────────── -->
        <SyncProgressView {syncState} {transfers} {storage} onremount={handleRemount} />

        <!-- ── Content ─────────────────────────────────────────────────── -->
        <div class="nc-content">
            {#if activeView === "login"}
                <LoginView initialServerUrl={configServerUrl} />

            {:else if activeView === "search"}
                <SearchView
                    class="flex flex-col flex-grow overflow-hidden"
                    onclose={() => { activeView = "notifications"; }}
                />

            {:else if activeView === "issues"}
                <IssuesView {errors} {conflicts} {pendingMutations} onclear={clearErrors} onresolve={resolveConflict} />

            {:else if activeView === "plugins"}
                <PluginsView onselect={(id) => { activeView = `plugin:${id}`; }} />

            {:else if activeView.startsWith("plugin:")}
                {@const pluginId = activeView.slice(7)}
                {@const entry = getPluginComponent(pluginId)}
                {#if entry}
                    {@const PluginComponent = entry.component}
                    <PluginComponent />
                {:else}
                    <div class="nc-empty-state">
                        <span style="font-size:13px;color:var(--nc-text-3)">Plugin view not available</span>
                    </div>
                {/if}

            {:else}
                <!-- Notifications list -->
                <div class="nc-notif-list">
                    {#if notifications.length === 0}
                        <div class="nc-empty-state">
                            <Icon style="width:30px;height:30px;opacity:0.25;color:var(--nc-text-3)" path={mdiBellOutline} />
                            <span style="font-size:13px;color:var(--nc-text-3)">No new notifications</span>
                        </div>
                    {:else}
                        {#each notifications as n (n.notification_id)}
                        {@const action = primaryAction(n)}
                        <div class="nc-notif">
                            {#if n.icon}
                                <img
                                    src={n.icon}
                                    alt={n.app}
                                    style="width:32px;height:32px;flex-shrink:0;object-fit:contain;border-radius:6px;"
                                    onerror={(e) => { (e.currentTarget as HTMLImageElement).style.display = 'none'; }}
                                />
                            {/if}
                            <div style="flex:1;min-width:0;">
                                <p style="font-weight:600;font-size:13px;line-height:1.3;color:var(--nc-text)">{n.subject}</p>
                                {#if n.message}
                                    <p style="font-size:12px;color:var(--nc-text-2);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;margin-top:1px">{n.message}</p>
                                {/if}
                                <div style="display:flex;align-items:center;gap:8px;margin-top:5px">
                                    <time style="font-size:11px;color:var(--nc-text-3)">{relativeTime(n.datetime)}</time>
                                    {#if action}
                                        <button class="nc-action-btn" onclick={() => openLink(action!.link)}>
                                            {action.label}
                                        </button>
                                    {:else if n.link}
                                        <button class="nc-action-btn" onclick={() => openLink(n.link)}>Open</button>
                                    {/if}
                                </div>
                            </div>
                            <button class="nc-dismiss-btn" onclick={() => dismiss(n.notification_id)} aria-label="Dismiss">
                                <Icon style="width:12px;height:12px" path={mdiClose} />
                            </button>
                        </div>
                        {/each}
                    {/if}
                </div>
            {/if}
        </div>

    </div>
</main>

<style>
.window {
    position: absolute;
    right: 20px;
    top: 20px;
    width: min(400px, 40vw);
    height: min(700px, 60vh);
    border-radius: 14px;
    overflow: hidden;
    background: var(--nc-bg);
    box-shadow: 0 8px 32px rgba(0,0,0,0.18), 0 2px 6px rgba(0,0,0,0.08);
    border: 1px solid var(--nc-border);
}

.nc-shell {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
}

/* ── Header ─────────────────────────────────────── */

.nc-header {
    display: flex;
    align-items: center;
    padding: 0 10px 0 12px;
    height: 56px;
    flex-shrink: 0;
    background: var(--nc-surface);
    border-bottom: 1px solid var(--nc-border);
    gap: 0;
}

.nc-avatar-wrap {
    position: relative;
    flex-shrink: 0;
    cursor: pointer;
    margin-right: 10px;
}

.nc-avatar {
    width: 34px;
    height: 34px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
}

.nc-avatar-initials {
    width: 34px;
    height: 34px;
    border-radius: 50%;
    background: var(--nc-accent);
    color: #fff;
    font-size: 15px;
    font-weight: 700;
    display: flex;
    align-items: center;
    justify-content: center;
}

.nc-status-orb {
    position: absolute;
    bottom: 0;
    right: 0;
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--nc-success);
    border: 2px solid var(--nc-surface);
}

.nc-user-block {
    flex: 1;
    min-width: 0;
}

.nc-username {
    font-size: 14px;
    font-weight: 600;
    color: var(--nc-text);
    cursor: pointer;
    display: flex;
    align-items: center;
    gap: 2px;
    background: none;
    border: none;
    padding: 0;
    line-height: 1.2;
    white-space: nowrap;
    transition: color 0.1s;
}
.nc-username:hover { color: var(--nc-accent); }

:global(.nc-chevron-icon) { width: 12px; height: 12px; flex-shrink: 0; }

.nc-server-label {
    font-size: 10px;
    color: var(--nc-text-3);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 130px;
    margin-top: 1px;
}

.nc-toolbar {
    display: flex;
    align-items: center;
    gap: 1px;
    flex-shrink: 0;
}

:global(.nc-icon) { width: 18px; height: 18px; }

/* ── Content area ────────────────────────────────── */

.nc-content {
    flex: 1;
    min-height: 0;
    overflow: hidden;
    display: flex;
    flex-direction: column;
    background: var(--nc-bg);
}

.nc-notif-list {
    overflow-y: auto;
    flex: 1;
}

.nc-empty-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    height: 100%;
    gap: 10px;
    padding: 40px;
}
</style>
