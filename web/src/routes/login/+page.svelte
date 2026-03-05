<script lang="ts">
	import { enhance } from '$app/forms';
	import type { ActionData } from './$types';

	let { form }: { form: ActionData } = $props();
</script>

<div class="min-h-screen bg-neutral-950 flex items-center justify-center font-mono">
	<div class="w-full max-w-sm p-6">
		<h1 class="text-green-400 text-xl mb-6 text-center font-bold tracking-wide">sctlin</h1>

		{#if form?.error}
			<div class="bg-red-950 border border-red-800 text-red-400 px-3 py-2 rounded mb-4 text-sm">
				{form.error}
			</div>
		{/if}

		<form method="POST" action="?/login" use:enhance>
			<div class="mb-4">
				<label for="username" class="block text-neutral-400 text-sm mb-1">username</label>
				<input
					id="username"
					name="username"
					type="text"
					value={form?.username ?? ''}
					autocomplete="username"
					class="w-full bg-neutral-900 border border-neutral-700 text-neutral-100 px-3 py-2 rounded text-sm focus:outline-none focus:border-green-600"
				/>
			</div>

			<div class="mb-6">
				<label for="password" class="block text-neutral-400 text-sm mb-1">password</label>
				<input
					id="password"
					name="password"
					type="password"
					autocomplete="current-password"
					class="w-full bg-neutral-900 border border-neutral-700 text-neutral-100 px-3 py-2 rounded text-sm focus:outline-none focus:border-green-600"
				/>
			</div>

			<button
				type="submit"
				class="w-full bg-green-900 hover:bg-green-800 text-green-100 py-2 rounded text-sm transition-colors cursor-pointer"
			>
				log in
			</button>
		</form>
	</div>
</div>
