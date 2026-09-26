import { describe, expect, it } from 'vitest'

import { pressKey, releaseKey } from './keyHighlight'

/** 标准模型只有 left-keys 一个键盘贴图目录，键名就是贴图文件名 */
const keyPath = (name: string, group = 'left-keys') => `C:\\model\\resources\\${group}\\${name}.png`

function createState() {
  return {
    displayed: {} as Record<string, string>,
    held: {} as Record<string, string>,
  }
}

describe('键盘高亮的显示与回退', () => {
  it('同一目录只显示一个键，但两个键都算按着', () => {
    const state = createState()

    pressKey(state.displayed, state.held, 'KeyW', keyPath('KeyW'))
    pressKey(state.displayed, state.held, 'KeyE', keyPath('KeyE'))

    expect(Object.keys(state.displayed)).toEqual(['KeyE'])
    expect(Object.keys(state.held)).toEqual(['KeyW', 'KeyE'])
  })

  // 用户报的 bug：按住 w、再按 e、松开 e 之后，w 应该重新亮起来
  it('松开后来的键时回退到仍然按着的那个键', () => {
    const state = createState()

    pressKey(state.displayed, state.held, 'KeyW', keyPath('KeyW'))
    pressKey(state.displayed, state.held, 'KeyE', keyPath('KeyE'))
    releaseKey(state.displayed, state.held, 'KeyE')

    expect(Object.keys(state.displayed)).toEqual(['KeyW'])
    expect(Object.keys(state.held)).toEqual(['KeyW'])
  })

  it('连按多个键时按按下顺序逐级回退', () => {
    const state = createState()

    pressKey(state.displayed, state.held, 'KeyW', keyPath('KeyW'))
    pressKey(state.displayed, state.held, 'KeyE', keyPath('KeyE'))
    pressKey(state.displayed, state.held, 'KeyR', keyPath('KeyR'))

    releaseKey(state.displayed, state.held, 'KeyR')
    expect(Object.keys(state.displayed)).toEqual(['KeyE'])

    releaseKey(state.displayed, state.held, 'KeyE')
    expect(Object.keys(state.displayed)).toEqual(['KeyW'])

    releaseKey(state.displayed, state.held, 'KeyW')
    expect(Object.keys(state.displayed)).toEqual([])
  })

  it('松开的是被顶下去的那个键时，显示的那个键不受影响', () => {
    const state = createState()

    pressKey(state.displayed, state.held, 'KeyW', keyPath('KeyW'))
    pressKey(state.displayed, state.held, 'KeyE', keyPath('KeyE'))
    releaseKey(state.displayed, state.held, 'KeyW')

    expect(Object.keys(state.displayed)).toEqual(['KeyE'])
    expect(Object.keys(state.held)).toEqual(['KeyE'])
  })

  it('不同贴图目录（键盘分区）可以同时显示', () => {
    const state = createState()

    pressKey(state.displayed, state.held, 'KeyW', keyPath('KeyW', 'left-keys'))
    pressKey(state.displayed, state.held, 'ArrowUp', keyPath('ArrowUp', 'right-keys'))

    expect(Object.keys(state.displayed).sort()).toEqual(['ArrowUp', 'KeyW'])

    // 松开其中一个目录里的键，另一个目录不受影响
    releaseKey(state.displayed, state.held, 'ArrowUp')

    expect(Object.keys(state.displayed)).toEqual(['KeyW'])
  })

  it('oS 自动重复不会把显示从后按下的键抢回去', () => {
    const state = createState()

    pressKey(state.displayed, state.held, 'KeyW', keyPath('KeyW'))
    pressKey(state.displayed, state.held, 'KeyE', keyPath('KeyE'))

    // Windows 上按住 w 会一直上报 KeyPress，这些重复事件不算新的按下
    pressKey(state.displayed, state.held, 'KeyW', keyPath('KeyW'))
    pressKey(state.displayed, state.held, 'KeyW', keyPath('KeyW'))

    expect(Object.keys(state.displayed)).toEqual(['KeyE'])
    expect(Object.keys(state.held)).toEqual(['KeyW', 'KeyE'])

    // 松开 e 之后才轮到 w
    releaseKey(state.displayed, state.held, 'KeyE')

    expect(Object.keys(state.displayed)).toEqual(['KeyW'])
  })
})
