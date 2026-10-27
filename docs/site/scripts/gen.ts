#!/usr/bin/env bun
// Regenerates the generated pages under src/content/docs/ (commit the output; CI has no rematch binary).
//
//   bun run gen                                    # uses target/release/rematch
//   REMATCH_BIN=target/debug/rematch bun run gen
//
// 1. reference/cli.md from the binary's own `--help`: `rematch --help`, then every subcommand listed
//    under "Commands:", recursively. `--help` never opens the ledger; REMATCH_ROOT is an empty temp dir anyway.
// 2. Every templates/**/*.md is copied to src/content/docs/ with each line
//      <!-- include: <repo path>[#anchor] [lang=<fence language>] -->
//    replaced by that file (or its `<!-- ANCHOR: x -->` … `<!-- ANCHOR_END: x -->` region; with lang=, as a
//    fenced block). Edit the template or the source doc, never the output.

import { mkdtempSync, rmSync, readdirSync, readFileSync, statSync, existsSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';

const site = resolve(import.meta.dir, '..');
const repo = resolve(site, '../..');
const out = join(site, 'src/content/docs');
const templates = join(site, 'templates');
const blob = 'https://github.com/vncp/rematch/blob/main';
const base = '/rematch';

// Source docs that have a page of their own; links and anchors into them go there.
const pageOf: Record<string, string> = {
	'docs/DESIGN.md': `${base}/background/design/`,
	'docs/research.md': `${base}/background/research/`,
	'docs/adr/001-lake.md': `${base}/background/adr-001-lake/`,
	'docs/domain.yml.md': `${base}/reference/configuration/domain-yml/`,
};

async function write(path: string, text: string) {
	mkdirSync(dirname(path), { recursive: true });
	await Bun.write(path, text);
	console.log(`gen: wrote ${relative(site, path)}`);
}

// ---------- 1. CLI reference ----------

async function cli() {
	const bin = process.env.REMATCH_BIN ? resolve(process.env.REMATCH_BIN) : join(repo, 'target/release/rematch');
	if (!existsSync(bin)) {
		console.error(`gen: ${bin} not found (cargo build --release, or set REMATCH_BIN); keeping the committed reference/cli.md`);
		return;
	}
	const root = mkdtempSync(join(tmpdir(), 'rematch-gen-'));
	const run = (args: string[]) => {
		const p = Bun.spawnSync([bin, ...args], { env: { ...process.env, REMATCH_ROOT: root } });
		if (p.exitCode !== 0) throw new Error(`gen: rematch ${args.join(' ')} exited ${p.exitCode}: ${p.stderr}`);
		return p.stdout.toString().replace(/\s+$/, '');
	};
	// Subcommand names listed under "Commands:" (clap indents each by two spaces).
	const subcommands = (help: string) => {
		const names: string[] = [];
		let inside = false;
		for (const line of help.split('\n')) {
			if (/^Commands:/.test(line)) inside = true;
			else if (inside && line.trim() === '') break;
			else if (inside && /^ {2}[a-z]/.test(line)) {
				const name = line.trim().split(/\s+/)[0];
				if (name !== 'help') names.push(name);
			}
		}
		return names;
	};
	try {
		const version = `rematch ${JSON.parse(run(['--version'])).data.version}`;
		const top = run(['--help']);
		const sections: string[] = [];
		const emit = (path: string[], depth: number) => {
			const help = run([...path, '--help']);
			sections.push(`${'#'.repeat(depth)} rematch ${path.join(' ')}\n\n\`\`\`text\n${help}\n\`\`\`\n`);
			for (const sub of subcommands(help)) emit([...path, sub], depth + 1);
		};
		for (const sub of subcommands(top)) emit([sub], 2);
		// One-line descriptions as table cells: escape pipes, and set --flags as code (no smart dashes).
		const cell = (text: string) =>
			text
				.replace(/\|/g, '\\|')
				.split(/(`[^`]*`)/)
				.map((part, i) => (i % 2 ? part : part.replace(/(^|[\s(])(--?[a-z][\w-]*)/g, '$1`$2`')))
				.join('');
		const list = subcommands(top)
			.map((s) => `| [\`${s}\`](#rematch-${s}) | ${cell(top.match(new RegExp(`^ {2}${s}\\s+(.*)$`, 'm'))?.[1] ?? '')} |`)
			.join('\n');
		const page = `---
title: CLI
description: Every rematch subcommand with its flags, from the binary's own --help.
editUrl: false
tableOfContents:
  maxHeadingLevel: 2
---

:::note[Generated]
From \`${version}\` by \`docs/site/scripts/gen.ts\` (\`bun run gen\`). Do not edit by hand.
:::

Every subcommand prints one JSON envelope on stdout unless \`--text\` asks for readable lines (DESIGN §2):

| Outcome | stdout | Exit |
|---|---|---|
| success | \`{"ok":true,"operation":…,"data":…}\` | 0 |
| failure | \`{"ok":false,…,"error":…}\` | 1 |
| usage error | — | 2 |
| cancelled | — | 130 |

In omp, most \`/rematch <command>\` invocations run the same command; the exceptions are listed in [day to day](/rematch/guides/day-to-day/#omp-commands-and-the-cli).

## Commands

| Command | What it does |
|---|---|
${list}

## rematch

\`\`\`text
${top}
\`\`\`

${sections.join('\n')}`;
		await write(join(out, 'reference/cli.md'), page);
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
}

// ---------- 2. Templates with included regions ----------

// Escape `<` that would start an HTML tag in prose (placeholders like <name>), outside fenced and inline code.
function escapeTags(md: string): string {
	const html = /^<\/?(p|picture|source|img|br|b|i|em|strong|code|sub|sup|details|summary|a|div|span|kbd)\b/i;
	return md
		.split(/(^ *```[\s\S]*?^ *```)/m)
		.map((chunk, i) =>
			i % 2 === 1
				? chunk
				: chunk.replace(/(?<!`)(`+)(?!`)(?:(?!\n\s*\n)[\s\S])*?[^`]\1(?!`)|<(?=[A-Za-z/])/g, (m, ticks, off, s) =>
						ticks || html.test(s.slice(off)) ? m : '&lt;',
					),
		)
		.join('');
}

function region(text: string, anchor: string, file: string): string {
	const start = text.indexOf(`<!-- ANCHOR: ${anchor} -->`);
	const end = text.indexOf(`<!-- ANCHOR_END: ${anchor} -->`);
	if (start < 0 || end < start) throw new Error(`gen: ${file} has no region "${anchor}"`);
	return text.slice(text.indexOf('\n', start) + 1, end).trim();
}

// Relative links → site pages or GitHub; `#anchor` links in a region → the source's full page.
function rewriteLinks(md: string, file: string, page: string): string {
	return md.replace(/\]\(([^)\s]+)\)/g, (whole, target: string) => {
		if (/^[a-z]+:/i.test(target)) return whole;
		const [path, hash] = target.split('#');
		const repoPath = path ? relative(repo, resolve(repo, dirname(file), path)) : file;
		const own = pageOf[repoPath];
		if (own) return `](${own === page ? '' : own}${hash ? `#${hash}` : ''})`;
		return `](${blob}/${repoPath}${hash ? `#${hash}` : ''})`;
	});
}

function include(spec: string, page: string): string {
	const [target, ...opts] = spec.trim().split(/\s+/);
	const [file, anchor] = target.split('#');
	const abs = join(repo, file);
	if (!existsSync(abs)) throw new Error(`gen: include ${file} not found`);
	const lang = opts.find((o) => o.startsWith('lang='))?.slice(5);
	let text = readFileSync(abs, 'utf8');
	if (lang) return `\`\`\`${lang} title="${file}"\n${text.replace(/\s+$/, '')}\n\`\`\``;
	if (anchor) text = region(text, anchor, file);
	else {
		// A full page: its own `# Title` is the page title (frontmatter), drop it.
		text = text.replace(/^# .*\n/m, '');
		text = text.replace(/<!-- ANCHOR(_END)?: [\w-]+ -->\n?/g, '');
	}
	return escapeTags(rewriteLinks(text.trim(), file, page));
}

function walk(dir: string): string[] {
	return readdirSync(dir).flatMap((name) => {
		const p = join(dir, name);
		return statSync(p).isDirectory() ? walk(p) : p.endsWith('.md') ? [p] : [];
	});
}

async function pages() {
	for (const tpl of walk(templates)) {
		const rel = relative(templates, tpl);
		const page = `${base}/${rel.replace(/\.md$/, '')}/`;
		const src = await Bun.file(tpl).text();
		const body = src.replace(/^<!-- include: (.+?) -->$/gm, (_, spec) => include(spec, page));
		await write(join(out, rel), body);
	}
}

await cli();
await pages();
