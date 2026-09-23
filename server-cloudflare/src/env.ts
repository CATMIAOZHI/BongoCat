import type { PairDurableObject } from './pair'

export interface Env {
  PAIR: DurableObjectNamespace<PairDurableObject>
  PAIR_AUTH_TOKEN: string
}
