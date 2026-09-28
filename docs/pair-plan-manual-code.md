# 配对码（手工信令）与自定义公益 STUN

状态：**已完成**（v1 范围 = 配对码 + 自定义公益 STUN；内置服务器地址与「公益档」留待后续）

落地位置：`src-tauri/src/core/pair/manual.rs`（码的编解码 + STUN 清单校验）、
`manager.rs`（`SessionMode::Manual` / `ManualStatus` / 五个命令）、`p2p.rs`（`Gathered` 出码
与手工模式的 ICE 时限）、`src/pages/preference/components/pair/index.vue`。下面各节已按最终
代码回写；之后不再追加修订（后续改动写进提交信息）。

这份文档是 v1 的设计依据。旧的三份计划（`pair-plan.md` / `pair-plan-cloud-p2p.md` /
`pair-plan-multi-session.md`）已在开头标了「已完成」，只作历史记录，不再追加修订；本
文档承接它们之后的新功能。

## 1. 为什么需要「配对码」

STUN 只回答一个问题：**我自己的公网映射是什么**。它不传送对端的 SDP / ICE candidate，
也从不转发任何数据。握手要求双方先交换「怎么找到我」的信息，所以必须有一个**双方事先
都知道的会合点**：

- 中继（现在这套）——需要填服务器地址；
- 局域网广播——只能同一个 WiFi；
- **人肉转送（配对码）**——不需要任何服务器，跨公网，代价是网络变了要重新发一次码。

所以「只加免费 STUN 就能不填地址」在原理上不成立；配对码是「免费 + 跨公网 + 不依赖
任何服务器」的唯一组合。

## 2. 交互流程（两段码）

**出码方固定为 offerer**，角色一旦定下就不再交换（现有 glare 裁决用的是 deviceId 字典序，
见 `p2p.rs`，码路径不能沿用它，否则两边都会 `create_offer`）。

1. A 点「生成配对码」：建 `PeerConnection` + 数据通道 + offer + `set_local_description`
   → 等候选收集（最多 5 秒）→ 取 `local_description()`（候选已内嵌）→ **码 1**；
2. B 粘贴码 1 → `set_remote_description(offer)` → `create_answer` → 等收集 → **码 2**；
3. A 粘贴码 2 → `set_remote_description(answer)` → ICE 开始 → 通道就绪。

B 的连通性检查在收到码 1 时就已经开始（A 的候选在 offer 里），所以第 3 步通常 1 秒内连上。
两段就够：候选随 SDP 一次带全。

## 3. 码的格式

```
BGP1: + base64url(no-pad)( nonce(24B) + XChaCha20-Poly1305( deflate( JSON ) ) )
```

JSON 字段：`v` 版本、`k` `offer`/`answer`、`d` 出码方 deviceId、`sid` 会话随机 id、
`t` 出码时间戳、`f` 能力位（`["reliable-channel"]`）、`s` 序列化后的
`RTCSessionDescription`（含类型与全部候选，和中继那条信令的 `description` 是同一个东西）。

**整段码是加密的**，key 由配对密码 HKDF 派生（info = `bongocat-pair-code-v1`），前缀进
AEAD 的 AAD。这一条同时解决三件事：伪造 / 篡改的码解不开（真实性）、码里的内网与公网
地址对微信这类第三方不可见（隐私）、配对密码不一致时表现成「码打不开」而不是「连上之后
什么都不通」（可诊断）。讨论代理建议的 HMAC 方案能达到前两点，但要多一个 `hmac` 直接
依赖、且码里的地址是明文可见的；整段加密更简单也更保守，所以实现取了这一版。

体积：压缩后约 **400~650 字符**（不压缩约 1.2~1.5K），微信发文本没问题。解析吃掉所有
空白，所以换行/折叠不影响粘贴。二维码 v1 不做（两端都是电脑，扫码解决不了手机→电脑；
不压缩也装不下 QR v40-M）。

依赖：只新增一个直接依赖 `flate2`（默认 miniz_oxide 后端，纯 Rust，不引入 C 工具链）。
它本来就在依赖树里（png / tiff 那几路在用），所以不新增编译单元。加密用的是仓库里已有的
`chacha20poly1305`。

## 4. 必须堵掉的四个坑（讨论代理逐条核过 crate 源码）

1. **「等收集完成」必须自带超时**：STUN 无响应时 `rtc-stun` 重传 7 次（RTO 300ms 翻倍，
   尾部约 38 秒）后，gatherer 只写 `error!`、**不从表里移除那个 client**，于是
   `stun_clients` 永不为空、`gathering complete` 永远不来。等待策略：命令层最多等 5 秒，
   到点主动取 `local_description()` 快照（未完成时也会带上已收集的候选）。有 ≥1 个候选
   就出码并在 UI 注明「可能只在本机可达」，0 个候选不出码。
2. **手工模式没有 `hello`**：`peer_features` 为空 → 对端 `acceptable_lane("reliable")`
   返回 `None` → 聊天 / 附件 / 语音全废，只剩对方猫。**能力位必须放进码里**，由发起方
   显式喂给 `P2pLink`。
3. **跳过中继探针**：`live` 的中继心跳探针会在 2×心跳（120 秒）判超时 → `Outcome::Lost`
   → 整条会话重启。手工模式必须整段跳过，WS Ping 同理。
4. **手工模式不要另写一条会话 loop**：给 `live` 加 `&SessionConfig`、按 `SessionMode`
   分叉，只改四处——跳过 `read_welcome`（额度照 Cloudflare 版的缺省值）、ICE 服务器换成
   `spawn_manual` 时算好的本机清单、`P2pEvent::Gathered` 的出口改成 `publish_manual_code`
   （而不是把码发进中继）、跳过中继探针与 WS Ping。传输用 `BlackHole`（黑洞 sink + 永不
   产出的 stream）占位即可（`live` 的泛型只要求 `Sink + Stream`）。手工模式**不重试**：
   重开一轮就是新 offer，对方手里的码会作废，所以失败就停在 `failed`，让用户重新出码。

## 5. 状态机与 UI

Rust 侧权威持有 `ManualPhase`：出码方 `gathering → offer-ready（等对方交回码 2）→
joining → connected`，粘贴方 `gathering → answer-ready（把码 2 交回去）→ joining →
connected`；任一环节没打通是 `failed`。**没有独立的「已过期 / 已取消」状态**——「取消」
就是停掉这条会话，码跟着一起消失。状态挂在 `PairStatus.manual`（`ManualStatus { phase,
role, code, expiresAt, sessionId, candidates, nonHostCandidates, error }`）上，随现有状态
事件推送。

**回码只在 `offer-ready` 这一步收得下**（`apply_manual_answer` 按 phase 分情况回绝）：
粘贴框在连上之后仍然摆在那儿、按钮也仍然能点，而每一步的「该怎么办」都不一样——已经连上
时那句「重新出码」照着点会掐断一条正在用的会话，这一轮已经废了时把码交给死掉的腿则只会
让面板永远停在「正在打洞连接」。

角色只有两个值：`host`（本端出码 1）与 `guest`（本端粘码 1、回码 2）。`candidates` 是自己
那段 SDP 带了几条候选，**0 就不出码**（那种码对方也连不上）；`nonHostCandidates` 是其中
**不是 `host`** 的条数（`srflx` 才是 STUN 要到的外网映射），**0 时界面提醒「大概率只有
同一个网络里能连」**。判据用非 host 数而不是候选总数：多网卡的机器（有线 + 无线 + 虚拟
网卡 / VPN）上没有 STUN 也会有好几条 host 候选，只看总数会把一段跨网络连不上的码判成
正常。

两份码的有效期不同：码 1 十分钟，码 2 **三分钟**。对方粘回码 2 时，它那边的 ICE 可能已经
跑了近两分钟，所以码 2 的窗口要跟放宽后的手工 ICE 时限对齐，否则会出现「码还没过期、ICE
已经放弃」这种错配。

偏好页「连接」区新增一块：生成按钮 + 只读码框（复制）、粘贴框 + 「用这个码连接」、
状态徽标 + 倒计时 + 「重新出码 / 取消」，以及两条 Alert：① 码里含本机网络地址，别发到
公开场合；② 手工码不走服务器，连上之前对方猫 / 聊天 / 附件 / 语音都不可用。

**码不落盘、不进设置**：它是会话瞬时值，走设置的整份跨窗口同步会被别的窗口用旧值盖掉
（R40/R45 那个坑）。放 `runtime`，粘贴框用组件本地 ref。

手工模式的 ICE 时限单独放宽（`SettingEngineBuilder::with_ice_timeouts`，只对这条模式生效）：
disconnected 10 秒、failed 120 秒。中继模式不动——那条路上 10~30 秒打不通就该回落了，而
手工码没有可回落的地方，早判失败等于让用户白贴一次码。

## 6. 必然会退化的能力（UI 要如实表达）

| 能力                                               | 手工码模式                                                                                                    |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| 服务器回落（presence / 快照 / 统计 / 聊天 / 附件） | 全无，只有 DataChannel 一条腿                                                                                 |
| 对方是否在线                                       | 不能靠中继的 `server.peer`，必须改成「DC 探针通过了才算在线」                                                 |
| DC 就绪前的聊天 / 附件 / 语音                      | 界面灰化并说明「直连还没建立」，不排队静默丢                                                                  |
| 附件 / 语音                                        | route 只能钉 `Direct`（手工模式无条件建 reliable 通道，出码时还不知道对方能力位），且必须等 DC 就绪再发 offer |
| 断线 / 换网络                                      | 没有自动重连，要重新出码；旧码用换 `sid` 作废                                                                 |

`p2p.rs` 的退避重试在手工模式下不会有意义，也不会跑（重试条件是 `offerer && peer_ready`）。

「发不出去」在 Rust 侧挡两道：`send_chat` 与附件 / 语音的 `stage_attachment` 都先问
`manual_blocked()`（不是 `connected` 一律拒，并给出「等对方猫出现再发」这句人话）。附件那
道挡在 `stage_copy` 之前，顺带保住了临时 wav，不必再靠失败路径去删源文件。界面同步按死：
聊天窗口的输入框（`manualBlocked` 并入 `sendReady`）与猫咪浮层的麦克风（`disabled`），两处
都写上当前 phase 的人话。

「对方是否在线」在手工模式下只能由 DataChannel 的探针决定：`live` 的 DC 验证通过那一步才
`publish_peer(true)` 并把 phase 置成 `connected`。

## 7. 自定义公益 STUN（v1 一起做）

- **只在手工码这条路生效**。中继模式下 ICE 条目仍然只来自 `server.welcome` 的广告：本地
  清单与它**不合并**。`turn:` 的唯一来源必须是中继（本地只接受 `stun:`），一旦两条路都
  生效，用户填错一行就可能把自建中继的 TURN 顶掉，而「打不通」在界面上长得一样。
- 默认清单（2026-09-28 实测）：`stun.miwifi.com:3478` → `stun.hitv.com:3478` →
  `stun.chat.bilibili.com:3478` → `stun.douyucdn.cn:18000` →
  `stun1.douyucdn.cn:18000` → `stun.cloudflare.com:3478`。
  **`stun1.douyucdn.cn:3478` 两轮实测都超时，是死端口，不要写进清单。**
  `stun.l.google.com:19302` 不进默认（国内 UDP 常受干扰），只在帮助文案里当示例。
- 校验（`pair_validate_stun`，返回 `StunList`）：裸 `host:port` 自动补 `stun:`，缺端口按
  3478；`turn:` / `turns:` / `stuns:`（含 `stun://`）与 IPv6 字面量明确报错；主机名只留
  字母数字点横杠（挡住 `?transport=udp`、`user:pass@host` 这类只对 TURN 有意义的写法）；
  上限 6 条，**非法项不静默丢，而是指出第几行**。「填入默认清单」按钮走
  `pair_default_stun`。
- 存 `settings.relay.stunText`（多行文本，跟着设置跨窗口同步）：**留空 = 用内置默认清单**，
  这时界面把真正生效的清单列出来给用户看，免得「空着」被当成「没有 STUN」。
- STUN 超时**不拖慢**建连（非阻塞、候选边到边 trickle），所以不需要并发预算；真正决定
  「这轮废了」的是 ICE agent 的 25 秒失败超时。
- 隐私：`protocol.rs` 里「客户端自己不填任何公共 STUN」那段注释与 README 的「不引入额外
  数据上报」都跟着改了。现在的口径是「除自建服务器与可自选的公开 STUN 外不向第三方上报」。

## 8. 后续（不在 v1）

- **内置服务器地址 + 公益档**：`PAIR_PUBLIC_SERVER_PASSWORD`（服务端判定档位）、welcome 带
  `tier`、公益档只放行 `FrameKind::Ping`(8)（信令与保活同在这个 kind）、不广告 TURN、
  连接窗口 60 秒到点硬断（「按需连接」比空闲超时更彻底，「不占名额」本身不够）、公益档
  总量与每 IP 限额、反代下的真实 IP（默认不信任 `X-Forwarded-For`，IPv6 按 /64 归并）。
  Cloudflare 版明确不做。
- **IPv6 双栈**（`[::]:0`）：需要清单里有 AAAA 的 STUN 才有意义，当前默认清单全是 A 记录。

## 9. 验证方式

单测（纯函数，`cargo test --lib`）已经覆盖：码的往返（offer / answer）、码长够短、
**码里不出现地址与 SDP 明文**、另一对密码解不开、被改过的码被拒、前缀大小写与空白容错、
补零容错、空 / 乱码 / 超长输入被拒、过期被拒、时间戳在未来仍能解、版本不符被拒、两种码的
TTL、`sid` 的唯一与长度、每个错误码都有人话、**非 ASCII 粘贴不 panic**（`split_at` 按字节
切会崩，改成 `str::get` 之后拿 `你好` / emoji 钉住）；STUN 侧覆盖空清单回退默认、规范化与
去重、`turn:` 报错带行号、坏行指出第几行、超过上限报错且保留前六条；`p2p.rs` 里
`candidate_stats`（`a=candidate` 行的 `typ` 解析、多网卡全是 host、畸形行按 host 算）。

vitest（`node node_modules/vitest/vitest.mjs run`，既有 14 个文件 / 126 条）这一轮只是**回归**
——本次新增的偏好页面板逻辑（倒计时、按钮可用性、角色切换、STUN 逐行报错）**没有专门的
spec**，前端这块目前靠 `tsc` + review 兜着。真要补，先把 `manualCountdown`、
`manualFewCandidates` 这类纯计算搬到 `src/utils/` 再测（组件内部的 computed 不好单独测）。

本机已跑通：`cargo test --all-targets`（163 passed / 0 failed / 10 ignored）、
`tsc --noEmit`、`eslint src`、`vitest run`（126 passed）。

**只能真机双端验证**（本机只有一台机器）：真实 NAT 下的打洞、局域网无 STUN 的直连、
公共 STUN 的耗时与成功率、微信/QQ 实际传输与粘贴、换 WiFi 后的表现、偏好页跨窗口表现。
