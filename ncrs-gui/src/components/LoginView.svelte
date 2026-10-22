<script lang="ts">
    import { invoke } from "@tauri-apps/api/core";
    import { listen } from "@tauri-apps/api/event";
    import { onMount } from "svelte";

    let { initialServerUrl = "" }: { initialServerUrl?: string } = $props();
    let serverUrl = $state(initialServerUrl);
    let status = $state<"idle" | "waiting" | "error">("idle");
    let errorMsg = $state("");
    let loginUrl = $state("");
    // null = a mount point is already configured (re-login keeps it), so the
    // folder field is hidden.
    let mountPoint = $state<string | null>(null);
    let mountExists = $state(false);
    // Until setup_mount_point answers, null would read as "keep the configured
    // folder" and a fresh install would sign in with none.
    let mountLoaded = $state(false);
    const canConnect = $derived(mountLoaded && (mountPoint === null || (!mountExists && mountPoint.trim().startsWith("/"))));

    // An existing folder may belong to another sync client (the official
    // client also defaults to ~/Nextcloud), so sign-in needs a new one.
    $effect(() => {
        if (mountPoint === null) return;
        const path = mountPoint.trim();
        invoke<boolean>("path_exists", { path }).then(exists => {
            if (path === mountPoint?.trim()) mountExists = exists;
        });
    });

    let unlisten: (() => void) | null = null;

    async function connect() {
        if (!canConnect) return;
        const trimmed = serverUrl.trim().replace(/\/+$/, "");
        if (!trimmed) { errorMsg = "Enter a server URL first."; status = "error"; return; }

        status = "waiting";
        errorMsg = "";
        loginUrl = "";

        try {
            loginUrl = await invoke<string>("start_login_flow", { serverUrl: trimmed, mountPoint: mountPoint?.trim() ?? null });
        } catch (e: unknown) {
            errorMsg = String(e);
            status = "error";
        }
    }

    onMount(() => {
        invoke<string | null>("setup_mount_point")
            .then(mp => { mountPoint = mp; }, () => { mountPoint = ""; })
            .finally(() => { mountLoaded = true; });
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

<div class="lv-root">
    <!-- Wordmark -->
    <div class="lv-brand">
        <p class="lv-product">ncRS Desktop</p>
        <p class="lv-tagline">Nextcloud sync client</p>
    </div>

    {#if status !== "waiting"}
        <!-- Form -->
        <div class="lv-form">
            <label class="lv-label" for="server-url">Nextcloud server</label>
            <input
                id="server-url"
                class="nc-input"
                type="url"
                placeholder="https://cloud.example.com"
                bind:value={serverUrl}
                onkeydown={(e) => { if (e.key === "Enter") connect(); }}
                autocomplete="url"
                spellcheck="false"
            />

            {#if mountPoint !== null}
                <label class="lv-label" for="mount-point">Local folder</label>
                <input
                    id="mount-point"
                    class="nc-input"
                    type="text"
                    bind:value={mountPoint}
                    onkeydown={(e) => { if (e.key === "Enter") connect(); }}
                    spellcheck="false"
                />
                {#if mountExists}
                    <p class="lv-error">This folder already exists — another sync client may be using it. Choose a new folder.</p>
                {/if}
            {/if}

            {#if status === "error" && errorMsg}
                <p class="lv-error">{errorMsg}</p>
            {/if}

            <button class="nc-btn-primary lv-connect-btn" onclick={connect} disabled={!canConnect}>
                Sign in with Nextcloud
            </button>
        </div>

        <p class="lv-hint">
            Your browser will open to complete sign-in. Your password is never stored in the config file.
        </p>

    {:else}
        <!-- Waiting state -->
        <div class="lv-waiting">
            <div class="lv-spinner" aria-hidden="true"></div>
            <p class="lv-waiting-title">Waiting for authorization…</p>
            <p class="lv-waiting-sub">
                Your browser should have opened. Log in and approve the connection.
            </p>

            {#if loginUrl}
                <button
                    class="lv-manual-link"
                    onclick={() => invoke("open_link", { url: loginUrl })}
                >
                    Open login page manually →
                </button>
            {/if}

            <button
                class="nc-btn-ghost lv-cancel-btn"
                onclick={() => { status = "idle"; errorMsg = ""; }}
            >
                Cancel
            </button>
        </div>
    {/if}
</div>

<style>
.lv-root {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 32px 28px;
    gap: 24px;
    background: var(--nc-bg);
}

/* ── Brand ─────────────────────────────────── */

.lv-brand {
    text-align: center;
}

.lv-product {
    font-size: 16px;
    font-weight: 700;
    color: var(--nc-text);
    line-height: 1.2;
    letter-spacing: -0.01em;
}

.lv-tagline {
    font-size: 11px;
    color: var(--nc-text-3);
    margin-top: 2px;
}

/* ── Form ──────────────────────────────────── */

.lv-form {
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: 10px;
}

.lv-label {
    font-size: 11px;
    font-weight: 600;
    color: var(--nc-text-2);
    letter-spacing: 0.04em;
    text-transform: uppercase;
}

.lv-error {
    font-size: 12px;
    color: var(--nc-error);
    line-height: 1.4;
}

.lv-connect-btn {
    margin-top: 4px;
}

/* ── Hint ──────────────────────────────────── */

.lv-hint {
    font-size: 11px;
    color: var(--nc-text-3);
    text-align: center;
    line-height: 1.5;
    max-width: 280px;
}

/* ── Waiting ───────────────────────────────── */

.lv-waiting {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    width: 100%;
}

@keyframes lv-rotate {
    to { transform: rotate(360deg); }
}

.lv-spinner {
    width: 28px;
    height: 28px;
    border-radius: 50%;
    border: 3px solid var(--nc-border);
    border-top-color: var(--nc-accent);
    flex-shrink: 0;
}

@media (prefers-reduced-motion: no-preference) {
    .lv-spinner { animation: lv-rotate 0.85s linear infinite; }
}

.lv-waiting-title {
    font-size: 14px;
    font-weight: 600;
    color: var(--nc-text);
}

.lv-waiting-sub {
    font-size: 12px;
    color: var(--nc-text-2);
    text-align: center;
    line-height: 1.5;
    max-width: 260px;
}

.lv-manual-link {
    font-size: 12px;
    color: var(--nc-accent);
    background: none;
    border: none;
    cursor: pointer;
    padding: 0;
    text-decoration: underline;
    text-underline-offset: 2px;
    transition: opacity 0.1s;
}
.lv-manual-link:hover { opacity: 0.75; }

.lv-cancel-btn {
    margin-top: 4px;
    width: auto;
}
</style>
