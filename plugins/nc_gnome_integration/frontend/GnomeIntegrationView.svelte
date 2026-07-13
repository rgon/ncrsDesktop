<script lang="ts">
    // @ts-nocheck — module resolution for @mdi/js is handled
    // by the ncrs-gui vite alias at build time; these imports aren't resolvable
    // from the plugin's location during static analysis.
    import Icon from "$components/Icon.svelte";
    import { mdiAccountSync } from "@mdi/js";
</script>

<div class="flex flex-col items-center justify-center flex-grow gap-4 p-8">
    <Icon class="w-16 h-16 text-blue-500" path={mdiAccountSync} />
    <h2 class="text-lg font-semibold">GNOME Online Accounts Connected</h2>
    <p class="text-sm text-gray-500 text-center max-w-sm">
        Your Nextcloud account is registered with GNOME Online Accounts.
        Calendar, Contacts, and other GNOME apps can access it automatically.
    </p>
</div>
