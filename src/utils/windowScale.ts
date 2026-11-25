import { round } from 'es-toolkit'

export const MIN_WINDOW_SCALE = 10
export const MAX_WINDOW_SCALE = 500

export function clampWindowScale(scale: number): number {
  return Math.max(MIN_WINDOW_SCALE, Math.min(MAX_WINDOW_SCALE, round(scale)))
}

export interface ResizedScaleOptions {
  size: { width: number, height: number }
  modelSize: { width: number, height: number }
  currentScale: number
  /** 叠加在模型区之外的固定物理高度（例如猫咪窗口顶部的聊天浮层） */
  extraHeight?: number
}

/**
 * 从窗口拖拽后的物理尺寸反推缩放百分比。
 *
 * 拖左右边或四角时宽度会变，按宽度反推；只拖上边或下边时宽度不变、只有高度在变，
 * 此时改按高度反推，避免松手后被旧宽度弹回原尺寸。
 */
export function resolveResizedScale(options: ResizedScaleOptions): number {
  const { size, modelSize, currentScale, extraHeight = 0 } = options
  const factor = currentScale / 100
  const expectedWidth = Math.round(modelSize.width * factor)
  const expectedHeight = Math.round(modelSize.height * factor + extraHeight)
  const widthDelta = Math.abs(size.width - expectedWidth)
  const heightDelta = Math.abs(size.height - expectedHeight)

  if (widthDelta <= 1 && heightDelta > widthDelta && modelSize.height > 0) {
    const modelHeight = Math.max(0, size.height - extraHeight)

    return clampWindowScale((modelHeight / modelSize.height) * 100)
  }

  return clampWindowScale((size.width / modelSize.width) * 100)
}

/**
 * Shift + 右键拖动缩放时累计亚像素位移，避免每次 1px 慢拖缩小时被四舍五入吞掉。
 */
export function applyScaleDragDelta(
  currentScale: number,
  movementX: number,
  movementY: number,
  remainder: number,
): { scale: number, remainder: number } {
  const raw = remainder + (movementX + movementY) * 0.5
  const step = Math.trunc(raw)
  const nextRemainder = raw - step

  if (step === 0) {
    return { scale: currentScale, remainder: nextRemainder }
  }

  const nextScale = clampWindowScale(currentScale + step)
  const clamped = nextScale === MIN_WINDOW_SCALE || nextScale === MAX_WINDOW_SCALE

  return {
    scale: nextScale,
    remainder: clamped ? 0 : nextRemainder,
  }
}
