<script lang="ts">
    import type { HTMLAttributes } from "svelte/elements";

    import { mdiCircle, mdiCancel, mdiMoonWaxingCrescent, mdiCircleOutline } from "@mdi/js";
    import Icon from "../components/Icon.svelte";

    interface Props extends HTMLAttributes<HTMLElement> {
        userName?: string;
        class?: string;
        selectedStatus?: string;
    }

    let {
        userName = "Your Name",
        class:mClass = "",
        selectedStatus = "Online",
        ...restProps
    }:Props = $props();
</script>

<!-- A selector, with images and/or colors to set the user status, on a grid-cols-2 -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<div tabindex="0" class="grid gap-1 grid-cols-2 w-96 {mClass}" {...restProps}>
    <div class="col-span-2">
        <h4 class="text-xl">Online status:</h4>
    </div>

    {#snippet silentmode(icon:string, iconClass:string, title:string, subtitle:string="")}
    {@const selected = selectedStatus === title}
    <button class="status-item flex gap-2 flex-row h-16 p-4 border {selected ? 'border-primary' : 'border-gray-100'} rounded-md cursor-pointer">
        <Icon class="w-4 h-4 inline-block {iconClass}" path={icon} />
        <div class="text-left">
            <h4 class="status-text leading-none">{title}</h4>
            {#if subtitle}
            <span class="text-xs text-gray-400 leading-none"> ({subtitle})</span>
            {/if}
        </div>
    </button>
    {/snippet}
    
    {@render silentmode(mdiCircle, "text-green-500", "Online")}
    {@render silentmode(mdiMoonWaxingCrescent, "text-orange-500", "Away")}
    {@render silentmode(mdiCancel, "text-red-500", "Busy", "mute all notifications")}
    {@render silentmode(mdiCircleOutline, "text-gray-500", "Invisible", "appear offline")}

    <!-- Search -->
    <div class="col-span-2 mt-4">
        <h4 class="text-xl">Set status message:</h4>
        <input type="text" class="input input-bordered w-full max-w-xs" placeholder="Enter status message..." />
    </div>

    <!-- 'Clear status message after' select -->
    <div class="col-span-2 mt-4">
        <h4 class="text-xl">Clear status message after:</h4>
        <select class="select select-bordered w-full max-w-xs">
            <option selected>Don't clear</option>
            <option>1 hour</option>
            <option>2 hours</option>
            <option>4 hours</option>
            <option>8 hours</option>
            <option>24 hours</option>
        </select>
    </div>
</div>