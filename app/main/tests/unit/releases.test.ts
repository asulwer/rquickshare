import { test, assert, describe } from 'vitest'
import { parseReleaseNotes, releasesSince } from '../../src/vue_lib/releases'

const body = [
	'## What\'s Changed',
	'* fix(ble): start the receiver once Bluetooth is on by @asulwer in https://github.com/asulwer/rquickshare/pull/96',
	'* docs: a link elsewhere by @someone in https://example.com/pull/1',
	'',
	'## New Contributors',
	'* @someone made their first contribution in https://github.com/asulwer/rquickshare/pull/90',
	'',
	'**Full Changelog**: https://github.com/asulwer/rquickshare/compare/v1.26.300.8...v1.26.301.10',
].join('\r\n')

describe('release notes', () => {
	test('lists the changes, linking only this repo\'s pull requests', () => {
		assert.deepEqual(parseReleaseNotes(body), [
			{ text: 'fix(ble): start the receiver once Bluetooth is on', url: 'https://github.com/asulwer/rquickshare/pull/96' },
			{ text: 'docs: a link elsewhere', url: null },
		])
	})

	test('a hand-written list is kept as text', () => {
		assert.deepEqual(parseReleaseNotes('Fixes:\n- one thing\n* another'), [
			{ text: 'one thing', url: null },
			{ text: 'another', url: null },
		])
	})

	test('only published releases newer than the running one, newest first', () => {
		const releases = [
			{ tag_name: 'v1.26.300.8', body },
			{ tag_name: 'v1.26.302.11', body: '' },
			{ tag_name: 'v1.26.303.12', draft: true },
			{ tag_name: 'v1.26.301.10', body },
			{ tag_name: 'nightly' },
		]
		const newer = releasesSince(releases, '1.26.300.8')
		assert.deepEqual(newer.map((r) => r.version), ['1.26.302.11', '1.26.301.10'])
		assert.equal(newer[0].url, 'https://github.com/asulwer/rquickshare/releases/tag/v1.26.302.11')
		assert.lengthOf(newer[1].changes, 2)
	})

	test('anything other than a list of releases gives nothing', () => {
		assert.deepEqual(releasesSince({ message: 'rate limited' }, '1.26.300.8'), [])
	})
})
