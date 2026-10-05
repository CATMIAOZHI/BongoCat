<script setup lang="ts">
import { convertFileSrc } from '@tauri-apps/api/core'
import { computed, onUnmounted, useTemplateRef, watch } from 'vue'

import type { Model } from '@/stores/model'

import { useCatStore } from '@/stores/cat'
import { useModelStore } from '@/stores/model'
import { extractKeyHighlight } from '@/utils/keyHighlightMask'

const props = defineProps<{ model?: Model }>()
const cat = useCatStore()
const keys = useModelStore()
const canvas = useTemplateRef<HTMLCanvasElement>('canvas')
const enabled = computed(() => cat.model.highlightAllHeldKeys !== false
  && props.model?.isPreset && ['standard', 'keyboard'].includes(props.model.mode))
const paths = computed(() => enabled.value
  ? Object.entries(keys.heldKeys).filter(([key]) => !keys.pressedKeys[key]).map(([, path]) => path)
  : [])

async function loadMask(path: string) {
  const image = new Image()
  image.crossOrigin = 'anonymous'
  image.src = convertFileSrc(path)
  await image.decode()
  const work = document.createElement('canvas')
  work.width = image.naturalWidth
  work.height = image.naturalHeight
  const context = work.getContext('2d', { willReadFrequently: true })!
  context.drawImage(image, 0, 0)
  const mask = extractKeyHighlight(context.getImageData(0, 0, work.width, work.height).data, work.width, work.height)
  if (!mask) return null
  // 缓存只保留小块键位，避免每个按键占用一整张 640×360 位图。
  const crop = document.createElement('canvas')
  crop.width = mask.width
  crop.height = mask.height
  crop.getContext('2d')!.putImageData(new ImageData(mask.pixels, mask.width, mask.height), 0, 0)
  return { crop, left: mask.left, top: mask.top, width: work.width, height: work.height }
}

const cache = new Map<string, ReturnType<typeof loadMask>>()
let revision = 0
watch(() => props.model?.path, () => cache.clear())
watch([paths, canvas, () => props.model?.path], async () => {
  const version = ++revision
  const target = canvas.value
  if (!target) return
  const context = target.getContext('2d')!
  context.clearRect(0, 0, target.width, target.height)
  const masks = await Promise.all(paths.value.map((path) => {
    if (!cache.has(path)) cache.set(path, loadMask(path).catch(() => null))
    return cache.get(path)!
  }))
  if (version !== revision) return
  const first = masks.find(mask => mask !== null)
  if (!first) return
  target.width = first.width
  target.height = first.height
  for (const mask of masks) {
    if (mask) context.drawImage(mask.crop, mask.left, mask.top)
  }
}, { immediate: true, flush: 'post' })
onUnmounted(() => {
  revision++
  cache.clear()
})
</script>

<template>
  <canvas
    ref="canvas"
    aria-hidden="true"
    class="pointer-events-none absolute size-full object-cover"
  />
</template>
