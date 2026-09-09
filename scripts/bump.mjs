#!/usr/bin/env node
// Bumps [workspace.package] version in the root Cargo.toml (the single source
// of truth both crates inherit via `version.workspace = true`) and refreshes
// Cargo.lock to match. Does not commit or tag - release-please owns that.
import { spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cargoTomlPath = path.join(repoRoot, 'Cargo.toml');

const SEMVER_RE = /^(\d+)\.(\d+)\.(\d+)$/;

function parseSemver(version) {
	const match = SEMVER_RE.exec(version);
	if (!match) return null;
	return { major: Number(match[1]), minor: Number(match[2]), patch: Number(match[3]) };
}

function compareSemver(a, b) {
	if (a.major !== b.major) return a.major - b.major;
	if (a.minor !== b.minor) return a.minor - b.minor;
	return a.patch - b.patch;
}

function nextVersion(current, kind) {
	if (kind === 'major') return { major: current.major + 1, minor: 0, patch: 0 };
	if (kind === 'minor') return { major: current.major, minor: current.minor + 1, patch: 0 };
	if (kind === 'patch') return { major: current.major, minor: current.minor, patch: current.patch + 1 };
	return null;
}

function fail(message) {
	console.error(`bump: ${message}`);
	process.exit(1);
}

const arg = process.argv[2];
if (!arg) fail('missing argument, expected "major", "minor", "patch", or an explicit x.y.z version');

const toml = readFileSync(cargoTomlPath, 'utf8');

// Scope the search to the [workspace.package] table so a same-named key in
// another table can't be matched by accident.
const sectionHeader = '[workspace.package]';
const sectionStart = toml.indexOf(`${sectionHeader}\n`);
if (sectionStart === -1) fail('could not find [workspace.package] section in Cargo.toml');
const bodyStart = sectionStart + sectionHeader.length + 1;
const nextHeaderMatch = /^\[/m.exec(toml.slice(bodyStart));
const bodyEnd = nextHeaderMatch ? bodyStart + nextHeaderMatch.index : toml.length;
const sectionBody = toml.slice(bodyStart, bodyEnd);

const versionLineRe = /^version\s*=\s*"([^"]*)"/m;
const versionMatches = sectionBody.match(new RegExp(versionLineRe.source, 'gm')) ?? [];
if (versionMatches.length === 0) fail('no version key found in [workspace.package]');
if (versionMatches.length > 1) fail(`expected exactly one version key in [workspace.package], found ${versionMatches.length}`);

const currentVersionStr = versionLineRe.exec(sectionBody)[1];
const currentVersion = parseSemver(currentVersionStr);
if (!currentVersion) fail(`current version "${currentVersionStr}" in Cargo.toml is not a valid x.y.z semver`);

let newVersion;
if (arg === 'major' || arg === 'minor' || arg === 'patch') {
	newVersion = nextVersion(currentVersion, arg);
} else {
	newVersion = parseSemver(arg);
	if (!newVersion) fail(`"${arg}" is not "major", "minor", "patch", or a valid x.y.z version`);
	if (compareSemver(newVersion, currentVersion) <= 0) {
		fail(`"${arg}" must be greater than the current version "${currentVersionStr}"`);
	}
}

const newVersionStr = `${newVersion.major}.${newVersion.minor}.${newVersion.patch}`;

const updatedBody = sectionBody.replace(versionLineRe, `version = "${newVersionStr}"`);
const updatedToml = toml.slice(0, bodyStart) + updatedBody + toml.slice(bodyEnd);
writeFileSync(cargoTomlPath, updatedToml);

const lockResult = spawnSync('cargo', ['update', '--workspace'], { cwd: repoRoot, stdio: 'inherit' });
if (lockResult.status !== 0) fail('cargo update --workspace failed, Cargo.toml was already written - check Cargo.lock manually');

console.log(`bump: ${currentVersionStr} -> ${newVersionStr}`);
