<script lang="ts">
    import type { HTMLAttributes } from "svelte/elements";

    interface Props extends HTMLAttributes<HTMLElement> {
        class?: string;
        syncState?: string;
    }

    let {
        class: mClass = "",
        syncState = "idle",
        ...restProps
    }: Props = $props();

    const isSyncing = $derived(syncState === "syncing");
    const label = $derived(() => {
        switch (syncState) {
            case "syncing": return "Syncing…";
            case "paused":  return "Sync paused";
            case "idle":    return "Up to date";
            default:        return syncState.startsWith("error") ? "Sync error" : syncState;
        }
    });
</script>

<div class="h-8 flex items-center justify-center bg-base-300 {mClass}" {...restProps}>
    <div class="w-64">
        {#if isSyncing}
        <progress class="progress progress-primary w-full"></progress>
        {:else}
        <progress class="progress progress-primary w-full" value="100" max="100"></progress>
        {/if}
    </div>
    <span class="ml-4 text-sm">{label()}</span>
</div>
