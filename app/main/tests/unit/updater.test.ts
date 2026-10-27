import { test, assert, describe } from 'vitest'
import { progressTracker } from '../../src/vue_lib/updater'

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
