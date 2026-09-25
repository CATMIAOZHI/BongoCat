import { describe, expect, it } from 'vitest'

import type { PairConnectionState } from './pair'

import { pairStateKey, pairStatusKey, recordingBlockReasonKey } from './pair'

/**
 * R44：叶子窗口（猫咪上的浮层 / 聊天窗口 / 对方猫）说的那句话。
 *
 * 这一版之前只分「联机没打开」和「对方离线」两支，于是「没连上服务器」「正在连」
 * 「连不上」和「连上了但对方没上线」在界面上长得一模一样——用户会去等对方，
 * 而真实原因可能是自己地址填错或服务器没开。这里把映射钉住。
 */
describe('联机状态到界面上那句话的映射（R44）', () => {
  it('联机总开关关掉时，任何连接状态都说「没打开」', () => {
    const states: PairConnectionState[] = ['disabled', 'disconnected', 'connecting', 'reconnecting', 'connected', 'peer-offline', 'error']

    for (const state of states) {
      expect(pairStateKey(state, false)).toBe('pages.pairState.disabled')
    }
  })

  it('只有真的连上才是「不用提示」，其它情况各说各的', () => {
    expect(pairStateKey('connected', true)).toBe('')

    expect(pairStateKey('disconnected', true)).toBe('pages.pairState.disconnected')
    expect(pairStateKey('connecting', true)).toBe('pages.pairState.connecting')
    expect(pairStateKey('reconnecting', true)).toBe('pages.pairState.connecting')
    expect(pairStateKey('error', true)).toBe('pages.pairState.error')
  })

  it('「连上了但对方没上线」不能和「没连上服务器」混成一句', () => {
    expect(pairStateKey('peer-offline', true)).toBe('pages.pairState.peerOffline')
    expect(pairStateKey('peer-offline', true)).not.toBe(pairStateKey('disconnected', true))
  })

  it('未知状态按「没连上」处理，不会误报成已连上', () => {
    expect(pairStateKey('something-new' as PairConnectionState, true)).toBe('pages.pairState.disconnected')
    expect(pairStatusKey('something-new' as PairConnectionState)).toBe('disconnected')
  })
})

/**
 * 待确认的语音「为什么发不出去」（R44 复审抓出的回归）。
 *
 * 这一版一度只看连接状态：`connected` 被当成「在等对方」，于是「录好了、对方也在线」
 * 这个正常状态也写着「对方不在线，等对方回来再发」，而 `recordingReady` 成了死分支。
 */
describe('待确认语音发不出去的原因（R41 / R44）', () => {
  const base = { enabled: true, connection: 'connected' as PairConnectionState, peerOnline: true, sending: false }

  it('一切都好时不给原因（界面落回「录音 Ns，待确认」）', () => {
    expect(recordingBlockReasonKey(base)).toBe('')
  })

  it('正在发送时也不给「发不出去」的原因', () => {
    expect(recordingBlockReasonKey({ ...base, sending: true })).toBe('')
  })

  it('自己没连上服务器才算「还没连上」，不能让用户去等对方', () => {
    for (const connection of ['connecting', 'reconnecting', 'disconnected', 'error'] as PairConnectionState[]) {
      expect(recordingBlockReasonKey({ ...base, connection, peerOnline: false }))
        .toBe('pages.main.hints.sendRecordingNotConnected')
    }
  })

  it('连上了但对方没上线，才说「等对方回来再发」', () => {
    expect(recordingBlockReasonKey({ ...base, connection: 'peer-offline', peerOnline: false }))
      .toBe('pages.main.hints.sendRecordingOffline')
  })

  it('联机总开关关掉时说「没打开」，压过其它状态', () => {
    for (const connection of ['connected', 'peer-offline', 'error'] as PairConnectionState[]) {
      expect(recordingBlockReasonKey({ ...base, enabled: false, connection, peerOnline: false }))
        .toBe('pages.main.hints.sendRecordingDisabled')
    }
  })

  it('总开关关掉但状态还没跟上（peerOnline 仍是 true）也不说「能发」', () => {
    expect(recordingBlockReasonKey({ ...base, enabled: false })).toBe('pages.main.hints.sendRecordingDisabled')
  })
})
