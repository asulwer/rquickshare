import { test, assert, describe } from 'vitest'
import { ChannelMessage } from '@martichou/core_lib/bindings/ChannelMessage'
import { addMissingTransfers } from '../../src/vue_lib/transfers'

const msg = (id: string, state: ChannelMessage['state']): ChannelMessage => ({
	id, direction: 'LibToFront', action: null, rtype: 'Inbound', state, meta: null,
})

describe('addMissingTransfers', () => {
	test('adds transfers the window missed, in order', () => {
		const requests: ChannelMessage[] = []
		addMissingTransfers(requests, [msg('a', 'Finished'), msg('b', 'WaitingForUserConsent')])
		assert.deepEqual(requests.map((r) => r.id), ['a', 'b'])
	})

	test('keeps what a live event already delivered', () => {
		const requests = [msg('a', 'ReceivingFiles')]
		addMissingTransfers(requests, [msg('a', 'WaitingForUserConsent'), msg('b', 'Finished')])
		assert.equal(requests[0].state, 'ReceivingFiles')
		assert.deepEqual(requests.map((r) => r.id), ['a', 'b'])
	})
})
