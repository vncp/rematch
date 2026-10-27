// @ts-check
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';

const repo = 'https://github.com/vncp/rematch';
const base = '/rematch';

// URLs of the former mdBook site (bookmarks, old README links) → their pages here.
const mdbook = {
	'get-started/install.html': 'start/install/',
	'get-started/first-session.html': 'start/first-correction/',
	'get-started/what-you-see.html': 'start/first-correction/',
	'concepts/loop.html': 'concepts/loop/',
	'concepts/evidence-boundary.html': 'concepts/evidence-boundary/',
	'concepts/entries.html': 'concepts/entries/',
	'concepts/predicates-and-domains.html': 'concepts/predicates-and-domains/',
	'concepts/counters.html': 'concepts/counters/',
	'concepts/profile.html': 'concepts/profile/',
	'concepts/lake.html': 'concepts/lake/',
	'guides/day-to-day.html': 'guides/day-to-day/',
	'guides/correcting.html': 'guides/correct-the-classifier/',
	'guides/approving.html': 'guides/approve-scripts-and-hooks/',
	'guides/create.html': 'guides/create-a-skill/',
	'guides/improve.html': 'guides/improve-a-skill/',
	'guides/batch.html': 'guides/batch-with-a-verifier/',
	'guides/sharing.html': 'guides/share/',
	'guides/profile.html': 'guides/sync-machines/',
	'guides/report.html': 'guides/read-the-report/',
	'guides/lake.html': 'guides/query-history/',
	'guides/troubleshooting.html': 'guides/troubleshooting/',
	'reference/cli.html': 'reference/cli/',
	'reference/trace-schema.html': 'reference/trace-schema/',
	'reference/entry-layout.html': 'reference/entry-layout/',
	'reference/lint.html': 'reference/lint/',
	'reference/config.html': 'reference/configuration/model-roles/',
	'reference/policy-json.html': 'reference/configuration/policy-json/',
	'reference/domain-rules.html': 'reference/configuration/domain-rules/',
	'reference/domain-yml.html': 'reference/configuration/domain-yml/',
	'reference/environment.html': 'reference/environment/',
	'reference/files.html': 'reference/files/',
	'research/decisions.html': 'background/decisions/',
	'research/provenance.html': 'background/research/',
	'design/architecture.html': 'background/architecture/',
	'design/design.html': 'background/design/',
	'design/adr-001-lake.html': 'background/adr-001-lake/',
	'status.html': 'background/status/',
	'print.html': '',
};

// Static redirect pages written after the build; the script keeps the #fragment, the meta refresh is the fallback.
const mdbookRedirects = {
	name: 'mdbook-redirects',
	hooks: {
		'astro:build:done': ({ dir }) => {
			for (const [from, to] of Object.entries(mdbook)) {
				const target = `${base}/${to}`;
				const file = fileURLToPath(new URL(from, dir));
				mkdirSync(dirname(file), { recursive: true });
				writeFileSync(
					file,
					`<!doctype html><meta charset="utf-8"><title>Moved</title><link rel="canonical" href="${target}">` +
						`<meta name="robots" content="noindex"><meta http-equiv="refresh" content="0; url=${target}">` +
						`<script>location.replace(${JSON.stringify(target)} + location.hash)</script>` +
						`<p>Moved to <a href="${target}">${target}</a>.</p>\n`,
				);
			}
		},
	},
};

export default defineConfig({
	site: 'https://vncp.github.io',
	base,
	trailingSlash: 'always',
	// Diagrams are read from docs/assets/readme (one copy, shared with the README).
	vite: { server: { fs: { allow: ['..'] } } },
	integrations: [
		mdbookRedirects,
		starlight({
			title: 'rematch',
			description:
				'Let the agent improve its own harness from the sessions you were going to run anyway, and keep only what later sessions confirm.',
			logo: {
				light: '../assets/readme/wordmark-light.svg',
				dark: '../assets/readme/wordmark-dark.svg',
				replacesTitle: true,
			},
			favicon: '/favicon.svg',
			social: [{ icon: 'github', label: 'GitHub', href: repo }],
			editLink: { baseUrl: `${repo}/edit/main/docs/site/` },
			customCss: ['./src/styles/custom.css'],
			expressiveCode: {
				// Plain frames for every block: no empty terminal title bars.
				defaultProps: { frame: 'code', wrap: true },
				styleOverrides: {
					borderRadius: '0.5rem',
					borderColor: 'var(--rm-border)',
					codeFontFamily: 'var(--sl-font-mono)',
					uiFontFamily: 'var(--sl-font)',
					frames: { shadowColor: 'transparent' },
				},
			},
			components: { Hero: './src/components/Hero.astro' },
			tableOfContents: { minHeadingLevel: 2, maxHeadingLevel: 3 },
			lastUpdated: false,
			pagination: true,
			plugins: [starlightLinksValidator()],
			sidebar: [
				{
					label: 'Start',
					items: ['start/install', 'start/first-correction', 'start/what-happens-next'],
				},
				{
					label: 'Guides',
					items: [
						'guides/day-to-day',
						'guides/correct-the-classifier',
						'guides/approve-scripts-and-hooks',
						'guides/automatic-probes',
						'guides/create-a-skill',
						'guides/improve-a-skill',
						'guides/batch-with-a-verifier',
						'guides/share',
						'guides/sync-machines',
						'guides/read-the-report',
						'guides/query-history',
						'guides/troubleshooting',
					],
				},
				{
					label: 'Concepts',
					items: [
						'concepts/loop',
						'concepts/evidence-boundary',
						'concepts/entries',
						'concepts/predicates-and-domains',
						'concepts/counters',
						'concepts/surfaces',
						'concepts/profile',
						'concepts/lake',
					],
				},
				{
					label: 'Reference',
					items: [
						'reference/cli',
						'reference/trace-schema',
						'reference/entry-layout',
						'reference/lint',
						{
							label: 'Configuration',
							items: [
								'reference/configuration/model-roles',
								'reference/configuration/policy-json',
								'reference/configuration/domain-rules',
								'reference/configuration/domain-yml',
							],
						},
						'reference/environment',
						'reference/files',
						'reference/harnesses',
						'reference/glossary',
					],
				},
				{
					label: 'Background',
					items: [
						'background/architecture',
						'background/decisions',
						'background/research',
						'background/design',
						'background/adr-001-lake',
						'background/status',
					],
				},
			],
		}),
	],
});
