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
    }

    let {
        class: mClass = "",
        syncState = "idle",
        transfers = [],
        storage = { kept_bytes: 0, cached_bytes: 0, remote_used: 0, remote_total: 0 },
        onremount,
        ...restProps
    }: Props = $props();

    const hasTransfers = $derived(transfers.length > 0);
    const label = $derived(() => {
        if (hasTransfers) {
            const dl = transfers.filter(t => t.direction === "Download").length;
            const ul = transfers.filter(t => t.direction === "Upload").length;
            const parts: string[] = [];
            if (dl) parts.push(`${dl} download${dl > 1 ? "s" : ""}`);
            if (ul) parts.push(`${ul} upload${ul > 1 ? "s" : ""}`);
            return parts.join(", ");
        }
        switch (syncState) {
            case "syncing":    return "Syncing…";
            case "paused":     return "Sync paused";
            case "unmounted":  return "Filesystem unmounted";
            case "wiped":      return "Device wiped by server";
            case "idle":       return "Up to date";
            default:           return syncState.startsWith("error") ? "Sync error" : syncState;
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

<div class="bg-base-300 {mClass}" {...restProps}>
    <div class="h-8 flex items-center justify-center">
        <div class="w-64">
            {#if hasTransfers}
            <progress class="progress progress-primary w-full"></progress>
            {:else if syncState === "syncing"}
            <progress class="progress progress-primary w-full"></progress>
            {:else}
            <progress class="progress progress-primary w-full" value="100" max="100"></progress>
            {/if}
        </div>
        <span class="ml-4 text-sm">{label()}</span>
        {#if syncState === "wiped"}
            <span class="text-error text-xs ml-2">Credentials cleared. Reconfigure to reconnect.</span>
        {:else if syncState === "unmounted" && onremount}
            <button class="btn btn-primary btn-xs ml-2" onclick={onremount}>Remount</button>
        {/if}
    </div>

    {#if hasTransfers}
    <div class="px-3 pb-2 space-y-1 max-h-32 overflow-y-auto">
        {#each transfers as t (t.path)}
            {@const pct = t.total_bytes > 0 ? Math.round((t.bytes_done / t.total_bytes) * 100) : 0}
            <div class="flex items-center gap-2 text-xs">
                <Icon class="w-3.5 h-3.5 flex-shrink-0 opacity-60" path={t.direction === "Download" ? mdiArrowDown : mdiArrowUp} />
                <span class="truncate flex-1 min-w-0" title={t.path}>{fileName(t.path)}</span>
                {#if t.total_bytes > 0}
                    <span class="flex-shrink-0 tabular-nums text-gray-500">{formatSize(t.bytes_done)} / {formatSize(t.total_bytes)}</span>
                {/if}
                <progress class="progress progress-primary w-16 flex-shrink-0" value={pct} max="100"></progress>
            </div>
        {/each}
    </div>
    {/if}

    {#if hasStorage}
    <div class="px-3 pb-2">
        <div class="flex items-center gap-3 text-xs text-gray-500">
            {#if storage.kept_bytes > 0}
                <span title="Files explicitly kept locally">
                    <span class="inline-block w-2 h-2 rounded-full bg-success mr-1"></span>Kept: {formatSize(storage.kept_bytes)}
                </span>
            {/if}
            {#if storage.cached_bytes > 0}
                <span title="Auto-cached files (read cache)">
                    <span class="inline-block w-2 h-2 rounded-full bg-info mr-1"></span>Cache: {formatSize(storage.cached_bytes)}
                </span>
            {/if}
            {#if storage.remote_total > 0}
                <span class="ml-auto" title="Server storage: {formatSize(storage.remote_used)} of {formatSize(storage.remote_total)}">
                    Server: {formatSize(storage.remote_used)} / {formatSize(storage.remote_total)}
                </span>
            {:else if storage.remote_used > 0}
                <span class="ml-auto">Server: {formatSize(storage.remote_used)}</span>
            {/if}
        </div>
        {#if storage.remote_total > 0}
            {@const localTotal = storage.kept_bytes + storage.cached_bytes}
            {@const serverPct = Math.min(100, Math.round((storage.remote_used / storage.remote_total) * 100))}
            {@const keptPct = storage.remote_total > 0 ? Math.min(100, Math.round((storage.kept_bytes / storage.remote_total) * 100)) : 0}
            {@const cachedPct = storage.remote_total > 0 ? Math.min(100, Math.round((storage.cached_bytes / storage.remote_total) * 100)) : 0}
            <div class="w-full bg-base-200 rounded-full h-1.5 mt-1 overflow-hidden flex">
                {#if keptPct > 0}
                    <div class="bg-success h-full" style="width: {keptPct}%"></div>
                {/if}
                {#if cachedPct > 0}
                    <div class="bg-info h-full" style="width: {cachedPct}%"></div>
                {/if}
                <div class="bg-primary/30 h-full" style="width: {Math.max(0, serverPct - keptPct - cachedPct)}%"></div>
            </div>
        {/if}
    </div>
    {/if}
</div>
