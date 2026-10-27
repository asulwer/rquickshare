import { test, assert, describe } from 'vitest'
import type { ChannelMessage } from '@martichou/core_lib/bindings/ChannelMessage'
import type { State } from '@martichou/core_lib/bindings/State'
import { progressTracker, transferInProgress } from '../../src/vue_lib/updater'

describe('update progress', () => {
	test('counts chunks against the announced size', () => {
		const progress = progressTracker()
		assert.equal(progress({ event: 'Started', data: { contentLength: 400 } }), 0)
		assert.equal(progress({ event: 'Progress', data: { chunkLength: 100 } }), 25)
		assert.equal(progress({ event: 'Progress', data: { chunkLength: 199 } }), 74)
		assert.equal(progress({ event: 'Progress', data: { chunkLength: 101 } }), 100)
		assert.equal(progress({ event: 'Finished' }), 100)
	})

	test('never goes past 100 when more arrives than announced', () => {
		const progress = progressTracker()
		progress({ event: 'Started', data: { contentLength: 10 } })
		assert.equal(progress({ event: 'Progress', data: { chunkLength: 50 } }), 100)
	})

	test('stays unknown without a size', () => {
		const progress = progressTracker()
		assert.isNull(progress({ event: 'Started', data: {} }))
		assert.isNull(progress({ event: 'Progress', data: { chunkLength: 100 } }))
		assert.equal(progress({ event: 'Finished' }), 100)
	})
})

describe('transfer in progress', () => {
	const msg = (state?: State) => ({ id: 'a', state } as ChannelMessage)

	test('idle with no transfers, or only finished ones', () => {
		assert.isFalse(transferInProgress([], 0))
		assert.isFalse(transferInProgress(['Finished', 'Cancelled', 'Rejected', 'Disconnected'].map((s) => msg(s as State)), 0))
	})

	test('busy while connecting, waiting for consent or moving files', () => {
		assert.isTrue(transferInProgress([], 1))
		assert.isTrue(transferInProgress([msg('WaitingForUserConsent')], 0))
		assert.isTrue(transferInProgress([msg('Finished'), msg('ReceivingFiles')], 0))
		assert.isTrue(transferInProgress([msg('SentUkeyServerInit')], 0))
		assert.isTrue(transferInProgress([msg()], 0))
	})
})
