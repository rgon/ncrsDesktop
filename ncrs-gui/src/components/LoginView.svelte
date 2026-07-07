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

            {#if status === "error" && errorMsg}
                <p class="lv-error">{errorMsg}</p>
            {/if}

            <button class="nc-btn-primary lv-connect-btn" onclick={connect}>
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
