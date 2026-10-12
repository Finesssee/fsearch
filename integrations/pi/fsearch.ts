/**
 * grep through the fsearch index for wide searches.
 *
 * Replaces the built-in grep. A search over a folder that isn't inside a git
 * repo (D:\code, a drive) goes to the fsearch daemon; anything else, or
 * anything fsearch can't answer the same way (glob, context lines, quotes in
 * the pattern, an index still building), runs the built-in ripgrep grep.
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { createGrepTool, DEFAULT_MAX_BYTES, formatSize, truncateHead, truncateLine } from "@mariozechner/pi-coding-agent";
import { execFileSync, spawn } from "child_process";
import { existsSync, statSync } from "fs";
import { homedir } from "os";
import path from "path";

const DEFAULT_LIMIT = 100;
const META = /[\\^$.|?*+()[\]{}]/;

function userEnv(name: string): string | undefined {
	if (process.env[name]) return process.env[name];
	try {
		const out = execFileSync("reg", ["query", "HKCU\\Environment", "/v", name], { encoding: "utf8" });
		return out.match(new RegExp(`${name}\\s+REG_\\w+\\s+(.+)`))?.[1].trim();
	} catch {
		return undefined;
	}
}

const ROOTS = (userEnv("FSEARCH_ROOTS") ?? "").split(";").filter(Boolean).map((r) => r.toLowerCase());
const HOME = userEnv("FSEARCH_HOME");
// An absolute path: on Windows a bare name is looked up in the cwd first, so a
// repo could ship its own fsearch.exe.
const BIN = process.env.FSEARCH_BIN ?? path.join(homedir(), ".local", "bin", process.platform === "win32" ? "fsearch.exe" : "fsearch");
const norm = (p: string) => p.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();

function inGitRepo(dir: string): boolean {
	for (let d = dir; ; d = path.dirname(d)) {
		if (existsSync(path.join(d, ".git"))) return true;
		if (path.dirname(d) === d) return false;
	}
}

/** The folder fsearch should search, or null if this search belongs to ripgrep. */
function wideScope(dir: string): string | null {
	if (!ROOTS.length || HOME === undefined || !path.isAbsolute(BIN) || !existsSync(BIN)) return null;
	try {
		if (!statSync(dir).isDirectory()) return null;
	} catch {
		return null;
	}
	const n = norm(dir);
	if (!ROOTS.some((r) => n === r || n.startsWith(r + "/"))) return null;
	if (HOME !== "/" && !(n + "/").startsWith(norm(HOME) + "/")) return null;
	return inGitRepo(dir) ? null : dir;
}

type Reply = { ok?: boolean; complete?: boolean; indexing?: number; files?: { path: string; matches?: { line: number; text: string }[] }[] };

function ask(req: object, signal?: AbortSignal): Promise<Reply> {
	return new Promise((resolve, reject) => {
		const env = { ...process.env, FSEARCH_ROOTS: ROOTS.join(";"), FSEARCH_HOME: HOME };
		const child = spawn(BIN, ["stdio"], { env, stdio: ["pipe", "pipe", "ignore"], windowsHide: true });
		let out = "";
		const onAbort = () => child.kill();
		signal?.addEventListener("abort", onAbort, { once: true });
		child.stdout.on("data", (c) => (out += c));
		child.on("error", reject);
		child.on("close", () => {
			signal?.removeEventListener("abort", onAbort);
			try {
				resolve(JSON.parse(out.split("\n")[0] || "{}"));
			} catch (e) {
				reject(e);
			}
		});
		child.stdin.end(JSON.stringify(req) + "\n");
	});
}

export default function (pi: ExtensionAPI) {
	pi.registerTool({
		...createGrepTool(process.cwd()),
		label: "grep (fsearch)",
		async execute(id, params, signal, onUpdate, ctx) {
			const builtin = () => createGrepTool(ctx.cwd).execute(id, params, signal, onUpdate);
			const { pattern, glob, ignoreCase, context } = params;
			const dir = path.resolve(ctx.cwd, params.path || ".");
			const scope = wideScope(dir);
			if (!scope || glob || (context ?? 0) > 0 || pattern.includes('"') || !pattern) return builtin();

			const literal = params.literal || !META.test(pattern);
			const lower = pattern === pattern.toLowerCase();
			// fsearch is smart-case: a lowercase pattern matches any case.
			if (ignoreCase && !lower && !literal) return builtin();
			const q = (literal ? "grep:" : "regex:") + `"${ignoreCase ? pattern.toLowerCase() : pattern}"`;
			let keep: (line: string) => boolean = () => true;
			if (!ignoreCase && lower) {
				if (literal) keep = (l) => l.includes(pattern);
				else {
					try {
						const re = new RegExp(pattern, "u");
						keep = (l) => re.test(l);
					} catch {
						return builtin();
					}
				}
			}

			const limit = Math.max(1, params.limit ?? DEFAULT_LIMIT);
			let v: Reply;
			try {
				// Filtered-out lines can leave a file with nothing, so ask for spare files.
				v = await ask({ q: `${q} in:${scope}`, limit: limit * 2, per_file: limit, budget_ms: 0 }, signal);
			} catch {
				return builtin();
			}
			if (!v.ok || !v.complete || v.indexing) return builtin();

			const out: string[] = [];
			let count = 0;
			let cut = false;
			for (const f of v.files ?? []) {
				const abs = f.path.replace(/^\//, "");
				const rel = path.relative(scope, abs).replace(/\\/g, "/");
				for (const m of f.matches ?? []) {
					if (!keep(m.text)) continue;
					if (count === limit) {
						cut = true;
						break;
					}
					const t = truncateLine(m.text.replace(/\r/g, ""));
					out.push(`${rel}:${m.line}: ${t.text}`);
					count++;
				}
				if (cut) break;
			}
			if (!count) return { content: [{ type: "text", text: "No matches found" }], details: undefined };
			const truncation = truncateHead(out.join("\n"), { maxLines: Number.MAX_SAFE_INTEGER });
			const notices = ["fsearch index: skips vendor/, node_modules/, .venv/, site-packages/ and files over 1 MB; pass a repo path or glob for ripgrep"];
			if (cut) notices.push(`${limit} matches limit reached. Use limit=${limit * 2} for more, or refine pattern`);
			if (truncation.truncated) notices.push(`${formatSize(DEFAULT_MAX_BYTES)} limit reached`);
			return { content: [{ type: "text", text: `${truncation.content}\n\n[${notices.join(". ")}]` }], details: undefined };
		},
	});
}
