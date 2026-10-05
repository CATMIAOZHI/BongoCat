import { describe, expect, it } from 'vitest'

import { extractKeyHighlight } from './keyHighlightMask'

describe('内置按键高亮分层', () => {
  it('移除灰白爪子、透明像素，保留青色高亮和半透明边缘并裁剪', () => {
    const rgba = new Uint8ClampedArray([
      255,
      255,
      255,
      255,
      0,
      255,
      255,
      128,
      0,
      255,
      255,
      0,
      30,
      30,
      30,
      255,
      10,
      160,
      180,
      255,
      220,
      220,
      220,
      255,
    ])
    const result = extractKeyHighlight(rgba, 3, 2)!
    expect([result.left, result.top, result.width, result.height]).toEqual([1, 0, 1, 2])
    expect([...result.pixels]).toEqual([0, 255, 255, 128, 10, 160, 180, 255])
    expect([...rgba.slice(0, 4)]).toEqual([255, 255, 255, 255])
  })

  it('没有青色键位时不生成额外贴图', () => {
    expect(extractKeyHighlight(new Uint8ClampedArray([255, 255, 255, 255]), 1, 1)).toBeNull()
  })

  it('裁剪框内的爪子仍然透明', () => {
    const result = extractKeyHighlight(new Uint8ClampedArray([
      0,
      255,
      255,
      255,
      255,
      255,
      255,
      255,
      0,
      255,
      255,
      255,
    ]), 3, 1)!
    expect([...result.pixels.slice(4, 8)]).toEqual([0, 0, 0, 0])
  })
})
