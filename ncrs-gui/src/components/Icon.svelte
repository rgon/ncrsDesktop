<!-- MDI Icon, pass path as parameter -->
<script lang="ts">
    import type { HTMLAttributes } from "svelte/elements";

    interface Props extends HTMLAttributes<SVGElement> {
        path: string;
    }
    
    let {
        path,
        ...restProps
    }:Props = $props()
</script>

<svg class="icon" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" {...restProps}>
    <path d={path} fill="currentColor" />
</svg>
