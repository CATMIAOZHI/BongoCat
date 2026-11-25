import { describe, expect, it } from 'vitest'

import { applyScaleDragDelta, resolveResizedScale } from './windowScale'

describe('窗口拖拽缩放计算', () => {
  const modelSize = { width: 612, height: 354 }

  it('拖左右边或四角改变宽度时，按宽度反推缩放', () => {
    expect(resolveResizedScale({
      size: { width: 306, height: 354 },
      modelSize,
      currentScale: 100,
    })).toBe(50)
  })

  it('只拖上边或下边（宽度不变、高度变化）时，按高度反推缩放而不弹回', () => {
    expect(resolveResizedScale({
      size: { width: 612, height: 177 },
      modelSize,
      currentScale: 100,
    })).toBe(50)
  })

  it('高分屏宽度存在 1px 量化偏差时，拖上下边仍按高度反推，拖左右边 1px 仍按宽度反推', () => {
    // 55% 缩放理论宽 337px、高 195px；系统量化成 336px 后拖下边到 248px（70%）
    expect(resolveResizedScale({
      size: { width: 336, height: 248 },
      modelSize,
      currentScale: 55,
    })).toBe(70)

    // 仅宽度变化 1px、高度未变时，保持按宽度反推
    expect(resolveResizedScale({
      size: { width: 336, height: 195 },
      modelSize,
      currentScale: 55,
    })).toBe(55)
  })

  it('猫咪窗口带聊天浮层时，拖上下边先扣除浮层物理高度再反推缩放', () => {
    // 50% 缩放：宽 306，模型高 177，叠加 222px 聊天浮层 -> 总高 399
    expect(resolveResizedScale({
      size: { width: 306, height: 576 },
      modelSize,
      currentScale: 50,
      extraHeight: 222,
    })).toBe(100)

    // 拖到比浮层还矮时夹到最小缩放 10%
    expect(resolveResizedScale({
      size: { width: 306, height: 150 },
      modelSize,
      currentScale: 50,
      extraHeight: 222,
    })).toBe(10)
  })

  it('按住 Shift 并右键慢拖放大与缩小都会累计亚像素位移，不会在 1px 缩小时卡住', () => {
    let scale = 44
    let remainder = 0

    for (let index = 0; index < 4; index++) {
      const next = applyScaleDragDelta(scale, -1, 0, remainder)
      scale = next.scale
      remainder = next.remainder
    }

    expect(scale).toBe(42)
    expect(remainder).toBe(0)

    for (let index = 0; index < 3; index++) {
      const next = applyScaleDragDelta(scale, 1, 0, remainder)
      scale = next.scale
      remainder = next.remainder
    }

    expect(scale).toBe(43)
    expect(remainder).toBe(0.5)
  })

  it('触达缩放上下限时夹紧并清空余数', () => {
    expect(applyScaleDragDelta(10, -10, -10, -0.5)).toEqual({ scale: 10, remainder: 0 })
    expect(applyScaleDragDelta(500, 10, 10, 0.5)).toEqual({ scale: 500, remainder: 0 })
  })
})
