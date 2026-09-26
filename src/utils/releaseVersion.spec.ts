import { expect, it } from 'vitest'

import { latestClientTag } from './releaseVersion'

it('ignores relay, draft and prerelease tags and compares versions numerically', () => {
  expect(latestClientTag([
    { tag_name: 'relay-v99' },
    { tag_name: 'v1.3.0', draft: true },
    { tag_name: 'v2.0.0', prerelease: true },
    { tag_name: 'v1.10.0' },
    { tag_name: 'v1.9.0' },
  ], '1.2.1')).toBe('v1.10.0')
  expect(latestClientTag([{ tag_name: 'v1.2.1' }], '1.2.1')).toBeUndefined()
})
