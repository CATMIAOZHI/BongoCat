# BongoCat 自建双人中继（多会话）

一套服务器同时承载多个双人会话。服务器**没有、也不需要**任何联机密钥：它只做四件事：

1. 按客户端给的 `X-Bongo-Room`（从联机密钥派生出来的 `ROOM_ID`）把连接划进各自的会话；
2. 维持每个会话两条 WebSocket（每组固定两台设备）；
3. 在**同一个会话内部**转发帧，并把上下线状态用明文控制帧告知对方；
4. 按 `PAIR_MAX_SESSIONS` 限制同时存在的会话数。

它**不保存**聊天记录、图片、语音、文件、输入统计，也不解析应用负载——只读 14 字节明文头做限流。服务器既拿不到原始联机密钥，也拿不到 E2EE 根密钥：鉴权用的是 `SHA256(AUTH_TOKEN)`，Room 只是一个分组键。

线上契约与 `../server-cloudflare/` 一致，差异是：`server.welcome` 里**可选**的 `limits` / `iceServers`、`/health` 里**可选**的 `"mode":"multi-pair"`、新增的 `X-Bongo-Room` 请求头，以及容量满时的 **HTTP 503**（CF 一个部署只服务一对用户，永远不会返回它）。

**升级顺序（先看这张表，别先把服务器升了）**：

| 客户端 | 连本版自建中继 | 连 Cloudflare 版 |
| --- | --- | --- |
| 新客户端（带 `X-Bongo-Room`） | 可以 | 可以（那边忽略未知头） |
| 旧客户端（不带这个头） | **连不上，HTTP 400**（`invalid room id`） | 可以 |

本版中继**要求**这个头：它是会话的分组键，没有它服务器无法知道该把连接放进哪一桌。所以自建管理员升级服务器前请先确认两台设备上的客户端都已更新——旧客户端只会看到一句**指错方向**的 400（它说的是「deviceId 被中继拒绝」，其实是缺分组键），新客户端才会说「服务器拒绝了这次连接：请检查服务器地址与联机密钥是否和对方完全一致」。

## 什么时候用自建版

| | Cloudflare 版 | 自建版 |
| --- | --- | --- |
| 成本 | 免费额度 | 一台最小云服务器 |
| 运维 | 零运维，不用域名 | `docker compose up -d` |
| 多会话 | 一个部署只服务一对用户 | **一套服务器 20 组同时在线**（可调） |
| 额度 | 每连接 30 帧/秒，另有每日请求数 | 自己定（可放开到 60 帧/秒） |
| TLS | 自带 | 域名 + Caddy，或裸 IP 明文模式 |
| 适合 | 想省事 | 想要自主、想上 60Hz、想顺带跑 STUN/TURN |

## 快速开始（推荐：域名 + 自动 HTTPS）

前提：一台有公网 IP 的机器（Linux，装了 Docker）、一个域名、能开 80 与 443。

**机器与带宽建议**

| 场景 | 建议 |
| --- | --- |
| 猫咪状态 + 文字聊天 | 1 核 1GB / 2 Mbps |
| 再加语音、偶尔图片 | 2~3 Mbps |
| 图片 / 文件传输体验正常 | 5 Mbps |
| 经常传几十 / 几百 MB | 10 Mbps+ |

猫咪状态本身约 350 字节 × 最高 60 次/秒，两个人合计可以忽略；真正吃带宽的只有文件。带宽是按**同时在线的人数**算的，多会话本身几乎不额外占带宽。

**地域**：香港节点（腾讯云 / 阿里云轻量）可以避开大陆 ICP 备案；买中国大陆节点对外提供服务依法需要备案。跨境链路抖动大，这反而正是 P2P 收益最大的场景。

**步骤**

```bash
cd server-relay
cp .env.example .env     # 只需要填 PAIR_DOMAIN，以及想改的容量/额度
docker compose up -d
```

客户端（**两个人填一样的值**）：

```text
服务器地址:  https://cat.example.com     # 客户端会自己补 /ws
联机密钥:    <双方约定或现场生成的那串>
```

密钥不需要服务器参与：任一方在设置页点「生成联机密钥」，把生成的那串发给对方即可（也可以用 `cargo run --release --bin generate-pair` 生成）。两个人都填同一个值就会进入同一个会话；第三台设备用同一个密钥会被拒绝（「该联机会话已有两台设备在线」）。

**证书是硬要求（仅这一种模式）**：客户端用 `rustls-tls-webpki-roots` 校验，也就是**编译进客户端的那份 Mozilla 根证书库**（它**不看** Windows 的证书存储，自己往系统里装的根证书不起作用）。所以走域名时必须用**受信任证书**：自签证书连不上。裸 IP 请用下面的 direct 模式（明文 `ws://`），不要对着裸 IP 试 `https://`。

## 裸 IP 模式（没有域名 / 临时测试）

```bash
cd server-relay
docker compose -f docker-compose.direct.yml up -d
```

客户端「服务器地址」填 `<公网 IP>:8080`（客户端会自动识别成 `ws://<IP>:8080/ws`），或者显式填 `http://<IP>:8080`。

⚠️ 这个模式**没有 TLS**：握手与信令（WebSocket 头、上下线控制帧）是明文。聊天、附件与桌宠状态本身仍然是端到端加密的（E2EE，服务器解不开），客户端也会显示一条同样的提醒。别把这种部署当成长期方案。

## 环境变量

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `PAIR_MAX_SESSIONS` | `20` | 同时承载的双人会话数。**只挡新会话**：已有会话的第二个人加入、同 deviceId 重连都不受影响；会话空了立刻释放名额。必须 ≥ 1 |
| `PAIR_LISTEN` | `0.0.0.0:8080` | 监听地址 |
| `PAIR_MAX_FRAMES_PER_SECOND` | `30` | 每连接的帧额度，会通过 `server.welcome` 广告给客户端（缺省 30 是为了与 CF 一致；compose 里默认给到 90 → 客户端 60Hz） |
| `PAIR_MAX_CHUNKS_PER_SECOND` | `20` | 附件分片额度 |
| `PAIR_MAX_BYTES_PER_SECOND` | `12582912` | 字节额度（12 MiB，要容得下 20 个 512 KiB 分片的突发） |
| `PAIR_STALE_AFTER_MS` | `120000` | 多久没消息的连接可以被同会话的新连接顶替（2 倍心跳） |
| `PAIR_ICE_SERVERS` | 无 | 可选，原样透传进 `server.welcome` 的 `iceServers`（见下） |
| `PAIR_DOMAIN` | 无 | 只给 Caddy 用（`docker-compose.yml`）；direct 模式不需要 |

额度是「广告」而不是写死的常量：客户端会读 `server.welcome` 里的 `limits`，再**按 2/3 的比例**推导自己的出站速率（帧广告 30 → 客户端 20；分片广告 20 → 客户端 15），所以不会贴着上限发。想让 60Hz 跑在自己中继上，把 `PAIR_MAX_FRAMES_PER_SECOND` 调到 `90` 即可（客户端推导出 60）。

客户端只消费其中两维：`framesPerSecond`（应用帧）与 `chunksPerSecond`（附件分片）。`bytesPerSecond` 是中继自己的字节桶（保护它不被大帧打爆），客户端的字节速率由「分片大小 × 分片速率」决定，本来就低于它。三个维度各自独立判定：某个字段缺失或不是正数时只有它退回 CF 缺省，另外两维照常生效；超过上限（帧/分片 240、字节 64 MiB）的值会被夹住，出错的中继不能把客户端速率推成任意高。

**容量会被陌生人占用（自建管理员需要知道）**：中继不认人，任何知道地址的人都能用一串随机的联机密钥开一个新会话、把两条连接挂着不做事。占满 `PAIR_MAX_SESSIONS` 之后，其他人拿到的是 503（直到那些连接断开）。这是「服务器不持有任何共享凭据」的必然结果，中继也没有空闲超时——旧的单会话版同样没有超时，只是那时占满一整桌就等于把唯一的会话抢走，所以这件事只有多会话之后才明显。**`PAIR_MAX_SESSIONS` 调小并不挡占位**（陌生人占更少的桌就能把你填满，调小只是限制资源占用）；要挡占位只能在前置网关上只放行你自己的 IP，或给中继加空闲超时。

## 多会话是怎么算的

```text
                    BongoCat Relay
                 PAIR_MAX_SESSIONS=20
                          |
          +---------------+---------------+ ...
          |               |
       Room A          Room B
      Secret A        Secret B
       /    \          /    \
     A1      A2      B1      B2
       \    /          \    /
       WebRTC          WebRTC
         P2P             P2P
```

- 会话由 `ROOM_ID = HKDF-SHA256(联机密钥, "bongocat-pair-room-v1")` 决定：同一个密钥 → 同一个会话。
- 每个会话最多 2 个不同 `deviceId`；第三台收到 `4003`。同一个 `deviceId` 重连是**顶替**（`4002`），不是第三台。
- 会话完全在内存里：所有人退出就删掉，服务器重启后全部消失（客户端会自动重连并重新建会话）。没有数据库、没有持久化。
- 转发、上下线公告、顶替、停摆摘除**全部**只在自己的会话内发生，互不可见。

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
| `GET /health` | `{"ok":true,"protocol":1,"mode":"multi-pair"}`，鉴权之前就返回，不暴露任何会话信息 |
| `GET /ws` | WebSocket 升级端点 |

`/ws` 必须带这些头（前三个与 Cloudflare 版一致，第四个是本版新增的分组键）：

```http
Authorization: Bearer <PAIR_AUTH_TOKEN>
X-Bongo-Room: <ROOM_ID>
X-Bongo-Client: <deviceId>
X-Bongo-Protocol: 1
```

错误码：`401` 联机密钥不对（同一会话上摘要对不上，或没带 Authorization）、`503` 服务器会话已满（**只针对新会话**）、`426` 协议不支持或不是 WebSocket 升级、`400` deviceId 或 `ROOM_ID` 非法、`404` 路径不对。

控制帧（`text` + JSON，服务端 → 客户端）：

```json
{ "type": "server.welcome", "protocol": 1, "peerOnline": false,
  "limits": { "framesPerSecond": 30.0, "chunksPerSecond": 20.0, "bytesPerSecond": 12582912.0 } }
{ "type": "server.peer", "online": true, "deviceId": "..." }
```

关闭码：`4002` 同一 deviceId 被新连接顶替、`4003` 该会话已有两台设备在线、`4004` 陈旧连接被顶替、`1008` 帧格式错误 / 客户端发 text / 超限、`1009` 单帧超过 1 MiB、`1011` 服务端内部错误。注意「服务器满员」走的是 HTTP `503` 而不是关闭码：客户端要能把它显示成「服务器双人联机会话已满，请稍后再试」，而不是一串协议码。

## 日志与隐私

- 日志里只出现**会话指纹**（`SHA256(ROOM_ID)` 的前 8 字节）与 `deviceId`；完整 `ROOM_ID`、`AUTH_TOKEN`、联机密钥、聊天内容一律不写。
- 服务器只在内存里保存 `SHA256(AUTH_TOKEN)`，不保存明文 token。
- README 承诺的「不收集任何用户数据」在这里同样成立：中继不做任何统计上报，只做转发。

## 与 Cloudflare 版的差异（逐条）

**真正的差别**是 `4004`、多会话、停摆摘除，以及 `1009` 里「超过 8 MiB」的那一段；`4002` 与 `1009` 的 1 MiB ~ 8 MiB 段其实与 CF 版**逐条一致**，一并列在这里说明：

- **`4002`（同一 deviceId 重连）不发离线通知**：新的连接已经挂在会话里，这条离线通知是伪广播，对方看到的是「同一个 deviceId 又上线了」。
- **`4004`（陈旧连接被顶替）会发一条离线通知**：存活方先收到「被顶替的那台离线」，再收到「新设备上线」，终态是在线。CF 版靠被顶替连接的 close 事件补这条（过滤只看同一 deviceId，不排除 4004），但它是在新连接被接受**之后**才补的，所以那边连**新连接自己**也会收到这条离线帧、反而会显示「对方离线」；自建版把通知放在新连接上线之前、只发给多出来的那一方，帧集合一致但不会踩这个坑。
- **`1009` 的边界**：1 MiB ~ 8 MiB 的帧会被完整读完再回 `1009`（干净关闭）；超过 8 MiB 时中继在读帧头时就知道太大，帧体还留在接收缓冲里，关连接会让 TCP 直接 RST，对端可能只看到「连接被重置」。两种情况对客户端是同一件事：重连。
- **多会话**：CF 版一个部署仍然只服务一对用户（忽略 `X-Bongo-Room`），所以同一个 URL 可以继续用，只是不会有第二组人。
- **停摆对端会被摘掉**：两条路径。转发应用帧时最多等 30 秒（`FORWARD_TIMEOUT`）——等满说明那个对端的出站队列（32 帧）已经塞死、消费不动，于是把**那一条连接**移出会话；广播控制帧（上下线）走的是 `try_send`，**队列一满当场就摘**，没有 30 秒宽限。两种情况都只摘那一条连接，另一个人会看到「对方离线」并重连。CF 版没有这两条（它只往 socket 里写、不等也不摘），所以一个卡死的对端在那边会一直占着位置——这是自建的额外兜底，代价是它对「网络很慢」比 CF 版更不宽容。

## 开发与测试

```bash
cd server-relay
cargo test                                    # 单元 + 集成（含真实 WebSocket 的端到端）
cargo clippy --all-targets
cargo run                                     # 不需要任何密钥：没有 PAIR_SECRET 这一项了
```

`cargo run` 会打印监听地址、会话上限与额度。本地联调不需要 Caddy，直接 HTTP 即可。

和客户端联调（真中继的端到端用例）：

```powershell
# 1. 起中继（本地不需要 Caddy，直接 HTTP 即可；多会话用例要求 PAIR_MAX_SESSIONS >= 2）
$env:PAIR_LISTEN = '127.0.0.1:8798'
$env:PAIR_MAX_SESSIONS = '20'
cargo run

# 2. 另一个终端跑客户端的端到端用例（含多会话隔离）
$env:BONGO_PAIR_E2E_RELAY = 'http://127.0.0.1:8798'
$env:BONGO_PAIR_E2E_SECRET = 'AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8'
$env:BONGO_PAIR_HEARTBEAT_SECS = '2'
cargo test --manifest-path ../src-tauri/Cargo.toml --lib pair::e2e -- --ignored --nocapture
```

`../server-cloudflare/README.md` 是这套线上契约的来源；改这里之前先读它。
