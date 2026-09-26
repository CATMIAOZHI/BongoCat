import type { Model, ModelMode } from '@/stores/model'

/** 只传跨设备可识别的名称，不传本机路径、随机 ID 或模型文件。 */
export interface PairModelIdentity {
  name: string
  mode: ModelMode
  isPreset: boolean
}

export function modelIdentity(model?: Model): PairModelIdentity | undefined {
  if (!model) return

  const name = model.isPreset
    ? model.mode
    : model.name?.trim() ?? ''

  return { name, mode: model.mode, isPreset: model.isPreset }
}

export function matchPeerModel(models: Model[], peer?: PairModelIdentity): Model | undefined {
  if (!peer?.name) return

  const matches = models.filter((model) => {
    const identity = modelIdentity(model)!

    return identity.isPreset === peer.isPreset
      && identity.mode === peer.mode
      && identity.name === peer.name
  })

  // 同名目录有多份时不猜，避免显示错误的模型。
  return matches.length === 1 ? matches[0] : undefined
}
