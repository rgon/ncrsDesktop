<script lang="ts">
    import { invoke } from "@tauri-apps/api/core";
    import { onMount } from "svelte";
    import Icon from "./Icon.svelte";
    import { mdiAppsBox, mdiPuzzle } from "@mdi/js";
    import { hasPluginComponent } from "../plugins/registry";

    interface PluginMeta {
        id: string;
        name: string;
        description: string;
        icon: string;
        version: string;
    }

    let { onselect }: { onselect: (id: string) => void } = $props();

    let plugins = $state<PluginMeta[]>([]);

    onMount(async () => {
        plugins = await invoke<PluginMeta[]>("get_plugin_metas");
    });
</script>

<div class="flex flex-col flex-grow overflow-hidden">
    <div class="overflow-y-auto p-4 flex-grow">
        {#if plugins.length === 0}
            <div class="flex flex-col items-center justify-center h-24 text-gray-400 gap-2">
                <Icon class="w-8 h-8 opacity-40" path={mdiAppsBox} />
                <span class="text-sm">No plugins installed</span>
            </div>
        {:else}
            <div class="grid grid-cols-2 gap-3">
                {#each plugins as plugin (plugin.id)}
                    <button
                        class="card bg-base-100 shadow-sm hover:shadow-md transition-shadow cursor-pointer p-4 text-left"
                        onclick={() => onselect(plugin.id)}
                        disabled={!hasPluginComponent(plugin.id)}
                    >
                        <Icon class="w-8 h-8 mb-2" path={mdiPuzzle} />
                        <h3 class="font-semibold text-sm">{plugin.name}</h3>
                        <p class="text-xs text-gray-500 mt-1">{plugin.description}</p>
                        <span class="text-xs text-gray-400 mt-1">v{plugin.version}</span>
                    </button>
                {/each}
            </div>
        {/if}
    </div>
</div>
