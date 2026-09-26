import { isEqual } from 'es-toolkit'

/**
 * `@tauri-store/pinia` 的同步口径：状态一变就发**整份**，而后端是按顶层键整份替换
 * （`tauri-store` 的 `StoreState::patch` 是 `HashMap::extend`），再原样广播给别的窗口。
 *
 * 于是「这个窗口只改了自己本地的状态」（比如每敲一次键就在改的按键高亮）也会把**它那份、
 * 常常已经过时的设置**推给别的窗口，把用户正在拖的滑块、正在打的字盖回去。这个文件里的
 * 判据就是：只有真正要同步的那些键变了才发。
 *
 * 只用于「状态里同时有要同步的设置和窗口本地的运行时数据」的 store；判据只比较
 * 顶层键里去掉 `localKeys` 之后的部分（和 store 的 `filterKeys` 是同一个名单）。
 */

/** 这份状态里真正要同步出去的部分（`localKeys` = 窗口本地、既不发也不落盘的键） */
export function pickSyncedState(
  state: Record<string, unknown>,
  localKeys: readonly string[],
): Record<string, unknown> {
  return Object.fromEntries(
    Object.entries(state).filter(([key]) => !localKeys.includes(key)),
  )
}

/**
 * 这次状态变化要不要发给后端；返回值同时是「后端现在长这样」的新快照。
 *
 * 返回 `undefined` 表示这次什么都不用发。
 *
 * 两边都过一遍同一套 JSON 归一化再比：序列化时 `undefined` 的键（比如「还没选模型」时的
 * `remoteCat.modelId`）本来就发不出去，留着它会让每次比较都不相等，判据也就废了。
 * 快照还必须是**脱离响应式**的副本：直接留着 store 里那个活对象的话，它永远等于当前状态，
 * 等于把设置永久锁住不再保存。
 */
export function nextSyncSnapshot(
  state: Record<string, unknown>,
  lastSynced: Record<string, unknown> | undefined,
  localKeys: readonly string[],
): Record<string, unknown> | undefined {
  const next = JSON.parse(JSON.stringify(pickSyncedState(state, localKeys))) as Record<string, unknown>

  if (lastSynced && isEqual(next, lastSynced)) return void 0

  return next
}

/**
 * 一个 store 的「发之前先看看有没有东西要发」的守卫。
 *
 * `remember` 在收到后端（或别的窗口）发来的状态时调用，记下「后端现在长这样」；
 * `sync` 直接交给 store 的 `beforeBackendSync`——返回 `undefined` 会中止这次同步。
 *
 * 每个窗口一份（模块级），和 store 实例一样是窗口本地的。
 */
export function createBackendSyncGuard(localKeys: readonly string[]) {
  let lastSynced: Record<string, unknown> | undefined

  return {
    remember(state: Record<string, unknown>) {
      lastSynced = nextSyncSnapshot(state, void 0, localKeys)
    },
    sync(state: Record<string, unknown>): Record<string, unknown> | undefined {
      const snapshot = nextSyncSnapshot(state, lastSynced, localKeys)

      if (!snapshot) return void 0

      lastSynced = snapshot

      return state
    },
  }
}
