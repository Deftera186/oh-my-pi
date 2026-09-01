import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { IMPORT_CONDITIONS, REQUIRE_CONDITIONS, resolveBareSpecifier } from "./node-modules-resolver";

/**
 * Fixture layout (a project `node_modules` tree, one nested scope) exercising the
 * resolver branches that the compiled-binary fallback depends on. See issue #10496.
 */
let root: string;
let projectDir: string;
let nestedDir: string;

function writePackage(nmDir: string, name: string, pkg: Record<string, unknown>, files: Record<string, string>): void {
	const pkgDir = path.join(nmDir, ...name.split("/"));
	fs.mkdirSync(pkgDir, { recursive: true });
	fs.writeFileSync(path.join(pkgDir, "package.json"), JSON.stringify({ name, version: "1.0.0", ...pkg }));
	for (const rel in files) {
		const filePath = path.join(pkgDir, rel);
		fs.mkdirSync(path.dirname(filePath), { recursive: true });
		fs.writeFileSync(filePath, files[rel]);
	}
}

beforeAll(() => {
	root = fs.mkdtempSync(path.join(os.tmpdir(), "omp-resolver-"));
	projectDir = path.join(root, "project");
	const projNm = path.join(projectDir, "node_modules");
	nestedDir = path.join(projNm, "outer");
	fs.mkdirSync(projNm, { recursive: true });

	// Dual condition + legacy fields: import/require must select different files.
	writePackage(
		projNm,
		"dual",
		{ main: "cjs.js", module: "esm.mjs", exports: { ".": { import: "./esm.mjs", require: "./cjs.js" } } },
		{
			"esm.mjs": "export default 'esm';",
			"cjs.js": "module.exports = 'cjs';",
		},
	);
	// Legacy fields only, no exports.
	writePackage(projNm, "legacy", { main: "lib/entry.js" }, { "lib/entry.js": "module.exports = 'legacy';" });
	// No entry fields at all: index.js fallback.
	writePackage(projNm, "indexonly", {}, { "index.js": "module.exports = 'index';" });
	// Scoped package with subpath pattern exports.
	writePackage(
		projNm,
		"@scope/pkg",
		{ exports: { ".": "./main.js", "./feature/*": "./src/*.js" } },
		{
			"main.js": "module.exports = 'scoped-main';",
			"src/thing.js": "module.exports = 'scoped-feature';",
		},
	);
	// Package resolvable only by walking up: installed in a nested dependency's node_modules.
	writePackage(
		path.join(nestedDir, "node_modules"),
		"shared",
		{ main: "index.js" },
		{ "index.js": "module.exports = 'shared';" },
	);
});

afterAll(() => {
	fs.rmSync(root, { recursive: true, force: true });
});

describe("resolveBareSpecifier", () => {
	test("selects the export target matching the active condition", () => {
		expect(resolveBareSpecifier("dual", projectDir, IMPORT_CONDITIONS)).toBe(
			path.join(projectDir, "node_modules", "dual", "esm.mjs"),
		);
		expect(resolveBareSpecifier("dual", projectDir, REQUIRE_CONDITIONS)).toBe(
			path.join(projectDir, "node_modules", "dual", "cjs.js"),
		);
	});

	test("falls back to legacy main and index when exports is absent", () => {
		expect(resolveBareSpecifier("legacy", projectDir, IMPORT_CONDITIONS)).toBe(
			path.join(projectDir, "node_modules", "legacy", "lib", "entry.js"),
		);
		expect(resolveBareSpecifier("indexonly", projectDir, IMPORT_CONDITIONS)).toBe(
			path.join(projectDir, "node_modules", "indexonly", "index.js"),
		);
	});

	test("resolves scoped root and subpath-pattern exports", () => {
		expect(resolveBareSpecifier("@scope/pkg", projectDir, IMPORT_CONDITIONS)).toBe(
			path.join(projectDir, "node_modules", "@scope", "pkg", "main.js"),
		);
		expect(resolveBareSpecifier("@scope/pkg/feature/thing", projectDir, IMPORT_CONDITIONS)).toBe(
			path.join(projectDir, "node_modules", "@scope", "pkg", "src", "thing.js"),
		);
	});

	test("walks the node_modules chain upward from the base directory", () => {
		// `shared` lives only in outer/node_modules; resolving from outer's own dir finds it.
		expect(resolveBareSpecifier("shared", nestedDir, IMPORT_CONDITIONS)).toBe(
			path.join(nestedDir, "node_modules", "shared", "index.js"),
		);
	});

	test("returns null for unresolvable specifiers", () => {
		expect(resolveBareSpecifier("does-not-exist", projectDir, IMPORT_CONDITIONS)).toBeNull();
		// exports omits this subpath -> no resolution, no legacy leak for non-root subpaths.
		expect(resolveBareSpecifier("@scope/pkg/nope", projectDir, IMPORT_CONDITIONS)).toBeNull();
	});
});
