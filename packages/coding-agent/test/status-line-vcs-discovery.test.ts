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

/** Records which repo instance each ref-sensitive read ran on. */
interface ReadProbe {
	kindIds: number[];
	statusIds: number[];
}

/** A Git-backed repo handle tagged so tests can tell reused vs freshly opened. */
function fakeRepo(id: number, probe: ReadProbe): VcsRepo {
	const git = {
		headSync: () => ({ kind: "ref", branch: "main" }),
		head: async () => ({ kind: "ref", branch: "main" }),
		linkedWorktree: () => null,
		defaultBranch: async () => "main",
	};
	const repo = {
		kind: () => {
			probe.kindIds.push(id);
			return "git";
		},
		asJj: () => null,
		asGit: () => git as unknown as VcsGitRepo,
		label: async () => "main",
		statusSummary: async () => {
			probe.statusIds.push(id);
			return { staged: 0, unstaged: 0, untracked: 0 };
		},
	};
	return repo as unknown as VcsRepo;
}

function fakeGitInfo(): VcsGitRepoInfo {
	const info = { isReftable: false, headPath: "/repo/.git/HEAD" };
	return info as unknown as VcsGitRepoInfo;
}

interface Harness {
	component: StatusLineComponent;
	probe: ReadProbe;
	repoSpy: Mock<typeof vcs.repo>;
	dispose: () => void;
}

function mountStatusLine(repoFactory: (id: number, probe: ReadProbe) => VcsRepo | null = fakeRepo): Harness {
	const probe: ReadProbe = { kindIds: [], statusIds: [] };
	let nextId = 0;
	const repoSpy: Mock<typeof vcs.repo> = spyOn(vcs, "repo").mockImplementation(() => repoFactory(nextId++, probe));
	const gitSpy = spyOn(vcs, "git").mockImplementation(() => fakeRepo(-1, probe).asGit());
	const infoSpy = spyOn(vcs, "gitInfo").mockImplementation(() => fakeGitInfo());

	const component = new StatusLineComponent(fakeSession());
	component.updateSettings({
		preset: "custom",
		leftSegments: ["path", "git"],
		rightSegments: ["pr"],
		separator: "powerline-thin",
		sessionAccent: false,
	});
	return {
		component,
		probe,
		repoSpy,
		dispose: () => {
			component.dispose();
			repoSpy.mockRestore();
			gitSpy.mockRestore();
			infoSpy.mockRestore();
		},
	};
}

describe("status line VCS discovery", () => {
	beforeAll(async () => {
		await Settings.init({ inMemory: true });
		await initTheme();
	});

	it("discovers the repository once, not on every rendered frame", () => {
		const { component, repoSpy, dispose } = mountStatusLine();
		try {
			// Warm the projectDir / active-repo caches, then simulate the
			// working-spinner repaint loop: the status line is rebuilt on every
			// painted frame while nothing about the repository changed.
			component.getTopBorder(120);
			const perFrame = () => {
				repoSpy.mockClear();
				component.getTopBorder(120);
				return repoSpy.mock.calls.length;
			};
			// Repository discovery is a native filesystem walk (costly on WSL). Once
			// the handle is memoized it must not re-walk on steady-state frames, and
			// the count must never scale with the frame rate.
			expect(perFrame()).toBe(0);
			expect(perFrame()).toBe(0);
		} finally {
			dispose();
		}
	});

	it("bounds negative discovery and retries after the fallback polling interval", () => {
		let now = 1_000_000;
		const nowSpy = spyOn(Date, "now").mockImplementation(() => now);
		const { component, repoSpy, dispose } = mountStatusLine(() => null);
		try {
			// Warm the unrelated active-repository resolution, then isolate the
			// status line's repo memo.
			component.getTopBorder(120);
			component.invalidateGitCaches();
			repoSpy.mockClear();

			component.getTopBorder(120);
			expect(repoSpy).toHaveBeenCalledTimes(1);
			component.getTopBorder(120);
			now += 4_999;
			component.getTopBorder(120);
			expect(repoSpy).toHaveBeenCalledTimes(1);

			// A bounded miss must expire so `git init` becomes visible without a
			// cwd or HEAD watcher event.
			now += 1;
			component.getTopBorder(120);
			expect(repoSpy).toHaveBeenCalledTimes(2);
		} finally {
			nowSpy.mockRestore();
			dispose();
		}
	});

	it("reads status through a freshly opened handle, not the memoized one", () => {
		const { component, probe, dispose } = mountStatusLine();
		try {
			// One render on a cold status cache: the cheap `kind()` gate runs on the
			// memoized discovery handle, while `statusSummary` (staged = index vs HEAD
			// tree) must reopen a fresh handle so a same-branch commit — which never
			// touches `.git/HEAD`, so the HEAD watcher stays quiet — cannot leave the
			// status segment comparing against a stale, ref-snapshotted HEAD.
			component.getTopBorder(120);
			expect(probe.statusIds.length).toBeGreaterThan(0);
			expect(probe.kindIds.length).toBeGreaterThan(0);
			const memoizedGateHandle = probe.kindIds[0];
			expect(probe.kindIds.every(id => id === memoizedGateHandle)).toBe(true);
			expect(probe.statusIds.every(id => id !== memoizedGateHandle)).toBe(true);
		} finally {
			dispose();
		}
	});
});
