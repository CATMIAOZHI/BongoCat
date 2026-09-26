import type { ComputedRef, Ref } from 'vue'

import { computed, ref, watch } from 'vue'

/**
 * pair 设置里单个字符串项的「草稿 + 保存」。
 *
 * 为什么必须用草稿：pair store 在几个窗口之间同步，而同步是**整份状态**——任何窗口只要
 * 改了自己那份里的任何东西，就会把自己的整份 `settings` patch 给后端。猫咪窗口恰恰每
 * 敲一次键、每点一下鼠标都在改统计（`stats`），于是「用户在偏好页打字」这条路上，每个
 * 按键都会有一帧**带着旧值**的状态发出去，并且通常会盖在刚打的字后面。直接 `v-model`
 * 绑到 store 上时，表现就是「输入框里的字会闪、丢字、打起来卡」（R40 / R45）。
 *
 * 于是这个机制有三段：
 *
 * 1. 输入框只认草稿，点「保存」才写进 store；
 * 2. 草稿一旦被用户改过就不再被 store 回填（`owned`）——不然异步载入和别的窗口的旧值
 *    会把正在打的字换掉；
 * 3. 保存后再守一小段时间：带旧值的那一帧常常比保存帧**晚**到后端，「谁最后发谁赢」
 *    会把刚保存的值顶掉，这段时间里发现被盖回来就再写一次。
 */
export interface PairSettingDraft {
  /** 输入框绑这个 */
  input: Ref<string>
  /** 草稿与 store 不一致：用来置灰「保存」并提示还没生效 */
  dirty: ComputedRef<boolean>
  /** 把草稿写进 store（先过 `normalize`） */
  save: () => void
}

/**
 * 保存后的守护时长。
 *
 * 人一停手，猫咪窗口就不再发帧了，所以旧值只会在这之后的一小段里到达；2000ms 够覆盖
 * 一次正常的 IPC + 同步往返（偏好页那边实测是几毫秒级）。
 */
const GUARD_MS = 2000

export function usePairSettingDraft(
  read: () => string,
  write: (value: string) => void,
  normalize: (value: string) => string = value => value,
): PairSettingDraft {
  const input = ref(read())
  /**
   * 草稿是否已经「归草稿所有」：回填只在这一位还是 false 时发生。
   *
   * 一次回填之后就置 true——包含初次从 store 载入的那一跳（那一跳草稿与 store 同值，
   * 所以判断的是「草稿变过」而不是「用户动过」）。之后一律以草稿为准。
   */
  const owned = ref(false)
  /** 刚保存的值；守护窗口里它被别的窗口盖回来就再写一次（见下面第二个 watch） */
  let saved = ''
  let guardUntil = 0

  /** 还没被用户碰过时才回填（store 是异步载入的，落在组件挂载之后） */
  watch(read, (value) => {
    if (owned.value) return

    input.value = value
  })

  watch(input, () => {
    owned.value = true
  })

  /** 保存后被盖回来就再写一次；写回同一个值不会再触发自己（下一轮 value 已经相等） */
  watch(read, (value) => {
    if (Date.now() > guardUntil) return
    if (input.value !== saved) return
    if (value === saved) return

    write(saved)
  })

  const save = () => {
    saved = normalize(input.value)
    guardUntil = Date.now() + GUARD_MS

    // 顺手把归一化结果落回草稿（例如去掉首尾空格），用户看到的就是真正会生效的值
    input.value = saved
    write(saved)
  }

  return {
    input,
    dirty: computed(() => input.value !== read()),
    save,
  }
}
