#!/usr/bin/env node
// Writes the latest.json the in-app updater reads (tauri-plugin-updater's
// static manifest) for a release's installers. release.yml runs it in the
// publish job and attaches the result to the release, where the app finds it
// at releases/latest/download/latest.json.
//
//   node scripts/updater-manifest.mjs <installers dir> <version> <tag> <repo>
//
//   version  The Cargo version, 1.26.298+42: the one the app reports and the
//            one the build signed into each .sig.
//   tag      The release tag the installers are downloaded from, v1.26.298.42.
//   repo     owner/name.
//
// Every installer the Release workflow builds must be there with its .sig, so a
// release never goes out with an updater that skips some of its users.
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';

// The updater looks for <os>-<arch>-<bundle type>, where the bundle type is the
// one the running app was installed from. No plain <os>-<arch> fallback: an app
// that can't tell how it was installed would be handed a package it can't
// install.
const PLATFORMS = [
	{ key: 'windows-x86_64-msi', match: /_x64_[^/]*\.msi$/ },
	{ key: 'windows-x86_64-nsis', match: /_x64-setup\.exe$/ },
	{ key: 'darwin-aarch64-app', match: /_aarch64\.app\.tar\.gz$/ },
	{ key: 'linux-x86_64-deb', match: /_amd64\.deb$/ },
	{ key: 'linux-x86_64-rpm', match: /-x86_64\.rpm$/ },
];

function fail(message) {
	console.error(`updater-manifest: ${message}`);
	process.exit(1);
}

const [dir, version, tag, repo] = process.argv.slice(2);
if (!dir || !version || !tag || !repo) {
	fail('usage: updater-manifest.mjs <installers dir> <version> <tag> <repo>');
}

const files = readdirSync(dir, { recursive: true, withFileTypes: true })
	.filter((entry) => entry.isFile())
	.map((entry) => path.join(entry.parentPath, entry.name));

const platforms = {};
for (const { key, match } of PLATFORMS) {
	const found = files.filter((file) => match.test(path.basename(file)));
	if (found.length !== 1) {
		fail(`${key}: expected one installer, found ${found.length} (${found.join(', ') || 'none'})`);
	}
	const installer = found[0];
	let signature;
	try {
		signature = readFileSync(`${installer}.sig`, 'utf8').trim();
	} catch {
		fail(`${key}: ${installer} has no .sig; was it built with the signing key?`);
	}
	const name = encodeURIComponent(path.basename(installer));
	platforms[key] = {
		signature,
		url: `https://github.com/${repo}/releases/download/${encodeURIComponent(tag)}/${name}`,
	};
}

process.stdout.write(JSON.stringify({
	version,
	pub_date: new Date().toISOString(),
	platforms,
}, null, 2) + '\n');
