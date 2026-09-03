/**
 * OpenCode Go/Zen (`opencode.ai/zen`) routing headers.
 *
 * The gateway requires a stable per-conversation `x-opencode-session` on
 * inference requests for routing/optimization and errors header-less callers
 * (operator notice: enforced starting 2026-09). `x-opencode-client` attributes
 * the calling client for upstream usage stats. Both are sent as defaults —
 * explicit user/config headers of the same name win at the call site.
 */

/** Client identifier reported to OpenCode's routing gateway. */
export const OPENCODE_CLIENT_ID = "omp";

/** Whether a provider routes through the OpenCode Go/Zen gateway. */
export function isOpenCodeProvider(provider: string): boolean {
	return provider === "opencode-go" || provider === "opencode-zen";
}

/**
 * Routing headers for an OpenCode inference request. `x-opencode-session` is
 * emitted only when a stable session id is available; `x-opencode-client` is
 * always present.
 */
export function opencodeRoutingHeaders(sessionId: string | undefined): Record<string, string> {
	const headers: Record<string, string> = { "x-opencode-client": OPENCODE_CLIENT_ID };
	if (sessionId) headers["x-opencode-session"] = sessionId;
	return headers;
}
