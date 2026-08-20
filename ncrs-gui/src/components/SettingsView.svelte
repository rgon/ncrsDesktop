<script lang="ts">
    import { invoke } from "@tauri-apps/api/core";
    import { onMount } from "svelte";

    interface ConfigSettings {
        mount_point: string;
        aggressive_prefetch: boolean;
        http3: boolean;
        max_concurrent_requests: number;
        optimistic_listing: boolean;
        dir_cache_max_stale_mins: number;
        auto_keep_locally_modified_files: boolean;
        auto_keep_cached_files: boolean;
        read_ahead_bytes: number;
        cache_max_size_bytes: number;
        cache_auto_purge_days: number;
        cache_cleanup_interval_secs: number;
        cache_streamed_reads: boolean;
        cleanup_stale_gio_temps: boolean;
        stale_gio_temp_mins: number;
    }

    let { forced = false }: { forced?: boolean } = $props();

    let settings = $state<ConfigSettings | null>(null);
    let version = $state("");
    let status = $state<"idle" | "saving" | "saved" | "error">("idle");
    let errorMsg = $state("");
    let remounting = $state(false);
    let purging = $state(false);
    let purgeMsg = $state("");

    // Displayed as MB / GB; stored as bytes
    let readAheadMb = $state(64);
    let cacheMaxGb = $state(32);

    onMount(async () => {
        const [cfg, ver] = await Promise.all([
            invoke<ConfigSettings>("get_config_values"),
            invoke<string>("get_app_version"),
        ]);
        settings = cfg;
        version = ver;
        readAheadMb = Math.round(cfg.read_ahead_bytes / (1024 * 1024));
        cacheMaxGb = Math.round(cfg.cache_max_size_bytes / (1024 * 1024 * 1024));
    });

    async function save() {
        if (!settings) return;
        status = "saving";
        errorMsg = "";
        const payload: ConfigSettings = {
            ...settings,
            read_ahead_bytes: readAheadMb * 1024 * 1024,
            cache_max_size_bytes: cacheMaxGb * 1024 * 1024 * 1024,
        };
        try {
            await invoke("save_config_values", { values: payload });
            settings = payload;
            status = "saved";
            setTimeout(() => { if (status === "saved") status = "idle"; }, 2500);
        } catch (e: unknown) {
            errorMsg = String(e);
            status = "error";
        }
    }

    async function remount() {
        remounting = true;
        errorMsg = "";
        try {
            await invoke("remount");
        } catch (e: unknown) {
            errorMsg = String(e);
        } finally {
            remounting = false;
        }
    }

    async function purgeCache() {
        purging = true;
        purgeMsg = "";
        try {
            const cleared = await invoke<number>("purge_cache");
            purgeMsg = `Cleared ${cleared} cached file${cleared === 1 ? "" : "s"}.`;
        } catch (e: unknown) {
            purgeMsg = `Failed: ${String(e)}`;
        } finally {
            purging = false;
        }
    }
</script>

<div class="sv-root">
    {#if forced}
        <div class="sv-banner">
            <strong>Mount point required.</strong> Set a local directory for your Nextcloud files.
        </div>
    {/if}

    {#if !settings}
        <div class="sv-loading">Loading…</div>
    {:else}
        <div class="sv-scroll">
            <!-- ── Mount ─────────────────────────── -->
            <section class="sv-section">
                <h3 class="sv-section-title">Mount</h3>

                <div class="sv-field">
                    <label class="sv-label" for="mount-point">Mount directory</label>
                    <input
                        id="mount-point"
                        class="nc-input"
                        type="text"
                        placeholder="/home/user/Nextcloud"
                        bind:value={settings.mount_point}
                        spellcheck="false"
                    />
                    <p class="sv-hint">Local path where your Nextcloud files appear. The directory is created automatically.</p>
                </div>
            </section>

            <!-- ── Cache ────────────────────────── -->
            <section class="sv-section">
                <h3 class="sv-section-title">Cache</h3>

                <div class="sv-field">
                    <label class="sv-label" for="cache-max">Max cache size (GB)</label>
                    <input
                        id="cache-max"
                        class="nc-input sv-num"
                        type="number"
                        min="1"
                        max="2000"
                        bind:value={cacheMaxGb}
                    />
                </div>

                <div class="sv-field">
                    <label class="sv-label" for="cache-purge">Auto-purge after (days)</label>
                    <input
                        id="cache-purge"
                        class="nc-input sv-num"
                        type="number"
                        min="1"
                        max="365"
                        bind:value={settings.cache_auto_purge_days}
                    />
                </div>

                <div class="sv-toggle">
                    <div>
                        <label class="sv-toggle-label" for="cache-streamed">Cache streamed reads</label>
                        <p class="sv-hint">Save data read via streaming (e.g. media playback) to the local cache. Subsequent opens are served from disk instead of re-downloading. Increases disk usage.</p>
                    </div>
                    <input id="cache-streamed" type="checkbox" class="sv-check" bind:checked={settings.cache_streamed_reads} />
                </div>

                <div class="sv-toggle">
                    <label class="sv-toggle-label" for="keep-modified">Keep locally modified files</label>
                    <input id="keep-modified" type="checkbox" class="sv-check" bind:checked={settings.auto_keep_locally_modified_files} />
                </div>

                <div class="sv-toggle">
                    <label class="sv-toggle-label" for="keep-cached">Keep all cached files</label>
                    <input id="keep-cached" type="checkbox" class="sv-check" bind:checked={settings.auto_keep_cached_files} />
                </div>

                <div class="sv-field">
                    <button class="nc-btn-ghost sv-purge-btn" onclick={purgeCache} disabled={purging}>
                        {purging ? "Purging…" : "Purge local cache"}
                    </button>
                    <p class="sv-hint">
                        Delete all locally cached copies so files re-download fresh from the
                        server. Files with unsynced local edits are kept. Use this if a cached
                        file looks stale or corrupt.
                    </p>
                    {#if purgeMsg}
                        <p class="sv-purge-msg">{purgeMsg}</p>
                    {/if}
                </div>
            </section>

            <!-- ── Performance ──────────────────── -->
            <section class="sv-section">
                <h3 class="sv-section-title">Performance</h3>

                <div class="sv-field">
                    <label class="sv-label" for="concurrent">Max concurrent requests</label>
                    <input
                        id="concurrent"
                        class="nc-input sv-num"
                        type="number"
                        min="1"
                        max="64"
                        bind:value={settings.max_concurrent_requests}
                    />
                </div>

                <div class="sv-field">
                    <label class="sv-label" for="read-ahead">Read-ahead buffer (MB)</label>
                    <input
                        id="read-ahead"
                        class="nc-input sv-num"
                        type="number"
                        min="1"
                        max="512"
                        bind:value={readAheadMb}
                    />
                </div>

                <div class="sv-toggle">
                    <div>
                        <label class="sv-toggle-label" for="prefetch">Aggressive prefetch</label>
                        <p class="sv-hint">Pre-fetch metadata and thumbnails for every file in a directory as soon as it is listed. Speeds up browsing at the cost of extra network traffic on large directories.</p>
                    </div>
                    <input id="prefetch" type="checkbox" class="sv-check" bind:checked={settings.aggressive_prefetch} />
                </div>

                <div class="sv-toggle">
                    <div>
                        <label class="sv-toggle-label" for="optimistic">Optimistic directory listing</label>
                        <p class="sv-hint">Return directory listings immediately from the local cache while a background refresh fetches the latest contents. Keeps the file manager responsive; disable if listings must always reflect live server state.</p>
                    </div>
                    <input id="optimistic" type="checkbox" class="sv-check" bind:checked={settings.optimistic_listing} />
                </div>

                <div class="sv-field">
                    <label class="sv-label" for="max-stale">Force refresh listings older than (minutes)</label>
                    <input
                        id="max-stale"
                        class="nc-input sv-num"
                        type="number"
                        min="0"
                        max="10080"
                        bind:value={settings.dir_cache_max_stale_mins}
                    />
                    <p class="sv-hint">A directory you have not opened for longer than this is fetched from the server before it is shown, so the first listing is already up to date instead of correcting itself on a second look. 0 always serves the cached listing first.</p>
                </div>

                <div class="sv-toggle">
                    <div>
                        <label class="sv-toggle-label" for="http3">HTTP/3 (QUIC)</label>
                        <p class="sv-hint">Reserved for future use. HTTP/3 support requires server-side QUIC and a probed fallback path; this toggle has no effect on current connections.</p>
                    </div>
                    <input id="http3" type="checkbox" class="sv-check" bind:checked={settings.http3} />
                </div>
            </section>

            <!-- ── Integration ──────────────────── -->
            <section class="sv-section">
                <h3 class="sv-section-title">Integration</h3>

                <div class="sv-toggle">
                    <div>
                        <label class="sv-toggle-label" for="gio-cleanup">GNOME auto-cleanup of intermediate files</label>
                        <p class="sv-hint">GTK/GIO apps (Nautilus, gedit, etc.) write files atomically via a <code>.goutputstream-*</code> or <code>.xdp-*</code> temp file that is renamed within seconds. When enabled, ncRS deletes these intermediates from the server immediately and hides them from directory listings. Orphans left by crashed apps are cleaned up on the next folder open. Disable only if another WebDAV client on the same account needs to see these files.</p>
                    </div>
                    <input id="gio-cleanup" type="checkbox" class="sv-check" bind:checked={settings.cleanup_stale_gio_temps} />
                </div>
            </section>
        </div>

        <!-- ── Footer ───────────────────────────── -->
        <div class="sv-footer">
            {#if status === "error" && errorMsg}
                <p class="sv-error">{errorMsg}</p>
            {:else if status === "saved"}
                <p class="sv-ok">Saved. Remount to apply changes.</p>
            {/if}
            <div class="sv-btn-row">
                <button
                    class="nc-btn-primary sv-save-btn"
                    onclick={save}
                    disabled={status === "saving"}
                >
                    {status === "saving" ? "Saving…" : "Save settings"}
                </button>
                <button
                    class="nc-btn-ghost sv-remount-btn"
                    onclick={remount}
                    disabled={remounting}
                    title="Unmount and remount to apply saved settings"
                >
                    {remounting ? "Remounting…" : "Remount"}
                </button>
            </div>
            <p class="sv-version">ncRS Desktop v{version}</p>
        </div>
    {/if}
</div>

<style>
.sv-root {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
}

.sv-banner {
    flex-shrink: 0;
    background: color-mix(in srgb, var(--nc-warning) 12%, transparent);
    border-bottom: 1px solid color-mix(in srgb, var(--nc-warning) 30%, transparent);
    color: var(--nc-warning);
    font-size: 12px;
    padding: 8px 14px;
    line-height: 1.4;
}

.sv-loading {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 12px;
    color: var(--nc-text-3);
}

.sv-scroll {
    flex: 1;
    overflow-y: auto;
    padding: 4px 0 8px;
}

.sv-section {
    padding: 10px 14px 4px;
    border-bottom: 1px solid var(--nc-border);
}
.sv-section:last-child { border-bottom: none; }

.sv-section-title {
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--nc-text-3);
    margin-bottom: 8px;
}

.sv-field {
    margin-bottom: 10px;
}

.sv-label {
    display: block;
    font-size: 11px;
    font-weight: 600;
    color: var(--nc-text-2);
    margin-bottom: 4px;
}

.sv-hint code {
    font-family: monospace;
    font-size: 9px;
    background: color-mix(in srgb, var(--nc-text-3) 12%, transparent);
    border-radius: 3px;
    padding: 0 3px;
}

.sv-hint {
    font-size: 10px;
    color: var(--nc-text-3);
    margin-top: 3px;
    line-height: 1.4;
}

.sv-num {
    width: 96px;
}

.sv-toggle {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 5px 0;
    border-top: 1px solid var(--nc-border);
}
.sv-toggle:first-of-type { border-top: none; }

.sv-toggle-label {
    font-size: 12px;
    color: var(--nc-text);
    cursor: pointer;
}

.sv-check {
    width: 16px;
    height: 16px;
    accent-color: var(--nc-accent);
    cursor: pointer;
    flex-shrink: 0;
}

/* ── Footer ──────────────────────────────── */

.sv-footer {
    flex-shrink: 0;
    padding: 10px 14px 12px;
    border-top: 1px solid var(--nc-border);
    background: var(--nc-surface);
    display: flex;
    flex-direction: column;
    gap: 6px;
}

.sv-btn-row {
    display: flex;
    gap: 8px;
}

.sv-save-btn {
    flex: 1;
}

.sv-remount-btn {
    flex-shrink: 0;
}

.sv-purge-btn {
    align-self: flex-start;
    margin-top: 2px;
}

.sv-purge-msg {
    font-size: 11px;
    color: var(--nc-text-2);
    margin-top: 4px;
}

.sv-error {
    font-size: 11px;
    color: var(--nc-error);
    line-height: 1.4;
}

.sv-ok {
    font-size: 11px;
    color: var(--nc-success);
}

.sv-version {
    font-size: 10px;
    color: var(--nc-text-3);
    text-align: center;
    margin-top: 2px;
}
</style>
