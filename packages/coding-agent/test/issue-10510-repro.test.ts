import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import * as path from "node:path";
import { type } from "@oh-my-pi/omptype";
import { Agent, type AgentMessage, type AgentTool, type AgentTurnEndContext } from "@oh-my-pi/pi-agent-core";
import { type Api, Effort, type Model } from "@oh-my-pi/pi-ai";
import { createMockModel, type MockResponse } from "@oh-my-pi/pi-ai/providers/mock";
import { getBundledModel } from "@oh-my-pi/pi-catalog/models";
import { ModelRegistry } from "@oh-my-pi/pi-coding-agent/config/model-registry";
import { Settings } from "@oh-my-pi/pi-coding-agent/config/settings";
import { AgentSession } from "@oh-my-pi/pi-coding-agent/session/agent-session";
import type { AuthStorage } from "@oh-my-pi/pi-coding-agent/session/auth-storage";
import { convertToLlm, PREWALK_PLAN_MESSAGE_TYPE } from "@oh-my-pi/pi-coding-agent/session/messages";
import { PrewalkCoordinator, type PrewalkCoordinatorHost } from "@oh-my-pi/pi-coding-agent/session/prewalk";
import { SessionManager } from "@oh-my-pi/pi-coding-agent/session/session-manager";
import { TempDir } from "@oh-my-pi/pi-utils";
import { createAssistantMessage, createInMemoryAuthStorage } from "./helpers/agent-session-setup";

/**
 * Issue #10510: with prewalk armed and `todo.eager=always`, the session injected
 * two contradictory hidden system messages — the forced eager-todo prelude
 * ("call todo first this turn") and the prewalk plan nudge ("write a complete
 * plan first, then todo"). Prewalk's plan flow already owns todo creation, so
 * the eager prelude must yield to it while a prewalk is armed.
 */
function modelOrThrow(id: string): Model<Api> {
	const model = getBundledModel("anthropic", id);
	if (!model) throw new Error(`Expected bundled model ${id}`);
	return model;
}

describe("issue #10510: prewalk + eager-todo conflict", () => {
	let tempDir: TempDir;
	let authStorage: AuthStorage;
	let modelRegistry: ModelRegistry;

	beforeAll(() => {
		tempDir = TempDir.createSync("@pi-issue-10510-");
		authStorage = createInMemoryAuthStorage();
		authStorage.setRuntimeApiKey("anthropic", "test-key");
		modelRegistry = new ModelRegistry(authStorage, path.join(tempDir.path(), "models.yml"));
	});

	afterAll(() => {
		authStorage.close();
		tempDir.removeSync();
	});

	function messageText(message: AgentMessage): string {
		if (!("content" in message)) return "";
		const content = message.content;
		if (typeof content === "string") return content;
		if (!Array.isArray(content)) return "";
		const parts: string[] = [];
		for (const block of content) {
			if (block && typeof block === "object" && "type" in block && block.type === "text" && "text" in block) {
				if (typeof block.text === "string") parts.push(block.text);
			}
		}
		return parts.join("\n");
	}

	const mkTool = (name: string, result: string): AgentTool => ({
		name,
		label: name,
		description: name,
		parameters: type({}),
		async execute() {
			return { content: [{ type: "text", text: result }], details: undefined };
		},
	});

	function toolCall(id: string, name: string): MockResponse {
		return { content: [{ type: "toolCall", id, name, arguments: {} }], stopReason: "toolUse" };
	}

	/** Runs a first-turn prompt and returns every hidden/visible text sent to the model. */
	async function collectInjectedText(options: { prewalk: "handoff" | "noop" | "off" }): Promise<string> {
		const primary = modelOrThrow("claude-sonnet-4-5");
		const handoffTarget = modelOrThrow("claude-sonnet-4-6");
		const prewalkTarget =
			options.prewalk === "handoff" ? handoffTarget : options.prewalk === "noop" ? primary : undefined;
		const recordTool = mkTool("record", "ok");
		const writeTool = mkTool("write", "wrote");
		const todoTool = mkTool("todo", "listed");
		const toolRegistry = new Map<string, AgentTool>([
			[recordTool.name, recordTool],
			[writeTool.name, writeTool],
			[todoTool.name, todoTool],
		]);
		const mock = createMockModel({
			responses: [toolCall("t1", "todo"), toolCall("t2", "record"), toolCall("t3", "write"), { content: ["done"] }],
		});

		const injected: string[] = [];
		const agent = new Agent({
			getApiKey: () => "test-key",
			initialState: {
				model: primary,
				systemPrompt: ["Test"],
				tools: [recordTool, writeTool, todoTool],
				messages: [],
				thinkingLevel: Effort.Medium,
			},
			convertToLlm,
			getToolChoice: () => session.nextToolChoiceDirective(),
			streamFn: (model, context, streamOptions) => {
				for (const message of context.messages) injected.push(messageText(message));
				return mock.stream(model, context, streamOptions);
			},
		});
		const session = new AgentSession({
			agent,
			sessionManager: SessionManager.inMemory(tempDir.path()),
			settings: Settings.isolated({
				"compaction.enabled": false,
				"todo.enabled": true,
				"todo.eager": "always",
				"todo.reminders": false,
			}),
			modelRegistry,
			toolRegistry,
			...(prewalkTarget ? { prewalk: { target: prewalkTarget } } : {}),
		});

		await session.prompt("do the task");
		await session.dispose();
		return injected.join("\n---\n");
	}

	it("suppresses the forced eager-todo prelude while prewalk is armed", async () => {
		const text = await collectInjectedText({ prewalk: "handoff" });
		expect(text.includes("write complete plan")).toBe(true);
		expect(text.includes("You MUST call") && text.includes("first in this turn")).toBe(false);
	});

	it("still injects the forced eager-todo prelude when prewalk is not armed", async () => {
		const text = await collectInjectedText({ prewalk: "off" });
		expect(text.includes("write complete plan")).toBe(false);
		expect(text.includes("You MUST call") && text.includes("first in this turn")).toBe(true);
	});

	it("keeps the forced eager-todo prelude when the armed prewalk is a no-op", async () => {
		const text = await collectInjectedText({ prewalk: "noop" });
		expect(text.includes("write complete plan")).toBe(false);
		expect(text.includes("You MUST call") && text.includes("first in this turn")).toBe(true);
	});
});

/**
 * Issue #10511: prewalk's plan nudge is steered live and never persisted, so a
 * mid-run compaction rebuild (which calls `agent.replaceMessages` with the
 * rebuilt-from-session context) drops it. The coordinator latched `#planInjected`
 * and never re-showed it, so the todo gate never opened and prewalk stayed on the
 * expensive model. It must re-inject while the gate is still closed.
 */
describe("PrewalkCoordinator plan-nudge robustness", () => {
	function makeAgent(model: Model<Api>): Agent {
		return new Agent({
			getApiKey: () => "test-key",
			initialState: {
				model,
				systemPrompt: ["Test"],
				tools: [],
				messages: [],
				thinkingLevel: Effort.Medium,
			},
			convertToLlm,
		});
	}

	function makeHost(agent: Agent, model: Model<Api>): PrewalkCoordinatorHost {
		const unsupported = (name: string): (() => never) => {
			return () => {
				throw new Error(`unexpected PrewalkCoordinatorHost call: ${name}`);
			};
		};
		return {
			agent,
			sessionManager: SessionManager.inMemory(),
			model: () => model,
			configuredThinkingLevel: () => Effort.Medium,
			emitNotice: () => {},
			getActiveToolNames: () => ["todo"],
			setModelTemporary: async () => {},
			setActiveToolsByName: async () => {},
			setActiveToolPresentation: async () => {},
			runToolRegistryMutation: async mutation => mutation(),
			getEnabledToolNames: () => ["todo"],
			getSelectedMCPToolNames: () => [],
			getMountedXdevToolNames: () => [],
			hasBuiltInTool: () => false,
			getPlanModeState: () => undefined,
			setPlanModeState: () => {},
			getPlanReferencePath: () => "",
			setPlanProposalHandler: () => {},
			waitForSessionMessagePersistence: async () => {},
			localProtocolOptions: unsupported("localProtocolOptions"),
		};
	}

	const turnEnd = (): AgentTurnEndContext => ({
		message: createAssistantMessage(""),
		toolResults: [],
		willContinue: false,
	});

	const planNudgeQueued = (agent: Agent): boolean =>
		agent
			.peekSteeringQueue()
			.some(
				(message: AgentMessage) => message.role === "custom" && message.customType === PREWALK_PLAN_MESSAGE_TYPE,
			);

	// Moves the steered nudge into live state exactly as the agent loop does when
	// it consumes a one-at-a-time steering message on the next turn.
	function consumeSteeredNudge(agent: Agent): void {
		for (const message of agent.peekSteeringQueue()) agent.appendMessage(message);
		agent.clearSteeringQueue();
	}

	it("re-injects the plan nudge after a rebuild drops it before the todo gate opens", async () => {
		const primary = modelOrThrow("claude-sonnet-4-5");
		const target = modelOrThrow("claude-sonnet-4-6");
		const agent = makeAgent(primary);
		const coordinator = new PrewalkCoordinator(makeHost(agent, primary), { prewalk: { target } });

		// Turn 1: no todo -> the plan nudge is steered.
		await coordinator.advanceAtTurnEnd(agent.state.messages, turnEnd());
		expect(planNudgeQueued(agent)).toBe(true);
		consumeSteeredNudge(agent);

		// Turn 2: nudge is now in live state and the gate is still closed -> no re-inject.
		await coordinator.advanceAtTurnEnd(agent.state.messages, turnEnd());
		expect(planNudgeQueued(agent)).toBe(false);

		// Compaction rebuild drops the non-persisted nudge from live state.
		agent.replaceMessages(
			agent.state.messages.filter(
				message => !(message.role === "custom" && message.customType === PREWALK_PLAN_MESSAGE_TYPE),
			),
		);

		// Turn 3: nudge gone, gate still closed -> it must be re-injected.
		await coordinator.advanceAtTurnEnd(agent.state.messages, turnEnd());
		expect(planNudgeQueued(agent)).toBe(true);
	});

	it("does not re-inject once the todo gate has opened", async () => {
		const primary = modelOrThrow("claude-sonnet-4-5");
		const target = modelOrThrow("claude-sonnet-4-6");
		const agent = makeAgent(primary);
		const coordinator = new PrewalkCoordinator(makeHost(agent, primary), { prewalk: { target } });

		await coordinator.advanceAtTurnEnd(agent.state.messages, turnEnd());
		consumeSteeredNudge(agent);

		// A successful todo result opens the gate.
		const todoResult: AgentTurnEndContext = {
			message: createAssistantMessage(""),
			toolResults: [
				{
					role: "toolResult",
					toolCallId: "c1",
					toolName: "todo",
					content: [{ type: "text", text: "listed" }],
					isError: false,
					timestamp: Date.now(),
				},
			],
			willContinue: false,
		};
		await coordinator.advanceAtTurnEnd(agent.state.messages, todoResult);

		// Even if a later rebuild drops the nudge, the consumed requirement must not return.
		agent.replaceMessages(
			agent.state.messages.filter(
				message => !(message.role === "custom" && message.customType === PREWALK_PLAN_MESSAGE_TYPE),
			),
		);
		await coordinator.advanceAtTurnEnd(agent.state.messages, turnEnd());
		expect(planNudgeQueued(agent)).toBe(false);
	});
});
