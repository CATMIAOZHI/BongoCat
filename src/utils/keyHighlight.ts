/**
 * 键盘高亮的两层状态（纯函数，见 `keyHighlight.spec.ts`）。
 *
 * 模型同一时刻只能显示**一张**键盘贴图（每个键都是一张「整块键盘 + 只亮一个键」的图），
 * 所以「显示」这一层每个贴图目录只留一个键；而「按着」这一层必须记住全部，
 * 否则会出现用户报过的那种情况：先按住 w、再按 e、松开 e 之后手上什么都没有，
 * 而 w 其实一直按着。
 *
 * 抽成不依赖 Vue / Tauri 的纯函数，是为了能直接单测这段回退逻辑。
 */

/**
 * 贴图路径的倒数第二段就是贴图目录：`…/resources/left-keys/KeyW.png` → `left-keys`。
 *
 * 这里不 import `@tauri-apps/api/path` 的 `sep()`，那样会把这个纯函数绑到 Tauri 上
 * 而没法在 vitest（node 环境）里直接跑；两种分隔符都认。
 */
export function textureGroup(path: string): string {
  const parts = path.split(/[/\\]/)

  return parts[parts.length - 2] ?? ''
}

/**
 * 按下：把同一个贴图目录里正在显示的那个键从**显示**里换下来，但保留它的「按着」记录。
 *
 * 已经按着的键再来一次按下（Windows 的 OS 自动重复会一直上报 `KeyPress`）不算新的按下：
 * 不做这一步的话，按住 w 再按 e 时，w 的重复事件每几十毫秒就会把显示抢回 w，
 * 看起来就像「e 没按上」。
 */
export function pressKey(
  displayed: Record<string, string>,
  held: Record<string, string>,
  key: string,
  path: string,
) {
  if (held[key]) return

  const group = textureGroup(path)

  for (const name of Object.keys(displayed)) {
    if (textureGroup(displayed[name]) === group) delete displayed[name]
  }

  held[key] = path
  displayed[key] = path
}

/**
 * 松开：如果松开的正好是显示中的键，回退到同目录里**最后按下**的那个（`held` 的插入顺序）。
 *
 * 没在显示的键（同目录里更早按下的那些）只需要从 `held` 里去掉，显示不受影响。
 */
export function releaseKey(
  displayed: Record<string, string>,
  held: Record<string, string>,
  key: string,
) {
  const released = displayed[key]

  delete held[key]

  if (!released) return

  delete displayed[key]

  const group = textureGroup(released)

  for (const name of Object.keys(held).reverse()) {
    const path = held[name]

    if (textureGroup(path) !== group) continue

    displayed[name] = path

    break
  }
}
