import * as fs from "node:fs";
import * as path from "node:path";

/**
 * On-disk `node_modules` resolver for eval JS bare specifiers.
 *
 * `bun build --compile` roots module resolution at the embedded `$bunfs`, so
 * `Bun.resolveSync`, `createRequire`, and bare `import()` never consult the real
 * project `node_modules` even when the kernel cwd is correct (issue #10496). This
 * walks the on-disk `node_modules` chain from a base directory and resolves a bare
 * specifier to an absolute file path, which the compiled runtime *can* load. It is
 * only used as a fallback when Bun's own resolver fails.
 */

/** File extensions probed when a specifier or exports target has none. */
const FILE_EXTENSIONS = [".js", ".mjs", ".cjs", ".json", ".node"];

/** Directory index filenames probed when a target resolves to a directory. */
const INDEX_FILES = ["index.js", "index.mjs", "index.cjs", "index.json"];

/** Condition preference for `import()`-shaped resolution. */
export const IMPORT_CONDITIONS = ["node", "import", "module", "default"];

/** Condition preference for `require()`-shaped resolution. */
export const REQUIRE_CONDITIONS = ["node", "require", "default"];

interface PackageJson {
	main?: unknown;
	module?: unknown;
	exports?: unknown;
}

/**
 * Resolve a bare specifier against on-disk `node_modules` starting from `baseDir`,
 * honoring `exports` (conditions + subpath patterns) and `main`/`module`/index
 * fallbacks. Returns an absolute file path, or `null` when nothing resolves.
 *
 * @param specifier bare package specifier, e.g. `xlsx`, `@scope/pkg`, `pkg/sub`.
 * @param baseDir directory the resolution walks up from.
 * @param conditions ordered export-condition preference (see {@link IMPORT_CONDITIONS}).
 */
export function resolveBareSpecifier(specifier: string, baseDir: string, conditions: string[]): string | null {
	const { name, subpath } = splitSpecifier(specifier);
	if (!name) return null;
	const pkgDir = findPackageDir(name, baseDir);
	if (!pkgDir) return null;
	const pkg = readJson(path.join(pkgDir, "package.json"));

	if (pkg && pkg.exports != null) {
		const target = resolveExports(pkg.exports, subpath, conditions);
		if (target && (target.startsWith("./") || target.startsWith("../"))) {
			const resolved = finalizeFile(path.resolve(pkgDir, target));
			if (resolved) return resolved;
		}
		// `exports` present but no usable match: fall through to legacy fields only for
		// the package root, mirroring Node's leniency for missing subpath maps.
	}

	if (subpath === ".") {
		for (const field of mainFields(pkg, conditions)) {
			const resolved = finalizeFile(path.resolve(pkgDir, field));
			if (resolved) return resolved;
		}
		return finalizeDir(pkgDir);
	}

	const abs = path.resolve(pkgDir, subpath);
	return finalizeFile(abs) ?? finalizeDir(abs);
}

/** Split `@scope/pkg/sub` or `pkg/sub` into package name and `.`-rooted subpath. */
function splitSpecifier(specifier: string): { name: string; subpath: string } {
	const parts = specifier.split("/");
	let name: string;
	let rest: string[];
	if (specifier.startsWith("@")) {
		name = parts.slice(0, 2).join("/");
		rest = parts.slice(2);
	} else {
		name = parts[0] ?? "";
		rest = parts.slice(1);
	}
	return { name, subpath: rest.length > 0 ? `./${rest.join("/")}` : "." };
}

/** Walk up from `baseDir` looking for `<dir>/node_modules/<name>`. */
function findPackageDir(name: string, baseDir: string): string | null {
	let dir = path.resolve(baseDir);
	for (;;) {
		if (path.basename(dir) !== "node_modules") {
			const candidate = path.join(dir, "node_modules", name);
			if (isDir(candidate)) return candidate;
		}
		const parent = path.dirname(dir);
		if (parent === dir) return null;
		dir = parent;
	}
}

/** Ordered legacy entry candidates for the package root. */
function mainFields(pkg: PackageJson | null, conditions: string[]): string[] {
	if (!pkg) return [];
	const main = typeof pkg.main === "string" ? pkg.main : null;
	const module = typeof pkg.module === "string" ? pkg.module : null;
	const preferModule = conditions.includes("import");
	const ordered = preferModule ? [module, main] : [main, module];
	return ordered.filter((value): value is string => value !== null);
}

/**
 * Resolve an `exports` value for `subpath` under `conditions`. Handles string
 * targets, condition maps, subpath maps, and `*` patterns per the Node exports spec.
 */
function resolveExports(exports: unknown, subpath: string, conditions: string[]): string | null {
	if (typeof exports === "string") return subpath === "." ? exports : null;
	if (Array.isArray(exports)) {
		for (const entry of exports) {
			const resolved = resolveExports(entry, subpath, conditions);
			if (resolved) return resolved;
		}
		return null;
	}
	if (!exports || typeof exports !== "object") return null;

	const record = exports as Record<string, unknown>;
	let isSubpathMap = false;
	for (const key in record) {
		if (key === "." || key.startsWith("./")) {
			isSubpathMap = true;
			break;
		}
	}
	if (!isSubpathMap) {
		// Bare condition map applies only to the package root.
		return subpath === "." ? resolveConditional(record, conditions) : null;
	}

	if (subpath in record) return resolveConditional(record[subpath], conditions);
	for (const key in record) {
		if (!key.includes("*")) continue;
		const captured = matchStar(key, subpath);
		if (captured === null) continue;
		const target = resolveConditional(record[key], conditions);
		if (target) return target.replace(/\*/g, captured);
	}
	return null;
}

/**
 * Collapse a conditional export value to a string target. Object keys are evaluated
 * in declaration order — the first key active in `conditions` (or `default`) wins,
 * matching Node's condition-resolution semantics.
 */
function resolveConditional(value: unknown, conditions: string[]): string | null {
	if (typeof value === "string") return value;
	if (Array.isArray(value)) {
		for (const entry of value) {
			const resolved = resolveConditional(entry, conditions);
			if (resolved) return resolved;
		}
		return null;
	}
	if (!value || typeof value !== "object") return null;
	const record = value as Record<string, unknown>;
	for (const key in record) {
		if (key !== "default" && !conditions.includes(key)) continue;
		const resolved = resolveConditional(record[key], conditions);
		if (resolved) return resolved;
	}
	return null;
}

/** Match a single-`*` exports pattern against `subpath`, returning the captured segment. */
function matchStar(pattern: string, subpath: string): string | null {
	const star = pattern.indexOf("*");
	if (star === -1) return null;
	const prefix = pattern.slice(0, star);
	const suffix = pattern.slice(star + 1);
	if (!subpath.startsWith(prefix) || !subpath.endsWith(suffix)) return null;
	if (subpath.length < prefix.length + suffix.length) return null;
	return subpath.slice(prefix.length, subpath.length - suffix.length);
}

/** Resolve `candidate` to an existing file, probing extensions when it has none. */
function finalizeFile(candidate: string): string | null {
	if (isFile(candidate)) return candidate;
	if (path.extname(candidate) === "") {
		for (const ext of FILE_EXTENSIONS) {
			if (isFile(candidate + ext)) return candidate + ext;
		}
	}
	return null;
}

/** Resolve a directory to its package `main` or an index file. */
function finalizeDir(dir: string): string | null {
	if (!isDir(dir)) return null;
	const pkg = readJson(path.join(dir, "package.json"));
	if (pkg && typeof pkg.main === "string") {
		const resolved = finalizeFile(path.resolve(dir, pkg.main));
		if (resolved) return resolved;
	}
	for (const index of INDEX_FILES) {
		const candidate = path.join(dir, index);
		if (isFile(candidate)) return candidate;
	}
	return null;
}

function readJson(file: string): PackageJson | null {
	try {
		return JSON.parse(fs.readFileSync(file, "utf8")) as PackageJson;
	} catch {
		return null;
	}
}

function isDir(target: string): boolean {
	try {
		return fs.statSync(target).isDirectory();
	} catch {
		return false;
	}
}

function isFile(target: string): boolean {
	try {
		return fs.statSync(target).isFile();
	} catch {
		return false;
	}
}
