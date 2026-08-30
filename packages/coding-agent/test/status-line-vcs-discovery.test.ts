import { beforeAll, describe, expect, it, type Mock, spyOn } from "bun:test";
import type { VcsGitRepo, VcsGitRepoInfo, VcsRepo } from "@oh-my-pi/pi-natives";
import * as vcs from "@oh-my-pi/pi-natives/vcs";
import { Settings } from "../src/config/settings";
import { StatusLineComponent } from "../src/modes/components/status-line";
import { initTheme } from "../src/modes/theme/theme";
import type { AgentSession } from "../src/session/agent-session";

function fakeSession(): AgentSession {
	const model = { id: "test-model", contextWindow: 200_000 };
	const messages = [{ role: "user", content: "hi" }];
	const session = {
		messages,
		systemPrompt: [],
		agent: { state: { tools: [] } },
		skills: [],
		model,
		modelRegistry: { isUsingOAuth: () => false },
		state: { messages, model },
		settings: undefined,
		sessionManager: {
			getUsageStatistics: () => ({
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 0,
				orchestrationInput: 0,
				orchestrationOutput: 0,
				orchestrationCacheRead: 0,
				premiumRequests: 0,
				cost: 0,
			}),
			getSessionName: () => "repro",
		},
		getAsyncJobSnapshot: () => ({ running: [] }),
		isFastModeActive: () => false,
		getContextUsage: () => ({ tokens: 6000, contextWindow: 200_000, percent: 3 }),
		contextUsageRevision: 0,
	};
	return session as unknown as AgentSession;
}

/** Git-specific handle covering the synchronous render-path reads. */
function fakeGitRepo(): VcsGitRepo {
	const git = {
		headSync: () => ({ kind: "ref", branch: "main" }),
		head: async () => ({ kind: "ref", branch: "main" }),
		linkedWorktree: () => null,
		defaultBranch: async () => "main",
	};
	return git as unknown as VcsGitRepo;
}

/** Backend-agnostic repo handle the status line rediscovers per segment. */
function fakeRepo(): VcsRepo {
	const repo = {
		kind: () => "git",
		asJj: () => null,
		asGit: () => fakeGitRepo(),
		label: async () => "main",
		statusSummary: async () => ({ staged: 0, unstaged: 0, untracked: 0 }),
	};
	return repo as unknown as VcsRepo;
}

function fakeGitInfo(): VcsGitRepoInfo {
	const info = { isReftable: false, headPath: "/repo/.git/HEAD" };
	return info as unknown as VcsGitRepoInfo;
}

describe("status line VCS discovery", () => {
	beforeAll(async () => {
		await Settings.init({ inMemory: true });
		await initTheme();
	});

	it("discovers the repository once, not on every rendered frame", () => {
		const repoSpy: Mock<typeof vcs.repo> = spyOn(vcs, "repo").mockImplementation(() => fakeRepo());
		const gitSpy: Mock<typeof vcs.git> = spyOn(vcs, "git").mockImplementation(() => fakeGitRepo());
		const infoSpy: Mock<typeof vcs.gitInfo> = spyOn(vcs, "gitInfo").mockImplementation(() => fakeGitInfo());

		const component = new StatusLineComponent(fakeSession());
		component.updateSettings({
			preset: "custom",
			leftSegments: ["path", "git"],
			rightSegments: ["pr"],
			separator: "powerline-thin",
			sessionAccent: false,
		});
		try {
			// Warm the projectDir/active-repo cache, then simulate the
			// working-spinner repaint loop: the status line is rebuilt on every
			// painted frame while nothing about the repository changed.
			component.getTopBorder(120);
			const perFrame = () => {
				repoSpy.mockClear();
				component.getTopBorder(120);
				return repoSpy.mock.calls.length;
			};
			// Repository discovery is a native filesystem walk (costly on WSL). Once
			// the handle is cached it must not re-walk on steady-state frames, and
			// the count must never scale with the frame rate.
			expect(perFrame()).toBe(0);
			expect(perFrame()).toBe(0);
		} finally {
			component.dispose();
			repoSpy.mockRestore();
			gitSpy.mockRestore();
			infoSpy.mockRestore();
		}
	});
});
