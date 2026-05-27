import type { Component } from 'svelte';

export interface PluginRegistryEntry {
    component: Component;
}

const registry: Record<string, PluginRegistryEntry> = {
    // Register plugin frontend components here:
    // 'nc_passwords': { component: PasswordsView },
};

export function getPluginComponent(id: string): PluginRegistryEntry | undefined {
    return registry[id];
}

export function hasPluginComponent(id: string): boolean {
    return id in registry;
}
