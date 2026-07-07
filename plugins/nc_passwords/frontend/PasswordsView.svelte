<script lang="ts">
    // @ts-nocheck — module resolution for @tauri-apps/api and @mdi/js is handled
    // by the ncrs-gui vite alias at build time; these imports aren't resolvable
    // from the plugin's location during static analysis.
    import { invoke } from "@tauri-apps/api/core";
    import { onMount } from "svelte";
    import Icon from "$components/Icon.svelte";
    import {
        mdiLock, mdiMagnify, mdiDotsVertical, mdiContentCopy,
        mdiEye, mdiEyeOff, mdiWeb, mdiRefresh,
    } from "@mdi/js";

    interface PasswordEntry {
        id: string;
        label: string;
        username: string;
        password: string;
        url: string;
        notes: string;
        folder: string;
        favorite: boolean;
        trashed: boolean;
        status_code: string;
        created: number;
        updated: number;
    }

    let connected = $state(false);
    let connecting = $state(false);
    let passwords = $state<PasswordEntry[]>([]);
    let searchTerm = $state("");
    let expandedId = $state<string | null>(null);
    let revealedPasswords = $state<Set<string>>(new Set());
    let error = $state<string | null>(null);
    let copied = $state<string | null>(null);

    async function connect() {
        connecting = true;
        error = null;
        try {
            await invoke("nc_passwords_connect");
            connected = true;
            await loadData();
        } catch (e) {
            error = String(e);
        } finally {
            connecting = false;
        }
    }

    async function loadData() {
        try {
            passwords = await invoke<PasswordEntry[]>("nc_passwords_list");
        } catch (e) {
            error = String(e);
        }
    }

    function toggleExpand(id: string) {
        expandedId = expandedId === id ? null : id;
    }

    function toggleReveal(id: string) {
        const next = new Set(revealedPasswords);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        revealedPasswords = next;
    }

    async function copyAndClose(text: string) {
        await navigator.clipboard.writeText(text);
        await invoke("close_window");
    }

    async function copyField(text: string, label: string) {
        await navigator.clipboard.writeText(text);
        copied = label;
        setTimeout(() => { copied = null; }, 1500);
    }

    let filteredPasswords = $derived.by(() => {
        let list = passwords.filter(p => !p.trashed);

        if (searchTerm) {
            const term = searchTerm.toLowerCase();
            list = list.filter(p =>
                p.label.toLowerCase().includes(term) ||
                p.username.toLowerCase().includes(term) ||
                p.url.toLowerCase().includes(term)
            );
        }

        list.sort((a, b) => {
            if (a.favorite !== b.favorite) return a.favorite ? -1 : 1;
            return a.label.localeCompare(b.label);
        });

        return list;
    });

    onMount(async () => {
        connected = await invoke<boolean>("nc_passwords_is_connected");
        if (connected) {
            await loadData();
        } else {
            try {
                await invoke("nc_passwords_connect");
                connected = true;
                await loadData();
            } catch {
                // Auto-connect failed — show manual connect button
            }
        }
    });
</script>

<div class="flex flex-col flex-grow overflow-hidden">
    {#if !connected}
        <div class="flex flex-col items-center justify-center flex-grow gap-4 p-6">
            <Icon class="w-12 h-12 text-gray-400" path={mdiLock} />
            <h2 class="text-lg font-semibold">Nextcloud Passwords</h2>
            <p class="text-sm text-gray-500 text-center">
                Connect to access your passwords stored in Nextcloud.
            </p>
            {#if error}
                <div class="alert alert-error text-xs p-2">{error}</div>
            {/if}
            <button
                class="btn btn-primary btn-sm"
                onclick={connect}
                disabled={connecting}
            >
                {connecting ? "Connecting..." : "Connect"}
            </button>
        </div>
    {:else}
        <!-- Search bar -->
        <div class="flex items-center gap-2 p-2 border-b border-gray-200">
            <Icon class="w-4 h-4 text-gray-400 flex-shrink-0" path={mdiMagnify} />
            <input
                type="text"
                class="input input-xs flex-grow bg-transparent border-0 focus:outline-none"
                placeholder="Search passwords..."
                bind:value={searchTerm}
            />
            <button class="btn btn-ghost btn-xs" onclick={() => loadData()} aria-label="Refresh">
                <Icon class="w-4 h-4" path={mdiRefresh} />
            </button>
        </div>

        {#if error}
            <div class="alert alert-error text-xs m-2 p-2">{error}</div>
        {/if}

        {#if copied}
            <div class="text-xs text-center text-success py-1">{copied} copied</div>
        {/if}

        <!-- Password list -->
        <div class="overflow-y-auto flex-grow">
            {#if filteredPasswords.length === 0}
                <div class="flex flex-col items-center justify-center h-20 text-gray-400 gap-1">
                    <Icon class="w-6 h-6 opacity-40" path={mdiLock} />
                    <span class="text-xs">
                        {searchTerm ? "No matching passwords" : "No passwords"}
                    </span>
                </div>
            {:else}
                {#each filteredPasswords as pw (pw.id)}
                    <div
                        class="flex items-center px-3 py-2 hover:bg-base-200 cursor-pointer border-b border-gray-100"
                        onclick={() => copyAndClose(pw.password)}
                        role="button"
                        tabindex="0"
                        onkeydown={(e) => { if (e.key === 'Enter') copyAndClose(pw.password); }}
                    >
                        <div class="flex-grow min-w-0">
                            <div class="text-sm font-medium truncate">{pw.label}</div>
                            <div class="text-xs text-gray-400 truncate">{pw.username}</div>
                        </div>

                        <button
                            class="btn btn-ghost btn-xs p-0 w-7 h-7 min-h-0 flex-shrink-0"
                            onclick={(e) => { e.stopPropagation(); toggleExpand(pw.id); }}
                            aria-label="Options"
                        >
                            <Icon class="w-4 h-4" path={mdiDotsVertical} />
                        </button>
                    </div>

                    {#if expandedId === pw.id}
                        <div class="bg-base-200 px-3 py-2 flex flex-col gap-2 border-b border-gray-200">
                            <div class="flex items-center gap-2">
                                <span class="text-xs text-gray-500 w-16 flex-shrink-0">Password</span>
                                <span class="text-xs font-mono flex-grow truncate select-all">
                                    {revealedPasswords.has(pw.id) ? pw.password : "••••••••"}
                                </span>
                                <button
                                    class="btn btn-ghost btn-xs p-0 w-6 h-6 min-h-0"
                                    onclick={() => toggleReveal(pw.id)}
                                    title={revealedPasswords.has(pw.id) ? "Hide" : "Reveal"}
                                >
                                    <Icon class="w-3.5 h-3.5" path={revealedPasswords.has(pw.id) ? mdiEyeOff : mdiEye} />
                                </button>
                                <button
                                    class="btn btn-ghost btn-xs p-0 w-6 h-6 min-h-0"
                                    onclick={() => copyField(pw.password, "Password")}
                                    title="Copy password"
                                >
                                    <Icon class="w-3.5 h-3.5" path={mdiContentCopy} />
                                </button>
                            </div>
                            <div class="flex items-center gap-2">
                                <span class="text-xs text-gray-500 w-16 flex-shrink-0">Username</span>
                                <span class="text-xs flex-grow truncate">{pw.username}</span>
                                <button
                                    class="btn btn-ghost btn-xs p-0 w-6 h-6 min-h-0"
                                    onclick={() => copyField(pw.username, "Username")}
                                    title="Copy username"
                                >
                                    <Icon class="w-3.5 h-3.5" path={mdiContentCopy} />
                                </button>
                            </div>
                            {#if pw.url}
                                <div class="flex items-center gap-2">
                                    <span class="text-xs text-gray-500 w-16 flex-shrink-0">URL</span>
                                    <span class="text-xs flex-grow truncate">{pw.url}</span>
                                    <button
                                        class="btn btn-ghost btn-xs p-0 w-6 h-6 min-h-0"
                                        onclick={() => invoke("open_link", { url: pw.url })}
                                        title="Open in browser"
                                    >
                                        <Icon class="w-3.5 h-3.5" path={mdiWeb} />
                                    </button>
                                </div>
                            {/if}
                        </div>
                    {/if}
                {/each}
            {/if}
        </div>
    {/if}
</div>
