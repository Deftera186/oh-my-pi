/**
 * Repro for #10653 — OMP sends no `x-opencode-session` / `x-opencode-client`
 * header on inference requests to `opencode-go` / `opencode-zen`. OpenCode's
 * gateway (`opencode.ai/zen`) now requires a stable per-conversation
 * `x-opencode-session` for routing and errors header-less requests (operator
 * notice: enforced starting 2026-09). Because the header-less caller is OMP's
 * own outbound request layer, users cannot add it on their side.
 *
 * Root cause: the OpenAI-family request setup
 * (`resolveOpenAIRequestSetup`), Anthropic client builder
 * (`buildAnthropicClientOptions`), and Google transport (`streamGoogle`) did
 * not attach OpenCode routing headers. The fix sends `x-opencode-session:
 * <sessionId>` and `x-opencode-client: omp` as defaults on every transport,
 * with explicit user/config `headers` winning.
 */
import { describe, expect, it } from "bun:test";
import { streamAnthropic } from "@oh-my-pi/pi-ai/providers/anthropic";
import { streamGoogle } from "@oh-my-pi/pi-ai/providers/google";
import { streamOpenAICompletions } from "@oh-my-pi/pi-ai/providers/openai-completions";
import { streamOpenAIResponses } from "@oh-my-pi/pi-ai/providers/openai-responses";
import type { Context, Model, ModelSpec } from "@oh-my-pi/pi-ai/types";
import { buildModel } from "@oh-my-pi/pi-catalog/build";

const GO_OPENAI_MODEL: Model<"openai-completions"> = buildModel({
	id: "kimi-k2.7-code",
	name: "Kimi K2.7 Code",
	api: "openai-completions",
	provider: "opencode-go",
	baseUrl: "https://opencode.ai/zen/go/v1",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 256_000,
	maxTokens: 16_384,
} as ModelSpec<"openai-completions">);

const ZEN_ANTHROPIC_MODEL: Model<"anthropic-messages"> = buildModel({
	id: "claude-haiku-4-5",
	name: "Claude Haiku 4.5",
	api: "anthropic-messages",
	provider: "opencode-zen",
	baseUrl: "https://opencode.ai/zen/v1",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 200_000,
	maxTokens: 8_192,
} as ModelSpec<"anthropic-messages">);

const ZEN_GOOGLE_MODEL: Model<"google-generative-ai"> = buildModel({
	id: "gemini-3-pro-preview",
	name: "Gemini 3 Pro Preview",
	api: "google-generative-ai",
	provider: "opencode-zen",
	baseUrl: "https://opencode.ai/zen/v1",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 1_000_000,
	maxTokens: 65_536,
} as ModelSpec<"google-generative-ai">);

const GO_RESPONSES_MODEL: Model<"openai-responses"> = buildModel({
	id: "deepseek-v4-flash",
	name: "DeepSeek V4 Flash",
	api: "openai-responses",
	provider: "opencode-go",
	baseUrl: "https://opencode.ai/zen/go/v1",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 128_000,
	maxTokens: 8_192,
} as ModelSpec<"openai-responses">);

const CONTEXT: Context = { systemPrompt: [], messages: [{ role: "user", content: "hi", timestamp: 0 }] };

function stubJson(status: number): Response {
	return new Response(JSON.stringify({ error: { type: "captured", message: "captured" } }), {
		status,
		headers: { "Content-Type": "application/json" },
	});
}

describe("issue #10653 — OpenCode routing headers", () => {
	it("attaches x-opencode-session + x-opencode-client on opencode-go OpenAI requests", async () => {
		let captured: Headers | undefined;
		const fetchMock = (async (_input: string | URL | Request, init?: RequestInit) => {
			captured = new Headers(init?.headers);
			return stubJson(400);
		}) as typeof fetch;

		await streamOpenAICompletions(GO_OPENAI_MODEL, CONTEXT, {
			apiKey: "sk-go-test",
			sessionId: "conv-abc-123",
			fetch: fetchMock,
		}).result();

		expect(captured?.get("x-opencode-session")).toBe("conv-abc-123");
		expect(captured?.get("x-opencode-client")).toBe("omp");
	});

	it("attaches x-opencode-session + x-opencode-client on opencode-zen Anthropic requests", async () => {
		let captured: Headers | undefined;
		const fetchMock = (async (_input: string | URL | Request, init?: RequestInit) => {
			captured = new Headers(init?.headers);
			return stubJson(400);
		}) as typeof fetch;

		await streamAnthropic(ZEN_ANTHROPIC_MODEL, CONTEXT, {
			apiKey: "sk-zen-test",
			sessionId: "conv-xyz-789",
			fetch: fetchMock,
		}).result();

		expect(captured?.get("x-opencode-session")).toBe("conv-xyz-789");
		expect(captured?.get("x-opencode-client")).toBe("omp");
	});

	it("attaches x-opencode-session + x-opencode-client on opencode-go Responses requests", async () => {
		let captured: Headers | undefined;
		const fetchMock = (async (_input: string | URL | Request, init?: RequestInit) => {
			captured = new Headers(init?.headers);
			return stubJson(400);
		}) as typeof fetch;

		await streamOpenAIResponses(GO_RESPONSES_MODEL, CONTEXT, {
			apiKey: "sk-go-test",
			sessionId: "conv-resp-456",
			fetch: fetchMock,
		}).result();

		expect(captured?.get("x-opencode-session")).toBe("conv-resp-456");
		expect(captured?.get("x-opencode-client")).toBe("omp");
	});

	it("attaches x-opencode-session + x-opencode-client on opencode-zen Google requests", async () => {
		let captured: Headers | undefined;
		const fetchMock = (async (_input: string | URL | Request, init?: RequestInit) => {
			captured = new Headers(init?.headers);
			return stubJson(400);
		}) as typeof fetch;

		await streamGoogle(ZEN_GOOGLE_MODEL, CONTEXT, {
			apiKey: "sk-zen-test",
			sessionId: "conv-google-789",
			fetch: fetchMock,
		}).result();

		expect(captured?.get("x-opencode-session")).toBe("conv-google-789");
		expect(captured?.get("x-opencode-client")).toBe("omp");
	});

	it("lets explicit Google headers override OpenCode routing defaults", async () => {
		let captured: Headers | undefined;
		const fetchMock = (async (_input: string | URL | Request, init?: RequestInit) => {
			captured = new Headers(init?.headers);
			return stubJson(400);
		}) as typeof fetch;

		await streamGoogle(ZEN_GOOGLE_MODEL, CONTEXT, {
			apiKey: "sk-zen-test",
			sessionId: "conv-google-789",
			headers: { "x-opencode-client": "pi", "x-opencode-session": "user-google" },
			fetch: fetchMock,
		}).result();

		expect(captured?.get("x-opencode-client")).toBe("pi");
		expect(captured?.get("x-opencode-session")).toBe("user-google");
	});

	it("lets an explicit user header override the default x-opencode-client", async () => {
		let captured: Headers | undefined;
		const fetchMock = (async (_input: string | URL | Request, init?: RequestInit) => {
			captured = new Headers(init?.headers);
			return stubJson(400);
		}) as typeof fetch;

		await streamOpenAICompletions(GO_OPENAI_MODEL, CONTEXT, {
			apiKey: "sk-go-test",
			sessionId: "conv-abc-123",
			headers: { "x-opencode-client": "pi", "x-opencode-session": "user-pinned" },
			fetch: fetchMock,
		}).result();

		expect(captured?.get("x-opencode-client")).toBe("pi");
		expect(captured?.get("x-opencode-session")).toBe("user-pinned");
	});

	it("still sends a non-empty x-opencode-session when the caller omits sessionId", async () => {
		const capture = async () => {
			let captured: Headers | undefined;
			const fetchMock = (async (_input: string | URL | Request, init?: RequestInit) => {
				captured = new Headers(init?.headers);
				return stubJson(400);
			}) as typeof fetch;
			await streamOpenAICompletions(GO_OPENAI_MODEL, CONTEXT, { apiKey: "sk-go-test", fetch: fetchMock }).result();
			return captured;
		};

		const first = await capture();
		const second = await capture();
		const fallback = first?.get("x-opencode-session");
		expect(fallback).toBeTruthy();
		expect(first?.get("x-opencode-client")).toBe("omp");
		// The fallback identity is process-stable so a session-less caller's
		// turns route as one conversation.
		expect(second?.get("x-opencode-session")).toBe(fallback);
	});
});
