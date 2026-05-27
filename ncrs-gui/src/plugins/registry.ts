import type { Component } from 'svelte';
// @ts-ignore -- plugin component lives outside src/, resolved by Vite alias
import PasswordsView from '$plugins/nc_passwords/frontend/PasswordsView.svelte';

export interface PluginRegistryEntry {
    component: Component;
}

const registry: Record<string, PluginRegistryEntry> = {
    'nc_passwords': { component: PasswordsView as unknown as Component },
};

export function getPluginComponent(id: string): PluginRegistryEntry | undefined {
    return registry[id];
}

export function hasPluginComponent(id: string): boolean {
    return id in registry;
}
