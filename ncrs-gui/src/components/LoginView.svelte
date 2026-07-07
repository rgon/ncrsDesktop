<script lang="ts">
    import { invoke } from "@tauri-apps/api/core";
    import { listen } from "@tauri-apps/api/event";
    import { onMount } from "svelte";

    let { initialServerUrl = "" }: { initialServerUrl?: string } = $props();
    let serverUrl = $state(initialServerUrl);
    let status = $state<"idle" | "waiting" | "error">("idle");
    let errorMsg = $state("");
    let loginUrl = $state("");

    let unlisten: (() => void) | null = null;

    async function connect() {
        const trimmed = serverUrl.trim().replace(/\/+$/, "");
        if (!trimmed) { errorMsg = "Enter a server URL first."; status = "error"; return; }

        status = "waiting";
        errorMsg = "";
        loginUrl = "";

        try {
            loginUrl = await invoke<string>("start_login_flow", { serverUrl: trimmed });
        } catch (e: unknown) {
            errorMsg = String(e);
            status = "error";
        }
    }

    onMount(() => {
        const errP = listen<string>("login-error", (e) => {
            errorMsg = e.payload;
            status = "error";
        });
        errP.then(fn => { unlisten = fn; });

        return () => {
            unlisten?.();
        };
    });
</script>

<div class="flex flex-col items-center justify-center h-full gap-6 px-8 py-6">
    <div class="text-center">
        <h2 class="text-lg font-bold">Connect to Nextcloud</h2>
        <p class="text-xs text-gray-500 mt-1">Enter your server address to sign in</p>
    </div>

    {#if status !== "waiting"}
        <div class="w-full flex flex-col gap-2">
            <label class="text-xs font-semibold" for="server-url">Server URL</label>
            <input
                id="server-url"
                class="input input-bordered input-sm w-full"
                type="url"
                placeholder="https://cloud.example.com"
                bind:value={serverUrl}
                onkeydown={(e) => { if (e.key === "Enter") connect(); }}
            />
        </div>

        {#if status === "error" && errorMsg}
            <p class="text-xs text-error text-center">{errorMsg}</p>
        {/if}

        <button class="btn btn-primary btn-sm w-full" onclick={connect}>
            Connect
        </button>
    {:else}
        <div class="flex flex-col items-center gap-3">
            <span class="loading loading-spinner loading-md text-primary"></span>
            <p class="text-sm font-medium text-center">Waiting for authorization in your browser…</p>
            <p class="text-xs text-gray-400 text-center">
                A browser window should have opened. Log in to Nextcloud and approve the connection.
            </p>
            {#if loginUrl}
                <a
                    class="text-xs text-primary underline break-all text-center"
                    href={loginUrl}
                    onclick={(e) => { e.preventDefault(); invoke("open_link", { url: loginUrl }); }}
                >
                    Open login page manually
                </a>
            {/if}
            <button
                class="btn btn-ghost btn-xs mt-2"
                onclick={() => { status = "idle"; errorMsg = ""; }}
            >
                Cancel
            </button>
        </div>
    {/if}
</div>
