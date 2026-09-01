<script lang="ts">
	import { cn, type WithElementRef } from "$lib/utils.js";
	import type { HTMLTextareaAttributes } from "svelte/elements";

	let {
		ref = $bindable(null),
		value = $bindable(),
		class: className,
		...restProps
	}: WithElementRef<HTMLTextareaAttributes> = $props();
</script>

<textarea
	bind:this={ref}
	data-slot="textarea"
	class={cn(
		"flex min-h-16 w-full rounded-lg border border-input bg-transparent px-2.5 py-1.5 text-base transition-colors outline-none field-sizing-content shadow-xs placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:cursor-not-allowed disabled:opacity-50 disabled:bg-input/50 aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20 md:text-sm dark:bg-input/30 dark:disabled:bg-input/80 dark:aria-invalid:border-destructive/50 dark:aria-invalid:ring-destructive/40",
		className
	)}
	bind:value
	{...restProps}
></textarea>
