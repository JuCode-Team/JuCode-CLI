// Fetches the pinned @oh-my-pi/pi-catalog release and rewrites
// crates/vendor/data/omp-catalog.json — the provider/model/auth metadata
// JuCode parses at runtime instead of maintaining its own tables.
//
// The pin lives in scripts/omp/OMP_VERSION: bump it, re-run this script, and
// commit the regenerated artifact together. Nothing is fetched at runtime.
//
// Usage: node scripts/omp/sync-catalog.mjs

import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const version = readFileSync(join(root, "scripts/omp/OMP_VERSION"), "utf8").trim();
const outPath = join(root, "crates/vendor/data/omp-catalog.json");

const meta = await (
	await fetch(`https://registry.npmjs.org/@oh-my-pi%2Fpi-catalog/${version}`)
).json();
if (meta.error) throw new Error(`npm lookup failed: ${meta.error}`);

const work = mkdtempSync(join(tmpdir(), "omp-catalog-"));
try {
	const tgz = join(work, "catalog.tgz");
	writeFileSync(tgz, Buffer.from(await (await fetch(meta.dist.tarball)).arrayBuffer()));
	execFileSync("tar", ["-xzf", tgz, "-C", work]);

	const pkg = join(work, "package/src");
	const models = JSON.parse(readFileSync(join(pkg, "models.json"), "utf8"));
	const rules = JSON.parse(readFileSync(join(pkg, "compat/rules.json"), "utf8"));

	// Keep only the fields agent-core consumes; the upstream compat/identity
	// blocks are TS-facing shaping rules that don't translate to the Rust side.
	const trimmed = {};
	for (const [provider, table] of Object.entries(models)) {
		const kept = {};
		for (const [id, m] of Object.entries(table)) {
			kept[id] = {
				id: m.id,
				name: m.name,
				api: m.api,
				baseUrl: m.baseUrl,
				reasoning: m.reasoning,
				input: m.input,
				contextWindow: m.contextWindow,
				maxTokens: m.maxTokens,
				cost: m.cost
					? {
							input: m.cost.input,
							output: m.cost.output,
							cacheRead: m.cost.cacheRead,
							cacheWrite: m.cost.cacheWrite,
						}
					: undefined,
			};
		}
		trimmed[provider] = kept;
	}

	// Upstream's curated default model per provider lives in TS, not JSON —
	// scrape the `id:`/`defaultModel:` pairs out of the descriptors source.
	const descriptors = readFileSync(
		join(pkg, "provider-models/descriptors.ts"),
		"utf8",
	);
	const defaultModels = {};
	for (const match of descriptors.matchAll(
		/id:\s*"([^"]+)"[\s\S]*?defaultModel:\s*"([^"]+)"/g,
	)) {
		defaultModels[match[1]] = match[2];
	}

	// JuCode only implements a few wire dialects (mirrors `protocol_for_api`
	// in crates/vendor/src/omp.rs). Auth rules for providers that can never
	// serve a model here are dead weight — a login would mint credentials no
	// request can use — and the Google ones embed OAuth client credentials
	// that trip push-protection secret scanning. Drop them at sync time.
	const SUPPORTED_APIS = new Set([
		"anthropic-messages",
		"openai-completions",
		"openrouter",
		"openai-responses",
		"openai-codex-responses",
		"azure-openai-responses",
	]);
	const routesByProvider = new Map(
		(rules.behavior.apiRoutes ?? []).map((r) => [r.provider, r]),
	);
	const usable = (id, seen = new Set()) => {
		if (seen.has(id)) return false;
		seen.add(id);
		if (Object.values(trimmed[id] ?? {}).some((m) => SUPPORTED_APIS.has(m.api)))
			return true;
		const routes = routesByProvider.get(id);
		return (
			SUPPORTED_APIS.has(routes?.default) ||
			(routes?.routes ?? []).some((r) => SUPPORTED_APIS.has(r.api))
		);
	};
	const authProviders = (rules.auth.providers ?? []).filter((p) =>
		usable(p.storeAs ?? p.id) || usable(p.id),
	);

	const artifact = {
		ompVersion: version,
		authProviders,
		apiRoutes: rules.behavior.apiRoutes,
		defaultModels,
		models: trimmed,
	};
	mkdirSync(dirname(outPath), { recursive: true });
	writeFileSync(outPath, `${JSON.stringify(artifact)}\n`);

	const modelCount = Object.values(trimmed).reduce((n, t) => n + Object.keys(t).length, 0);
	const size = (readFileSync(outPath).length / 1024 / 1024).toFixed(2);
	console.log(
		`omp-catalog.json <- @oh-my-pi/pi-catalog@${version}: ` +
			`${Object.keys(trimmed).length} providers, ${modelCount} models, ` +
			`${artifact.authProviders.length} auth rules (${size} MiB)`,
	);
} finally {
	rmSync(work, { recursive: true, force: true });
}
