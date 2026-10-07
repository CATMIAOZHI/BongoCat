import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { MenuResources } from './useAppMenu'

import { closeMenuResources, trackMenuResource, useAppMenu } from './useAppMenu'

const state = vi.hoisted(() => ({
  cat: { window: { scale: 75, opacity: 50, alwaysOnTop: true, passThrough: false, visible: true } },
  pair: {
    settings: {
      enabled: true,
      presence: 'active',
      chat: { visible: false, overlayVisible: undefined as boolean | undefined },
      remoteCat: { scale: 125, opacity: 75, alwaysOnTop: false, passThrough: false, visible: true, showStats: true, syncModel: true },
    },
    runtime: { connection: 'connected' },
  },
  remoteVisible: vi.fn(),
  showWindow: vi.fn(),
  closes: [] as ReturnType<typeof vi.fn>[],
}))
vi.mock('@tauri-apps/api/menu', () => {
  const factory = { new: async (options: object) => {
    const close = vi.fn(async () => {})
    state.closes.push(close)
    return { ...options, close }
  } }
  return { MenuItem: factory, CheckMenuItem: factory, Submenu: factory, PredefinedMenuItem: factory }
})
vi.mock('@tauri-apps/plugin-process', () => ({ exit: vi.fn(), relaunch: vi.fn() }))
vi.mock('vue-i18n', () => ({ useI18n: () => ({ t: (key: string) => key }) }))
vi.mock('@/stores/cat', () => ({ useCatStore: () => state.cat }))
vi.mock('@/stores/pair', () => ({ usePairStore: () => state.pair, pairStatusKey: () => 'connected' }))
vi.mock('@/utils/platform', () => ({ isMac: false }))
vi.mock('@/plugins/window', () => ({ showWindow: state.showWindow }))
vi.mock('@/composables/usePairOverlay', () => ({
  setChatVisible: vi.fn(),
  setRemoteCatVisible: state.remoteVisible,
}))

interface Item {
  text: string
  checked?: boolean
  items?: Item[]
  action?: () => void
}

describe('猫咪右键菜单设置隔离', () => {
  beforeEach(() => {
    state.cat.window = { scale: 75, opacity: 50, alwaysOnTop: true, passThrough: false, visible: true }
    state.pair.settings.remoteCat = { scale: 125, opacity: 75, alwaysOnTop: false, passThrough: false, visible: true, showStats: true, syncModel: true }
    state.pair.settings.chat.overlayVisible = undefined
    vi.clearAllMocks()
    state.closes = []
  })

  it('对方猫尺寸、透明度、置顶与穿透只修改对方窗口', async () => {
    const ownBefore = { ...state.cat.window }
    const menu = await useAppMenu().getBaseMenu('remote') as unknown as Item[]
    const find = (suffix: string) => menu.find(item => item.text?.endsWith(suffix))!
    const sizes = find('.windowSize').items!
    expect(sizes.find(item => item.checked)?.text).toBe('125%')
    sizes.find(item => item.text === '150%')!.action!()
    find('.opacity').items!.find(item => item.text === '100%')!.action!()
    find('.alwaysOnTop').action!()
    find('.passThrough').action!()
    expect(state.pair.settings.remoteCat).toMatchObject({ scale: 150, opacity: 100, alwaysOnTop: true, passThrough: true })
    expect(state.cat.window).toEqual(ownBefore)
  })

  it('自己猫聊天浮层默认开启，菜单开关可来回切换', async () => {
    const menu = await useAppMenu().getBaseMenu() as unknown as Item[]
    const overlay = menu.find(item => item.text?.endsWith('.chatOverlay'))!
    expect(overlay.checked).toBe(true)
    overlay.action!()
    expect(state.pair.settings.chat.overlayVisible).toBe(false)
    overlay.action!()
    expect(state.pair.settings.chat.overlayVisible).toBe(true)
  })

  it('对方猫保留偏好入口与隐藏入口，不出现本机猫专属开关', async () => {
    const menu = await useAppMenu().getBaseMenu('remote') as unknown as Item[]
    expect(menu.some(item => item.text?.endsWith('.chatOverlay'))).toBe(false)
    expect(menu.some(item => item.text?.endsWith('.hideCat'))).toBe(false)
    menu.find(item => item.text?.endsWith('.preference'))!.action!()
    expect(state.showWindow).toHaveBeenCalledWith('preference')
    menu.find(item => item.text?.endsWith('.hideRemoteCat'))!.action!()
    expect(state.remoteVisible).toHaveBeenCalledWith(false)
  })

  it('释放整棵菜单资源，包含尺寸和透明度子菜单的独立项目', async () => {
    const resources: MenuResources = []
    const appMenu = useAppMenu(resources)
    await appMenu.getBaseMenu('remote')
    await appMenu.getExitMenu()
    expect(resources.length).toBeGreaterThan(20)
    await closeMenuResources(resources)
    expect(resources).toHaveLength(0)
    for (const close of state.closes) expect(close).toHaveBeenCalledOnce()
  })

  it('构建中途失败后仍等待并释放尚未创建完成的项目', async () => {
    const resources: MenuResources = []
    const close = vi.fn(async () => {})
    let finish!: () => void
    trackMenuResource(new Promise<{ close: typeof close }>((resolve) => {
      finish = () => resolve({ close })
    }), resources)
    trackMenuResource(Promise.reject(new Error('failed')), resources).catch(() => {})
    const cleanup = closeMenuResources(resources)
    expect(close).not.toHaveBeenCalled()
    finish()
    await cleanup
    expect(close).toHaveBeenCalledOnce()
  })
})
