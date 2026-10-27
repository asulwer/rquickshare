import { test, assert, describe } from 'vitest'
import { displayVersion, isReleaseBuild } from '../../src/vue_lib/version'

describe('version', () => {
	test('a stamped release shows four numbers', () => {
		assert.equal(displayVersion('1.26.298+5'), '1.26.298.5')
		assert.equal(displayVersion('1.0.0'), '1.0.0')
	})

	test('only the unstamped <major>.0.0 is a dev build', () => {
		assert.isFalse(isReleaseBuild('1.0.0'))
		assert.isFalse(isReleaseBuild('2.0.0'))
		assert.isTrue(isReleaseBuild('1.26.298.5'))
		assert.isTrue(isReleaseBuild('1.26.1.1'))
		assert.isTrue(isReleaseBuild('1.0.1'))
	})
})
