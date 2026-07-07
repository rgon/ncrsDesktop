<script lang="ts">
    import type { HTMLAttributes } from "svelte/elements";

    import { invoke } from "@tauri-apps/api/core";
    import { onMount } from "svelte";
    import Icon from './Icon.svelte';
    import { mdiMagnify, mdiClose, mdiArrowLeft, mdiOpenInNew } from '@mdi/js';

    // ── Types ─────────────────────────────────────────────────────────────────

    interface SearchEntry {
        title: string;
        subline: string;
        resource_url: string;
        thumbnail_url: string;
        icon: string;
        rounded: boolean;
        local_path: string | null;
    }

    interface SearchResultGroup {
        provider_id: string;
        provider_name: string;
        entries: SearchEntry[];
    }

    interface SearchProvider {
        id: string;
        name: string;
        icon: string;
        order: number;
    }

    interface Props extends HTMLAttributes<HTMLElement> {
        onclose?: () => void;
    }

    let { onclose, ...restProps }: Props = $props();

    // ── State ─────────────────────────────────────────────────────────────────

    let query = $state("");
    let results = $state<SearchResultGroup[]>([]);
    let loading = $state(false);
    let searched = $state(false);
    let debounceTimer: ReturnType<typeof setTimeout> | null = null;
    let searchGen = 0;

    let providers = $state<SearchProvider[]>([]);
    let selectedProviders = $state<Set<string>>(new Set());

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    onMount(async () => {
        try {
            providers = await invoke<SearchProvider[]>("fetch_search_providers");
        } catch (e) {
            console.error("fetch providers failed:", e);
        }
    });

    // ── Handlers ──────────────────────────────────────────────────────────────

    function handleInput() {
        if (debounceTimer) clearTimeout(debounceTimer);
        if (query.length < 3) {
            results = [];
            searched = false;
            loading = false;
            return;
        }
        searchGen++;
        loading = true;
        debounceTimer = setTimeout(doSearch, 500);
    }

    function handleKeydown(e: KeyboardEvent) {
        if (e.key === "Escape") {
            if (query) {
                query = "";
                results = [];
                searched = false;
                loading = false;
                searchGen++;
            } else {
                onclose?.();
            }
        }
    }

    function toggleProvider(id: string) {
        const next = new Set(selectedProviders);
        if (next.has(id)) {
            next.delete(id);
        } else {
            next.add(id);
        }
        selectedProviders = next;
        if (query.length >= 3) {
            searchGen++;
            loading = true;
            if (debounceTimer) clearTimeout(debounceTimer);
            debounceTimer = setTimeout(doSearch, 200);
        }
    }

    async function doSearch() {
        if (query.length < 3) return;
        const gen = searchGen;
        const ids = [...selectedProviders];
        try {
            const r = await invoke<SearchResultGroup[]>("search_nextcloud", {
                term: query,
                providerIds: ids,
            });
            if (gen !== searchGen) return;
            results = r;
        } catch (e) {
            if (gen !== searchGen) return;
            console.error("search failed:", e);
            results = [];
        }
        loading = false;
        searched = true;
    }

    async function openResult(entry: SearchEntry) {
        if (entry.local_path) {
            await invoke("reveal_in_file_manager", { path: entry.local_path });
        } else if (entry.resource_url) {
            await invoke("open_link", { url: entry.resource_url });
        }
    }

    async function viewOnline(e: MouseEvent | KeyboardEvent, url: string) {
        e.stopPropagation();
        if (url) await invoke("open_link", { url });
    }

    function clearQuery() {
        query = "";
        results = [];
        searched = false;
        loading = false;
        searchGen++;
    }
</script>

<div {...restProps}>
    <!-- Search input bar -->
    <div class="flex items-center gap-2 p-3 border-b border-gray-200 bg-base-200">
        <button class="btn btn-ghost btn-sm btn-square" onclick={onclose} aria-label="Back">
            <Icon class="w-5 h-5" path={mdiArrowLeft} />
        </button>
        <div class="flex-1 relative">
            <Icon class="w-4 h-4 absolute left-2 top-1/2 -translate-y-1/2 text-gray-400 pointer-events-none" path={mdiMagnify} />
            <!-- svelte-ignore a11y_autofocus -->
            <input
                type="text"
                bind:value={query}
                oninput={handleInput}
                onkeydown={handleKeydown}
                placeholder="Search Nextcloud..."
                class="input input-sm input-bordered w-full pl-8 pr-8"
                autofocus
            />
            {#if query}
            <button
                class="btn btn-ghost btn-xs btn-circle absolute right-1 top-1/2 -translate-y-1/2"
                onclick={clearQuery}
                aria-label="Clear"
            >
                <Icon class="w-3.5 h-3.5" path={mdiClose} />
            </button>
            {/if}
        </div>
    </div>

    <!-- Provider filter chips -->
    {#if providers.length > 0}
    <div class="flex flex-wrap gap-1 px-3 py-2 border-b border-gray-200">
        {#each providers as p (p.id)}
            <button
                class="badge badge-sm cursor-pointer select-none transition-colors
                    {selectedProviders.size === 0 || selectedProviders.has(p.id) ? 'badge-primary' : 'badge-ghost opacity-50'}"
                onclick={() => toggleProvider(p.id)}
            >
                {p.name}
            </button>
        {/each}
    </div>
    {/if}

    <!-- Results area -->
    <div class="overflow-y-auto flex-1 p-2">
        {#if loading && results.length === 0}
            <div class="flex justify-center p-6">
                <span class="loading loading-spinner loading-sm text-gray-400"></span>
            </div>
        {:else if searched && !loading && results.length === 0}
            <p class="text-center text-gray-400 text-sm p-6">No results for "{query}"</p>
        {:else}
            {#if loading}
                <div class="flex justify-center py-1">
                    <span class="loading loading-spinner loading-xs text-gray-400"></span>
                </div>
            {/if}
            {#each results as group (group.provider_id)}
                <div class="mb-2">
                    <h3 class="text-xs font-semibold text-gray-400 uppercase tracking-wide px-2 pt-2 pb-1">
                        {group.provider_name}
                    </h3>
                    {#each group.entries as entry}
                    <button
                        class="group w-full flex items-center gap-2 px-2 py-1.5 rounded-lg hover:bg-base-200 text-left cursor-pointer transition-colors"
                        onclick={() => openResult(entry)}
                    >
                        {#if entry.icon}
                        <img
                            src={entry.icon}
                            alt=""
                            class="w-7 h-7 flex-shrink-0 object-contain {entry.rounded ? 'rounded-full' : 'rounded'}"
                            onerror={(e) => { (e.currentTarget as HTMLImageElement).style.display = 'none'; }}
                        />
                        {/if}
                        <div class="min-w-0 flex-1">
                            <p class="text-sm font-medium truncate">{entry.title}</p>
                            {#if entry.subline}
                            <p class="text-xs text-gray-400 truncate">{entry.subline}</p>
                            {/if}
                        </div>
                        {#if entry.resource_url}
                        <div
                            role="button"
                            tabindex="0"
                            class="btn btn-ghost btn-xs btn-square opacity-0 group-hover:opacity-100 transition-opacity flex-shrink-0"
                            onclick={(e) => viewOnline(e, entry.resource_url)}
                            onkeydown={(e) => e.key === 'Enter' && viewOnline(e, entry.resource_url)}
                            title="View in Nextcloud Web"
                        >
                            <Icon class="w-3.5 h-3.5" path={mdiOpenInNew} />
                        </div>
                        {/if}
                    </button>
                    {/each}
                </div>
            {/each}
        {/if}
    </div>
</div>
