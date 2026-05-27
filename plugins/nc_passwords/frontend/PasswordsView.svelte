<script lang="ts">
    import { invoke } from "@tauri-apps/api/core";
    import { onMount } from "svelte";
    import Icon from "$components/Icon.svelte";
    import {
        mdiLock, mdiMagnify, mdiContentCopy, mdiEye, mdiEyeOff,
        mdiStar, mdiStarOutline, mdiFolder, mdiArrowLeft, mdiRefresh,
        mdiPlus, mdiDelete, mdiWeb,
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
        status_code: number;
        created: number;
        updated: number;
    }

    interface FolderEntry {
        id: string;
        label: string;
        parent: string;
        favorite: boolean;
    }

    let connected = $state(false);
    let connecting = $state(false);
    let passwords = $state<PasswordEntry[]>([]);
    let folders = $state<FolderEntry[]>([]);
    let searchTerm = $state("");
    let currentFolder = $state<string | null>(null);
    let revealedPasswords = $state<Set<string>>(new Set());
    let error = $state<string | null>(null);

    const BASE_FOLDER = "00000000-0000-0000-0000-000000000000";

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
            folders = await invoke<FolderEntry[]>("nc_passwords_folders");
        } catch (e) {
            error = String(e);
        }
    }

    async function refresh() {
        await loadData();
    }

    async function deletePassword(id: string) {
        try {
            await invoke("nc_passwords_delete", { id });
            passwords = passwords.filter(p => p.id !== id);
        } catch (e) {
            error = String(e);
        }
    }

    function toggleReveal(id: string) {
        const next = new Set(revealedPasswords);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        revealedPasswords = next;
    }

    async function copyToClipboard(text: string) {
        await navigator.clipboard.writeText(text);
    }

    function extractDomain(url: string): string {
        try {
            return new URL(url).hostname;
        } catch {
            return url;
        }
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
        } else if (currentFolder !== null) {
            list = list.filter(p => p.folder === currentFolder);
        }

        list.sort((a, b) => {
            if (a.favorite !== b.favorite) return a.favorite ? -1 : 1;
            return a.label.localeCompare(b.label);
        });

        return list;
    });

    let currentFolderName = $derived(
        currentFolder === null || currentFolder === BASE_FOLDER
            ? "All Passwords"
            : folders.find(f => f.id === currentFolder)?.label ?? "Folder"
    );

    let subfolders = $derived(
        folders.filter(f =>
            f.parent === (currentFolder ?? BASE_FOLDER)
        )
    );

    onMount(async () => {
        connected = await invoke<boolean>("nc_passwords_is_connected");
        if (connected) await loadData();
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
        <!-- Toolbar -->
        <div class="flex items-center gap-2 p-2 border-b border-gray-200">
            {#if currentFolder !== null}
                <button
                    class="btn btn-ghost btn-xs"
                    onclick={() => { currentFolder = null; }}
                    aria-label="Back"
                >
                    <Icon class="w-4 h-4" path={mdiArrowLeft} />
                </button>
            {/if}

            <span class="text-sm font-semibold flex-shrink-0">{currentFolderName}</span>

            <div class="flex-grow"></div>

            <div class="join">
                <input
                    type="text"
                    class="input input-xs join-item w-32"
                    placeholder="Search..."
                    bind:value={searchTerm}
                />
                <button class="btn btn-ghost btn-xs join-item" aria-label="Search">
                    <Icon class="w-4 h-4" path={mdiMagnify} />
                </button>
            </div>

            <button class="btn btn-ghost btn-xs" onclick={refresh} aria-label="Refresh">
                <Icon class="w-4 h-4" path={mdiRefresh} />
            </button>
        </div>

        {#if error}
            <div class="alert alert-error text-xs m-2 p-2">{error}</div>
        {/if}

        <!-- Content -->
        <div class="overflow-y-auto flex-grow p-2">
            <!-- Subfolders -->
            {#if !searchTerm && subfolders.length > 0}
                <div class="mb-2">
                    {#each subfolders as folder (folder.id)}
                        <button
                            class="flex items-center gap-2 w-full p-2 hover:bg-base-200 rounded-lg text-left"
                            onclick={() => { currentFolder = folder.id; }}
                        >
                            <Icon class="w-5 h-5 text-yellow-500" path={mdiFolder} />
                            <span class="text-sm font-medium">{folder.label}</span>
                        </button>
                    {/each}
                </div>
            {/if}

            <!-- Passwords -->
            {#if filteredPasswords.length === 0}
                <div class="flex flex-col items-center justify-center h-20 text-gray-400 gap-1">
                    <Icon class="w-6 h-6 opacity-40" path={mdiLock} />
                    <span class="text-xs">
                        {searchTerm ? "No matching passwords" : "No passwords in this folder"}
                    </span>
                </div>
            {:else}
                {#each filteredPasswords as pw (pw.id)}
                    <div class="flex items-center gap-2 p-2 hover:bg-base-200 rounded-lg group">
                        <div class="flex-shrink-0 w-8 h-8 rounded-full bg-base-300 flex items-center justify-center text-xs font-bold">
                            {pw.label[0]?.toUpperCase() ?? "?"}
                        </div>

                        <div class="flex-grow min-w-0">
                            <div class="flex items-center gap-1">
                                {#if pw.favorite}
                                    <Icon class="w-3 h-3 text-yellow-500" path={mdiStar} />
                                {/if}
                                <span class="text-sm font-medium truncate">{pw.label}</span>
                            </div>
                            <div class="text-xs text-gray-500 truncate">{pw.username}</div>
                            {#if pw.url}
                                <div class="text-xs text-gray-400 truncate">{extractDomain(pw.url)}</div>
                            {/if}
                        </div>

                        <div class="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity flex-shrink-0">
                            <button
                                class="btn btn-ghost btn-xs p-0 w-6 h-6 min-h-0"
                                onclick={() => copyToClipboard(pw.username)}
                                title="Copy username"
                            >
                                <Icon class="w-3.5 h-3.5" path={mdiContentCopy} />
                            </button>
                            <button
                                class="btn btn-ghost btn-xs p-0 w-6 h-6 min-h-0"
                                onclick={() => toggleReveal(pw.id)}
                                title={revealedPasswords.has(pw.id) ? "Hide password" : "Show password"}
                            >
                                <Icon class="w-3.5 h-3.5" path={revealedPasswords.has(pw.id) ? mdiEyeOff : mdiEye} />
                            </button>
                            <button
                                class="btn btn-ghost btn-xs p-0 w-6 h-6 min-h-0"
                                onclick={() => copyToClipboard(pw.password)}
                                title="Copy password"
                            >
                                <Icon class="w-3.5 h-3.5" path={mdiLock} />
                            </button>
                            {#if pw.url}
                                <button
                                    class="btn btn-ghost btn-xs p-0 w-6 h-6 min-h-0"
                                    onclick={() => invoke("open_link", { url: pw.url })}
                                    title="Open URL"
                                >
                                    <Icon class="w-3.5 h-3.5" path={mdiWeb} />
                                </button>
                            {/if}
                            <button
                                class="btn btn-ghost btn-xs p-0 w-6 h-6 min-h-0 text-error"
                                onclick={() => deletePassword(pw.id)}
                                title="Delete"
                            >
                                <Icon class="w-3.5 h-3.5" path={mdiDelete} />
                            </button>
                        </div>
                    </div>

                    {#if revealedPasswords.has(pw.id)}
                        <div class="ml-12 mb-2 p-2 bg-base-200 rounded text-xs font-mono select-all">
                            {pw.password}
                        </div>
                    {/if}
                {/each}
            {/if}
        </div>
    {/if}
</div>
