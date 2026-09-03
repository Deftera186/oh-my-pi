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

/**
 * Process-stable fallback session id for direct `pi-ai` callers that omit
 * `sessionId`. The gateway rejects inference requests without
 * `x-opencode-session`, so the header must always be present; a single id per
 * process keeps such calls grouped as one conversation and distinct across
 * processes. Lazily minted so importing this module allocates nothing.
 */
let fallbackSessionId: string | undefined;

/**
 * Routing headers for an OpenCode inference request. `x-opencode-session`
 * carries the caller's stable per-conversation id, falling back to a
 * process-stable id when none is supplied so the required header is never
 * omitted; `x-opencode-client` attributes the caller.
 */
export function opencodeRoutingHeaders(sessionId: string | undefined): Record<string, string> {
	fallbackSessionId ??= crypto.randomUUID();
	return {
		"x-opencode-client": OPENCODE_CLIENT_ID,
		"x-opencode-session": sessionId || fallbackSessionId,
	};
}
