import { isNewerVersion } from './version';

export const repoUrl = 'https://github.com/asulwer/rquickshare';

// One line of a release's "What's Changed": the PR title, and the PR itself
// when the line links one.
export interface ReleaseChange {
	text: string;
	url: string | null;
}

export interface ReleaseNotes {
	version: string;
	url: string;
	changes: ReleaseChange[];
}

// `gh release create --generate-notes` writes each merged PR as
// "* <title> by @<author> in <PR url>".
const generatedLine = /^[*-] (.+?) by @\S+ in (\S+)$/;
const pullUrl = new RegExp(`^${repoUrl.replace(/[.]/g, '\\.')}/pull/\\d+$`);

// The changes listed in a release body. Only the "What's Changed" list when
// there is one, so "New Contributors" and the changelog link are left out.
// The text is shown as text, never as HTML.
export function parseReleaseNotes(body: string): ReleaseChange[] {
	const lines = body.split(/\r?\n/).map((line) => line.trim());
	const start = lines.findIndex((line) => /^#+ What's Changed$/i.test(line));
	const changes: ReleaseChange[] = [];
	for (const line of lines.slice(start + 1)) {
		if (start >= 0 && line.startsWith('#')) break;
		const generated = generatedLine.exec(line);
		if (generated) {
			// A link only to this repo's pull requests: it's opened outside the app.
			const url = pullUrl.test(generated[2]) ? generated[2] : null;
			changes.push({ text: generated[1], url });
		} else if (/^[*-] /.test(line)) {
			changes.push({ text: line.slice(2), url: null });
		}
	}
	return changes;
}

// The published releases newer than `current`, newest first, from the GitHub
// releases API's JSON.
export function releasesSince(releases: unknown, current: string): ReleaseNotes[] {
	if (!Array.isArray(releases)) return [];
	const newer: ReleaseNotes[] = [];
	for (const release of releases) {
		if (release?.draft || release?.prerelease) continue;
		const tag = release?.tag_name;
		if (typeof tag !== 'string' || !tag.startsWith('v')) continue;
		const version = tag.substring(1);
		if (!isNewerVersion(version, current)) continue;
		newer.push({
			version,
			url: `${repoUrl}/releases/tag/${encodeURIComponent(tag)}`,
			changes: parseReleaseNotes(typeof release.body === 'string' ? release.body : ''),
		});
	}
	return newer.sort((a, b) => (isNewerVersion(a.version, b.version) ? -1 : 1));
}
