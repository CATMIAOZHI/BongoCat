<script setup lang="ts">
import { emit } from '@tauri-apps/api/event'
import { writeText } from '@tauri-apps/plugin-clipboard-manager'
import { save } from '@tauri-apps/plugin-dialog'
import { useDebounceFn } from '@vueuse/core'
import { Alert, Badge, Button, Flex, Input, InputNumber, message, Modal, Select, Slider, Switch, Tag } from 'antdv-next'
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'

import type { ExportFormat, HistoryStats } from '@/composables/usePair'

import ProListItem from '@/components/pro-list-item/index.vue'
import ProList from '@/components/pro-list/index.vue'
import {
  ATTACHMENT_MAX_MB,
  pairConnect,
  pairDeleteSecret,
  pairDeleteServerPassword,
  pairDisconnect,
  pairGenerateSecret,
  pairGetSecret,
  pairGetSecretFingerprint,
  pairGetServerPassword,
  pairHasSecret,
  pairHasServerPassword,
  pairHistoryExport,
  pairHistoryStartNewEpoch,
  pairHistoryStats,
  pairSetMaxAttachmentMb,
  pairSetSecret,
  pairSetServerPassword,
} from '@/composables/usePair'
import { chatExportFileName } from '@/composables/usePairChat'
import { playPairMessageSound } from '@/composables/usePairMessageSound'
import { setChatVisible, setRemoteCatVisible } from '@/composables/usePairOverlay'
import { useTauriListen } from '@/composables/useTauriListen'
import { LISTEN_KEY } from '@/constants'
import { useModelStore } from '@/stores/model'
import { pairStatusKey, usePairStore } from '@/stores/pair'
import { usePairStatsStore } from '@/stores/pairStats'

const pairStore = usePairStore()
const pairStatsStore = usePairStatsStore()
const modelStore = useModelStore()
const { t } = useI18n()
const secretInput = ref('')
const serverPasswordInput = ref('')

/**
 * R45：两个密码和服务器地址一样一直明文显示在框里（用户要求）。
 *
 * 这两个是凭据库里**已保存**的值；框里是草稿，和它不一样时「保存」才可点，并提示还没生效。
 */
const savedSecret = ref('')
const savedServerPassword = ref('')
const secretDirty = computed(() => secretInput.value.trim() !== savedSecret.value)
const serverPasswordDirty = computed(() => serverPasswordInput.value.trim() !== savedServerPassword.value)

/**
 * 从凭据库读回两个值，填进框里（打开页面时，以及保存之后拿 Rust 规范化过的那串）。
 *
 * `written` = 刚刚写进去的值：保存已经成功，只是读回失败时就把它当成已保存值，
 * 别把一次成功的保存报成失败、也别让框里挂着「改过了，点保存才会生效」。
 */
async function loadSavedSecret(written?: string) {
  try {
    savedSecret.value = (await pairGetSecret()) ?? ''
  } catch (reason) {
    if (written === void 0) throw reason

    savedSecret.value = written
  }

  secretInput.value = savedSecret.value
}

async function loadSavedServerPassword(written?: string) {
  try {
    savedServerPassword.value = (await pairGetServerPassword()) ?? ''
  } catch (reason) {
    if (written === void 0) throw reason

    savedServerPassword.value = written
  }

  serverPasswordInput.value = savedServerPassword.value
}

/**
 * 服务器地址在输入框里是**草稿**（R40）。
 *
 * 以前直接用 `v-model` 绑在设置上：每敲一个字就写一次设置并落盘——粘贴长地址或用输入法
 * 时会丢字符、光标乱跳。现在和两个密码一样：先编辑，点「保存」才生效。
 */
const relayUrlInput = ref(pairStore.settings.relay.url)
const savingRelayUrl = ref(false)

/** 只有首尾空白和结尾的 `/` 会被 Rust 当成同一个地址，比较口径跟着它走 */
function normalizeRelayUrl(value?: string) {
  return (value ?? '').trim().replace(/\/+$/, '')
}

/** 草稿和已保存的不一样：保存按钮才可点，并且提示「还没生效」 */
const relayUrlDirty = computed(
  () => normalizeRelayUrl(relayUrlInput.value) !== normalizeRelayUrl(pairStore.settings.relay.url),
)

/** 地址被清空了：这时保存按不动，也不该让它连出「框里是空的、用的却是旧地址」 */
const relayUrlEmpty = computed(() => relayUrlInput.value.trim() === '')

/**
 * 设置里的地址变了就回填输入框（落盘值到得比组件晚——`@tauri-store/pinia` 是异步载入的），
 * 但用户正在编辑草稿时不打断他：草稿还等于上一个值，才算「没动过」。
 */
watch(() => pairStore.settings.relay.url, (value, previous) => {
  if (relayUrlInput.value === previous) relayUrlInput.value = value
})

const saving = ref(false)
const savingServerPassword = ref(false)
const connecting = ref(false)
const generating = ref(false)

/**
 * §23 的明文提醒只在「它说的就是当前这个地址」时显示。
 *
 * Rust 侧只在连接时按当次地址算 `plaintext`，改地址不会把它清掉——直接绑上去会变成
 * 「填了域名、却还挂着一条说 IP 是明文的提醒」。
 *
 * 比对的是**输入框里的地址**（也就是点「立即连接」真正会用的那个），草稿为空才退回已保存值：
 * R40 把地址改成草稿之后，只跟已保存值比会让「粘上一个 http:// 地址」要等到保存才提醒。
 */
const plaintextServerWarning = computed(() => {
  if (!pairStore.runtime.plaintext) return false

  const inUse = normalizeRelayUrl(pairStore.runtime.relayUrl)
  const shown = normalizeRelayUrl(relayUrlInput.value) || normalizeRelayUrl(pairStore.settings.relay.url)

  return inUse !== '' && inUse === shown
})

/**
 * R40：错误详情里那一行地址。
 *
 * 优先用**这次连接真正用的**地址（`runtime.relayUrl` 由 Rust 在连接时回填），没有再退回
 * 设置里保存的那个——两个都没有时说明这台设备还没连过，如实说「没有」比空着好。
 */
const lastErrorAddress = computed(() => {
  return normalizeRelayUrl(pairStore.runtime.relayUrl)
    || normalizeRelayUrl(pairStore.settings.relay.url)
    || t('pages.preference.pair.hints.lastErrorNoAddress')
})

/**
 * 凭据库里有没有配对密码，**这次没读出来**。
 *
 * 读失败时 `hasSecret` 会停在 false，界面据此说「还没有配置配对密码」并挡住
 * 「立即连接」——可密码其实在，只是这次没读到。这个时候不替用户下结论，
 * 让他点下去看 Rust 的真实报错。
 */
const secretUnknown = ref(false)

/**
 * 「立即连接」还缺什么（R44）。
 *
 * 以前只要打开了联机总开关就能点，缺地址或缺配对密码都要等 Rust 报错才知道——
 * 那两个错误（「服务器地址不能为空」「还没有配置配对密码」）界面上本来就能看出来。
 */
const connectMissing = computed(() => {
  const url = normalizeRelayUrl(relayUrlInput.value) || normalizeRelayUrl(pairStore.settings.relay.url)

  if (!url) return 'pages.preference.pair.hints.missingRelayUrl'
  if (!secretInput.value.trim() && !pairStore.hasSecret && !secretUnknown.value) {
    return 'pages.preference.pair.hints.missingSecret'
  }

  return ''
})
const exportFormat = ref<ExportFormat>('json')
const exporting = ref(false)
const historyStats = ref<HistoryStats>({ epoch: 1, current: 0, total: 0 })

/** §36：保存上限只用来提示进度，达到上限也不会自动删消息 */
const historyLimit = computed(() => {
  const value = Math.round(Number(pairStore.settings.chat.historyMaxMessages))

  return Math.max(1, Number.isFinite(value) ? value : 50_000)
})

const historyPercent = computed(() => {
  return Math.min(100, Math.round(historyStats.value.current / historyLimit.value * 100))
})

const historyWarning = computed(() => {
  if (historyStats.value.current >= historyLimit.value) return 'full'
  if (historyStats.value.current >= historyLimit.value * 0.9) return 'near'

  return ''
})

const exportFormatOptions = computed(() => {
  return (['json', 'txt', 'md'] as const).map(format => ({
    label: t(`pages.preference.pair.options.exportFormat.${format}`),
    value: format,
  }))
})

const exportFilters = computed(() => {
  const name = {
    json: 'JSON',
    txt: 'Text',
    md: 'Markdown',
  }[exportFormat.value]

  return [{ name, extensions: [exportFormat.value] }]
})

const refreshHistoryStatsLater = useDebounceFn(refreshHistoryStats, 2000)

async function refreshHistoryStats() {
  try {
    historyStats.value = await pairHistoryStats()
  } catch (reason) {
    message.error(String(reason))
  }
}

/** 对方发来的消息也要反映到「保存进度」上，聊天时 2 秒刷新一次就够了 */
useTauriListen(LISTEN_KEY.PAIR_MESSAGE_RECEIVED, refreshHistoryStatsLater)

/** §42：附件上限存在偏好里，改完同步给 Rust 侧（发送与接收用的是同一份限制） */
function applyAttachmentLimit(mb: number) {
  pairSetMaxAttachmentMb(mb).catch((reason) => {
    message.error(String(reason))
  })
}

/** 数字框每敲一下都会改值，别每次都 invoke */
const applyAttachmentLimitLater = useDebounceFn(applyAttachmentLimit, 400)

watch(() => pairStore.settings.chat.attachmentMaxMb, (value) => {
  applyAttachmentLimitLater(Number(value))
})

function pickExportPath() {
  return save({
    defaultPath: chatExportFileName(exportFormat.value, Date.now()),
    filters: exportFilters.value,
  })
}

async function handleExportHistory() {
  exporting.value = true

  try {
    const path = await pickExportPath()

    if (!path) return

    const summary = await pairHistoryExport(exportFormat.value, path)

    message.success(t('pages.preference.pair.hints.chatExported', { count: summary.messages }))

    await refreshHistoryStats()
  } catch (reason) {
    message.error(String(reason))
  } finally {
    exporting.value = false
  }
}

/**
 * §36：导出并开始新的记录周期。
 *
 * 先把当前周期导出成文件，导出成功后周期号才 +1；旧记录保留还是删除由用户在
 * 「删除旧记录」开关里决定，任何情况下都不会静默删掉聊天记录。
 */
function handleStartNewEpoch() {
  Modal.confirm({
    title: t('pages.preference.pair.hints.newEpochTitle'),
    content: pairStore.settings.chat.deleteOldOnReset
      ? t('pages.preference.pair.hints.newEpochDeleteOld')
      : t('pages.preference.pair.hints.newEpochKeepOld'),
    okText: t('pages.preference.pair.buttons.exportAndReset'),
    async onOk() {
      try {
        const path = await pickExportPath()

        if (!path) {
          message.info(t('pages.preference.pair.hints.exportCanceled'))

          return
        }

        const summary = await pairHistoryExport(exportFormat.value, path)
        const epoch = await pairHistoryStartNewEpoch(pairStore.settings.chat.deleteOldOnReset)

        message.success(t('pages.preference.pair.hints.newEpochDone', {
          epoch,
          count: summary.messages,
        }))

        await emit(LISTEN_KEY.CHAT_HISTORY_RESET)
        await refreshHistoryStats()
      } catch (reason) {
        message.error(String(reason))
      }
    },
  })
}

// 连接状态由偏好窗口根组件统一订阅（§49 / §50），secret 的「存过没有」只存在于
// 系统凭据库，页面重新挂载时要重新拉一次，否则重启后看不到「已配置」与删除入口
onMounted(async () => {
  // 「存过没有」决定「已配置」标记与删除入口，指纹只用于核对，所以两者分开处理：
  // 指纹读不出来时不要把前者一起吞掉，否则用户会以为自己的 secret 丢了
  await pairHasSecret()
    .then((hasSecret) => {
      pairStore.hasSecret = hasSecret
      secretUnknown.value = false
    })
    .catch((reason) => {
      // 读失败 ≠ 没配过：别让界面据此说「还没有配置配对密码」（见 `connectMissing`）
      secretUnknown.value = true
      message.error(String(reason))
    })

  await pairGetSecretFingerprint()
    .then((fingerprint) => {
      pairStore.secretFingerprint = fingerprint ?? ''
    })
    .catch((reason) => {
      message.error(String(reason))
    })

  // R45：把已保存的两个值填回框里（读失败时框留空，「已配置」标记照旧由上面那次判断给）
  await loadSavedSecret().catch((reason) => {
    message.error(String(reason))
  })

  await loadSavedServerPassword().catch((reason) => {
    message.error(String(reason))
  })

  // 服务器密码同理：它决定「已配置」标记与删除入口，与配对密码各存一个条目
  await pairHasServerPassword()
    .then((hasServerPassword) => {
      pairStore.hasServerPassword = hasServerPassword
    })
    .catch((reason) => {
      message.error(String(reason))
    })

  await refreshHistoryStats()

  // 重启后 Rust 侧回到默认上限，把用户设置补回去
  applyAttachmentLimit(pairStore.settings.chat.attachmentMaxMb)
})

const status = computed(() => {
  const key = pairStatusKey(pairStore.runtime.connection)

  const color = {
    connected: 'success',
    peerOffline: 'warning',
    connecting: 'processing',
    error: 'error',
  }[key] ?? 'default'

  return { key, color } as { key: ReturnType<typeof pairStatusKey>, color: 'success' | 'warning' | 'processing' | 'error' | 'default' }
})

/** P2P 这条腿只做显示（R28）：它掉了不影响聊天、附件、语音，所以这里没有按钮 */
const p2pStatus = computed(() => {
  const key = pairStore.runtime.p2p

  const color = {
    connected: 'success',
    connecting: 'processing',
    failed: 'warning',
  }[key] ?? 'default'

  return { key, color } as { key: typeof key, color: 'success' | 'processing' | 'warning' | 'default' }
})

const modelOptions = computed(() => {
  return modelStore.models.map((model) => {
    const current = model.id === modelStore.currentModel?.id

    return {
      label: current ? `${model.mode} ✓` : model.mode,
      value: model.id,
    }
  })
})

const statusText = computed(() => `pages.preference.pair.status.${status.value.key}`)

/**
 * 换了任一凭据之后，旧连接上用的凭据已经失效。
 *
 * - 本来连着（或正在重连）：用新值重连一次。
 * - 停在「连接失败」：只清掉那条旧错误并提示再点一次。不清的话用户会以为新值没生效
 *   ——状态徽标和红字都还是上一次的（只读审计的 P2-1）。
 * - 没连接过：什么都不做，等用户自己点「立即连接」。
 */
async function reconnectAfterCredentialChange() {
  const { connection } = pairStore.runtime
  const url = pairStore.settings.relay.url

  if (!url) return

  if (connection === 'error') {
    pairStore.runtime.lastError = void 0
    message.info(t('pages.preference.pair.hints.retryAfterSave'))

    return
  }

  const wasConnected = ['connecting', 'connected', 'peer-offline', 'reconnecting'].includes(connection)

  if (!wasConnected) return

  pairStore.runtime.lastError = void 0

  await pairDisconnect()
  await pairConnect(url)
}

/**
 * 保存服务器地址（R40）。
 *
 * 与两个密码一样：保存才写进设置并落盘，保存成功后按新地址重连一次——换了地址就是换了
 * 一台服务器，旧连接必然失效。
 */
async function handleSaveRelayUrl() {
  const url = normalizeRelayUrl(relayUrlInput.value)

  // 连按回车会重入：按钮有 loading 挡着，键盘没有，两次「断开 → 重连」会交错
  if (!url || savingRelayUrl.value) return

  savingRelayUrl.value = true

  try {
    relayUrlInput.value = url
    pairStore.settings.relay.url = url

    message.success(t('pages.preference.pair.hints.relayUrlSaved'))

    await reconnectAfterCredentialChange()
  } catch (reason) {
    message.error(String(reason))
  } finally {
    savingRelayUrl.value = false
  }
}

async function handleSaveSecret() {
  const secret = secretInput.value.trim()

  // 连按回车会重入：按钮有 loading 挡着，键盘没有，两次「断开 → 重连」会交错
  if (!secret || !secretDirty.value || saving.value) return

  saving.value = true

  try {
    pairStore.secretFingerprint = await pairSetSecret(secret)
    pairStore.hasSecret = true
    secretUnknown.value = false
    // Rust 存的是规范化后的那串（base64url 无填充），读回来显示，框里和凭据库保持一致
    await loadSavedSecret(secret)

    message.success(t('pages.preference.pair.hints.secretSaved'))

    await reconnectAfterCredentialChange()
  } catch (reason) {
    message.error(String(reason))
  } finally {
    saving.value = false
  }
}

/**
 * §22：密钥由 Rust 用系统 CSPRNG 生成，前端把它填进输入框（要留下得自己点保存）。
 *
 * 生成后顺手复制一次，省得再点「复制」。
 */
async function handleGenerateSecret() {
  generating.value = true

  try {
    secretInput.value = await pairGenerateSecret()
  } catch (reason) {
    message.error(String(reason))
    generating.value = false

    return
  }

  generating.value = false

  // 复制和生成分开处理：剪贴板被别的程序占着时密钥其实**已经生成、就在输入框里**，
  // 报「生成失败」会让用户再点一次、把刚生成好的那把换掉
  try {
    await writeText(secretInput.value.trim())

    message.success(t('pages.preference.pair.hints.secretGenerated'))
  } catch {
    message.warning(t('pages.preference.pair.hints.secretGeneratedNotCopied'))
  }
}

/** 复制的是输入框里当前的值：生成之后还没保存也能先发给对方 */
async function handleCopySecret() {
  try {
    await writeText(secretInput.value.trim())

    message.success(t('pages.preference.pair.hints.secretCopied'))
  } catch (reason) {
    message.error(String(reason))
  }
}

/**
 * 删除配对密码。
 *
 * 它是**不可逆**的：删掉之后除了让对方重发一次没有别的办法拿回来，所以这里先确认一次。
 */
function handleDeleteSecret() {
  confirmDeleteCredential(
    t('pages.preference.pair.labels.pairSecret'),
    t('pages.preference.pair.hints.deleteSecretBody'),
    async () => {
      await pairDisconnect()
      await pairDeleteSecret()

      pairStore.hasSecret = false
      pairStore.secretFingerprint = ''
      savedSecret.value = ''
      secretInput.value = ''
    },
  )
}

/**
 * 保存服务器密码（R36）。
 *
 * 与配对密码一样只进系统凭据库，框里一直显示它（R45），而且**换了要重连**——
 * 中继会按新值重新判一次门槛。
 */
async function handleSaveServerPassword() {
  const password = serverPasswordInput.value.trim()

  if (!password || !serverPasswordDirty.value || savingServerPassword.value) return

  savingServerPassword.value = true

  try {
    await pairSetServerPassword(password)
    pairStore.hasServerPassword = true
    await loadSavedServerPassword(password)

    message.success(t('pages.preference.pair.hints.serverPasswordSaved'))

    await reconnectAfterCredentialChange()
  } catch (reason) {
    message.error(String(reason))
  } finally {
    savingServerPassword.value = false
  }
}

/** 复制框里显示的服务器密码 */
async function handleCopyServerPassword() {
  try {
    await writeText(serverPasswordInput.value.trim())

    message.success(t('pages.preference.pair.hints.serverPasswordCopied'))
  } catch (reason) {
    message.error(String(reason))
  }
}

/** 复制服务器地址：它本来就是要发给对方（或从对方那儿拿）的那一串，省得手抄 */
async function handleCopyRelayUrl() {
  try {
    // 复制的是「框里显示的那一串」——框空着时退回已保存值，免得复制出一个空字符串
    const value = normalizeRelayUrl(relayUrlInput.value) || normalizeRelayUrl(pairStore.settings.relay.url)

    await writeText(value)

    message.success(t('pages.preference.pair.hints.relayUrlCopied'))
  } catch (reason) {
    message.error(String(reason))
  }
}

/**
 * 删除服务器密码：删掉就得再去找部署服务器的人要一次。
 */
function handleDeleteServerPassword() {
  confirmDeleteCredential(
    t('pages.preference.pair.labels.serverPassword'),
    t('pages.preference.pair.hints.deleteServerPasswordBody'),
    async () => {
      await pairDisconnect()
      await pairDeleteServerPassword()

      pairStore.hasServerPassword = false
      savedServerPassword.value = ''
      serverPasswordInput.value = ''
    },
  )
}

/**
 * 删掉一个凭据前的确认 + 删除后的一句反馈。
 *
 * 两个「删除」按钮都是不可逆的，此前点一下就没了、连提示都没有（只读审计的 P2-4）。
 */
function confirmDeleteCredential(name: string, body: string, remove: () => Promise<void>) {
  Modal.confirm({
    title: t('pages.preference.pair.hints.deleteConfirmTitle', { name }),
    content: body,
    okText: t('pages.preference.pair.buttons.delete'),
    okType: 'danger',
    async onOk() {
      try {
        await remove()

        message.success(t('pages.preference.pair.hints.deleted'))
      } catch (reason) {
        message.error(String(reason))
      }
    },
  })
}

async function handleConnect() {
  // 连点两次会跑两轮「落盘 → 连接」：第二条 `pair_connect` 会把刚起来的会话取消再重启，
  // 而且会连出两条一样的提示（落盘本身是幂等的，不会写坏东西）
  if (connecting.value) return

  connecting.value = true

  try {
    pairStore.runtime.lastError = void 0

    const relayUrl = normalizeRelayUrl(relayUrlInput.value)
    const secret = secretInput.value.trim()
    const serverPassword = serverPasswordInput.value.trim()
    const hadSecret = pairStore.hasSecret
    let replacedSecret = false
    let remembered = false

    // 地址也是「输入框里填了就先落盘」：粘贴完直接点「立即连接」也能生效，
    // 不会出现界面上写着新地址、实际连的是旧地址这种自相矛盾的状态
    if (relayUrl) {
      // 这里是**静默**落盘的：地址那一栏的「改过了，点保存才生效」会跟着消失，
      // 而用户并没有点「保存」——不说一句，界面上就看不到任何变化（R44）
      const changed = relayUrl !== normalizeRelayUrl(pairStore.settings.relay.url)

      relayUrlInput.value = relayUrl
      pairStore.settings.relay.url = relayUrl

      if (changed) message.success(t('pages.preference.pair.hints.relayUrlSaved'))
    } else {
      // 框里是空的：连的还是已保存的那个地址，那就把它显示回框里，
      // 别让「空框 + 实际用了旧地址」同时成立（那也是这一版要消灭的错觉）
      relayUrlInput.value = pairStore.settings.relay.url
    }

    // 连接用的值**就是**记住的值：输入框里填了就先落盘（粘贴完不必先点「保存」）。
    //
    // 不这样做会出现自相矛盾的状态：界面显示「已连接」、输入框里明晃晃留着密码，
    // 但凭据库是空/旧的——重启后自动连接报「还没有配置配对密码」，用户会以为填错了
    // （只读审计的 P1-1）。
    //
    // R45：框里一直显示已保存的值，所以只有**改过**的才落盘，没改就不重复写、也不说「已记住」。
    if (secret && secretDirty.value) {
      const fingerprint = await pairSetSecret(secret)

      // 指纹和原来那串一样，就是用户不放心又把同一串粘了一遍：别说「替换」。
      // 指纹是空的时候（读失败）也不说——宁可不提，别报一句不成立的话。
      const known = pairStore.secretFingerprint

      replacedSecret = hadSecret && known !== '' && known !== fingerprint
      pairStore.secretFingerprint = fingerprint
      pairStore.hasSecret = true
      secretUnknown.value = false
      await loadSavedSecret(secret)
      remembered = true
    } else if (!secret) {
      // 框被清空了：连的还是已保存的那串，把它显示回框里（和地址一栏同一处理）
      secretInput.value = savedSecret.value
    }

    if (serverPassword && serverPasswordDirty.value) {
      await pairSetServerPassword(serverPassword)
      pairStore.hasServerPassword = true
      await loadSavedServerPassword(serverPassword)
      remembered = true
    } else if (!serverPassword) {
      serverPasswordInput.value = savedServerPassword.value
    }

    // 先把「记住了」说出来再连接：连接失败时只看到一条红字，容易读成「白填了」。
    // 落盘确实已经成功，这句话和后面那条错误不矛盾。
    if (remembered) {
      // 本来存过一串、现在填的是**不一样**的，就是替换——这件事得明说，
      // 否则用户不知道老的那串已经没了
      message.success(
        replacedSecret
          ? t('pages.preference.pair.hints.valuesReplaced')
          : t('pages.preference.pair.hints.valuesRemembered'),
      )
    }

    // 值已经在凭据库里了，这里只交地址：Rust 会用刚存下的那两个
    await pairConnect(relayUrl || pairStore.settings.relay.url)
  } catch (reason) {
    message.error(String(reason))
  } finally {
    connecting.value = false
  }
}

async function handleDisconnect() {
  try {
    await pairDisconnect()
  } catch (reason) {
    message.error(String(reason))
  }
}

function handlePresenceChange(away: boolean) {
  pairStore.settings.presence = away ? 'away' : 'active'
}

/** §46：偏好页里点一下就能听到提示音，不用等对方发消息（这里的点击也解掉自动播放限制） */
function handlePreviewSound() {
  playPairMessageSound(pairStore.settings.chat.notificationVolume).catch(reason =>
    message.error(String(reason)),
  )
}

/** 声音关掉或音量为 0 时点了也不会有声，直接禁用「试听」 */
const canPreviewSound = computed(
  () =>
    pairStore.settings.chat.notificationSound
    && pairStore.settings.chat.notificationVolume > 0,
)

/**
 * 暂离举牌文字的草稿，以及为什么必须用草稿。
 *
 * pair store 在几个窗口之间同步，而同步是**整份状态**：任何窗口只要改了自己那份里的
 * 任何东西，就会把自己的整份 `settings` patch 给后端。猫咪窗口恰恰每敲一次键、每点一下
 * 鼠标都在改统计（`stats`），于是「用户在偏好页打字」这条路上，每个按键都会有一帧**带着
 * 旧 `away.message`** 的状态发出去，并且通常会盖在刚打的字后面。直接 `v-model` 绑到 store
 * 上时，表现就是「输入框里的字会闪、丢字、打起来卡」——和用户之前报的服务器地址是同一个
 * 病根（R40 / R45 就是因此改成草稿的），暂离文字当时漏了。
 *
 * 所以这里：输入框只认草稿，点「保存」才写进 store；草稿一旦被用户改过就不再被 store 回填。
 */
const awayMessageInput = ref(pairStore.settings.away.message)
/**
 * 草稿是否已经「归草稿所有」：回填只在这一位还是 false 时发生。
 *
 * 一次回填之后就置 true——包含初次从 store 载入的那一跳（那一跳草稿与 store 同值，
 * 所以判断的是「草稿变过」而不是「用户动过」）。之后一律以草稿为准，别的窗口的旧值
 * 再也进不来。全仓库只有偏好页会写 `away.message`，所以不再跟随 store 是安全的。
 */
const awayMessageDraftOwned = ref(false)
/** 刚保存的值；保存后的守护窗口里它被别的窗口盖回来就再写一次（见下面的 watch） */
let awayMessageSaved = ''
let awayMessageGuardUntil = 0
/**
 * 保存后的守护时长。
 *
 * 带旧值的那一帧常常比我们的保存帧**晚**到后端，于是「谁最后发谁赢」会把刚保存的值顶掉：
 * 盘上留下旧文字，对面猫头上的牌子也不跟着变。人一停手，猫咪窗口就不再发帧了，所以在这段
 * 时间里把它写回去就能定下来。
 */
const AWAY_MESSAGE_GUARD_MS = 2000

const awayMessageDirty = computed(
  () => awayMessageInput.value !== pairStore.settings.away.message,
)

/** 还没被用户碰过时才回填（store 是异步载入的，落在组件挂载之后） */
watch(() => pairStore.settings.away.message, (value) => {
  if (awayMessageDraftOwned.value) return

  awayMessageInput.value = value
})

watch(awayMessageInput, () => {
  awayMessageDraftOwned.value = true
})

function handleSaveAwayMessage() {
  awayMessageSaved = awayMessageInput.value
  awayMessageGuardUntil = Date.now() + AWAY_MESSAGE_GUARD_MS

  pairStore.settings.away.message = awayMessageSaved
}

/** 保存后被盖回来就再写一次；写回同一个值不会再触发自己（下一轮 value 已经相等） */
watch(() => pairStore.settings.away.message, (value) => {
  if (Date.now() > awayMessageGuardUntil) return
  if (awayMessageInput.value !== awayMessageSaved) return
  if (value === awayMessageSaved) return

  pairStore.settings.away.message = awayMessageSaved
})
</script>

<template>
  <ProList :title="$t('pages.preference.pair.labels.connectionSettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.enabled')"
      :title="$t('pages.preference.pair.labels.enabled')"
    >
      <Switch v-model:checked="pairStore.settings.enabled" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.relayUrl')"
      :title="$t('pages.preference.pair.labels.relayUrl')"
      vertical
    >
      <Flex
        align="center"
        class="w-full"
        gap="small"
        wrap
      >
        <Input
          v-model:value="relayUrlInput"
          class="w-56"
          :placeholder="$t('pages.preference.pair.placeholders.relayUrl')"
          @press-enter="handleSaveRelayUrl"
        />

        <Button
          :disabled="!relayUrlInput.trim() || !relayUrlDirty"
          :loading="savingRelayUrl"
          type="primary"
          @click="handleSaveRelayUrl"
        >
          {{ $t('pages.preference.pair.buttons.save') }}
        </Button>

        <Button
          :disabled="!relayUrlInput.trim() && !pairStore.settings.relay.url"
          @click="handleCopyRelayUrl"
        >
          {{ $t('pages.preference.pair.buttons.copy') }}
        </Button>
      </Flex>

      <!-- 清空了：保存按不动，得说清楚「留空不等于清掉地址」 -->
      <Alert
        v-if="relayUrlEmpty"
        class="mt-2 w-full"
        :message="$t('pages.preference.pair.hints.relayUrlEmpty')"
        show-icon
        type="warning"
      />

      <!-- 草稿还没保存：地址还不是正在用的那个，得说清楚，不然用户以为已经生效 -->
      <Alert
        v-else-if="relayUrlDirty"
        class="mt-2 w-full"
        :message="$t('pages.preference.pair.hints.relayUrlUnsaved')"
        show-icon
        type="info"
      />

      <!-- §23：地址是明文时只提醒一句，绝不阻止连接 -->
      <Alert
        v-if="plaintextServerWarning"
        class="mt-2 w-full"
        :message="$t('pages.preference.pair.hints.plaintextServer')"
        show-icon
        type="warning"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.serverPassword')"
      :title="$t('pages.preference.pair.labels.serverPassword')"
      vertical
    >
      <Flex
        align="center"
        class="w-full"
        gap="small"
        wrap
      >
        <Input
          v-model:value="serverPasswordInput"
          class="w-56"
          :placeholder="$t('pages.preference.pair.placeholders.serverPassword')"
          @press-enter="handleSaveServerPassword"
        />

        <Button
          :disabled="!serverPasswordInput.trim() || !serverPasswordDirty"
          :loading="savingServerPassword"
          type="primary"
          @click="handleSaveServerPassword"
        >
          {{ $t('pages.preference.pair.buttons.save') }}
        </Button>

        <!-- R44：和地址那一行同序（先「保存」再「复制」），免得同一位置一个是保存一个是复制 -->
        <Button
          :disabled="!serverPasswordInput.trim()"
          @click="handleCopyServerPassword"
        >
          {{ $t('pages.preference.pair.buttons.copy') }}
        </Button>

        <Tag
          v-if="pairStore.hasServerPassword"
          color="success"
        >
          {{ $t('pages.preference.pair.hints.secretConfigured') }}
        </Tag>

        <Button
          v-if="pairStore.hasServerPassword"
          danger
          type="text"
          @click="handleDeleteServerPassword"
        >
          {{ $t('pages.preference.pair.buttons.delete') }}
        </Button>
      </Flex>

      <!-- R45：框里改过、还没保存：和地址一栏一样说清楚还没生效 -->
      <Alert
        v-if="serverPasswordDirty && serverPasswordInput.trim()"
        class="w-full"
        :message="$t('pages.preference.pair.hints.credentialUnsaved')"
        show-icon
        type="info"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.pairSecret')"
      :title="$t('pages.preference.pair.labels.pairSecret')"
      vertical
    >
      <Flex
        align="center"
        class="w-full"
        gap="small"
        wrap
      >
        <Input
          v-model:value="secretInput"
          class="w-56"
          :placeholder="$t('pages.preference.pair.placeholders.pairSecret')"
          @press-enter="handleSaveSecret"
        />

        <Button
          :loading="generating"
          @click="handleGenerateSecret"
        >
          {{ $t('pages.preference.pair.buttons.generateSecret') }}
        </Button>

        <Button
          :disabled="!secretInput.trim()"
          @click="handleCopySecret"
        >
          {{ $t('pages.preference.pair.buttons.copy') }}
        </Button>

        <Button
          :disabled="!secretInput.trim() || !secretDirty"
          :loading="saving"
          type="primary"
          @click="handleSaveSecret"
        >
          {{ $t('pages.preference.pair.buttons.save') }}
        </Button>

        <Tag
          v-if="pairStore.hasSecret"
          color="success"
        >
          {{ $t('pages.preference.pair.hints.secretConfigured') }}
        </Tag>

        <Button
          v-if="pairStore.hasSecret"
          danger
          type="text"
          @click="handleDeleteSecret"
        >
          {{ $t('pages.preference.pair.buttons.delete') }}
        </Button>
      </Flex>

      <Alert
        v-if="secretDirty && secretInput.trim()"
        class="w-full"
        :message="$t('pages.preference.pair.hints.credentialUnsaved')"
        show-icon
        type="info"
      />

      <Flex
        v-if="pairStore.secretFingerprint"
        class="mt-2"
        vertical
      >
        <span class="text-3 color-text-tertiary">
          {{ $t('pages.preference.pair.hints.fingerprint') }}
        </span>

        <span class="font-mono">{{ pairStore.secretFingerprint }}</span>
      </Flex>
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.autoConnect')"
      :title="$t('pages.preference.pair.labels.autoConnect')"
    >
      <Switch v-model:checked="pairStore.settings.relay.autoConnect" />
    </ProListItem>

    <!--
      R44：这一项里有 `w-full` 的子节点（下面的 Alert 与说明），必须用 `vertical`。

      横向布局里，默认插槽的每个根节点都是同一行 flex 的子项；标题块是 `flex: 1 1 0%`，
      一遇到不换行的 `w-full` 兄弟就会被压到 min-content（一个汉字一行的竖排字）。
      这个文件里其它 12 处 `w-full` 都在 `vertical` 的项里，这里以前只有一个按钮组、不超宽，
      加了 Alert 与说明之后才必须跟着改。
    -->
    <ProListItem
      :title="$t('pages.preference.pair.labels.status')"
      vertical
    >
      <Flex
        align="center"
        gap="small"
      >
        <Badge
          :status="status.color"
          :text="$t(statusText)"
        />

        <Button
          :disabled="!pairStore.settings.enabled || Boolean(connectMissing)"
          :loading="connecting"
          size="small"
          @click="handleConnect"
        >
          {{ $t('pages.preference.pair.buttons.connect') }}
        </Button>

        <Button
          :disabled="!pairStore.settings.enabled"
          size="small"
          @click="handleDisconnect"
        >
          {{ $t('pages.preference.pair.buttons.disconnect') }}
        </Button>
      </Flex>

      <!-- 缺东西就别让人干点（点了只会等一句 Rust 的报错） -->
      <Alert
        v-if="pairStore.settings.enabled && connectMissing"
        class="w-full"
        :message="$t(connectMissing)"
        show-icon
        type="info"
      />

      <!--
        R44：用户填完三个值之后不知道自己该点哪里。把顺序写死在状态行下面，
        并且点明连上之后还要自己去打开对方猫与聊天窗口（它们的默认值是关的）。
      -->
      <span class="w-full break-all text-3 color-text-tertiary">
        {{ $t('pages.preference.pair.hints.connectSteps') }}
      </span>
    </ProListItem>

    <ProListItem
      v-if="pairStore.settings.enabled"
      :description="$t('pages.preference.pair.p2p.hint')"
      :title="$t('pages.preference.pair.p2p.label')"
    >
      <Badge
        :status="p2pStatus.color"
        :text="$t(`pages.preference.pair.p2p.${p2pStatus.key}`)"
      />
    </ProListItem>

    <ProListItem
      v-if="pairStore.runtime.lastError"
      :title="$t('pages.preference.pair.labels.lastError')"
      vertical
    >
      <span class="w-full break-all text-3 color-red-5">
        {{ pairStore.runtime.lastError }}
      </span>

      <!--
        R40：只给一句结论太笼统——把「这次连的是哪个地址」和「两个密码各填了没有」
        一起摆出来，用户自己就能判断下一步是去要密码、还是去查服务器前面的代理。
      -->
      <Flex
        class="mt-2 w-full"
        gap="small"
        vertical
      >
        <span class="text-3 color-text-tertiary">
          {{ $t('pages.preference.pair.labels.lastErrorAddress') }}：{{ lastErrorAddress }}
        </span>

        <span class="text-3 color-text-tertiary">
          {{ $t('pages.preference.pair.labels.serverPassword') }}：{{
            pairStore.hasServerPassword
              ? $t('pages.preference.pair.hints.secretConfigured')
              : $t('pages.preference.pair.hints.secretMissing')
          }}
        </span>

        <span class="text-3 color-text-tertiary">
          {{ $t('pages.preference.pair.labels.pairSecret') }}：{{
            pairStore.hasSecret
              ? $t('pages.preference.pair.hints.secretConfigured')
              : $t('pages.preference.pair.hints.secretMissing')
          }}
        </span>
      </Flex>

      <!--
        R44：Rust 报出来的原文（「连接失败: io 错误」「连接任务已结束」这类）对不懂技术的人
        等于没有信息。这里固定补一句人话：这件事多半就是那三种原因之一。
      -->
      <span class="mt-2 w-full break-all text-3 color-text-tertiary">
        {{ $t('pages.preference.pair.hints.errorHowTo') }}
      </span>
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.remoteCatSettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.remoteCat')"
      :title="$t('pages.preference.pair.labels.showRemoteCat')"
    >
      <Switch
        :checked="pairStore.settings.remoteCat.visible"
        @update:checked="setRemoteCatVisible"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.remoteModel')"
      :title="$t('pages.preference.pair.labels.remoteModel')"
    >
      <Select
        v-model:value="pairStore.settings.remoteCat.modelId"
        allow-clear
        class="w-40"
        :options="modelOptions"
        :placeholder="$t('pages.preference.pair.placeholders.remoteModel')"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.remoteStats')"
      :title="$t('pages.preference.pair.labels.remoteStats')"
    >
      <Switch v-model:checked="pairStore.settings.remoteCat.showStats" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.windowSize')"
      :title="$t('pages.preference.pair.labels.windowSize')"
    >
      <Flex align="center">
        <InputNumber
          v-model:value="pairStore.settings.remoteCat.scale"
          class="w-20"
          :max="500"
          :min="10"
        />

        <span class="ml-2">%</span>
      </Flex>
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.opacity')">
      <Slider
        v-model:value="pairStore.settings.remoteCat.opacity"
        class="w-40 m-0!"
        :max="100"
        :min="10"
        :tooltip="{
          formatter(value) {
            return `${value}%`
          },
        }"
      />
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.windowRadius')">
      <Flex align="center">
        <InputNumber
          v-model:value="pairStore.settings.remoteCat.radius"
          class="w-20"
          :min="0"
        />

        <span class="ml-2">%</span>
      </Flex>
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.alwaysOnTop')">
      <Switch v-model:checked="pairStore.settings.remoteCat.alwaysOnTop" />
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.passThrough')">
      <Switch v-model:checked="pairStore.settings.remoteCat.passThrough" />
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.chatSettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.chatWindow')"
      :title="$t('pages.preference.pair.labels.showChat')"
    >
      <Switch
        :checked="pairStore.settings.chat.visible"
        @update:checked="setChatVisible"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.bubbleCount')"
      :title="$t('pages.preference.pair.labels.bubbleCount')"
    >
      <InputNumber
        v-model:value="pairStore.settings.chat.bubbleCount"
        class="w-20"
        :max="20"
        :min="1"
      />
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.chatAlwaysOnTop')">
      <Switch v-model:checked="pairStore.settings.chat.alwaysOnTop" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.chatPassThrough')"
      :title="$t('pages.preference.pair.labels.chatPassThrough')"
    >
      <Switch v-model:checked="pairStore.settings.chat.passThrough" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.notificationSound')"
      :title="$t('pages.preference.pair.labels.notificationSound')"
    >
      <Switch v-model:checked="pairStore.settings.chat.notificationSound" />
    </ProListItem>

    <ProListItem :title="$t('pages.preference.pair.labels.notificationVolume')">
      <Flex
        align="center"
        gap="small"
      >
        <Slider
          v-model:value="pairStore.settings.chat.notificationVolume"
          class="w-40 m-0!"
          :disabled="!pairStore.settings.chat.notificationSound"
          :max="100"
          :min="0"
          :tooltip="{
            formatter(value) {
              return `${value}%`
            },
          }"
        />

        <Button
          :disabled="!canPreviewSound"
          size="small"
          @click="handlePreviewSound"
        >
          {{ $t('pages.preference.pair.buttons.previewSound') }}
        </Button>
      </Flex>
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.attachmentLimit')"
      :title="$t('pages.preference.pair.labels.attachmentLimit')"
    >
      <InputNumber
        v-model:value="pairStore.settings.chat.attachmentMaxMb"
        class="w-24"
        :max="ATTACHMENT_MAX_MB.max"
        :min="ATTACHMENT_MAX_MB.min"
      />
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.chatHistory')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.chatHistory')"
      :title="$t('pages.preference.pair.labels.chatHistoryUsage')"
      vertical
    >
      <span>
        {{ $t('pages.preference.pair.labels.chatHistoryProgress', {
          epoch: historyStats.epoch,
          current: historyStats.current,
          limit: historyLimit,
          percent: historyPercent,
        }) }}
      </span>

      <span class="text-3 color-text-tertiary">
        {{ $t('pages.preference.pair.labels.chatHistoryTotal', { total: historyStats.total }) }}
      </span>
    </ProListItem>

    <ProListItem
      v-if="historyWarning"
      vertical
    >
      <Alert
        class="w-full"
        :message="historyWarning === 'full'
          ? $t('pages.preference.pair.hints.historyFull')
          : $t('pages.preference.pair.hints.historyNear')"
        show-icon
        :type="historyWarning === 'full' ? 'error' : 'warning'"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.historyLimit')"
      :title="$t('pages.preference.pair.labels.historyLimit')"
    >
      <InputNumber
        v-model:value="pairStore.settings.chat.historyMaxMessages"
        class="w-28"
        :max="500000"
        :min="1000"
        :step="1000"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.exportFormat')"
      :title="$t('pages.preference.pair.labels.exportFormat')"
    >
      <Select
        v-model:value="exportFormat"
        class="w-32"
        :options="exportFormatOptions"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.exportChat')"
      :title="$t('pages.preference.pair.labels.exportChat')"
    >
      <Button
        :loading="exporting"
        @click="handleExportHistory"
      >
        {{ $t('pages.preference.pair.buttons.export') }}
      </Button>
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.deleteOldOnReset')"
      :title="$t('pages.preference.pair.labels.deleteOldOnReset')"
    >
      <Switch v-model:checked="pairStore.settings.chat.deleteOldOnReset" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.newEpoch')"
      :title="$t('pages.preference.pair.labels.newEpoch')"
    >
      <Button
        danger
        @click="handleStartNewEpoch"
      >
        {{ $t('pages.preference.pair.buttons.newEpoch') }}
      </Button>
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.privacySettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.shareTypingActivity')"
      :title="$t('pages.preference.pair.labels.shareTypingActivity')"
    >
      <Switch v-model:checked="pairStore.settings.privacy.shareTypingActivity" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.sharePointer')"
      :title="$t('pages.preference.pair.labels.sharePointer')"
    >
      <Switch v-model:checked="pairStore.settings.privacy.sharePointer" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.shareInputStats')"
      :title="$t('pages.preference.pair.labels.shareInputStats')"
    >
      <Switch v-model:checked="pairStore.settings.privacy.shareInputStats" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.pauseActivitySync')"
      :title="$t('pages.preference.pair.labels.pauseActivitySync')"
    >
      <Switch v-model:checked="pairStore.settings.privacy.pauseActivitySync" />
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.statsSettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.stats')"
      :title="$t('pages.preference.pair.labels.myInput')"
      vertical
    >
      <Flex
        align="center"
        class="w-full"
        gap="large"
        wrap
      >
        <span>
          {{ $t('pages.preference.pair.labels.todayInput', {
            keyboard: pairStatsStore.stats.todayKeyboard,
            mouse: pairStatsStore.stats.todayMouse,
          }) }}
        </span>

        <span class="color-text-tertiary">
          {{ $t('pages.preference.pair.labels.totalInput', {
            keyboard: pairStatsStore.stats.totalKeyboard,
            mouse: pairStatsStore.stats.totalMouse,
          }) }}
        </span>
      </Flex>
    </ProListItem>
  </ProList>

  <ProList :title="$t('pages.preference.pair.labels.awaySettings')">
    <ProListItem
      :description="$t('pages.preference.pair.hints.autoReturn')"
      :title="$t('pages.preference.pair.labels.autoReturn')"
    >
      <Switch v-model:checked="pairStore.settings.away.autoReturn" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.awayMessage')"
      :title="$t('pages.preference.pair.labels.awayMessage')"
      vertical
    >
      <Flex
        align="center"
        class="w-full"
        gap="small"
        wrap
      >
        <Input
          v-model:value="awayMessageInput"
          class="flex-1"
          :placeholder="$t('pages.preference.pair.placeholders.awayMessage')"
          @press-enter="handleSaveAwayMessage"
        />

        <Button
          :disabled="!awayMessageDirty"
          type="primary"
          @click="handleSaveAwayMessage"
        >
          {{ $t('pages.preference.pair.buttons.save') }}
        </Button>
      </Flex>

      <Alert
        v-if="awayMessageDirty"
        class="w-full"
        :message="$t('pages.preference.pair.hints.credentialUnsaved')"
        show-icon
        type="info"
      />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.sendSystemNotice')"
      :title="$t('pages.preference.pair.labels.sendSystemNotice')"
    >
      <Switch v-model:checked="pairStore.settings.away.sendSystemNotice" />
    </ProListItem>

    <ProListItem
      :description="$t('pages.preference.pair.hints.presence')"
      :title="$t('pages.preference.pair.labels.presence')"
    >
      <Switch
        :checked="pairStore.settings.presence === 'away'"
        @update:checked="handlePresenceChange"
      />
    </ProListItem>
  </ProList>
</template>
