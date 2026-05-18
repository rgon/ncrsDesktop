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

    interface Props extends HTMLAttributes<HTMLElement> {
        class?: string;
        syncState?: string;
        transfers?: TransferProgress[];
        onremount?: () => void;
    }

    let {
        class: mClass = "",
        syncState = "idle",
        transfers = [],
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
            case "idle":       return "Up to date";
            default:           return syncState.startsWith("error") ? "Sync error" : syncState;
        }
    });

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
        {#if syncState === "unmounted" && onremount}
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
</div>
