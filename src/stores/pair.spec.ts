import { describe, expect, it } from 'vitest'

import type { ManualPhase, ManualStatus } from '@/composables/usePair'

import type { PairConnectionState } from './pair'

import { outboundBlockKey, pairStateKey, pairStatusKey, recordingBlockReasonKey } from './pair'

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

  /**
   * 公益档：服务器只帮忙打洞，不转发数据。所以「对方在服务器上在线」不等于「发得出去」——
   * 照旧说「能发」的话，录完一分钟才发现发不出去（Rust 侧会拒，服务器会用 1008 关连接）。
   */
  it('公益档下只看直连通没通，不看对方在不在服务器上', () => {
    expect(recordingBlockReasonKey({ ...base, tier: 'public', p2p: 'off' }))
      .toBe('pages.main.hints.sendRecordingPublicNotDirect')
    expect(recordingBlockReasonKey({ ...base, tier: 'public', p2p: 'connecting' }))
      .toBe('pages.main.hints.sendRecordingPublicNotDirect')
    expect(recordingBlockReasonKey({ ...base, tier: 'public', p2p: 'failed' }))
      .toBe('pages.main.hints.sendRecordingPublicNotDirect')

    // 直连通了就照旧能发（这条路是真的端到端）
    expect(recordingBlockReasonKey({ ...base, tier: 'public', p2p: 'connected' })).toBe('')
  })

  it('公益档也不越过「联机没打开」和「正在发送」这两条', () => {
    expect(recordingBlockReasonKey({ ...base, enabled: false, tier: 'public', p2p: 'off' }))
      .toBe('pages.main.hints.sendRecordingDisabled')
    expect(recordingBlockReasonKey({ ...base, sending: true, tier: 'public', p2p: 'off' })).toBe('')
  })
})

/**
 * 「现在发不出去」的硬理由（R47 的公益档）。
 *
 * 两份判据同形：**发出去只会掉进黑洞，而本地那条记录已经被标成「已发送」**。
 * 配对码那条连服务器都没有；公益档那台服务器只帮忙打洞、不中继（收到数据帧就用 1008
 * 关掉整条连接，信令也得跟着重来）。Rust 侧 `outbound_blocked` 是同一套判据。
 */
describe('聊天窗口「现在发不出去」的原因', () => {
  /** 只关心走到了哪一步，其余字段照 Rust 侧的默认形状填 */
  const manualAt = (phase: ManualPhase) => {
    return { phase, role: 'host', candidates: 1, nonHostCandidates: 0 } as ManualStatus
  }

  it('部署者那一档（full）什么都不挡，直连没建立也照样能发', () => {
    for (const p2p of ['off', 'connecting', 'failed'] as const) {
      expect(outboundBlockKey({ tier: 'full', p2p })).toBe('')
    }
    expect(outboundBlockKey({ tier: 'full', p2p: 'connected' })).toBe('')
  })

  it('公益档在直连建立之前挡住，建立之后放行', () => {
    for (const p2p of ['off', 'connecting', 'failed'] as const) {
      expect(outboundBlockKey({ tier: 'public', p2p })).toBe('pages.chat.hints.publicNotDirect')
    }
    expect(outboundBlockKey({ tier: 'public', p2p: 'connected' })).toBe('')
  })

  it('配对码还没连上时先说配对码那句（公益档那句在那儿是错的）', () => {
    expect(outboundBlockKey({ tier: 'full', p2p: 'off', manual: manualAt('joining') }))
      .toBe('pages.chat.hints.manualNotReady')
    expect(outboundBlockKey({ tier: 'public', p2p: 'off', manual: manualAt('gathering') }))
      .toBe('pages.chat.hints.manualNotReady')
  })

  it('配对码已经连上时，剩下的理由照常判', () => {
    expect(outboundBlockKey({ tier: 'full', p2p: 'off', manual: manualAt('connected') })).toBe('')
    expect(outboundBlockKey({ tier: 'public', p2p: 'connecting', manual: manualAt('connected') }))
      .toBe('pages.chat.hints.publicNotDirect')
    expect(outboundBlockKey({ tier: 'public', p2p: 'connected', manual: manualAt('connected') })).toBe('')
  })
})
