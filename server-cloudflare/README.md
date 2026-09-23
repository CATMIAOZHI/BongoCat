# BongoCat Pair Relay（固定双人中继）

一对用户部署一个实例。中继只做三件事：

1. 用派生出的 `PAIR_AUTH_TOKEN` 鉴权；
2. 维持两条 WebSocket（固定两个人，第三个不同客户端拒绝）；
3. A ↔ B 转发帧，并把上下线状态用明文控制帧告知对方。

它**不保存**聊天记录、图片、语音、文件、输入统计——也不解析应用负载，只读 14 字节明文帧头做限流。

## 部署步骤

```bash
cd server-cloudflare
pnpm install
npx wrangler login
```

生成一对密钥（`PAIR_SECRET` 交给对方，`PAIR_AUTH_TOKEN` 只留在 Cloudflare）：

```bash
node scripts/generate-pair.mjs --deploy
```

- 脚本会显示一次 `PAIR_SECRET` 和一个指纹，方便你和对方核对是否填了同一个值。
- 派生出的 `PAIR_AUTH_TOKEN` **不会**被打印、不会被写入文件，而是通过 stdin 管道直接交给 `wrangler secret put`（不进 shell history）。
- 加 `--write` 才会把 `PAIR_SECRET` 写入本地的 `pair-secret.txt`（已在 `.gitignore` 中忽略），不需要时请自行删除。

部署：

```bash
pnpm deploy
```

输出里的地址就是双方要填的 Relay URL，例如 `https://bongocat-pair-relay.<账号>.workers.dev`。客户端填 `https://` 或 `wss://` 都可以（客户端会自己补成 `/ws` 并转成 WebSocket 地址）。

## 端点

| 端点 | 说明 |
| --- | --- |
| `GET /health` | 返回 `{ "ok": true, "protocol": 1 }`，不暴露任何配对状态 |
| `GET /ws` | WebSocket 升级端点 |

`/ws` 必须带这些头：

```http
Authorization: Bearer <PAIR_AUTH_TOKEN>
X-Bongo-Client: <deviceId>
X-Bongo-Protocol: 1
```

错误码：`401` 鉴权失败，`426` 协议不支持或不是 WebSocket 升级，`400` deviceId 非法。部署后如果**所有人**都是 `401`，通常是 `PAIR_AUTH_TOKEN` 没设置成功（此时是 fail-closed）。

## 协议

客户端 → 服务端的应用帧：只接受 `binary`。客户端发 `text` 帧属于协议错误，中继直接 `close 1008`——`text` 只留给服务端控制帧，否则已配对的一方能伪造下面的 `server.*`。

```text
kind(1B) | flags(1B) | transferId(8B) | seq(4B) | nonce(24B) | ciphertext + tag
└──────────── 14 字节明文帧头 ────────┘
```

`kind`：`1` pet · `2` presence · `3` stats · `4` chat · `5` transfer control · `6` transfer chunk · `7` ack · `8` ping。

帧头是**明文**（中继靠它分桶限流），但客户端必须把它作为 AEAD 的 associated data 参与认证——否则中转方可以改 `kind` 绕开限流。分桶只是防误用，不是安全边界。

服务端 → 客户端的控制帧：`text` + JSON（每条都很小）。

```json
{ "type": "server.welcome", "protocol": 1, "peerOnline": false }
{ "type": "server.peer", "online": true, "deviceId": "..." }
```

`server.error` 是预留类型，当前实现不会发送（错误都用关闭码表达）。客户端收到未知的 text 类型应当忽略，不要当成对端消息。

关闭码：

| 码 | 含义 |
| --- | --- |
| `4002` | 同一 deviceId 的新连接顶替了旧连接 |
| `4003` | 配对已满（第三个不同客户端） |
| `4004` | 长时间无活动的旧连接被顶替 |
| `1008` | 帧格式错误 / 客户端发 text / 超过限流 |
| `1009` | binary 单帧超过 1 MiB |
| `1011` | 服务端内部错误（例如缺少连接 attachment） |

## 限制与配额

- 单帧：binary ≤ 1 MiB（按整帧计，含帧头与 nonce/tag）。
- 限流：每个连接 30 帧/秒、20 个 transfer chunk/秒、12 MiB/秒（令牌桶：容量就是这三个上限，按时间连续补充，超出即 `close 1008`）。12 MiB 是为了让 20 个 512 KiB chunk 的突发（合计约 10 MiB）合法。
- 免费额度：Durable Object 每天 10 万次请求 + 13000 GB-s。**收到的 WebSocket 消息按 20 条 = 1 次请求计**，打开一次连接也算 1 次请求，出站不计费。按客户端 ≤ 3Hz 估算约 1.3 万次请求/天，余量充足。
- 使用 WebSocket Hibernation：连接空闲时 Durable Object 休眠，不产生 duration 计费，也不占用内存。服务端不使用 `setInterval` / `setTimeout`。

## 开发与测试

```bash
pnpm dev        # 本地 wrangler dev，用 .dev.vars 提供 PAIR_AUTH_TOKEN
pnpm test       # vitest + @cloudflare/vitest-pool-workers（真实 workerd 里跑）
pnpm typecheck  # tsc --noEmit
pnpm cf-typegen # wrangler 配置变化后重新生成 worker-configuration.d.ts
pnpm pair:selftest # Node 侧核对生成脚本的派生结果与 Rust/Workers 测试向量一致
```

本地开发时在 `.dev.vars` 里放一个测试用 token（该文件已在 `.gitignore` 中忽略）：

```text
PAIR_AUTH_TOKEN=dev-token
```

测试覆盖：鉴权（缺 token / 错 token / 多余段落的 Bearer / 协议不符 / deviceId 非法）、`/health`、双人上限、同 deviceId 重连顶替（且不广播伪离线）、deviceId 大小写归一、陈旧连接顶替（4004）、二进制只转发给对端、客户端 text 帧被拒、断开后通知对端、超大/畸形/未知 kind 帧、限流正反例、20 个 512 KiB chunk 突发不被误关。
