import { readFile, writeFile } from 'node:fs/promises';

const output = new URL('../crates/agent-core/src/anthropic-skills.json', import.meta.url);
const repository = 'https://github.com/anthropics/skills';
const headers = {
	Accept: 'application/vnd.github+json',
	'User-Agent': 'jucode-index-refresh'
};

async function get(url, optional = false) {
	const response = await fetch(url, { headers });
	if (optional && response.status === 404) return '';
	if (!response.ok) throw new Error(`${response.status} ${response.statusText}: ${url}`);
	return response.text();
}

function scalar(value) {
	const trimmed = value.trim();
	if (trimmed.startsWith('"')) {
		try {
			return JSON.parse(trimmed);
		} catch {
			// Keep the source text when an upstream scalar is not JSON-compatible.
		}
	}
	if (trimmed.startsWith("'") && trimmed.endsWith("'")) return trimmed.slice(1, -1);
	return trimmed;
}

function frontmatter(markdown, key) {
	const block = markdown.match(/^---\r?\n([\s\S]*?)\r?\n---/)?.[1] ?? '';
	const value = block.match(new RegExp(`^${key}:\\s*(.+)$`, 'm'))?.[1] ?? '';
	return scalar(value);
}

const previous = JSON.parse(await readFile(output, 'utf8'));
const known = new Map(previous.skills.map((skill) => [skill.id, skill]));
const revision = JSON.parse(await get('https://api.github.com/repos/anthropics/skills/commits/main')).sha;
const directories = JSON.parse(
	await get(`https://api.github.com/repos/anthropics/skills/contents/skills?ref=${revision}`)
)
	.filter((entry) => entry.type === 'dir')
	.sort((left, right) => left.name.localeCompare(right.name));

const skills = [];
for (const directory of directories) {
	const id = directory.name;
	if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(id)) {
		throw new Error(`Refusing unsafe skill id: ${id}`);
	}
	const rawBase = `https://raw.githubusercontent.com/anthropics/skills/${revision}/skills/${id}`;
	const markdown = await get(`${rawBase}/SKILL.md`);
	const licenseText = await get(`${rawBase}/LICENSE.txt`, true);
	const redistributable =
		!licenseText.includes('All rights reserved') &&
		!licenseText.includes('ADDITIONAL RESTRICTIONS');
	const old = known.get(id);
	skills.push({
		id,
		name: old?.name ?? frontmatter(markdown, 'name') ?? id,
		description: frontmatter(markdown, 'description') || old?.description || `Anthropic ${id} skill`,
		tags: old?.tags ?? [],
		license: redistributable ? 'Apache-2.0' : 'Anthropic source-available',
		redistributable
	});
}

const index = { name: 'anthropic', repository, revision, skills };
await writeFile(output, `${JSON.stringify(index, null, 2)}\n`);
console.log(`Updated ${skills.length} Anthropic skills in ${output.pathname}`);
