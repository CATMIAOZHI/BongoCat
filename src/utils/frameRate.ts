/** 0 表示不限帧；输入框暂时清空或旧设置非法时使用默认值。 */
export function normalizeMaxFPS(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : 60
}

/** 远端快照最多 60Hz；低于它的用户设置也必须限制参数插值。 */
export function remoteFrameInterval(value: unknown): number {
  return 1000 / Math.min(normalizeMaxFPS(value) || 60, 60)
}
