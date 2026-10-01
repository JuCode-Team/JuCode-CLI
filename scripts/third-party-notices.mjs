#!/usr/bin/env node
// Writes the license texts of the third-party code a build ships into one
// file: Rust crates (`--cargo <Cargo.toml>`, repeatable) and the production
// npm dependencies of a pnpm project (`--pnpm <dir>`). Identical texts are
// printed once with every package that carries them.
//
//   node scripts/third-party-notices.mjs --cargo src-tauri/Cargo.toml --pnpm . --out static/third-party-notices.txt
//
// Needs `cargo` (crate sources are fetched first) and `pnpm` on PATH.

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';

const args = process.argv.slice(2);
const cargo = [];
const pnpm = [];
let out = '';
let title = 'Third-party software';
for (let i = 0; i < args.length; i += 1) {
	const value = args[i + 1];
	if (args[i] === '--cargo') cargo.push(value);
	else if (args[i] === '--pnpm') pnpm.push(value);
	else if (args[i] === '--out') out = value;
	else if (args[i] === '--title') title = value;
	else throw new Error(`unknown argument: ${args[i]}`);
	i += 1;
}
if (!out) throw new Error('--out is required');

const LICENSE_FILE = /^(licen[cs]e|copying|notice)([-._].*)?$/i;

/** The license and notice files at the top of a package directory. */
function licenseTexts(dir) {
	if (!dir || !existsSync(dir)) return [];
	return readdirSync(dir)
		.filter((name) => LICENSE_FILE.test(name))
		.sort()
		.map((name) => readFileSync(path.join(dir, name), 'utf8').trim())
		.filter(Boolean);
}

/** name@version → { license, texts } */
const packages = new Map();
function add(id, license, texts) {
	if (!packages.has(id)) packages.set(id, { license: license || 'see text', texts });
}

for (const manifest of cargo) {
	const run = (cmd) =>
		execFileSync('cargo', [...cmd, '--manifest-path', manifest], {
			encoding: 'utf8',
			maxBuffer: 256 * 1024 * 1024,
			stdio: ['ignore', 'pipe', 'inherit']
		});
	run(['fetch']);
	const meta = JSON.parse(run(['metadata', '--format-version', '1']));
	const own = new Set(meta.workspace_members);
	for (const pkg of meta.packages) {
		// Workspace members and path dependencies are this project's own code.
		if (own.has(pkg.id) || !pkg.source) continue;
		const dir = path.dirname(pkg.manifest_path);
		const texts = licenseTexts(dir);
		if (pkg.license_file) {
			const file = path.join(dir, pkg.license_file);
			if (existsSync(file)) texts.push(readFileSync(file, 'utf8').trim());
		}
		add(`${pkg.name} ${pkg.version}`, pkg.license, texts);
	}
}

for (const dir of pnpm) {
	const list = JSON.parse(
		execFileSync('pnpm', ['licenses', 'list', '--prod', '--json'], {
			cwd: dir,
			encoding: 'utf8',
			maxBuffer: 64 * 1024 * 1024,
			stdio: ['ignore', 'pipe', 'inherit']
		})
	);
	for (const [license, entries] of Object.entries(list)) {
		for (const entry of entries) {
			entry.versions.forEach((version, i) => {
				add(`${entry.name} ${version}`, license, licenseTexts(entry.paths[i] ?? entry.paths[0]));
			});
		}
	}
}

// Group packages by their exact license text.
const groups = new Map();
const missing = [];
for (const [id, { license, texts }] of [...packages].sort(([a], [b]) => a.localeCompare(b))) {
	if (!texts.length) {
		missing.push(`${id} (${license})`);
		continue;
	}
	const text = texts.join('\n\n');
	if (!groups.has(text)) groups.set(text, []);
	groups.get(text).push(`${id} (${license})`);
}

const rule = '-'.repeat(78);
let body = `${title}\n\nThis software includes the following third-party components under their own licenses.\n`;
for (const [text, ids] of groups) {
	body += `\n${rule}\n${ids.join('\n')}\n${rule}\n\n${text}\n`;
}
if (missing.length) {
	body += `\n${rule}\nComponents whose package carries no license file; their licenses are named in parentheses:\n${rule}\n\n${missing.join('\n')}\n`;
}
mkdirSync(path.dirname(out), { recursive: true });
writeFileSync(out, body);
console.log(`wrote ${out}: ${packages.size} components, ${groups.size} distinct license texts`);
