<script lang="ts">
    import Icon from './Icon.svelte';
    import { mdiSwapHorizontal, mdiCheck, mdiClockOutline } from '@mdi/js';

    interface ConflictRecord {
        id: number;
        kind: Record<string, unknown>;
        timestamp_ms: number;
        resolved: boolean;
    }

    let {
        conflicts = [],
        pendingMutations = 0,
        onresolve,
    }: {
        conflicts: ConflictRecord[];
        pendingMutations: number;
        onresolve: (id: number) => void;
    } = $props();

    function describeConflict(kind: Record<string, unknown>): { label: string; detail: string } {
        if ("EditConflict" in kind) {
            const c = kind.EditConflict as { local_path: string; conflicted_copy_path: string };
            return {
                label: "Edit conflict",
                detail: `Local and server versions differ. Conflicted copy: ${fileName(c.conflicted_copy_path)}`,
            };
        }
        if ("MoveSourceGone" in kind) {
            const c = kind.MoveSourceGone as { from: string; to: string };
            return {
                label: "Move failed",
                detail: `${fileName(c.from)} was moved locally but deleted on server`,
            };
        }
        if ("MoveDestExists" in kind) {
            const c = kind.MoveDestExists as { from: string; to: string };
            return {
                label: "Move blocked",
                detail: `${fileName(c.from)} → ${fileName(c.to)}: destination already exists`,
            };
        }
        if ("PermanentFailure" in kind) {
            const c = kind.PermanentFailure as { description: string };
            return { label: "Failed", detail: c.description };
        }
        return { label: "Unknown", detail: JSON.stringify(kind) };
    }

    function fileName(path: string): string {
        return path.split('/').pop() || path;
    }

    function relativeTime(ms: number): string {
        const diffMin = Math.floor((Date.now() - ms) / 60_000);
        if (diffMin < 1) return 'just now';
        if (diffMin < 60) return `${diffMin}m ago`;
        const diffHr = Math.floor(diffMin / 60);
        if (diffHr < 24) return `${diffHr}h ago`;
        return `${Math.floor(diffHr / 24)}d ago`;
    }
</script>

<div class="flex flex-col flex-grow overflow-hidden">
    {#if pendingMutations > 0}
        <div class="flex items-center gap-2 px-3 py-1.5 border-b border-gray-200">
            <Icon class="w-4 h-4 opacity-60" path={mdiClockOutline} />
            <span class="text-xs text-gray-500">{pendingMutations} pending mutation{pendingMutations !== 1 ? 's' : ''} queued for sync</span>
        </div>
    {/if}

    <div class="overflow-y-auto p-2 flex-grow">
        {#if conflicts.length === 0 && pendingMutations === 0}
            <div class="flex flex-col items-center justify-center h-24 text-gray-400 gap-2">
                <Icon class="w-8 h-8 opacity-40" path={mdiSwapHorizontal} />
                <span class="text-sm">No conflicts</span>
            </div>
        {:else if conflicts.length === 0}
            <div class="flex flex-col items-center justify-center h-24 text-gray-400 gap-2">
                <Icon class="w-8 h-8 opacity-40" path={mdiSwapHorizontal} />
                <span class="text-sm">No conflicts — mutations will sync when online</span>
            </div>
        {:else}
            {#each conflicts as conflict (conflict.id)}
                {@const desc = describeConflict(conflict.kind)}
                <div class="alert alert-warning shadow-none rounded-lg mb-1.5 py-2 px-3">
                    <div class="flex flex-col min-w-0 flex-1">
                        <div class="flex items-center justify-between gap-2">
                            <span class="badge badge-sm badge-outline">{desc.label}</span>
                            <time class="text-xs opacity-70">{relativeTime(conflict.timestamp_ms)}</time>
                        </div>
                        <p class="text-xs opacity-80 mt-0.5">{desc.detail}</p>
                    </div>
                    <button
                        class="btn btn-ghost btn-xs flex-shrink-0"
                        onclick={() => onresolve(conflict.id)}
                        aria-label="Dismiss"
                    >
                        <Icon class="w-4 h-4" path={mdiCheck} />
                    </button>
                </div>
            {/each}
        {/if}
    </div>
</div>
