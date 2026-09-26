/**
 * 「还按着东西」时该不该补发一帧（纯逻辑，见 `keepAlive.spec.ts`）。
 *
 * 为什么要补：对方猫的按键/爪子 TTL 看的是**收到包的时间**（800ms），而按住键不动时
 * 快照没有任何变化（R4：量化后一样就不发），对面就会自己把贴图与爪子放下来。
 */

export interface KeepAliveCheck {
  /** 本机快照里现在按着的键 */
  keys: readonly string[]
  leftDown: boolean
  rightDown: boolean
  /** 距离上一次真的发出去过了多久（毫秒） */
  sinceLastSentMs: number
  /** 两次补发之间至少隔多久（毫秒） */
  minIntervalMs: number
  /** 本窗口的页面现在可不可见 */
  pageVisible: boolean
}

export function shouldSendKeepAlive(check: KeepAliveCheck) {
  // 判据只用「对方猫真的会画出来的东西」：贴图看 keys、爪子看指针左右键。快照里还有
  // 「本机模型能显示、对端不显示」的键（方向键、媒体键…），用它们当判据只会白发帧
  if (check.keys.length === 0 && !check.leftDown && !check.rightDown) return false

  // 窗口藏起来时页面会被 Chromium 冻结，这个 50ms 的节拍被夹到 ≥1 秒：补发就成了
  // 「新鲜 800ms + 断 200ms」的每秒一闪，比不补还难看。藏着自己看不见猫，也就不必补，
  // 回到改动前「稳定放下」的表现
  if (!check.pageVisible) return false

  return check.sinceLastSentMs >= check.minIntervalMs
}
