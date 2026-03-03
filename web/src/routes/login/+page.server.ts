import { fail, redirect } from '@sveltejs/kit';
import { base } from '$app/paths';
import { sessions, AUTH_CONFIG } from '$lib/server/auth';
import type { Actions } from './$types';

export const actions: Actions = {
	login: async ({ request, cookies }) => {
		const data = await request.formData();
		const username = data.get('username')?.toString() ?? '';
		const password = data.get('password')?.toString() ?? '';

		if (username !== AUTH_CONFIG.USERNAME || password !== AUTH_CONFIG.PASSWORD) {
			return fail(401, { username, error: 'Invalid username or password' });
		}

		const sessionId = crypto.randomUUID();
		sessions.add(sessionId);

		cookies.set('sctlin-session', sessionId, {
			path: base || '/',
			httpOnly: true,
			secure: false,
			maxAge: 60 * 60 * 24 * 7,
			sameSite: 'lax'
		});

		redirect(303, base || '/');
	},

	logout: async ({ cookies }) => {
		const sessionId = cookies.get('sctlin-session');
		if (sessionId) {
			sessions.delete(sessionId);
			cookies.delete('sctlin-session', { path: base || '/' });
		}
		redirect(303, `${base}/login`);
	}
};
