# BongoCat 自建双人中继

一对用户部署一个实例。它只做三件事：

1. 用从 `PAIR_SECRET` 派生出的 `PAIR_AUTH_TOKEN` 鉴权；
2. 维持两条 WebSocket（固定两个人，第三个不同客户端拒绝）；
3. A ↔ B 转发帧，并把上下线状态用明文控制帧告知对方。

它**不保存**聊天记录、图片、语音、文件、输入统计，也不解析应用负载——只读 14 字节明文帧头做限流。线上契约与 `../server-cloudflare/` 一致（只多出 `server.welcome` 里**可选**的 `limits` / `iceServers` 字段，旧客户端会忽略）：同一份客户端只改 URL 就能在两者之间切换（已用同一套端到端用例在两边各跑一遍验证过）。

## 什么时候用自建版

| | Cloudflare 版 | 自建版 |
| --- | --- | --- |
| 成本 | 免费额度 | 一台最小云服务器 |
| 运维 | 零运维，不用域名 | `docker compose up -d` |
| 额度 | 每连接 30 帧/秒，另有每日请求数 | 自己定（可放开到 60 帧/秒） |
| 适合 | 想省事 | 想要自主、想上 60Hz、想顺带跑 STUN/TURN |

## 快速开始

前提：一台有公网 IP 的机器（Linux，装了 Docker）、一个域名、能开 80 与 443。

**机器与带宽建议**

| 场景 | 建议 |
| --- | --- |
| 猫咪状态 + 文字聊天 | 1 核 1GB / 2 Mbps |
| 再加语音、偶尔图片 | 2~3 Mbps |
| 图片 / 文件传输体验正常 | 5 Mbps |
| 经常传几十 / 几百 MB | 10 Mbps+ |

猫咪状态本身约几百字节 × 最高 60 次/秒，两个人合计可以忽略；真正吃带宽的只有文件。

**地域**：香港节点（腾讯云 / 阿里云轻量）可以避开大陆 ICP 备案；买中国大陆节点对外提供服务依法需要备案。跨境链路抖动大，这反而正是 P2P 收益最大的场景。

**步骤**

```bash
# 0. 在 server-relay 目录里操作（.env 与二进制路径都相对它）
cd server-relay

# 1. 生成一对密钥：PAIR_SECRET 双方共用，PAIR_AUTH_TOKEN 由中继自己派生
cargo run --release --bin generate-pair -- --write

# 2. 编辑 .env：PAIR_DOMAIN 填你的域名（PAIR_SECRET 上一步已经写好）

# 3. 起服务
docker compose up -d
```

`--write` 只在 `.env` 不存在时写入（不会覆盖你已经改过的配置）；已经存在时它会打印那行 `PAIR_SECRET=`，你自己粘进去即可。

客户端（双方填一样的值）：

```text
Relay URL:   https://cat.example.com     # 客户端会自己补 /ws
Pair Secret: <上一步显示的那串>
```

**证书是硬要求**：客户端用 `rustls-tls-webpki-roots` 校验，也就是**编译进客户端的那份 Mozilla 根证书库**（它**不看** Windows 的证书存储，自己往系统里装的根证书不起作用）。所以必须是**域名 + 受信任证书**：裸 IP、自签证书、只有 HTTP 的部署都会连不上（这一步最容易在「compose 起来了但连不上」时被忽略）。

**Secret 纪律**：`PAIR_AUTH_TOKEN` 不会被打印、不会落盘；`PAIR_SECRET` 只写进 `.env`（已 gitignore），不要写进命令行参数，避免进 shell 历史。不需要时删掉 `.env` 即可。

## 环境变量

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `PAIR_SECRET` | 无 | 与 `PAIR_AUTH_TOKEN` 二选一；给了它中继会自己派生 token（推荐） |
| `PAIR_AUTH_TOKEN` | 无 | 直接给派生结果（与 `../server-cloudflare` 用同一个值时可复用） |
| `PAIR_LISTEN` | `0.0.0.0:8080` | 监听地址 |
| `PAIR_MAX_FRAMES_PER_SECOND` | `30` | 每连接的帧额度，会通过 `server.welcome` 广告给客户端 |
| `PAIR_MAX_CHUNKS_PER_SECOND` | `20` | 附件分片额度 |
| `PAIR_MAX_BYTES_PER_SECOND` | `12582912` | 字节额度（12 MiB，要容得下 20 个 512 KiB 分片的突发） |
| `PAIR_STALE_AFTER_MS` | `120000` | 多久没消息的连接可以被新连接顶替（2 倍心跳） |
| `PAIR_ICE_SERVERS` | 无 | 可选，原样透传进 `server.welcome` 的 `iceServers`（见下） |

额度是「广告」而不是写死的常量：客户端会读 `server.welcome` 里的 `limits`，再**按 2/3 的比例**推导自己的出站速率（帧广告 30 → 客户端 20；分片广告 20 → 客户端 15），所以不会贴着上限发。想让 60Hz 跑在自己中继上，把 `PAIR_MAX_FRAMES_PER_SECOND` 调到 `90` 即可（客户端推导出 60）。

客户端只消费其中两维：`framesPerSecond`（应用帧）与 `chunksPerSecond`（附件分片）。`bytesPerSecond` 是中继自己的字节桶（保护它不被大帧打爆），客户端的字节速率由「分片大小 × 分片速率」决定，本来就低于它。三个维度各自独立判定：某个字段缺失或不是正数时只有它退回 CF 缺省，另外两维照常生效；超过上限（帧/分片 240、字节 64 MiB）的值会被夹住，出错的中继不能把客户端速率推成任意高。

## 可选：STUN/TURN

两个人都在家庭 NAT / 校园网 / CGNAT 后面时，WebRTC 打洞不一定成功，`coturn` 是成熟的开源兜底：

```bash
docker compose -f docker-compose.yml -f docker-compose.coturn.yml up -d
```

`turnserver.conf` 里有一份最小配置示例（记得改密码、配 TLS 证书、同时开 UDP 与 TCP/443）。起好之后把地址广告给客户端：

```yaml
PAIR_ICE_SERVERS: '[{"urls":["stun:cat.example.com:3478"]},{"urls":["turn:cat.example.com:3478"],"username":"bongo","credential":"..."}]'
```

隐私提醒：STUN 必然让第三方看到双方的公网 IP，这是 WebRTC 的固有特性。中继**默认不填**任何公共 STUN；填了就会被客户端使用。

## 端点

| 端点 | 说明 |
| --- | --- |
| `GET /health` | `{"ok":true,"protocol":1}`，鉴权之前就返回，不暴露任何配对状态 |
| `GET /ws` | WebSocket 升级端点 |

`/ws` 必须带这些头（与 Cloudflare 版一致）：

```http
Authorization: Bearer <PAIR_AUTH_TOKEN>
X-Bongo-Client: <deviceId>
X-Bongo-Protocol: 1
```

错误码：`401` 鉴权失败、`426` 协议不支持或不是 WebSocket 升级、`400` deviceId 非法、`404` 路径不对。

控制帧（`text` + JSON，服务端 → 客户端）：

```json
{ "type": "server.welcome", "protocol": 1, "peerOnline": false,
  "limits": { "framesPerSecond": 30.0, "chunksPerSecond": 20.0, "bytesPerSecond": 12582912.0 } }
{ "type": "server.peer", "online": true, "deviceId": "..." }
```

关闭码：`4002` 同一 deviceId 被新连接顶替、`4003` 配对已满、`4004` 陈旧连接被顶替、`1008` 帧格式错误 / 客户端发 text / 超限、`1009` 单帧超过 1 MiB、`1011` 服务端内部错误。

下面几处容易误解的行为都与 Cloudflare 版**逐条一致**，只有 `4004` 那一条是有意的差别（条目里写明）：

- **`4002`（同一 deviceId 重连）不发离线通知**：新的连接已经挂在注册表里，这条离线通知是伪广播，对方看到的是「同一个 deviceId 又上线了」。
- **`4004`（陈旧连接被顶替）会发一条离线通知**：存活方先收到「被顶替的那台离线」，再收到「新设备上线」，终态是在线。CF 版靠被顶替连接的 close 事件补这条（过滤只看同一 deviceId，不排除 4004），但它是在新连接被接受**之后**才补的，所以那边连**新连接自己**也会收到这条离线帧、反而会显示「对方离线」；自建版把通知放在新连接上线之前、只发给多出来的那一方，帧集合一致但不会踩这个坑。
- **`1009` 的边界**：1 MiB ~ 8 MiB 的帧会被完整读完再回 `1009`（干净关闭）；超过 8 MiB 时中继在读帧头时就知道太大，帧体还留在接收缓冲里，关连接会让 TCP 直接 RST，对端可能只看到「连接被重置」。两种情况对客户端是同一件事：重连。

## 开发与测试

```bash
cd server-relay
cargo test                                    # 单元 + 集成（含真实 WebSocket 的端到端）
cargo clippy --all-targets
cargo run                                     # 没配 PAIR_SECRET / PAIR_AUTH_TOKEN 时会打印该配哪些环境变量
```

和客户端联调（真中继的端到端用例）：

```powershell
# 1. 起中继（本地不需要 Caddy，直接 HTTP 即可）
$env:PAIR_SECRET = '<.env 里的值>'
$env:PAIR_LISTEN = '127.0.0.1:8798'
cargo run

# 2. 另一个终端跑客户端的端到端用例
$env:BONGO_PAIR_E2E_RELAY = 'http://127.0.0.1:8798'
$env:BONGO_PAIR_E2E_SECRET = '<同一个 Pair Secret>'
$env:BONGO_PAIR_HEARTBEAT_SECS = '2'
cargo test --manifest-path ../src-tauri/Cargo.toml --lib pair::e2e -- --ignored --nocapture
```

`../server-cloudflare/README.md` 是这套线上契约的来源；改这里之前先读它。
