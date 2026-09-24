# BongoCat 自建双人中继（多会话）

一套服务器同时承载多个双人会话。服务器**没有、也不需要**任何配对密码：它只做五件事：

1. 按客户端给的 `X-Bongo-Room`（从配对密码派生出来的 `ROOM_ID`）把连接划进各自的会话；
2. 维持每个会话两条 WebSocket（每组固定两台设备）；
3. 在**同一个会话内部**转发帧，并把上下线状态用明文控制帧告知对方；
4. 按 `PAIR_MAX_SESSIONS` 限制同时存在的会话数。
5. 用**服务器密码**（`PAIR_SERVER_PASSWORD`）挡住不认识的人——见下一节。

它**不保存**聊天记录、图片、语音、文件、输入统计，也不解析应用负载——只读 14 字节明文头做限流。服务器既拿不到原始配对密码，也拿不到 E2EE 根密钥：鉴权用的是 `SHA256(AUTH_TOKEN)`，Room 只是一个分组键。

## 两种凭据，别搞混

| | 谁设置 | 作用 | 填在哪 |
| --- | --- | --- | --- |
| **服务器密码** | 部署服务器的人（`.env` 的 `PAIR_SERVER_PASSWORD`） | 决定**谁能用这台服务器**。没有它连握手都过不去（HTTP 403），也就拿不到 `server.welcome` 里的 TURN 凭据 | 客户端「服务器密码」，所有用这台服务器的人都填同一个值 |
| **配对密码** | 每一对用户自己（客户端点「生成配对密码」） | 决定**谁是同一对**。填同一个值的两个人进同一个双人会话，也是 E2EE 密钥材料 | 客户端「配对密码」，两个人填同一个值 |

服务器密码是**必填**的：省掉它，任何知道地址的人都能开一个自己的会话、把连接挂着占位，并且顺手拿走你 `welcome` 里广告的 TURN 凭据（那是按流量计费的）。中继启动时只会读它一次、只保留摘要，进程里不会留下密码原文。

线上契约与 `../server-cloudflare/` 一致，差异是：`server.welcome` 里**可选**的 `limits` / `iceServers`、`/health` 里**可选**的 `"mode":"multi-pair"` 与 `"passwordRequired":true`、新增的 `X-Bongo-Room` / `X-Bongo-Server` 请求头、**HTTP 403**（服务器密码不对，CF 永远不会返回它），以及容量满时的 **HTTP 503**（CF 一个部署只服务一对用户，也永远不会返回它）。

**升级顺序（先看这张表，别先把服务器升了）**：

| 客户端 | 连本版自建中继（多会话 + 服务器密码） | 连改造前的老版本（单会话、无分组键） | 连 Cloudflare 版 |
| --- | --- | --- | --- |
| 新客户端（带 `X-Bongo-Room`，填了服务器密码） | 可以 | 可以（旧版忽略未知头） | 可以（那边忽略未知头） |
| 新客户端，**没填**服务器密码 | **连不上，HTTP 403**（`server password required`） | 可以 | 可以 |
| 旧客户端（既没有分组键也没有服务器密码） | **连不上，HTTP 403** | 可以 | 可以 |

如果你的自建中继是**上一个小版本**（已经支持多会话、但还没有服务器密码）：新客户端三格都能连（它忽略未知的 `X-Bongo-Server` 头），旧客户端会被 **400** 挡住（那一版就已经要求分组键了）；那一版的 `welcome` 带 `limits`，所以 60Hz 正常。

两个头本版都**要求**：`X-Bongo-Room` 是会话的分组键（没有它服务器不知道该把连接放进哪一桌），`X-Bongo-Server` 是服务器门槛。所以自建管理员升级服务器前请先确认两台设备上的客户端都已更新、并且都填了服务器密码——旧客户端只会看到一句指错方向的错误（它把 403 显示成「服务器返回 HTTP 403，稍后自动重试」），新客户端才会说「服务器密码不正确或还没填：请向部署这台服务器的人索取」。

反过来，**升级服务器前**客户端先升到新版、服务器还是**改造前的老版本**（单会话、welcome 不带 `limits`）时，功能全部可用，只有一处变化：客户端按 CF 缺省推导出 20 帧/秒（< 60），所以桌宠快照会临时退回 **3Hz**（看起来「猫变卡了」）。把服务器升上去就会自动恢复 60Hz。（上一个小版本带 `limits`，不在此列。）

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
| 猫咪状态 + 文字聊天（1~2 组人） | 1 核 1GB / 2 Mbps |
| 再加语音、偶尔图片 | 2~3 Mbps |
| 图片 / 文件传输体验正常 | 5 Mbps |
| 经常传几十 / 几百 MB | 10 Mbps+ |

猫咪状态一帧的明文 JSON 约 350 字节、**整帧约 400 字节**（外面还有 14 字节帧头 + 24 字节 nonce + 16 字节 tag），最高 60 次/秒，两个人合计约 200 kbps —— 可以忽略；真正吃带宽的只有文件。带宽是按**同时在线的人数**算的，多会话本身几乎不额外占带宽。

**内存：1GB 只够几组人**。一条连接最坏要占「32 帧出站队列 + 4 MiB 写缓冲 + 最多 8 MiB 读帧」≈ 44 MiB（8 MiB 那段是 `1009` 契约要的余量），20 组满了就是 1.7 GiB 量级。1GB 的机器请把 `PAIR_MAX_SESSIONS` 调到个位数（比如 4）；2GB 可以留 10~20。真被灌满时的表现是 OOM、所有会话一起断（容器会自己重启）。

**地域**：香港节点（腾讯云 / 阿里云轻量）可以避开大陆 ICP 备案；买中国大陆节点对外提供服务依法需要备案。跨境链路抖动大，这反而正是 P2P 收益最大的场景。

**步骤**

```bash
cd server-relay
cargo run --release --bin generate-pair -- --server   # 先生成服务器密码，再填进下面的 .env
cp .env.example .env     # 填 PAIR_DOMAIN、PAIR_SERVER_PASSWORD，以及想改的容量/额度
docker compose up -d     # 改过 .env 之后都要重新 up -d；restart 不会重读 .env
```

服务器密码必须是**至少 16 个字符**的任意文本（中继启动时会校验）；**没装 Rust** 就用第二条：

```bash
openssl rand -base64 24                               # 任何 ≥16 个字符的密码都行
```

**把下面这三个值一起发给对方**（一个人部署好服务器之后，两个人填一样的值）：

```text
服务器地址:  https://cat.example.com     # 客户端会自己补 /ws
服务器密码:  <服务器 .env 里 PAIR_SERVER_PASSWORD 的值>
配对密码:    <双方约定或现场生成的那串>
```

配对密码不需要服务器参与：任一方在设置页点「生成配对密码」，把生成的那串发给对方即可（也可以用 `cargo run --release --bin generate-pair` 生成）。两个人都填同一个值就会进入同一个会话；第三台设备用同一个密码会被拒绝（「该联机会话已有两台设备在线」）。

**连不上时按这三步自查**（这三步能解决绝大多数「容器是 Up 但客户端一直转圈」）：

1. **云控制台的安全组放行 80 与 443**（Caddy 要用 80 申请证书；direct 模式放行 TCP 8080）。默认安全组往往只开了 22 / 3389。装了 coturn 的话还要放行 **3478/UDP + 3478/TCP、5349/TCP、49160~49260/UDP**；coturn 用的是 host 网络，Docker 的 iptables 规则**不会**替它放行，机器上开着 ufw / firewalld 时也要在那里放行（否则日志一切正常、打洞一直失败）。
2. **域名 A 记录指到这台机器的公网 IP**，`ping <域名>` 或 `nslookup <域名>` 看到的是你的服务器地址。
3. `curl https://<你的域名>/health` 应当打印 `{"ok":true,"protocol":1,"mode":"multi-pair","passwordRequired":true}`。看不到就说明还没到中继这一层：`docker compose logs caddy`（证书有没有申请成功）与 `docker compose logs relay`（中继起来没有、有没有拒绝记录）。

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
| `PAIR_SERVER_PASSWORD` | **无（必填）** | 服务器密码：决定「谁能用这台服务器」。≥ 16 个字符，客户端要填同一个值；缺了它中继会拒绝启动 |
| `PAIR_MAX_SESSIONS` | `20` | 同时承载的双人会话数。**只挡新会话**：已有会话的第二个人加入、同 deviceId 重连都不受影响；会话空了立刻释放名额。必须 ≥ 1 |
| `PAIR_LISTEN` | `0.0.0.0:8080` | 监听地址（容器健康检查按它的主机去探 `/health`，所以绑具体网卡也能正确判定）。**一般别改**：改成 `127.0.0.1:8080` 会让 Caddy 连不上上游（502，它是从另一个容器连 `relay:8080` 的）；改端口还要同时改 Caddy 的上游（域名模式）或 `docker-compose.direct.yml` 里写死的 `ports: "8080:8080"`（direct 模式）。这两种改坏的共同症状都是「健康检查 healthy、客户端连不上」 |
| `PAIR_MAX_FRAMES_PER_SECOND` | `30` | 每连接的帧额度，会通过 `server.welcome` 广告给客户端（缺省 30 是为了与 CF 一致；compose 里默认给到 90 → 客户端 60Hz） |
| `PAIR_MAX_CHUNKS_PER_SECOND` | `20` | 附件分片额度 |
| `PAIR_MAX_BYTES_PER_SECOND` | `12582912` | 字节额度（12 MiB，要容得下 20 个 512 KiB 分片的突发） |
| `PAIR_STALE_AFTER_MS` | `120000` | 多久没消息的连接可以被同会话的新连接顶替（2 倍心跳） |
| `PAIR_ICE_SERVERS` | 无 | 可选，原样透传进 `server.welcome` 的 `iceServers`（见下） |
| `PAIR_DOMAIN` | 无 | 只给 Caddy 用（`docker-compose.yml`）；direct 模式不需要 |

表里的每一项都由两份 compose 透传给容器（`.env` 是给 compose 做变量替换用的，容器**不读** `.env` 文件本身）：想改哪一项就改 `.env`，改完 `docker compose up -d` 重建容器。唯一要注意的是 `PAIR_ICE_SERVERS` 那种 JSON 值**必须整行写、不要加引号**（compose 里才需要引号）。

额度是「广告」而不是写死的常量：客户端会读 `server.welcome` 里的 `limits`，再**留出余量**地推导自己的出站速率——**帧取 2/3**（广告 30 → 客户端 20、广告 90 → 客户端 60），**分片取 3/4**（广告 20 → 客户端 15），所以不会贴着上限发。想让 60Hz 跑在自己中继上，把 `PAIR_MAX_FRAMES_PER_SECOND` 调到 `90` 即可（客户端推导出 60；**设成 60 只有 40 → 仍然是 3Hz**）。

客户端只消费其中两维：`framesPerSecond`（应用帧）与 `chunksPerSecond`（附件分片）。`bytesPerSecond` 是中继自己的字节桶（保护它不被大帧打爆），客户端的字节速率由「分片大小 × 分片速率」决定，本来就低于它。三个维度各自独立判定：某个字段缺失或不是正数时只有它退回 CF 缺省，另外两维照常生效；超过上限（帧/分片 240、字节 64 MiB）的值会被夹住，出错的中继不能把客户端速率推成任意高。

**服务器密码挡住了陌生人，但挡住不了「手里有密码的人」**：服务器密码是一道共享门槛，凡是拿到它的人都能开新会话、把连接挂着占位（占满 `PAIR_MAX_SESSIONS` 之后其他人拿到 503），也能拿走 `welcome` 里的 TURN 凭据。所以：把密码只发给要用的人；万一漏出去了，换掉它（所有人都要重填一次）就是最快的止血——改完 `.env` **要 `docker compose up -d` 重建容器**，`docker compose restart` 不会重读 `.env`。中继**没有**空闲超时（旧的单会话版也没有），所以「挂着不做事占位」这件事在门槛之内仍然存在——真要防，就在前置网关上只放行你自己的 IP。

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

- 会话由 `ROOM_ID = HKDF-SHA256(配对密码, "bongocat-pair-room-v1")` 决定：同一个密钥 → 同一个会话。
- 每个会话最多 2 个不同 `deviceId`；第三台收到 `4003`。同一个 `deviceId` 重连是**顶替**（`4002`），不是第三台。
- 会话完全在内存里：所有人退出就删掉，服务器重启后全部消失（客户端会自动重连并重新建会话）。没有数据库、没有持久化。
- 转发、上下线公告、顶替、停摆摘除**全部**只在自己的会话内发生，互不可见。

## 可选：STUN/TURN

两个人都在家庭 NAT / 校园网 / CGNAT 后面时，WebRTC 打洞不一定成功，`coturn` 是成熟的开源兜底：

```bash
docker compose -f docker-compose.yml -f docker-compose.coturn.yml up -d
```

`turnserver.conf` 里有一份最小配置示例（选项集是按 coturn **4.18** 核过的，`coturn/coturn:latest` 现在就是这一版；将来镜像大版本升级时值得再核一遍），上线前记得改这几处：`user=` 的密码、`external-ip`（**写公网 IP 字面量**；官方只支持 IP，写域名会被解析一次，解析到 CDN 边缘 IP 或解析失败（那样 `external-ip` 会被清空、报出去的是内网地址）都会坏。云主机在 NAT 后面时写 `<公网IP>/<内网IP>`）、`realm` / `server-name`（不改也能用，但日志里会一直出现示例域名）；要 TLS 再配证书。**它是被 git 跟踪的文件，改完别提交**（真实密码会进仓库）。改完要让它生效：`docker compose -f docker-compose.yml -f docker-compose.coturn.yml restart coturn`（改的是挂载进去的文件，`up -d` 不会重启它）。

**这些上限也是正常用量的天花板**：`user-quota` / `total-quota` 是按**整台服务器**算的（coturn 只有一张静态凭据，`iceServers` 是统一广告的，没法按组发不同凭据），每一组双人会话约占 2 个分配，所以默认给的 64 / 128 是配 `PAIR_MAX_SESSIONS=20` 的；把组数调大时这几个数与 `min-port`~`max-port`（还有安全组里对应的放行）都要跟着放大，不然「双方都在大内网、只能靠 TURN」的那几组会拿不到分配（同样表现为日志正常、打洞失败）。默认那 101 个端口也是按 20 组给的，端口先耗尽还是这个症状。`max-bps=1048576` 是每个分配、上下行各 ≈8 Mbps，只影响「两边都只能走 TURN」时的文件传输速度。`denied-peer-ip` 挡的是往内网、回环与云元数据地址（169.254.169.254 那类）的转发。

起好之后把地址广告给客户端：

```text
# 写进 .env（**不要加引号**；整行一次写完，别换行）
PAIR_ICE_SERVERS=[{"urls":["stun:cat.example.com:3478"]},{"urls":["turn:cat.example.com:3478"],"username":"bongo","credential":"<turnserver.conf 里 user= 的密码>"}]
```

想在 compose 文件里直接写这一项也可以，但那里**必须**用单引号把整串包住（YAML 会把不带引号的 JSON 解析成列表并报 `must be a string`）。

`iceServers` 只会广告给**通过服务器密码**的连接（R36），所以不必担心凭据被陌生人顺手拿走。更硬的做法是给 coturn 配 `use-auth-secret` + 限时凭据（泄漏的凭据会自己过期），但那需要中继按会话签发 HMAC 凭据，属于下一步的事。

隐私提醒：STUN 必然让第三方看到双方的公网 IP，这是 WebRTC 的固有特性。中继**默认不填**任何公共 STUN；填了就会被客户端使用。

## 端点

| 端点 | 说明 |
| --- | --- |
| `GET /health` | `{"ok":true,"protocol":1,"mode":"multi-pair","passwordRequired":true}`，鉴权之前就返回，不暴露任何会话信息（`passwordRequired` 只是常量，方便部署者一条 curl 确认装对了） |
| `GET /ws` | WebSocket 升级端点 |

`/ws` 必须带这些头（前三个与 Cloudflare 版一致；`X-Bongo-Room` 与 `X-Bongo-Server` 是本版新增的，后者只有自建版认）：

```http
Authorization: Bearer <由配对密码派生，客户端自动带>
X-Bongo-Server: <服务器密码派生出来的凭据>
X-Bongo-Room: <ROOM_ID>
X-Bongo-Client: <deviceId>
X-Bongo-Protocol: 1
```

判定顺序（前一步不过就不会走到下一步）：路径 → 升级头 → WebSocket 版本 → `X-Bongo-Protocol` → **服务器密码（403）** → `ROOM_ID` 格式（400）→ Authorization 非空（401）→ 会话密钥/容量（401 / 503）→ `deviceId`（400）。

错误码：`403` 服务器密码不对（没带：`server password required`；带错：`server password incorrect`）、`401` 配对密码不对（同一会话上摘要对不上，或没带 Authorization）、`503` 服务器会话已满（**只针对新会话**）、`426` 协议不支持或不是 WebSocket 升级、`400` deviceId 或 `ROOM_ID` 非法、`404` 路径不对。

控制帧（`text` + JSON，服务端 → 客户端）：

```json
{ "type": "server.welcome", "protocol": 1, "peerOnline": false,
  "limits": { "framesPerSecond": 30.0, "chunksPerSecond": 20.0, "bytesPerSecond": 12582912.0 } }
{ "type": "server.peer", "online": true, "deviceId": "..." }
```

关闭码：`4002` 同一 deviceId 被新连接顶替、`4003` 该会话已有两台设备在线、`4004` 陈旧连接被顶替、`1008` 帧格式错误 / 客户端发 text / 超限、`1009` 单帧超过 1 MiB、`1011` 服务端内部错误。注意「服务器满员」走的是 HTTP `503` 而不是关闭码：客户端要能把它显示成「服务器双人联机会话已满，请稍后再试」，而不是一串协议码。

## 日志与隐私

- 日志里只出现**会话指纹**（`SHA256(ROOM_ID)` 的前 8 字节）与 `deviceId`；完整 `ROOM_ID`、`AUTH_TOKEN`、配对密码、服务器密码、聊天内容一律不写。
- **凭据与会话相关的**拒绝各留一行（对端地址 + 阶段 + HTTP 状态码，例如「被拒绝：服务器密码不正确（HTTP 403）」）。按**阶段**记的是这六段：协议版本、服务器密码、`ROOM_ID`、`Authorization`、容量、`deviceId`。没有这几行时，「两台设备连不上」和「客户端根本没连到这」在日志里长得一样。
- **不记**的是「连接还没进到协议这一层」的那几步：路径不对（404）、缺 `Upgrade` / `Sec-WebSocket-Version` / `Sec-WebSocket-Key`（426 / 400）。它们跟凭据无关，客户端自己会说「服务器地址路径不对」，记下来只会被公网扫描器刷满。（握手中断，例如请求头超过 8 KiB，走的是另一条日志：「连接 … 结束：请求头过大」。）
- 服务器只在内存里保存 `SHA256(AUTH_TOKEN)` 与 `SHA256(服务器凭据)` 两个摘要，不保存它们的明文。
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
PAIR_SERVER_PASSWORD=dev-server-password cargo run --bin bongocat-pair-relay   # 本地也必须设服务器密码（≥16 字符）
```

`cargo run` 要带 `--bin bongocat-pair-relay`（仓库里还有 `generate-pair` 这个可执行文件，不带就会报「could not determine which binary to run」）。它会打印监听地址、会话上限与额度。本地联调不需要 Caddy，直接 HTTP 即可。

注意**裸 `cargo run` 不设 `PAIR_MAX_FRAMES_PER_SECOND` 时广告的是 30 帧/秒**，客户端推导出 20（< 60），桌宠快照会跑 **3Hz**——不是客户端坏了。想在本机看 60Hz 就设 `PAIR_MAX_FRAMES_PER_SECOND=90`（compose 里默认已经是 90）。

和客户端联调（真中继的端到端用例）：

```powershell
# 1. 起中继（本地不需要 Caddy，直接 HTTP 即可；多会话用例要求 PAIR_MAX_SESSIONS >= 2；
#    PAIR_MAX_FRAMES_PER_SECOND=90 才会走 60Hz 那条腿）
$env:PAIR_LISTEN = '127.0.0.1:8798'
$env:PAIR_MAX_SESSIONS = '20'
$env:PAIR_MAX_FRAMES_PER_SECOND = '90'
$env:PAIR_SERVER_PASSWORD = 'dev-server-password'
cargo run --bin bongocat-pair-relay

# 2. 另一个终端跑客户端的端到端用例（含多会话隔离）
$env:BONGO_PAIR_E2E_RELAY = 'http://127.0.0.1:8798'
$env:BONGO_PAIR_E2E_SECRET = 'AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8'
$env:BONGO_PAIR_E2E_SERVER_PASSWORD = 'dev-server-password'
$env:BONGO_PAIR_HEARTBEAT_SECS = '2'
cargo test --manifest-path ../src-tauri/Cargo.toml --lib pair::e2e -- --ignored --nocapture
```

`../server-cloudflare/README.md` 是这套线上契约的来源；改这里之前先读它。
