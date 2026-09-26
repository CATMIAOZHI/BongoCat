import { describe, expect, it } from 'vitest'

import type { Model } from '@/stores/model'

import { matchPeerModel, modelIdentity } from './pairModel'

const own: Model = { id: 'own', path: 'C:\\models\\keyboard', mode: 'keyboard', isPreset: true }
const custom: Model = { id: 'local-random-id', name: '小猫', path: 'D:\\custom-models\\local-random-id', mode: 'standard', isPreset: false }

describe('peer model metadata', () => {
  it('only sends portable names and matches different device IDs and paths', () => {
    const peer = modelIdentity({ ...custom, id: 'remote-id', path: 'E:\\私有目录\\custom-models\\remote-id' })
    expect(peer).toEqual({ name: '小猫', mode: 'standard', isPreset: false })
    expect(matchPeerModel([own, custom], peer)).toBe(custom)
  })

  it('matches built-in models by mode, regardless of install path', () => {
    expect(matchPeerModel([own], modelIdentity({ ...own, path: '/another/install/keyboard', id: 'other' }))).toBe(own)
  })

  it('does not mistake custom models for presets or other modes', () => {
    expect(matchPeerModel([own], { name: 'keyboard', mode: 'keyboard', isPreset: false })).toBeUndefined()
    expect(matchPeerModel([custom], { name: '小猫', mode: 'keyboard', isPreset: false })).toBeUndefined()
  })

  it('leaves missing, legacy and ambiguous models for the local fallback', () => {
    expect(matchPeerModel([own], modelIdentity(custom))).toBeUndefined()
    expect(matchPeerModel([own])).toBeUndefined()
    expect(matchPeerModel([custom, { ...custom, id: 'duplicate' }], modelIdentity(custom))).toBeUndefined()
    const legacy = { ...custom, name: undefined }
    expect(modelIdentity(legacy)?.name).toBe('')
    expect(matchPeerModel([legacy], modelIdentity(legacy))).toBeUndefined()
  })
})
