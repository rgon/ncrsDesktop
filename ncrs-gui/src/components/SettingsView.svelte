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
        fuse_passthrough: boolean;
        walker_rate_limit: boolean;
        walker_listings_per_sec: number;
    }

    // One desktop profile, as reported by the service (`INTEGRATIONS`):
    // a toolkit (GIO, KIO) or a file browser that requires one.
    interface Integration {
        id: string;
        kind: "toolkit" | "browser";
        name: string;
        summary: string;
        installed: boolean;
        mode: "auto" | "on" | "off";
        enabled: boolean;
        requires: string[];
        required_by: string[];
        adapter_client_ids: string[];
        adapter_installed: boolean;
        adapter_needs_package: string | null;
        adapter_connected: boolean;
    }

    let { forced = false }: { forced?: boolean } = $props();

    let settings = $state<ConfigSettings | null>(null);
    let version = $state("");
    let status = $state<"idle" | "saving" | "saved" | "error">("idle");
    let errorMsg = $state("");
    let remounting = $state(false);
    let purging = $state(false);
    let purgeMsg = $state("");
    let passthroughStatus = $state<string | null>(null);
    let passthroughBusy = $state(false);
    // null = daemon too old for INTEGRATIONS; undefined = not loaded / unreachable.
    let integrations = $state<Integration[] | null | undefined>(undefined);
    let integrationBusy = $state<string | null>(null);
    let integrationError = $state("");
    let copiedPkg = $state<string | null>(null);

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
        refreshPassthroughStatus();
        refreshIntegrations();
    });

    async function refreshIntegrations() {
        try {
            integrations = await invoke<Integration[] | null>("list_integrations");
        } catch (err: unknown) {
            integrations = undefined;
            integrationError = String(err);
        }
    }

    // The service owns every file-browser side effect; we only send the mode.
    // Like passthrough, this applies live rather than on "Save".
    async function setIntegration(id: string, mode: Integration["mode"]) {
        integrationBusy = id;
        integrationError = "";
        try {
            await invoke("set_integration", { id, mode });
        } catch (err: unknown) {
            integrationError = String(err);
        } finally {
            integrationBusy = null;
            await refreshIntegrations();
        }
    }

    // Toolkits first: they are what the browsers below depend on.
    const integrationGroups = $derived(
        integrations
            ? [
                  { title: "Desktop toolkits", items: integrations.filter((i) => i.kind === "toolkit") },
                  { title: "File browsers", items: integrations.filter((i) => i.kind !== "toolkit") },
              ].filter((g) => g.items.length)
            : [],
    );

    function profileName(id: string): string {
        return integrations?.find((i) => i.id === id)?.name ?? id;
    }

    async function copyInstallCommand(pkg: string) {
        try {
            await navigator.clipboard.writeText(`sudo apt install ${pkg}`);
            copiedPkg = pkg;
            setTimeout(() => { if (copiedPkg === pkg) copiedPkg = null; }, 2000);
        } catch {
            copiedPkg = null;
        }
    }

    async function refreshPassthroughStatus() {
        try {
            passthroughStatus = await invoke<string | null>("get_passthrough_status");
        } catch {
            passthroughStatus = null;
        }
    }

    // Applied live via IPC (no remount needed) as soon as it's toggled, unlike
    // the rest of this form which only takes effect after "Save" + "Remount".
    async function togglePassthrough(e: Event) {
        if (!settings) return;
        const enabled = (e.target as HTMLInputElement).checked;
        passthroughBusy = true;
        try {
            await invoke("set_passthrough_enabled", { enabled });
            settings.fuse_passthrough = enabled;
        } catch (err: unknown) {
            errorMsg = String(err);
            status = "error";
        } finally {
            passthroughBusy = false;
            await refreshPassthroughStatus();
        }
    }

    async function save() {
        if (!settings) return;
        status = "saving";
        errorMsg = "";
        const payload: ConfigSettings = {
            ...settings,
            read_ahead_bytes: readAheadMb * 1024 * 1024,
            cache_max_size_bytes: cacheMaxGb * 1024 * 1024 * 1024,
            // Clearing a number input binds null, which the u64 field cannot
            // deserialize — the whole save would fail with an opaque error.
            dir_cache_max_stale_mins: Math.max(0, Math.round(settings.dir_cache_max_stale_mins || 0)),
            walker_listings_per_sec: Math.min(1000, Math.max(1, Math.round(settings.walker_listings_per_sec || 10))),
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

                <div class="sv-toggle">
                    <div>
                        <label class="sv-toggle-label" for="walker-limit">Slow down folder crawlers</label>
                        <p class="sv-hint">
                            Protects your Nextcloud server from being overwhelmed. Programs that
                            walk the whole folder tree — a <code>find</code> or search across the
                            disk, a backup tool, an indexer, a coding assistant looking for a
                            file — cause one server request for every folder they enter, and can
                            send thousands a minute. With this on, such a program is paced to the
                            rate below once it has used up a short burst. Folders you already have
                            cached are never slowed, so normal browsing is unaffected. Applies
                            immediately when saved.
                        </p>
                    </div>
                    <input id="walker-limit" type="checkbox" class="sv-check" bind:checked={settings.walker_rate_limit} />
                </div>

                <div class="sv-field">
                    <label class="sv-label" for="walker-rate">Crawler pace (folders per second, per program)</label>
                    <input
                        id="walker-rate"
                        class="nc-input sv-num"
                        type="number"
                        min="1"
                        max="1000"
                        disabled={!settings.walker_rate_limit}
                        bind:value={settings.walker_listings_per_sec}
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
                        <label class="sv-toggle-label" for="passthrough">Zero-copy passthrough reads</label>
                        <p class="sv-hint">
                            Once a file is fully downloaded and cached, let the kernel serve reads
                            directly from the cached copy, bypassing ncRS entirely. Requires Linux
                            6.9+ and a capability granted at install time; falls back to normal
                            reads automatically when unavailable. Applies immediately, no remount
                            needed.
                        </p>
                        {#if passthroughStatus}
                            {@const [, capability] = passthroughStatus.split(":")}
                            <p class="sv-hint">
                                {#if capability === "capable"}
                                    Available this session.
                                {:else}
                                    Not available this session (missing capability or kernel
                                    support) — reads fall back to normal, no action needed.
                                {/if}
                            </p>
                        {/if}
                    </div>
                    <input
                        id="passthrough"
                        type="checkbox"
                        class="sv-check"
                        checked={settings.fuse_passthrough}
                        disabled={passthroughBusy}
                        onchange={togglePassthrough}
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
                    <p class="sv-hint">Before showing a directory you have not opened for this long, ask the server whether it changed — so the first listing is already current, at the cost of one small request. Only applies while push notifications are down; 0 disables it.</p>
                </div>

                <div class="sv-toggle">
                    <div>
                        <label class="sv-toggle-label" for="http3">HTTP/3 (QUIC)</label>
                        <p class="sv-hint">Reserved for future use. HTTP/3 support requires server-side QUIC and a probed fallback path; this toggle has no effect on current connections.</p>
                    </div>
                    <input id="http3" type="checkbox" class="sv-check" bind:checked={settings.http3} />
                </div>
            </section>

            <!-- ── File browsers ────────────────── -->
            {#if integrations === null || integrations?.length || integrationError}
                <section class="sv-section">
                    <h3 class="sv-section-title">Desktop integration</h3>

                    {#if integrations === null}
                        <p class="sv-hint">Update the ncrs service to manage file browsers.</p>
                    {:else if integrations}
                        {#each integrationGroups as group (group.title)}
                        <h4 class="sv-fb-group">{group.title}</h4>
                        {#each group.items as fb (fb.id)}
                            <div class="sv-toggle sv-fb">
                                <div class="sv-fb-body">
                                    <div class="sv-fb-head">
                                        <label class="sv-toggle-label" for="fb-{fb.id}">{fb.name}</label>
                                        {#if !fb.installed}
                                            <span class="sv-badge">Not installed</span>
                                        {:else if fb.adapter_client_ids.length && !fb.adapter_installed}
                                            <span class="sv-badge sv-badge-warn">Emblems not installed</span>
                                        {:else if fb.adapter_needs_package}
                                            <span class="sv-badge sv-badge-warn">Emblems need {fb.adapter_needs_package}</span>
                                        {:else if fb.adapter_connected}
                                            <span class="sv-badge sv-badge-ok">Connected</span>
                                        {/if}
                                    </div>
                                    <p class="sv-hint">{fb.summary}</p>
                                    {#if fb.installed && fb.adapter_installed && fb.adapter_needs_package}
                                        <button
                                            class="sv-fb-cmd"
                                            title="Copy to clipboard"
                                            onclick={() => copyInstallCommand(fb.adapter_needs_package!)}
                                        >
                                            <code>sudo apt install {fb.adapter_needs_package}</code>
                                            <span>{copiedPkg === fb.adapter_needs_package ? "Copied" : "Copy"}</span>
                                        </button>
                                    {/if}
                                    {#if fb.required_by.length}
                                        <p class="sv-hint">Kept on by {fb.required_by.map(profileName).join(", ")}</p>
                                    {/if}
                                    {#if fb.mode !== "auto"}
                                        <p class="sv-hint">
                                            Manual ·
                                            <button
                                                class="sv-link"
                                                disabled={integrationBusy === fb.id}
                                                onclick={() => setIntegration(fb.id, "auto")}
                                            >Reset to automatic</button>
                                        </p>
                                    {/if}
                                </div>
                                <input
                                    id="fb-{fb.id}"
                                    type="checkbox"
                                    class="sv-check"
                                    checked={fb.enabled}
                                    disabled={integrationBusy === fb.id || fb.required_by.length > 0}
                                    onchange={(e) => setIntegration(fb.id, (e.target as HTMLInputElement).checked ? "on" : "off")}
                                />
                            </div>
                        {/each}
                        {/each}
                    {/if}
                    {#if integrationError}
                        <p class="sv-error">{integrationError}</p>
                    {/if}
                </section>
            {/if}

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
    appearance: none;
    -webkit-appearance: none;
    width: 16px;
    height: 16px;
    border: 1px solid var(--nc-border);
    border-radius: 4px;
    background: var(--nc-surface);
    display: inline-block;
    cursor: pointer;
    flex-shrink: 0;
    transition: background-color 0.12s, border-color 0.12s;
}

/* Tick geometry: stroke bbox is x [3,13] / y [4.05,11.95] in the 16 viewBox,
   both centred on 8.0, so it stays centred at any background-size. */
.sv-check:checked {
    background-color: var(--nc-accent);
    border-color: var(--nc-accent);
    background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Cpath d='M4 8.15 L6.8 10.95 L12 5.05' fill='none' stroke='%23fff' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'/%3E%3C/svg%3E");
    background-repeat: no-repeat;
    background-position: center;
    background-size: contain;
}

.sv-check:focus-visible {
    outline: 2px solid var(--nc-accent);
    outline-offset: 2px;
}

/* ── File browsers ───────────────────────── */

.sv-fb { align-items: flex-start; }
.sv-fb-group { margin: 0.75rem 0 0.25rem; font-size: 0.8rem; font-weight: 600; color: var(--nc-text-2); text-transform: uppercase; letter-spacing: 0.04em; }
.sv-fb .sv-check { margin-top: 2px; }

.sv-fb-body { min-width: 0; }

.sv-fb-head {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
}

.sv-badge {
    font-size: 9px;
    font-weight: 600;
    line-height: 1.5;
    padding: 0 6px;
    border-radius: 999px;
    color: var(--nc-text-3);
    background: color-mix(in srgb, var(--nc-text-3) 12%, transparent);
}
.sv-badge-warn {
    color: var(--nc-warning);
    background: color-mix(in srgb, var(--nc-warning) 12%, transparent);
}
.sv-badge-ok {
    color: var(--nc-success);
    background: color-mix(in srgb, var(--nc-success) 12%, transparent);
}

.sv-fb-cmd {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    margin-top: 4px;
    padding: 2px 6px;
    font-size: 10px;
    color: var(--nc-text-2);
    background: color-mix(in srgb, var(--nc-text-3) 8%, transparent);
    border: 1px solid var(--nc-border);
    border-radius: 4px;
    cursor: pointer;
}
.sv-fb-cmd code { font-family: monospace; user-select: all; }
.sv-fb-cmd span { color: var(--nc-accent); }

.sv-link {
    font-size: inherit;
    color: var(--nc-accent);
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
}
.sv-link:hover { text-decoration: underline; }
.sv-link:disabled { opacity: 0.5; cursor: default; }

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
