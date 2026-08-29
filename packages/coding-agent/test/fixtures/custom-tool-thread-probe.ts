import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import { loadCustomTools } from "../../src/extensibility/custom-tools/loader";

async function countBunPoolThreads(): Promise<number> {
	const taskDir = `/proc/${process.pid}/task`;
	const entries = await fs.readdir(taskDir);
	const names = await Promise.all(entries.map(entry => Bun.file(path.join(taskDir, entry, "comm")).text()));
	return names.filter(name => name.startsWith("Bun Pool ")).length;
}

const root = await fs.mkdtemp(path.join(os.tmpdir(), "omp-custom-tool-thread-probe-"));
try {
	const toolPath = path.join(root, "typed-tool.ts");
	await Bun.write(
		toolPath,
		[
			'type Reply = { content: Array<{ type: "text"; text: string }> };',
			"export default api => ({",
			'  name: "typed_pool_probe",',
			'  description: "Exercises TypeScript custom tool loading",',
			"  parameters: api.arktype({}),",
			"  async execute(): Promise<Reply> {",
			'    return { content: [{ type: "text", text: "ok" }] };',
			"  },",
			"});",
		].join("\n"),
	);
	const before = await countBunPoolThreads();
	const result = await loadCustomTools([{ path: toolPath }], root, []);
	await Bun.sleep(100);
	const after = await countBunPoolThreads();
	console.log(
		JSON.stringify({
			poolDelta: after - before,
			errors: result.errors,
			toolNames: result.tools.map(tool => tool.tool.name),
		}),
	);
} finally {
	await fs.rm(root, { recursive: true, force: true });
}
