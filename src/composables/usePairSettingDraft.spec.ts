import { describe, expect, it, vi } from 'vitest'
import { nextTick, ref } from 'vue'

import { usePairSettingDraft } from './usePairSettingDraft'

/** 造一个「store 里的字符串字段」：读走 read，写走 write */
function createDraft(initial: string, normalize?: (value: string) => string) {
  const store = ref(initial)
  const draft = usePairSettingDraft(
    () => store.value,
    (value) => {
      store.value = value
    },
    normalize,
  )

  return { store, draft }
}

describe('pair 设置项的草稿机制', () => {
  it('store 异步载入时回填草稿', async () => {
    const { store, draft } = createDraft('')

    store.value = '盘上的值'
    await nextTick()

    expect(draft.input.value).toBe('盘上的值')
    expect(draft.dirty.value).toBe(false)
  })

  it('用户改过之后，别的窗口带旧值的整份状态再也盖不动草稿', async () => {
    const { store, draft } = createDraft('旧值')

    draft.input.value = '我正在打字'
    await nextTick()

    // 猫咪窗口每敲一次键就会把带旧值的那份 settings 发给后端。
    // 这里必须是**与 store 当前值不同**的字符串：写回同一个值时 Vue 的 ref 判等会让这次
    // 赋值压根不触发 watch，用例就变成空过（回填那条 watch 也就不会被检验到）。
    store.value = '别处的旧值'
    await nextTick()

    expect(draft.input.value).toBe('我正在打字')
    expect(draft.dirty.value).toBe(true)
  })

  it('保存才写进 store，并且先过归一化', async () => {
    const { store, draft } = createDraft('', value => value.trim().slice(0, 3))

    draft.input.value = '   小猫咪   '
    await nextTick()
    expect(store.value).toBe('')

    draft.save()
    await nextTick()

    expect(store.value).toBe('小猫咪')
    expect(draft.input.value).toBe('小猫咪')
    expect(draft.dirty.value).toBe(false)
  })

  it('保存后晚到的旧值会被写回去', async () => {
    const { store, draft } = createDraft('旧值')

    draft.input.value = '新名字'
    await nextTick()
    draft.save()
    await nextTick()
    expect(store.value).toBe('新名字')

    // 这一帧比我们的保存帧晚到，谁最后发谁赢
    store.value = '旧值'
    await nextTick()

    expect(store.value).toBe('新名字')
  })

  it('守护窗口里用户又改了：让位，不把旧草稿写回去', async () => {
    const { store, draft } = createDraft('旧值')

    draft.input.value = '第一次'
    await nextTick()
    draft.save()
    await nextTick()

    draft.input.value = '第二次'
    await nextTick()

    store.value = '旧值'
    await nextTick()

    expect(store.value).toBe('旧值')
    expect(draft.input.value).toBe('第二次')
  })

  it('守护窗口过去之后不再往回写', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(0)

    try {
      const { store, draft } = createDraft('旧值')

      draft.input.value = '新名字'
      await nextTick()
      draft.save()
      await nextTick()

      vi.setSystemTime(3000)
      store.value = '旧值'
      await nextTick()

      expect(store.value).toBe('旧值')
    } finally {
      vi.useRealTimers()
    }
  })
})
