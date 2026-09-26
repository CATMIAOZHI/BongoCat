import { computed } from 'vue'

import { useModelStore } from '@/stores/model'
import { usePairStore } from '@/stores/pair'
import { matchPeerModel } from '@/utils/pairModel'

export function usePeerModel() {
  const models = useModelStore()
  const pair = usePairStore()
  const automatic = computed(() => pair.settings.remoteCat.syncModel !== false)
  const peer = computed(() => pair.runtime.peerOnline ? pair.runtime.peerModel : undefined)
  const matched = computed(() => matchPeerModel(models.models, peer.value))
  const missing = computed(() => automatic.value && !!peer.value && !matched.value)
  const selected = computed(() => {
    if (automatic.value) return matched.value ?? models.currentModel

    return models.models.find(model => model.id === pair.settings.remoteCat.modelId) ?? models.currentModel
  })

  return { automatic, peer, missing, selected }
}
