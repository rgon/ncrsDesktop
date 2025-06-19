<script lang="ts">
    import type { HTMLAttributes } from "svelte/elements";
    
    // import Icon from "../components/Icon.svelte";
    // import { mdiCircle, mdiCancel, mdiMoonWaxingCrescent, mdiCircleOutline } from "@mdi/js";
    interface Props extends HTMLAttributes<HTMLElement> {
        class?: string;
    }

    let {
        class:mClass = "",
        ...restProps
    }:Props = $props();
</script>

<!-- Sync progress bar -->

<div class="h-8 flex items-center justify-center bg-base-300 {mClass}" {...restProps}>
    <div class="w-64">
        <progress class="progress progress-primary w-full" value="" max="100">70%</progress>
    </div>
    <span class="ml-4 text-lg">Syncing...</span>
</div>