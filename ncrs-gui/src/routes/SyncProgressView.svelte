<script lang="ts">
    import type { HTMLAttributes } from "svelte/elements";
    import Icon from "../components/Icon.svelte";
    import { mdiArrowDown, mdiArrowUp } from "@mdi/js";

    interface TransferProgress {
        path: string;
        direction: "Download" | "Upload";
        bytes_done: number;
        total_bytes: number;
    }

    interface StorageStats {
        kept_bytes: number;
        cached_bytes: number;
        remote_used: number;
        remote_total: number;
    }

    interface Props extends HTMLAttributes<HTMLElement> {
        class?: string;
        syncState?: string;
        transfers?: TransferProgress[];
        storage?: StorageStats;
        onremount?: () => void;
        onlogout?: () => void;
    }

    let {
        class: mClass = "",
        syncState = "idle",
        transfers = [],
        storage = { kept_bytes: 0, cached_bytes: 0, remote_used: 0, remote_total: 0 },
        onremount,
        onlogout,
        ...restProps
    }: Props = $props();

    const hasTransfers = $derived(transfers.length > 0);

    const dotClass = $derived(() => {
        // Offline is checked before transfers: queued uploads are stalled, not
        // progressing, so a spinning "syncing" dot would be a lie.
        if (syncState === "offline") return "nc-dot nc-dot-error";
        if (hasTransfers || syncState === "syncing") return "nc-dot nc-dot-syncing";
        if (syncState === "paused") return "nc-dot nc-dot-paused";
        if (syncState === "unmounted" || syncState === "wiped" || syncState.startsWith("error:")) return "nc-dot nc-dot-error";
        if (syncState.startsWith("degraded:")) return "nc-dot nc-dot-degraded";
        return "nc-dot nc-dot-idle";
    });

    const statusLabel = $derived(() => {
        if (syncState === "offline") return "Offline — server unreachable";
        if (hasTransfers) {
            const dl = transfers.filter(t => t.direction === "Download").length;
            const ul = transfers.filter(t => t.direction === "Upload").length;
            const parts: string[] = [];
            if (dl) parts.push(`${dl} ↓`);
            if (ul) parts.push(`${ul} ↑`);
            return parts.join("  ");
        }
        switch (syncState) {
            case "syncing":   return "Syncing…";
            case "paused":    return "Paused";
            case "unmounted": return "Unmounted";
            case "wiped":     return "Wiped by server";
            case "idle":      return "Up to date";
            default:
                if (syncState.startsWith("error:")) return syncState.slice(6);
                if (syncState.startsWith("degraded:")) return "Degraded — " + syncState.slice(9);
                return syncState;
        }
    });

    const hasStorage = $derived(storage.kept_bytes > 0 || storage.cached_bytes > 0 || storage.remote_used > 0);

    function formatSize(bytes: number): string {
        if (bytes < 1024) return `${bytes} B`;
        if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)} KB`;
        if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)} MB`;
        return `${(bytes / 1073741824).toFixed(1)} GB`;
    }

    function fileName(path: string): string {
        return path.split("/").pop() || path;
    }
</script>

<div class="nc-sync-bar {mClass}" {...restProps}>
    <!-- Status row -->
    <div class="nc-sync-row">
        <span class={dotClass()}></span>
        <span class="nc-sync-label" title={statusLabel()}>{statusLabel()}</span>

        {#if syncState === "wiped"}
            <span class="nc-sync-alert">Credentials cleared — reconfigure to reconnect.</span>
        {:else if syncState.startsWith("error:authentication") && onlogout}
            <button class="nc-remount-btn" onclick={onlogout}>Log in</button>
        {:else if (syncState === "unmounted" || syncState.startsWith("error:")) && onremount}
            <button class="nc-remount-btn" onclick={onremount}>Remount</button>
        {/if}

        <!-- Storage summary (inline, right-aligned) -->
        {#if hasStorage && !hasTransfers}
            <span class="nc-storage-inline">
                {#if storage.remote_total > 0}
                    {formatSize(storage.remote_used)} / {formatSize(storage.remote_total)}
                {:else if storage.remote_used > 0}
                    {formatSize(storage.remote_used)}
                {/if}
            </span>
        {/if}
    </div>

    <!-- Active transfers -->
    {#if hasTransfers}
    <div class="nc-transfers">
        {#each transfers as t (t.path)}
            {@const pct = t.total_bytes > 0 ? Math.round((t.bytes_done / t.total_bytes) * 100) : 0}
            <div class="nc-transfer-row">
                <Icon
                    class="nc-transfer-arrow"
                    path={t.direction === "Download" ? mdiArrowDown : mdiArrowUp}
                />
                <span class="nc-transfer-name" title={t.path}>{fileName(t.path)}</span>
                {#if t.total_bytes > 0}
                    <span class="nc-transfer-size">{formatSize(t.bytes_done)}/{formatSize(t.total_bytes)}</span>
                {/if}
                <div class="nc-transfer-track">
                    <div class="nc-transfer-fill" style="width:{pct}%"></div>
                </div>
            </div>
        {/each}
    </div>
    {/if}

    <!-- Storage bar -->
    {#if hasStorage && storage.remote_total > 0}
        {@const keptPct = Math.min(100, Math.round((storage.kept_bytes / storage.remote_total) * 100))}
        {@const cachedPct = Math.min(100, Math.round((storage.cached_bytes / storage.remote_total) * 100))}
        {@const serverPct = Math.min(100, Math.round((storage.remote_used / storage.remote_total) * 100))}
        <div class="nc-storage-bar-wrap">
            <div class="nc-storage-track">
                {#if keptPct > 0}
                    <div class="nc-storage-seg nc-seg-kept" style="width:{keptPct}%"></div>
                {/if}
                {#if cachedPct > 0}
                    <div class="nc-storage-seg nc-seg-cached" style="width:{cachedPct}%"></div>
                {/if}
                <div class="nc-storage-seg nc-seg-remote" style="width:{Math.max(0, serverPct - keptPct - cachedPct)}%"></div>
            </div>
        </div>
    {/if}
</div>

<style>
.nc-sync-bar {
    flex-shrink: 0;
    background: var(--nc-surface);
    border-bottom: 1px solid var(--nc-border);
}

.nc-sync-row {
    display: flex;
    align-items: center;
    gap: 7px;
    padding: 0 14px;
    height: 32px;
}

.nc-sync-label {
    font-size: 12px;
    color: var(--nc-text-2);
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
}

.nc-sync-alert {
    font-size: 11px;
    color: var(--nc-error);
}

.nc-remount-btn {
    font-size: 11px;
    font-weight: 600;
    color: var(--nc-accent);
    background: none;
    border: 1px solid var(--nc-accent);
    border-radius: 4px;
    padding: 2px 8px;
    cursor: pointer;
    transition: background 0.1s, color 0.1s;
}
.nc-remount-btn:hover { background: var(--nc-accent); color: #fff; }

.nc-storage-inline {
    font-size: 11px;
    color: var(--nc-text-3);
    flex-shrink: 0;
    margin-left: auto;
    padding-left: 8px;
}

/* ── Transfers ─────────────────────────────── */

.nc-transfers {
    padding: 4px 14px 6px;
    display: flex;
    flex-direction: column;
    gap: 3px;
    max-height: 120px;
    overflow-y: auto;
}

.nc-transfer-row {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    color: var(--nc-text-2);
}

:global(.nc-transfer-arrow) {
    width: 12px;
    height: 12px;
    flex-shrink: 0;
    opacity: 0.6;
}

.nc-transfer-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
}

.nc-transfer-size {
    color: var(--nc-text-3);
    flex-shrink: 0;
    font-variant-numeric: tabular-nums;
}

.nc-transfer-track {
    width: 48px;
    height: 3px;
    border-radius: 2px;
    background: var(--nc-border);
    flex-shrink: 0;
    overflow: hidden;
}

.nc-transfer-fill {
    height: 100%;
    background: var(--nc-accent);
    border-radius: 2px;
    transition: width 0.3s;
}

/* ── Storage bar ───────────────────────────── */

.nc-storage-bar-wrap {
    padding: 0 14px 6px;
}

.nc-storage-track {
    height: 2px;
    background: var(--nc-border);
    border-radius: 1px;
    display: flex;
    overflow: hidden;
}

.nc-storage-seg { height: 100%; }
.nc-seg-kept   { background: var(--nc-success); }
.nc-seg-cached { background: var(--nc-accent); opacity: 0.5; }
.nc-seg-remote { background: var(--nc-accent); opacity: 0.2; }
</style>
