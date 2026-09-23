# BongoCat 双人联机版完整实现设计

> ## 修订记录 v2（2026-09-23，实现前共识）
>
> 本文档为初版设计。下面每一条修订都由主代理与只读审查 subagent 逐条讨论后达成一致；实现以修订项为准，未列出的章节保持原文。本次实现范围 **仅 Windows**，不验证也不改动 macOS / Linux 行为。
>
> **R1（§5 Windows 多窗口置顶）** 置顶保持状态改为按窗口 label 独立（`HashMap<label, Arc<AtomicBool>>`）；重复开启只停掉**本窗口**的旧保持线程，关闭置顶只影响本窗口。保留上游 16ms 轮询频率不变。窗口销毁与应用退出时保持线程必须能停，不能对失效 HWND 永久空转。
>
> **R2（§17 左右手判定）** 不再用模型目录 `left-keys` / `right-keys` 判断左右手（`keyboard` 模型的 `right-keys` 只有 4 个方向键，`standard` 模型没有该目录）。改为前端静态物理分区表，按 rdev 原始键名（`KeyA`、`ShiftLeft`…）直接判定，不走 `getSupportedKey` 归一化；未知键不进左右手。本地动画继续沿用模型目录判断，两条路径不合并。
>
> **R3（§18 输入强度）** 强度 = 500ms 窗口内「去重后的按下集合」大小，档位 0 / 0.2 / 0.4 / 0.6 / 0.8 / 1.0。除修饰键外所有按键都计入强度；左右手只由分区表决定。发送前把强度量化到 0.2。
>
> **R4（§20 发送频率）** 快照不是纯 3Hz 定时：**状态发生变化时立即发送一次**（点击、按键不等 tick），持续活动期间才受 ≤3Hz 刷新上限约束，这样 §87 的「200ms 内反馈」验收标准不变。再叠加 Cloudflare 免费额度约束：指针 ratio 量化 0.02、强度量化 0.2，量化后的值不变就不发。
>
> **R5（§15 心跳）** 心跳间隔 60 秒。任何一次发送失败或 pong 超时都立即触发重连，断线检测不依赖心跳。
>
> **R6（§25 / §51 统计分享）** `shareInputStats` 默认关闭，UI 示意图里的开关也画成关闭状态。
>
> **R7（§24 统计口径）** 计数按物理键名去重：只有 NEW → PRESSED 才 +1，release 移除；这样才能过滤 OS 自动重复（长按）与 Windows 3 秒 auto-release 的影响。数据源是 device 事件流，与 §18 用的是同一份数据（不是 `modelStore.pressedKeys`）。
>
> **R8（§63 / §64 分帧与限流）** 每个应用帧在密文前加 14 字节明文帧头：`kind(1B) + flags(1B) + transferId(8B) + seq(4B)`，Durable Object 只读帧头做大小校验与限流（pet state 与 transfer chunk 分开限流），不解析内容。**帧头必须作为 AEAD 的 associated data 参与认证**，否则中转方可以改 kind 绕开限流。文档中「服务器只看到密文」改为「只看到密文 + 帧头元数据（类型 / 大小 / 时序）」。
>
> **R9（§11 / §59 双人限制）** 明确写「双人限制是体验约束，不是安全边界」：持有同一 `PAIR_AUTH_TOKEN` 的人可以复用已有 deviceId 顶掉对方。陈旧连接按「最后活动时间」判断是否可被替换（阈值取 2 倍心跳 = 120 秒），最后活动时间最多每 10 秒写一次 attachment，避免高频写。
>
> **R10（§9 / §10 / §65 密钥流程）** `PAIR_SECRET` 与 `PAIR_AUTH_TOKEN` 都不进 stdout、shell history 或普通日志；`wrangler secret put` 用管道喂 stdin。secret 落盘必须显式 opt-in（例如 `--write`）+ 警告 + 提供删除命令，并把产物加入 `.gitignore`。§10 表述修正为：不知道 `PAIR_SECRET` 的是 Cloudflare 平台，部署者本人（用户 A）知道。
>
> **R11（§21 / §22 对方猫渲染）** remote-cat 不复用会写共享 store 的加载路径（`modelStore.currentMotions` / `currentExpressions` / `shortcuts` / `catStore.window.scale` 会经 tauri-pinia 跨窗口持久化并污染主窗口），改为「只加载渲染、不写共享 store」。也**不能**用 `modelStore.pressedKeys` 渲染按键图（那是本地按键），远端键盘图形必须跟随远端快照。模型来自 `pair.remoteCat.modelId`。
>
> **R12（§19 鼠标比例）** ratio 只在 `useDevice` 计算一次；`useModel` 暴露 `handleMouseRatio(x, y)`，本地与远程共用。`mouseMirror` 属于渲染偏好，远端猫沿用本机设置。
>
> **R13（§70 / §82 平台范围）** `capabilities/default.json` 已是 `windows: ["*"]`，无需修改。不改 macOS / Linux 代码路径；macOS 上 remote-cat / chat 会走通用窗口分支，属「大概能用但未验证」，本次不承诺。
>
> **R14（§79 - §81 测试设施）** 单列一个「引入测试设施」阶段：前端 vitest（隐私回归：`PetSnapshot` 不含真实键名与真实像素坐标）、Rust `cargo test`（protocol / crypto / stats）、relay 用 vitest + `@cloudflare/vitest-pool-workers`。为此 mapper 必须是**纯函数形态**（不 import Vue / Tauri / Pinia），否则无法有意义地测试。
>
> **R15（§0 / 全文 i18n）** 新增文案只写 `zh-CN` 与 `en-US`；`fallbackLocale` 已是 `en-US`，另外 3 个语言文件会显示英文，这是预期行为而不是缺陷。
>
> **R16（§44 / §45 语音）** 改用 `cpal`（Windows 走 WASAPI，纯 Rust）录音 + `hound` 写 16-bit PCM WAV，按设备原生采样率录、立体声降混为单声道、**不做朴素重采样**（避免混叠）；60 秒约 5.8 MB，走同一 transfer 管线（约 12 个 512KiB chunk）。播放用 `<audio>` + 已启用的 asset protocol。Opus 编码留作后续优化（前置条件：cmake 3.x + VS 桌面 C++ 工作负载）。这样也去掉了「WebView2 麦克风权限」这个未验证依赖。
>
> **平台事实（已核实，2026-09-23）**
> - Cloudflare WebSocket 单帧上限自 2025-10-31 起为 32 MiB（此前 1 MiB），超限由平台自动 `close 1009`。注意：这管的是 Worker / DO **收到**方向、且按整帧计（含帧头与 nonce/tag）；客户端接收方向的上限未核实，**不要**写进文档或依赖它。512 KiB 分片继续保留。
> - Durable Object 免费额度：每天 10 万次请求 + 13,000 GB-s/天；超限是「该类型后续操作失败」，relay 直接不可用。计费折算：**收到的 WebSocket 消息按 20 条 = 1 次请求计**，另加「打开一次 WebSocket 也算 1 次请求」，出站不计费。按 3Hz 连续活动估算约 1.3 万次请求/天，额度仍有余量，所以 R4 / R5 / R6 主要作为性能、电量与体验要求保留（不是额度红线）。换算细节 Phase 2 联调时对着 Cloudflare 面板计数器实测一次再定频率参数。

## 0. 任务目标

基于当前 `ayangweb/BongoCat` master 实现一个**仅支持固定双人的联机桌宠模式**。

本项目不是公共 IM，不提供官方服务器，也不考虑多人房间、注册、好友列表、账号体系。

设计前提：

* 永远只有两个人。
* 每一对用户自行部署自己的 Cloudflare Relay。
* 一个 Cloudflare 部署实例对应一对用户。
* Relay 只做实时转发，不保存聊天记录，不保存文件。
* 客户端保存聊天记录、输入统计和收到的附件。
* 双方各自配置同一个服务器地址和 Pair Secret。
* 自动断线重连。
* 支持开机启动后自动连接。
* 不要求公网 VPS。
* 第一服务端实现仅支持 Cloudflare Workers + Durable Objects。
* 不提供官方中转服务。

最终体验：

```text
用户 A                             用户 B
┌────────────────┐               ┌────────────────┐
│ 🐱 自己的猫      │               │ 🐱 自己的猫      │
│ 🐱 对方的猫      │               │ 🐱 对方的猫      │
│                  │               │                  │
│ 💬 桌面聊天气泡   │               │ 💬 桌面聊天气泡   │
└────────────────┘               └────────────────┘
          │                               │
          └──────── WSS ──────────────────┘
                       │
                Cloudflare Relay
                       │
               不保存聊天/文件
```

---

# 1. 硬性设计原则

以下原则不要为了“实现方便”改变。

## 1.1 不同步真实键盘内容

绝对不要通过网络发送：

```text
KeyA
KeyB
Key1
Control
Backspace
Enter
```

也不要发送用户实际输入的文本。

否则项目会退化成远程键盘记录器。

本地仍然按照原版逻辑读取真实键盘事件用于本地猫咪动画，但网络层只能收到抽象后的桌宠状态。

例如：

```json
{
  "keyboard": {
    "active": true,
    "leftHand": true,
    "rightHand": false,
    "intensity": 0.72
  }
}
```

鼠标不要发送真实桌面像素坐标：

```json
{
  "x": 2874,
  "y": 913
}
```

改成归一化位置：

```json
{
  "x": 0.72,
  "y": 0.31
}
```

并做量化，例如精度控制在 0.02 左右。

这样仍然可以让远程猫咪跟随鼠标方向，但不会暴露屏幕分辨率和精确桌面坐标。

---

## 1.2 Relay 不保存用户内容

Cloudflare 中转服务器不得保存：

* 聊天记录
* 图片
* 语音
* 文件
* 输入统计历史
* 键鼠事件历史

服务器只允许维护 WebSocket 当前连接状态。

聊天记录全部写入双方自己的本地 SQLite。

文件：

```text
A → WebSocket chunk → Durable Object → WebSocket chunk → B
```

收到后 B 写入本地文件。

服务器不落盘。

---

## 1.3 不需要这些功能

不要实现：

* 用户注册
* 登录
* OAuth
* 好友系统
* 房间列表
* 创建房间
* 加入房间
* 多人房间
* 在线用户列表
* 管理后台
* 官方服务器
* 消息云同步
* R2
* D1
* KV 聊天存储
* 离线服务器消息存储

一次 Cloudflare 部署本身就是一对用户。

服务器永远：

```text
Client A
Client B
```

第三个不同客户端连接必须拒绝。

---

# 2. 当前 BongoCat 代码切入点

当前输入链路：

```text
src-tauri/src/core/device.rs
            │
            │ device-changed
            ▼
src/composables/useDevice.ts
            │
            ▼
src/composables/useModel.ts
            │
            ▼
       Live2D / 按键图片
```

保持现有本地逻辑不变。

新增：

```text
device.rs
   │
   ▼
useDevice.ts
   │
   ├────────────→ 原有本地猫咪
   │
   ▼
PairActivityMapper
   │
   ▼
PetSnapshot
   │
   ▼
Rust PairManager
   │
   ▼
Encrypted WebSocket
```

即：

**不要把网络逻辑直接塞进 `device.rs`。**

`device.rs` 继续负责捕获本地输入。

前端负责把真实输入转换成安全的桌宠语义状态。

Rust 负责连接、加密、重连、聊天、文件传输和本地数据库。

---

# 3. 总体客户端架构

新增三个核心概念：

```text
Local Cat
Remote Cat
Chat Overlay
```

建议最终窗口：

```text
main
preference
remote-cat
chat
```

分别对应：

```text
main
    原来的自己的猫

remote-cat
    显示对方的猫

chat
    桌面聊天气泡

preference
    设置
```

不要把两只猫强行塞进同一个原生窗口。

理由：

* 两只猫需要独立位置。
* 两只猫需要独立缩放。
* 对方猫可以单独隐藏。
* Chat 可以自由移动。
* Chat 可以独立调整大小。
* 可以直接复用现有 `windowState` 持久化机制。
* 多显示器体验更好。

---

# 4. 新增窗口

修改：

```text
src-tauri/tauri.conf.json
```

新增：

## remote-cat

建议：

```text
label: remote-cat
transparent: true
decorations: false
skipTaskbar: true
alwaysOnTop: true
visible: false
maximizable: false
```

URL：

```text
index.html/#/remote-cat
```

## chat

建议：

```text
label: chat
transparent: true
decorations: false
skipTaskbar: true
alwaysOnTop: true
visible: false
resizable: true
```

URL：

```text
index.html/#/chat
```

Chat 设置合理的：

```text
minWidth
minHeight
```

但不要锁死最终尺寸。

---

# 5. 必须修复现有多窗口问题

## Windows

当前：

```text
src-tauri/src/plugins/window/src/commands/windows.rs
```

`TOPMOST_RUNNING` 是单个全局 `AtomicBool`。

这意味着当前实现实际上只考虑一个需要强制置顶的窗口。

新增：

```text
main
remote-cat
chat
```

后必须把它改成**按 HWND / window label 独立管理**。

例如：

```text
HashMap<WindowLabel, TopmostState>
```

或者：

```text
HashMap<HWND, Arc<AtomicBool>>
```

绝对不要让：

```text
remote-cat 开启置顶
```

导致：

```text
main/chat 的置顶线程失效
```

这是新增多窗口后必须优先处理的兼容问题。

---

## macOS

当前 macOS NSPanel 初始化明显只针对：

```text
MAIN_WINDOW_LABEL
```

必须把 overlay 窗口概念抽出来：

```text
main
remote-cat
chat
```

其中：

```text
main
remote-cat
```

作为桌面浮动面板。

`chat` 同样是浮动窗口，但必须允许在打开输入框时获得键盘焦点。

建议抽：

```rust
setup_overlay_panel(...)
```

不要复制三套 NSPanel 初始化逻辑。

Preference 保持普通窗口。

---

# 6. Router

修改：

```text
src/router/index.ts
```

新增：

```text
/remote-cat
/chat
```

新增：

```text
src/pages/remote-cat/index.vue
src/pages/chat/index.vue
```

---

# 7. WINDOW_LABEL

修改：

```text
src/constants/index.ts
```

加入：

```ts
REMOTE_CAT: 'remote-cat'
CHAT: 'chat'
```

WindowState 继续按 label 保存：

```text
main
remote-cat
chat
preference
```

这样三块 UI 的位置和大小都可以自动恢复。

---

# 8. Pair Store

新增：

```text
src/stores/pair.ts
```

这个 Store 保存**非敏感配置与 UI 状态**。

建议结构：

```ts
interface PairSettings {
  enabled: boolean

  relay: {
    url: string
    autoConnect: boolean
  }

  identity: {
    displayName: string
  }

  remoteCat: {
    visible: boolean
    scale: number
    opacity: number
    alwaysOnTop: boolean
    passThrough: boolean
    modelId?: string
    showStats: boolean
  }

  chat: {
    visible: boolean
    alwaysOnTop: boolean
    passThrough: boolean
    bubbleCount: number
    notificationSound: boolean
    notificationVolume: number
  }

  privacy: {
    shareInputStats: boolean
    sharePointer: boolean
    shareTypingActivity: boolean
  }

  away: {
    autoReturn: boolean
    message: string
    sendSystemNotice: boolean
  }
}
```

运行时状态：

```ts
interface PairRuntime {
  connection:
    | 'disabled'
    | 'connecting'
    | 'connected'
    | 'peer-offline'
    | 'reconnecting'
    | 'error'

  peerOnline: boolean
  peerName?: string

  presence:
    | 'active'
    | 'away'

  remotePresence:
    | 'offline'
    | 'active'
    | 'away'

  latestRemoteStats?: InputStats
}
```

注意：

**Pair Secret 禁止保存到 Pinia Store。**

---

# 9. Pair Secret 保存

新增 Rust secret storage。

优先使用系统 Credential Store / Keychain。

例如：

```text
Windows Credential Manager
macOS Keychain
Linux Secret Service
```

可以考虑 Rust `keyring` crate。

提供 Tauri commands：

```text
pair_set_secret
pair_has_secret
pair_delete_secret
```

前端不能读取回完整 secret。

设置时：

```text
输入 secret
↓
invoke(pair_set_secret)
↓
Rust 保存
```

不要：

```text
localStorage
Pinia
普通 JSON 设置文件
日志
```

记录 Pair Secret。

---

# 10. Relay 鉴权与 E2EE

> ⚠️ 已被 R10 部分覆盖：不知道 `PAIR_SECRET` 的是 Cloudflare 平台；部署者本人（用户 A）知道 secret。

Pair Secret 使用 32 bytes cryptographically random。

不要直接把 Pair Secret 发给 Cloudflare。

从 Pair Secret 派生两类密钥。

概念：

```text
PAIR_SECRET
   │
   ├── HKDF → AUTH_TOKEN
   │
   └── HKDF → E2EE_ROOT_KEY
```

例如：

```text
auth info = "bongocat-pair-auth-v1"
crypto info = "bongocat-pair-e2ee-v1"
```

服务器只保存：

```text
PAIR_AUTH_TOKEN
```

服务器永远不知道：

```text
PAIR_SECRET
E2EE_ROOT_KEY
```

WebSocket Upgrade：

```http
Authorization: Bearer <PAIR_AUTH_TOKEN>
X-Bongo-Client: <device-id>
X-Bongo-Protocol: 1
```

---

# 11. Device ID

客户端第一次运行 Pair 功能时生成：

```text
UUID v4
```

永久保存在本机。

它不是账号。

只是用于区分：

```text
设备 A
设备 B
```

Relay 最大允许两个不同 `deviceId`。

如果同一个 `deviceId` 重连：

```text
关闭旧 socket
接受新 socket
```

这样网络切换后不会把旧连接算成第三个人。

---

# 12. E2EE

> ⚠️ 已被 R8 覆盖：服务器看到的是「密文 + 14 字节帧头元数据（类型 / 大小 / 时序）」，不是纯密文。

聊天、Presence、Pet State、统计和文件 metadata 建议全部使用端到端加密。

建议 Rust：

```text
HKDF-SHA256
XChaCha20-Poly1305
```

每个 application message：

```text
AppEnvelope
↓
JSON serialize
↓
XChaCha20-Poly1305
↓
binary websocket frame
```

服务器只看到密文。

每个消息使用独立随机 nonce。

应用消息：

```ts
interface AppEnvelope<T> {
  v: 1
  id: string
  seq: number
  sentAt: number
  type: string
  payload: T
}
```

接收端维护最近 message id 的 LRU，防止重复处理重连/重发消息。

---

# 13. Rust PairManager

新增：

```text
src-tauri/src/core/pair/
```

建议结构：

```text
pair/
├── mod.rs
├── manager.rs
├── client.rs
├── protocol.rs
├── crypto.rs
├── secret.rs
├── history.rs
├── transfer.rs
├── audio.rs
└── stats.rs
```

职责严格拆开。

## manager.rs

负责：

```text
连接生命周期
connect
disconnect
reconnect
状态机
发送队列
广播 Tauri event
```

## client.rs

负责：

```text
WSS
HTTP Upgrade
WebSocket read/write loop
ping/pong
```

## crypto.rs

负责：

```text
HKDF
XChaCha20
encrypt
decrypt
```

## protocol.rs

只有 protocol structs/enums。

不要放 UI 逻辑。

## history.rs

本地 SQLite。

## transfer.rs

图片、语音、普通文件传输。

## audio.rs

录音。

## stats.rs

输入统计。

---

# 14. PairManager 状态机

状态：

```text
Disabled
Disconnected
Connecting
ConnectedPeerOffline
Connected
Reconnecting
Error
```

建议：

```text
启动
 │
 ├─ Pair disabled → Disabled
 │
 └─ Pair enabled
        ↓
     Connecting
        │
        ├─ Relay connected
        │      ↓
        │ ConnectedPeerOffline
        │      │
        │      └─ peer joins → Connected
        │
        └─ failed
               ↓
          Reconnecting
```

重连：

```text
1s
2s
5s
10s
30s
30s
...
```

加入 ±20% jitter。

网络恢复后立即尝试。

用户手动 Disconnect 时不要自动重连。

---

# 15. Heartbeat

> ⚠️ 已被 R5 覆盖：心跳间隔为 60 秒；发送失败或 pong 超时立即重连。

客户端每：

```text
30 秒
```

发送一次 heartbeat。

不要让 Durable Object 自己使用：

```text
setInterval
setTimeout heartbeat
```

否则会破坏 Hibernation。

Heartbeat 由客户端负责。

服务器只响应。

---

# 16. Pet State 抽象层

新增：

```text
src/composables/usePairActivity.ts
```

或者类似：

```text
PairActivityMapper
```

输入：

```text
KeyboardPress
KeyboardRelease
MousePress
MouseRelease
MouseMove
```

输出：

```ts
interface PetSnapshot {
  keyboard: {
    active: boolean
    leftHand: boolean
    rightHand: boolean
    intensity: number
  }

  pointer: {
    active: boolean
    x: number
    y: number
    speed: number
    leftDown: boolean
    rightDown: boolean
  }
}
```

---

# 17. 键盘左右手判断

不要把 key 发到网络。

本地可以利用现有：

```text
modelStore.supportKeys
```

对应文件目录：

```text
left-keys
right-keys
```

判断本次按键属于：

```text
left hand
right hand
```

然后丢弃具体 key。

最终网络层只能看到：

```text
leftHand = true
```

不知道是：

```text
Q
W
A
S
1
2
```

---

# 18. Typing Intensity

不要一按键就网络发一条消息。

维护短时间窗口：

```text
过去 500ms 的 KeyboardPress 次数
```

转换：

```text
0 → 0
1 → 0.2
2 → 0.4
3 → 0.6
4 → 0.8
5+ → 1.0
```

或者平滑处理。

这样对方猫可以：

```text
慢慢输入
快速输入
疯狂敲键盘
```

但完全不知道具体键值。

---

# 19. 鼠标状态

`useModel.ts` 当前：

```text
handleMouseMove(PhysicalPosition)
↓
查 monitor
↓
计算 xRatio/yRatio
↓
设置 Live2D 参数
```

重构：

```text
handleMouseMove()
    ↓
计算 ratio
    ↓
handleMouseRatio(xRatio, yRatio)
```

新增：

```ts
handleMouseRatio(xRatio: number, yRatio: number)
```

本地鼠标：

```text
PhysicalPosition
↓
ratio
↓
handleMouseRatio()
```

远程鼠标：

```text
网络 xRatio/yRatio
↓
handleMouseRatio()
```

这样远程猫不需要伪造一个本机 PhysicalPosition。

---

# 20. 状态发送频率

> ⚠️ 已被 R4 覆盖：持续活动上限由 5Hz 收紧为 ≤3Hz，但「状态变化立即发送」优先，单次变化仍在 200ms 内到达。

这是硬要求。

不要：

```text
MouseMove → 每个事件直接 WebSocket.send()
```

本地捕获可以是 60/120/500Hz。

网络最多：

```text
5 Hz
```

即约每：

```text
200ms
```

发送一次活动快照。

规则：

```text
有活动：
最多 5Hz

没有状态变化：
不发

鼠标 click：
允许立即触发一次状态更新

Presence：
立即发送

Chat：
立即发送

Stats：
30 秒一次 + 建连时一次
```

这能够让 Cloudflare Free 长时间运行仍然有足够余量。

---

# 21. Remote Cat

新增：

```text
src/pages/remote-cat/index.vue
```

职责只有：

```text
渲染对方猫
接受 PetSnapshot
接受 Presence
接受 NewMessage effect
显示输入统计
```

不要让它监听本机 device events。

---

# 22. Remote Cat Model

不要自动网络传输 Live2D/custom model。

原因：

```text
文件大
版权问题
安全问题
实现复杂
```

V1 设计：

设置中新增：

```text
对方猫咪模型
```

用户从**本机已经安装的模型**里选择。

默认：

```text
与自己的猫使用相同模型
```

未来如果双方使用公共模型，可以通过 model fingerprint 自动匹配，但不是 V1 必须项。

---

# 23. Remote Cat 无事件恢复

网络不是可靠实时动画协议。

必须有 TTL。

例如收到：

```text
leftHand=true
```

但下一包丢失。

不能让猫永远按着左手。

建议：

```text
typing state TTL: 800ms
mouse click TTL: 500ms
pet snapshot TTL: 1500ms
```

超过 TTL：

```text
release left
release right
release mouse
typing=false
```

Peer offline：

```text
全部释放
```

然后根据设置：

```text
猫咪隐藏
或
进入睡觉/离线状态
```

---

# 24. 输入统计

统计定义明确：

```text
keyboardCount = KeyboardPress 数量
mouseCount = Mouse ButtonPress 数量
```

MouseMove 不计数。

保存：

```text
今日键盘
今日鼠标点击
累计键盘
累计鼠标点击
```

展示：

```text
今日输入
累计输入
```

也可以展示 breakdown。

建议 `device.rs` 本地 callback 中直接调用 StatsManager 增量，而不是每次 invoke 前端。

只在：

```text
KeyboardPress
MousePress
```

时计数。

---

# 25. Stats 网络同步

如果：

```text
shareInputStats = true
```

发送：

```json
{
  "date": "2026-09-23",
  "todayKeyboard": 12345,
  "todayMouse": 2345,
  "totalKeyboard": 1234567,
  "totalMouse": 234567
}
```

频率：

```text
连接成功立即一次
最多每 30 秒一次
重大变化/午夜 rollover 再一次
```

关闭分享后立即停止发送。

---

# 26. Presence / 暂离

Presence：

```text
active
away
```

Away 支持：

```text
快捷键进入
自定义举牌文字
```

例如：

```json
{
  "state": "away",
  "message": "去吃饭啦"
}
```

对方猫显示：

```text
┌───────────┐
│ 去吃饭啦 │
└───────────┘
     🐱
```

---

# 27. 自动回来

进入 away 后：

```text
KeyboardPress
MousePress
明显 MouseMove
```

任何一个发生：

```text
away → active
```

MouseMove 需要设置最小阈值，例如累计：

```text
> 4px
```

避免鼠标传感器微小抖动导致自动回来。

设置：

```text
autoReturn
```

允许关闭。

---

# 28. “我暂离 / 我回来啦”

不要强制作为普通 ChatMessage。

更合理的是：

```text
presence event
```

客户端自己渲染系统提示：

```text
对方暂离啦
对方回来啦
```

设置：

```text
sendSystemNotice
```

开启时才显示。

Presence 事件默认不计入普通聊天消息。

---

# 29. Chat Overlay

新增：

```text
src/pages/chat/index.vue
```

桌面常驻气泡。

支持：

* 最新 N 条消息
* N 可配置
* 滚轮查看历史
* 拖动窗口
* 调整窗口大小
* 复制文字
* 图片预览
* 文件打开
* 文件另存
* 一键隐藏
* 置顶
* 可选窗口穿透

默认：

```text
bubbleCount = 5
```

---

# 30. Chat 输入模式

增加全局快捷键：

```text
toggleChatInput
```

行为：

```text
按快捷键
↓
显示 Chat Window
↓
临时关闭 pass-through
↓
输入框 focus
```

Enter：

```text
发送
```

Shift+Enter：

```text
换行
```

Esc 或再次快捷键：

```text
关闭输入模式
```

如果用户设置了 pass-through：

退出输入模式后恢复 pass-through。

---

# 31. Chat Message

文本：

```json
{
  "type": "chat.text",
  "messageId": "...",
  "text": "你好"
}
```

限制：

```text
UTF-8
单条最多 8 KiB
```

本地先生成 UUID message id。

发送过程：

```text
写本地 DB pending
↓
网络发送
↓
对方收到
↓
对方写 DB
↓
发送 chat.ack
↓
本地标记 delivered
```

状态：

```text
pending
sent
delivered
failed
```

---

# 32. Peer 离线时发文字

可以做**客户端本地队列**。

服务器仍然不存。

例如：

```text
A 在线
B 离线

A 发“回来叫我”
↓
A 本地 DB pending

之后 A/B 同时在线
↓
自动重试
↓
B ack
↓
delivered
```

如果 A 一直不上线：

B 不可能收到。

这是预期行为。

---

# 33. 本地聊天数据库

不要把聊天历史塞进 Pinia。

新增本地 SQLite：

```text
AppData/BongoCat/pair/pair.db
```

建议 tables：

```sql
messages
attachments
input_stats
metadata
```

messages：

```text
id
direction
kind
created_at
text
status
attachment_id
conversation_epoch
```

attachments：

```text
id
kind
original_name
mime
size
sha256
local_path
created_at
```

input_stats：

```text
date
keyboard_count
mouse_click_count
```

metadata：

```text
key
value
```

所有 DB 操作放 Rust。

---

# 34. Chat 历史读取

前端不要一次加载几万条。

API：

```text
pair_history_list(before, limit)
```

例如：

```text
limit = 50
```

Chat 页面滚到顶部：

```text
加载更旧 50 条
```

避免长时间使用后 UI 卡顿。

---

# 35. Chat 导出

提供：

```text
导出聊天记录
```

格式优先：

```text
JSON
```

同时可以提供：

```text
TXT / Markdown
```

附件不要直接全部塞进 JSON。

Export：

```text
history.json
attachments/
```

可以最终打包 ZIP。

---

# 36. 本地保存上限

设置：

```text
historyMaxMessages
```

例如默认：

```text
50,000
```

达到 90%：

```text
提示即将达到保存上限
```

达到上限不要直接删除消息。

提示：

```text
导出并开始新的记录周期
```

用户确认后：

```text
export
↓
conversation_epoch + 1
```

旧历史仍可以选择：

```text
保留
删除
```

不要为了“达到限制”静默删聊天。

---

# 37. 图片发送

Chat 输入处理 Paste。

如果 ClipboardEvent 中包含 image：

```text
读取 image
↓
保存临时文件
↓
进入 Transfer pipeline
```

收到图片：

```text
保存到本地 attachment cache
↓
DB 记录 local path
↓
显示 thumbnail
```

点击：

```text
预览
```

允许：

```text
复制图片
另存为
```

---

# 38. 文件协议

服务器不保存文件。

文件流程：

```text
A                           B

transfer.offer
───────────────→

                ←──────────
                transfer.accept

chunk 0
───────────────→
chunk 1
───────────────→
chunk 2
───────────────→

transfer.complete
───────────────→

                ←──────────
                transfer.verified
```

---

# 39. 文件 Offer

```json
{
  "type": "transfer.offer",
  "id": "...",
  "kind": "file",
  "name": "example.zip",
  "size": 12345678,
  "mime": "application/zip",
  "sha256": "..."
}
```

不要发送发送方本地完整路径。

禁止：

```text
C:\Users\xxx\Desktop\secret.zip
```

只能：

```text
secret.zip
```

---

# 40. 文件 Chunk

建议：

```text
512 KiB/chunk
```

不要靠近 Cloudflare 32 MiB 上限。

Binary Frame 包含：

```text
version
transfer id
chunk index
nonce
ciphertext
authentication tag
```

每个 Transfer 派生自己的 encryption key。

例如：

```text
HKDF(E2EE_ROOT_KEY, transferId)
```

这样 chunk nonce 更容易安全管理。

---

# 41. 文件接收

接收端：

```text
AppData/pair/tmp/<uuid>.part
```

边收边写。

不要：

```text
先把 500MB 文件全部放内存
```

接收完成：

```text
SHA-256
↓
和 metadata 对比
```

正确：

```text
rename 到 attachments
```

错误：

```text
删除 .part
标记 failed
```

---

# 42. 文件安全

收到普通文件：

* 不自动执行。
* 不自动打开。
* 原文件名必须 sanitize。
* 实际落盘文件使用 UUID。
* MIME 不可信。
* extension 不可信。
* 点击“打开”必须用户明确操作。
* 最大文件大小默认 256MB。
* 设置可以提高。
* 建议硬上限 1GB。

图片和语音允许自动下载，因为它们要用于聊天 UI。

普通大文件超过 50MB 可以先弹确认。

---

# 43. 文件断线

V1 不做 chunk resume。

网络中断：

```text
transfer = interrupted
```

删除或保留 `.part`：

建议删除。

UI：

```text
传输失败
[重试]
```

不要为了断点续传显著扩大第一版范围。

---

# 44. 语音消息

> ⚠️ 已被 R16 覆盖：改用 `cpal` 录音 + `hound` 写 16-bit PCM WAV（设备原生采样率、立体声降混单声道、不做朴素重采样），Opus 延后为可选优化。

语音最终走与文件完全相同的 Transfer pipeline。

区别只是：

```text
kind = voice
```

不要单独造另一套上传逻辑。

建议 Native Rust 录音：

```text
cpal
+
Opus
```

目标格式：

```text
48kHz
mono
Opus
```

避免 WebView MediaRecorder 在：

```text
WebView2
WKWebView
WebKitGTK
```

之间行为差异过大。

---

# 45. 语音快捷键

新增：

```text
pushToTalk
```

行为：

```text
Pressed
↓
开始录音

Released
↓
结束录音
↓
生成 voice attachment
↓
发送
```

最大：

```text
60 秒
```

可取消。

如果用户只轻点，可以规定：

```text
< 300ms
```

不发送，避免误触。

---

# 46. 新消息反馈

收到：

```text
chat.text
image
voice
file
```

触发 Remote Cat：

## Q 弹

整个猫咪容器做短 CSS animation：

```text
translateY
scale
```

不要移动原生窗口坐标。

## 键盘粉色闪光

加：

```text
notification flash overlay
```

约：

```text
300~500ms
```

不要修改模型文件。

## 提示音

打包一个短消息音效。

设置：

```text
notificationSound
notificationVolume 0..100
```

音量 0 等价静音。

---

# 47. Notification Event

Rust 收到新消息后 emit：

```text
pair-message-received
```

Chat Window：

```text
增加消息
```

Remote Cat Window：

```text
触发 Q 弹/闪光/声音
```

不要让 Chat 页面负责通知动画。

---

# 48. Shortcut Store

修改：

```text
src/stores/shortcut.ts
```

新增：

```text
visibleRemoteCat
visibleChat
toggleChatInput
toggleAway
pushToTalk
```

更新：

```text
src/pages/preference/components/shortcut/index.vue
```

全部使用现有 global shortcut 体系。

现有 `useKeyPress` 忽略 Released。

Push-To-Talk 需要新增一个支持：

```text
Pressed
Released
```

都回调的 composable。

不要破坏原来的 `useKeyPress` 行为。

可以新增：

```text
useKeyStateShortcut
```

---

# 49. Preference 新增 Pair 页面

修改：

```text
src/pages/preference/index.vue
```

新增菜单：

```text
Pair / 双人联机
```

新增：

```text
src/pages/preference/components/pair/index.vue
```

建议分组：

```text
连接
对方猫咪
聊天
隐私
暂离
通知
存储
```

---

# 50. Connection Settings

UI：

```text
启用双人联机       [Switch]

Relay URL
https://xxxx.workers.dev

Pair Secret
••••••••••••••••

[保存]

自动连接            [Switch]

连接状态:
● 已连接
● 对方离线
● 重连中
● 连接失败

[立即连接]
[断开]
```

Secret 保存成功后不要重新把原文显示出来。

显示：

```text
已配置
```

即可。

---

# 51. Privacy Settings

> ⚠️ 已被 R6 覆盖：`shareInputStats` 默认关闭，下图开关按「关闭」理解。

明确显示：

```text
分享键盘活动        ON
分享鼠标活动        ON
分享输入统计        ON
```

解释：

```text
键盘活动只同步左右手和输入强度，
不会发送实际按键内容。
```

这个说明必须写进 UI。

---

# 52. “暂停同步”

增加：

```text
暂停活动同步
```

开启后：

```text
聊天仍然可用
Presence 仍然可用
Pet State 停止
Stats 停止
```

用于用户临时不希望展示活动状态。

快捷键可以以后加，但设置里至少要有。

---

# 53. Context Menu / Tray

修改：

```text
src/composables/useAppMenu.ts
```

增加：

```text
显示/隐藏对方猫
显示/隐藏聊天
暂离/回来
连接状态
```

不要塞太多设置进去。

完整配置仍然进入 Preference。

---

# 54. Cloudflare Server

仓库新增：

```text
server-cloudflare/
```

结构：

```text
server-cloudflare/
├── package.json
├── tsconfig.json
├── wrangler.jsonc
├── README.md
└── src/
    ├── index.ts
    ├── pair.ts
    └── protocol.ts
```

服务器越小越好。

---

# 55. Cloudflare 架构

固定：

```text
Worker
  │
  │ GET /ws
  ▼
PAIR Durable Object
  │
  ├── WebSocket A
  └── WebSocket B
```

固定对象：

```ts
env.PAIR.getByName('pair')
```

不需要 room id。

---

# 56. Durable Object

使用 SQLite-backed Durable Object。

即使完全不使用数据库，也要使用 Free 计划支持的 SQLite DO 类型。

必须使用：

```text
WebSocket Hibernation API
```

而不是普通 WebSocket API。

---

# 57. Relay Endpoint

只需要：

```text
GET /health
GET /ws
```

`/health`：

```json
{
  "ok": true,
  "protocol": 1
}
```

不要返回：

```text
当前谁在线
device id
IP
secret
```

---

# 58. Worker 鉴权

`/ws`：

读取：

```text
Authorization
X-Bongo-Client
X-Bongo-Protocol
```

验证：

```text
PAIR_AUTH_TOKEN
```

错误：

```text
401 authentication failed
426 unsupported protocol
```

正确：

```text
forward to Durable Object
```

---

# 59. 两人限制

Durable Object：

```text
当前 socket = 0
→ 接受

当前 socket = 1
→ 接受

当前 socket = 2
```

如果新 socket `deviceId` 与已有某个相同：

```text
关闭旧 socket
接受新 socket
```

如果是第三个不同 ID：

```text
close code 4003
reason pair is full
```

---

# 60. Hibernation

必须：

```text
ctx.acceptWebSocket()
```

并实现：

```text
webSocketMessage()
webSocketClose()
webSocketError()
```

每个 socket：

```text
serializeAttachment({
  deviceId
})
```

这样 Durable Object Hibernate 后可以恢复 socket 身份。

服务器禁止长期：

```text
setInterval
setTimeout
```

---

# 61. Relay 行为

客户端发：

```text
binary frame
```

DO：

```text
收到
↓
找到另外一个 socket
↓
直接 send
```

不要 decrypt。

不要 parse application payload。

不要保存。

---

# 62. Server Control Frame

服务器自己需要通知：

```text
peer-online
peer-offline
```

可以使用很小的 plaintext JSON control message：

```json
{
  "type": "server.peer",
  "online": true,
  "deviceId": "..."
}
```

这类消息不包含用户聊天内容。

Application frame 则全部 binary + E2EE。

---

# 63. Server 限制

> ⚠️ 已被 R8 覆盖：`text/control <= 32 KiB`、`binary <= 1 MiB` 的口径按「含 14 字节明文帧头 + nonce/tag 的整帧」计算（该口径属于 R8 的一部分）。

应用层主动限制：

```text
text/control frame <= 32 KiB
binary frame <= 1 MiB
```

超出：

```text
close 1009
```

虽然 Cloudflare 自己的上限更高，但我们自己的 protocol 不需要那么大。

---

# 64. Server Rate Guard

> ⚠️ 已被 R8 覆盖：DO 可以读取 14 字节明文帧头（kind / transferId / seq），因此能按帧类型分桶限流；但帧头由客户端自报，属「防误用」而非安全边界。该明文帧头必须作为 AEAD 的 associated data 参与认证。

做非常轻量保护。

例如单客户端：

```text
控制消息 <= 30/s
binary <= 20/s 正常情况
```

不要因为鼠标 bug 让客户端每秒向 DO 发几千包。

出现异常高频：

```text
drop / close
```

但文件传输期间要允许正常 chunk burst，因此 binary transfer 和 pet-state 应区分处理策略。

如果 application frame 全部 E2EE 无法辨别类型，则主要靠客户端节流，Relay 只实施极高的 hard limit。

---

# 65. Cloudflare Deployment

> ⚠️ 已被 R10 覆盖：`PAIR_SECRET` 与 `PAIR_AUTH_TOKEN` 都不进 stdout / shell history / 日志；`wrangler secret put` 用管道喂 stdin，secret 落盘必须显式 opt-in 并加入 `.gitignore`。

README 给出：

```bash
cd server-cloudflare
pnpm install
```

提供脚本：

```bash
pnpm pair:generate
```

生成：

```text
PAIR_SECRET=<32-byte random>
PAIR_AUTH_TOKEN=<derived>
```

然后：

```bash
npx wrangler login
npx wrangler secret put PAIR_AUTH_TOKEN
pnpm deploy
```

输出：

```text
Relay URL:
https://xxxx.workers.dev

Pair Secret:
xxxxxxxxxxxxxxxx
```

用户把：

```text
Relay URL
Pair Secret
```

发给另一位用户。

Cloudflare 上只保存：

```text
PAIR_AUTH_TOKEN
```

---

# 66. 可以增加 Pair Config

为了方便复制，可以支持：

```json
{
  "server": "https://xxxx.workers.dev",
  "secret": "..."
}
```

或者以后：

```text
bongocat://pair?...
```

但自定义 URL Scheme 不是 V1 必须项。

---

# 67. Connection Events

Rust → Frontend：

加入 constants：

```text
PAIR_CONNECTION_CHANGED
PAIR_PEER_CHANGED
PAIR_PET_STATE
PAIR_STATS
PAIR_PRESENCE
PAIR_MESSAGE
PAIR_TRANSFER_PROGRESS
PAIR_TRANSFER_COMPLETE
PAIR_NOTIFICATION
```

使用 Tauri event 广播给相关窗口。

---

# 68. Tauri Commands

建议：

```text
pair_connect
pair_disconnect
pair_get_status

pair_set_secret
pair_has_secret
pair_delete_secret

pair_send_pet_state
pair_send_presence
pair_send_chat

pair_send_file
pair_accept_transfer
pair_reject_transfer
pair_cancel_transfer

pair_history_list
pair_history_export

pair_start_recording
pair_stop_recording
pair_cancel_recording
```

不要做一个：

```text
pair_command(action: string, payload: any)
```

这样的万能 command。

保持 typed API。

---

# 69. 文件职责总结

建议新增：

```text
src/
├── composables/
│   ├── usePair.ts
│   ├── usePairActivity.ts
│   ├── useRemotePet.ts
│   └── useKeyStateShortcut.ts
│
├── stores/
│   └── pair.ts
│
├── pages/
│   ├── remote-cat/
│   │   └── index.vue
│   └── chat/
│       └── index.vue
│
└── pages/preference/components/
    └── pair/
        └── index.vue
```

Rust：

```text
src-tauri/src/core/pair/
├── mod.rs
├── manager.rs
├── client.rs
├── protocol.rs
├── crypto.rs
├── secret.rs
├── history.rs
├── transfer.rs
├── stats.rs
└── audio.rs
```

Server：

```text
server-cloudflare/
```

---

# 70. 主要现有文件改动

需要重点检查并修改：

```text
src/composables/useDevice.ts
src/composables/useModel.ts

src/stores/shortcut.ts
src/stores/app.ts

src/pages/main/index.vue
src/pages/preference/index.vue
src/pages/preference/components/shortcut/index.vue

src/composables/useWindowState.ts
src/composables/useAppMenu.ts

src/constants/index.ts
src/router/index.ts

src/App.vue

src-tauri/src/core/device.rs
src-tauri/src/core/setup/mod.rs
src-tauri/src/core/setup/macos.rs

src-tauri/src/lib.rs
src-tauri/tauri.conf.json
src-tauri/Cargo.toml
src-tauri/capabilities/default.json

src-tauri/src/plugins/window/src/commands/mod.rs
src-tauri/src/plugins/window/src/commands/windows.rs
src-tauri/src/plugins/window/src/commands/macos.rs
src-tauri/src/plugins/window/src/commands/linux.rs
```

不要机械地修改所有文件。

先根据当前代码确认实际需要。

---

# 71. WindowState 重构

当前 `useWindowState.ts` 对：

```text
keepInScreen
```

只特别考虑 MAIN。

加入 remote-cat/chat 后重构为：

```text
isOverlayWindow(label)
```

分别读取：

```text
main → cat settings
remote-cat → pair remoteCat settings
chat → pair chat settings
```

三个窗口的位置尺寸都要独立保存。

---

# 72. App.vue

现在每个 WebView 都会初始化：

```text
appStore
modelStore
catStore
generalStore
shortcutStore
```

加入：

```text
pairStore
```

但不要让每个窗口都主动创建一条 WebSocket。

**整个应用只能存在一个 PairManager 网络连接。**

PairManager 在 Rust 中是 app-global managed state。

所有 WebView 共享它。

Frontend Store 只是观察状态。

---

# 73. 单连接原则

严格保证：

```text
main window
remote-cat window
chat window
preference window
```

不会分别连接 Cloudflare。

只能：

```text
Rust PairManager
        │
        └── 唯一 WebSocket
```

这是把网络放 Rust 的核心原因。

---

# 74. 开机启动与自动连接

现有项目已经支持 autostart。

Pair enabled + autoConnect 时：

```text
应用启动
↓
Rust / frontend 完成初始化
↓
PairManager.connect()
```

不要求用户打开 Preference。

如果 Relay 不可达：

```text
后台重连
```

不能不断弹错误窗口。

只更新：

```text
connection status
```

---

# 75. App 退出

真正退出应用：

```text
PairManager shutdown
↓
停止录音
↓
取消 transfer
↓
flush SQLite
↓
close socket
```

窗口 hide 不等于应用退出。

不要因为：

```text
main window hidden
```

断开 Pair。

---

# 76. 离线行为

Relay down：

```text
自己的猫正常工作
Chat 历史可看
Remote cat 显示离线
文字进入 pending queue
文件传输暂停/失败
```

BongoCat 原有离线功能不能受影响。

Pair 整套功能必须是 optional。

---

# 77. 原版功能兼容性

必须保持：

```text
本地猫咪
键盘
鼠标
手柄
自定义模型
缩放
透明度
窗口穿透
置顶
托盘
自动启动
更新
快捷键
macOS / Windows / Linux
```

都继续可用。

Pair disabled 时，行为应该尽可能与上游版本完全一致。

---

# 78. 隐私日志

日志中禁止：

```text
Pair Secret
Auth Token
聊天 plaintext
解密后的文件 chunk
完整文件路径
```

Connection 日志只允许：

```text
relay connected
relay disconnected
peer online
transfer abc started
transfer abc completed
```

错误日志需要 redact。

---

# 79. 测试

至少增加以下测试。

## Protocol

测试：

```text
serialize/deserialize
unknown version
unknown message type
oversized payload
duplicate message id
```

## Crypto

测试：

```text
same secret → 可以解密
different secret → 解密失败
tampered ciphertext → 失败
different nonce → ciphertext 不同
```

## Pet Mapper

测试：

```text
真实 key 不出现在 PetSnapshot
鼠标真实 pixel 不出现在 PetSnapshot
typing intensity 限制 0..1
pointer ratio 限制 0..1
```

这是非常重要的隐私回归测试。

---

# 80. Relay Tests

使用 Cloudflare Workers test 环境测试：

```text
无 token → 401
错误 token → 401
protocol mismatch → reject

A connect
B connect
→ success

C connect
→ pair full

A reconnect same device id
→ old A replaced

A binary
→ only B receives

A disconnect
→ B receives peer offline
```

---

# 81. File Transfer Tests

至少：

```text
0 byte
1 byte
512 KiB
512 KiB + 1
10 MB

hash correct
hash incorrect
disconnect middle
receiver reject
sender cancel
unsafe filename
duplicate id
oversize
```

---

# 82. UI 回归

Windows/macOS/Linux 至少验证：

```text
main 独立拖动
remote-cat 独立拖动
chat 独立拖动

关闭/打开 remote-cat 后位置恢复
关闭/打开 chat 后位置恢复

三窗口 always-on-top 不互相影响
pass-through 不互相影响
多显示器移动正常
DPI 改变正常
```

特别关注 Windows 多窗口 topmost 重构。

---

# 83. 网络压力测试

模拟：

```text
双方连续 mouse/keyboard 活动 1 小时
```

验证：

```text
Pet Snapshot <= 5Hz/client
内存不持续增长
发送队列不持续增长
CPU 空闲时低占用
Durable Object 能 Hibernate
```

如果 WebSocket 网络速度下降：

```text
旧 PetSnapshot 可以直接丢
```

不要排队几十秒。

实时状态必须采用：

```text
latest wins
```

聊天和文件则不能丢。

---

# 84. Outbound Queue 分类

Rust PairManager 必须区分：

## Reliable

```text
chat
presence
transfer control
file chunks
ack
```

## Replaceable

```text
pet snapshot
stats snapshot
```

如果网络堵塞：

```text
PetSnapshot 旧数据直接被新数据覆盖
```

不要无限堆积。

这是避免长时间运行内存增长的关键。

---

# 85. 实现阶段

不要一次提交一个巨大 patch。

建议按以下阶段推进。

## Phase 1：基础窗口

完成：

```text
remote-cat
chat
window labels
router
window state
Windows/macOS multi-window 修复
```

此阶段不联网。

确保三个窗口稳定。

## Phase 2：Cloudflare Relay

完成：

```text
server-cloudflare
PairManager
secret
auth
WSS
reconnect
peer online/offline
E2EE base
```

先只发送测试 ping。

## Phase 3：Remote Cat

完成：

```text
PairActivityMapper
5Hz snapshot
remote cat animation
pointer ratio
typing
click
stats
presence
away
```

## Phase 4：文字聊天

完成：

```text
SQLite
Chat Overlay
text
ack
pending queue
history
export
notification
```

## Phase 5：附件

完成：

```text
image
generic file
chunk
SHA-256
progress
local cache
```

## Phase 6：语音

> ⚠️ 已被 R16 覆盖：编码改为 16-bit PCM WAV（`hound`），不含 Opus。

完成：

```text
native recording
Opus
push-to-talk
voice playback
```

每阶段通过测试后再继续。

---

# 86. Commit 拆分建议

建议至少拆：

```text
feat(window): add remote pet and chat overlay windows

refactor(window): support multiple overlay windows

feat(pair-server): add cloudflare pair relay

feat(pair): add encrypted pair connection manager

feat(pair): synchronize remote pet state

feat(pair): add presence and input statistics

feat(chat): add local pair chat history

feat(chat): add desktop bubble overlay

feat(transfer): add encrypted pair file transfer

feat(voice): add pair voice messages
```

不要一个 commit 改几千行所有功能。

---

# 87. 验收标准

> ⚠️ 已被 R4 覆盖：「200ms 左右内产生对应反馈」指单次状态变化；连续活动按 ≤3Hz 刷新计算。

最终至少满足：

### 双方连接

A/B 输入相同：

```text
Relay URL
Pair Secret
```

然后：

```text
自动连接
```

不需要账号/房间号。

### 桌宠

A 操作键鼠：

```text
B 的 remote-cat 200ms 左右内产生对应反馈
```

网络数据中不能出现：

```text
真实按键名称
真实输入文字
真实屏幕 pixel 坐标
```

### Stats

双方能看到：

```text
今日输入
累计输入
```

可关闭分享。

### Chat

支持：

```text
文字
历史
复制
气泡数量
拖动
缩放
隐藏
快捷键输入
```

### Attachment

支持：

```text
粘贴图片
图片预览
文件
传输进度
打开
另存为
```

### Voice

支持：

```text
快捷键录音
发送
播放
```

### Away

支持：

```text
手动暂离
举牌
检测活动自动回来
```

### Notification

新消息：

```text
猫 Q 弹
键盘粉色闪
提示音
```

音量可调。

### Persistence

应用重启后：

```text
Relay URL 保留
Secret 安全保留
窗口位置保留
Chat 历史保留
Stats 保留
快捷键保留
```

并自动重连。

---

# 88. 非目标

第一版明确不要做：

```text
多人
群聊
公共服务器
好友系统
账号
云聊天历史
R2
WebRTC
NAT 穿透
移动端
消息撤回
已读回执
表情包平台
模型自动网络传输
文件断点续传
端到端设备迁移
```

这些全部不要顺手加入。

---

# 89. 实现时的优先级

优先级：

```text
P0
连接稳定
不泄露真实键盘
原 BongoCat 不回归
多窗口稳定
自动重连
聊天不丢
文件不损坏

P1
动画体验
气泡 UI
通知效果
统计 UI
Away 动画

P2
视觉 polish
导出格式
高级设置
```

遇到时间或复杂度问题：

**优先完成 P0/P1，不扩大范围。**

---

# 90. 最终核心架构

最终应该形成：

```text
                    ┌────────────────────┐
                    │ Cloudflare Worker  │
                    │       │            │
                    │ Pair Durable Object│
                    │ WebSocket Hibernate│
                    └─────────┬──────────┘
                              │
                    Encrypted Relay Only
                 ┌────────────┴────────────┐
                 │                         │
          ┌──────▼──────┐           ┌──────▼──────┐
          │ BongoCat A  │           │ BongoCat B  │
          │             │           │             │
          │ PairManager │           │ PairManager │
          │   │         │           │   │         │
          │   ├─ SQLite │           │   ├─ SQLite │
          │   ├─ Crypto │           │   ├─ Crypto │
          │   ├─ Files  │           │   ├─ Files  │
          │   └─ Stats  │           │   └─ Stats  │
          │             │           │             │
          │ 🐱 Local     │           │ 🐱 Local     │
          │ 🐱 Remote    │           │ 🐱 Remote    │
          │ 💬 Chat      │           │ 💬 Chat      │
          └─────────────┘           └─────────────┘
```

Cloudflare 的角色严格限制为：

```text
鉴权
维持两条 WebSocket
A → B
B → A
```

除此之外不应该承担任何长期用户数据职责。

---

# 91. Codex 执行要求

开始改动前先重新检查当前 master，因为上游可能已经变化。

不要仅根据本文档里的文件路径机械修改。

先确认：

```text
device event pipeline
model render pipeline
window plugin
Pinia persistence
shortcut system
macOS NSPanel
Windows topmost
```

和本文描述是否仍一致。

如果一致，再按照上面的阶段实施。

如果上游结构变化：

保持本文的**架构语义**，适配新的代码结构。

整个改造要保持模块化，Pair 功能关闭时不得影响原版 BongoCat 的离线桌宠行为。
