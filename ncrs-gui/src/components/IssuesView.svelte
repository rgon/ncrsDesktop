<script lang="ts">
    import Icon from './Icon.svelte';
    import { mdiAlertCircleOutline, mdiSwapHorizontal, mdiCheck, mdiClose, mdiClockOutline, mdiChevronDown } from '@mdi/js';

    interface SyncError {
        path: string;
        kind: string | { ServerError: number };
        message: string;
        timestamp_ms: number;
    }

    interface ConflictRecord {
        id: number;
        kind: Record<string, unknown>;
        timestamp_ms: number;
        resolved: boolean;
    }

    let {
        errors = [],
        conflicts = [],
        pendingMutations = 0,
        onclear,
        ondismissone,
        onresolve,
    }: {
        errors: SyncError[];
        conflicts: ConflictRecord[];
        pendingMutations: number;
        onclear: () => void;
        ondismissone: (timestamp_ms: number) => void;
        onresolve: (id: number) => void;
    } = $props();

    function errorKindLabel(kind: string | { ServerError: number }): string {
        if (typeof kind === 'object' && 'ServerError' in kind) return `Server error ${kind.ServerError}`;
        const labels: Record<string, string> = {
            UploadFailed: 'Upload failed',
            Conflict: 'Conflict',
            PermissionDenied: 'Permission denied',
            NetworkError: 'Network error',
            QuotaExceeded: 'Quota exceeded',
            InvalidFilename: 'Invalid filename',
            Locked: 'File locked',
        };
        return labels[kind] ?? kind;
    }

    function describeConflict(kind: Record<string, unknown>): { label: string; detail: string } {
        if ("EditConflict" in kind) {
            const c = kind.EditConflict as { local_path: string; conflicted_copy_path: string };
            return { label: "Edit conflict", detail: `Local and server versions differ. Conflicted copy: ${fileName(c.conflicted_copy_path)}` };
        }
        if ("MoveSourceGone" in kind) {
            const c = kind.MoveSourceGone as { from: string; to: string };
            return { label: "Move failed", detail: `${fileName(c.from)} was moved locally but deleted on server` };
        }
        if ("MoveDestExists" in kind) {
            const c = kind.MoveDestExists as { from: string; to: string };
            return { label: "Move blocked", detail: `${fileName(c.from)} → ${fileName(c.to)}: destination already exists` };
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

    const isEmpty = $derived(errors.length === 0 && conflicts.length === 0 && pendingMutations === 0);

    let expandedErrors = $state<Set<number>>(new Set());
    function toggleError(i: number) {
        const next = new Set(expandedErrors);
        if (next.has(i)) { next.delete(i); } else { next.add(i); }
        expandedErrors = next;
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
        {#if isEmpty}
            <div class="flex flex-col items-center justify-center h-24 text-gray-400 gap-2">
                <Icon class="w-8 h-8 opacity-40" path={mdiAlertCircleOutline} />
                <span class="text-sm">No issues</span>
            </div>
        {:else}
            {#if errors.length > 0}
                <div class="flex items-center justify-between px-1 pb-1">
                    <span class="text-xs text-gray-500">{errors.length} error{errors.length !== 1 ? 's' : ''}</span>
                    <button class="btn btn-ghost btn-xs" onclick={onclear}>Clear all</button>
                </div>
                {#each errors as err, i (i)}
                    {@const expanded = expandedErrors.has(i)}
                    <div class="alert alert-error shadow-none rounded-lg mb-1.5 overflow-hidden py-2 px-3"
                         style="display:flex; align-items:flex-start; gap:6px;">
                        <div class="flex flex-col min-w-0 flex-1">
                            <div class="flex items-center justify-between gap-2">
                                <span class="badge badge-sm badge-outline flex-shrink-0">{errorKindLabel(err.kind)}</span>
                                <time class="text-xs opacity-70 flex-shrink-0">{relativeTime(err.timestamp_ms)}</time>
                            </div>
                            {#if fileName(err.path)}
                                <p class="text-sm font-semibold truncate mt-0.5">{fileName(err.path)}</p>
                            {/if}
                            {#if expanded}
                                <p class="text-xs opacity-60 break-all mt-0.5">{err.path}</p>
                                <p class="text-xs opacity-80 break-words mt-1">{err.message}</p>
                            {:else}
                                <p class="text-xs opacity-80 truncate mt-0.5">{err.message}</p>
                            {/if}
                        </div>
                        <div class="flex flex-col gap-0.5 flex-shrink-0 self-start">
                            <button
                                class="btn btn-ghost btn-xs p-0.5 opacity-50 hover:opacity-100"
                                onclick={() => toggleError(i)}
                                aria-label={expanded ? 'Collapse' : 'Show full path and message'}
                                title={expanded ? 'Collapse' : 'Show full path and message'}
                            >
                                <Icon class="w-3.5 h-3.5 transition-transform {expanded ? 'rotate-180' : ''}" path={mdiChevronDown} />
                            </button>
                            <button
                                class="btn btn-ghost btn-xs p-0.5 opacity-50 hover:opacity-100"
                                onclick={() => ondismissone(err.timestamp_ms)}
                                aria-label="Dismiss"
                                title="Dismiss"
                            >
                                <Icon class="w-3.5 h-3.5" path={mdiClose} />
                            </button>
                        </div>
                    </div>
                {/each}
            {/if}

            {#if conflicts.length > 0}
                {#if errors.length > 0}
                    <div class="flex items-center px-1 pt-2 pb-1">
                        <span class="text-xs text-gray-500">{conflicts.length} conflict{conflicts.length !== 1 ? 's' : ''}</span>
                    </div>
                {/if}
                {#each conflicts as conflict (conflict.id)}
                    {@const desc = describeConflict(conflict.kind)}
                    <div class="alert alert-warning shadow-none rounded-lg mb-1.5 py-2 px-3"
                         style="display:flex; align-items:flex-start; gap:6px;">
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
        {/if}
    </div>
</div>
