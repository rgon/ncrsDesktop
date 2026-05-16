<script lang="ts">
    import Icon from './Icon.svelte';
    import { mdiClose, mdiAlertCircleOutline } from '@mdi/js';

    interface SyncError {
        path: string;
        kind: string | { ServerError: number };
        message: string;
        timestamp_ms: number;
    }

    let { errors = [], onclear }: { errors: SyncError[]; onclear: () => void } = $props();

    function kindLabel(kind: string | { ServerError: number }): string {
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

    function relativeTime(ms: number): string {
        const diffMin = Math.floor((Date.now() - ms) / 60_000);
        if (diffMin < 1) return 'just now';
        if (diffMin < 60) return `${diffMin}m ago`;
        const diffHr = Math.floor(diffMin / 60);
        if (diffHr < 24) return `${diffHr}h ago`;
        return `${Math.floor(diffHr / 24)}d ago`;
    }

    function fileName(path: string): string {
        return path.split('/').pop() || path;
    }
</script>

<div class="flex flex-col flex-grow overflow-hidden">
    {#if errors.length > 0}
        <div class="flex items-center justify-between px-3 py-1.5 border-b border-gray-200">
            <span class="text-xs text-gray-500">{errors.length} error{errors.length !== 1 ? 's' : ''}</span>
            <button class="btn btn-ghost btn-xs" onclick={onclear}>Clear all</button>
        </div>
    {/if}

    <div class="overflow-y-auto p-2 flex-grow">
        {#if errors.length === 0}
            <div class="flex flex-col items-center justify-center h-24 text-gray-400 gap-2">
                <Icon class="w-8 h-8 opacity-40" path={mdiAlertCircleOutline} />
                <span class="text-sm">No sync errors</span>
            </div>
        {:else}
            {#each errors as err, i (i)}
                <div class="alert alert-error shadow-none rounded-lg mb-1.5 py-2 px-3">
                    <div class="flex flex-col min-w-0 flex-1">
                        <div class="flex items-center justify-between gap-2">
                            <span class="badge badge-sm badge-outline">{kindLabel(err.kind)}</span>
                            <time class="text-xs opacity-70">{relativeTime(err.timestamp_ms)}</time>
                        </div>
                        <p class="text-sm font-semibold truncate mt-0.5" title={err.path}>{fileName(err.path)}</p>
                        <p class="text-xs opacity-80 truncate">{err.message}</p>
                    </div>
                </div>
            {/each}
        {/if}
    </div>
</div>
