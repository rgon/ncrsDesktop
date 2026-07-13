<script lang="ts">
    // @ts-nocheck — module resolution for @mdi/js is handled
    // by the ncrs-gui vite alias at build time; these imports aren't resolvable
    // from the plugin's location during static analysis.
    import Icon from "$components/Icon.svelte";
    import { mdiCalendar } from "@mdi/js";
</script>

<div class="flex flex-col items-center justify-center flex-grow gap-4 p-8">
    <Icon class="w-16 h-16 text-blue-500" path={mdiCalendar} />
    <h2 class="text-lg font-semibold">Calendar Connected</h2>
    <p class="text-sm text-gray-500 text-center max-w-sm">
        Your Nextcloud calendar is registered with GNOME Online Accounts. Open
        GNOME Calendar to view your events.
    </p>
</div>
