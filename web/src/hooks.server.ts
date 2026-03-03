import { dev } from '$app/environment';
import { base } from '$app/paths';
import { redirect, type Handle } from '@sveltejs/kit';
import { sessions } from '$lib/server/auth';

export const handle: Handle = async ({ event, resolve }) => {
	if (dev) return resolve(event);

	const { pathname } = event.url;

	if (
		pathname.startsWith(`${base}/_app/`) ||
		pathname === `${base}/login` ||
		pathname === '/favicon.png'
	) {
		return resolve(event);
	}

	const sessionId = event.cookies.get('sctlin-session');
	if (!sessionId || !sessions.has(sessionId)) {
		redirect(303, `${base}/login`);
	}

	return resolve(event);
};
