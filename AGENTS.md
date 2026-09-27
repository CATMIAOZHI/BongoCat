# Workspace AGENTS.md

本文件适用于 `C:\Users\CAT\Documents\workspace\bongocat` 工作区；用户当前明确要求优先。协作与设计偏好见工作区根目录的 `taste.md`。

## 用户偏好

- 开始规划或实施任务前，先阅读本文件和 `taste.md`。
- 用简洁易懂的中文交流，先说结论。

## 工作区结构

- 工作区根目录就是仓库本体（BongoCat 的本地 fork），没有子仓库。
- 双人联机功能的三份设计计划都**已完成并归档**（开头标了「状态：已完成」）：`docs/pair-plan.md`（Phase 1~6：多窗口 / Cloudflare Relay / 对方猫 / 聊天 / 附件 / 语音，修订记录到 R46）、`docs/pair-plan-cloud-p2p.md`（Phase 7~10：自建中继 / P2P / 60Hz / reliable 通道）与 `docs/pair-plan-multi-session.md`（Phase 11~12：多会话服务器 / 服务器密码）。它们只作历史记录：以后的改动不再往里追加修订记录（改动说明写在提交信息里；需要新计划时另开一份新文档），**独立审计也不以它们为参考**，以当前代码、提交信息与本文件为准。
- 联机凭据有两层：**配对密码**（每一对用户自己的，决定「谁是同一对」，同时是 E2EE 密钥材料）与**服务器密码**（部署者在自建中继上设置的 `PAIR_SERVER_PASSWORD`，决定「谁能用这台服务器」，Phase 12 / R36）。客户端设置页因此有三项：服务器地址 / 服务器密码 / 配对密码；两个密码分别存在系统凭据库的两个条目里，且**不填服务器密码时就不发 `X-Bongo-Server` 头**（官方 Cloudflare 中继不需要它）。
- 昵称（`settings.identity.displayName`，偏好页「连接」一栏的「我的昵称」）没有独立的线上字段，搭 presence 帧里已有的 `displayName` 走；显示点是聊天窗口标题和对方猫底部那行小字。发送侧**永远带上**这个字段（空串也带），接收侧据此区分「老客户端根本没带」和「对面把昵称清空了」，后者要把旧名字清掉。`usePairState` 会在对端上线、切暂离、以及**改昵称**时各重发一次 presence。偏好页里跨窗口的文本设置（暂离举牌文字、昵称）统一走 `usePairSettingDraft` 的「草稿 + 保存」：直接 `v-model` 绑 store 会被别的窗口带旧值的整份状态覆盖，表现为打字时闪、丢字。
- 按键高亮与自动释放（R46）：模型同一时刻只能显示**一张**键盘贴图，所以 `stores/model.ts` 的 `pressedKeys` 每个贴图目录只留一个键，而「真的按着」那份事实记在 `heldKeys` 里（`utils/keyHighlight.ts`，松开时回退到同目录里**最后按下**的那个）——没有它就会出现「按住 w、再按 a/d、松开 a/d 之后什么都不亮」。Windows 上所有键都走 `utils/keyAutoRelease.ts` 的「到点再确认」：**安静 ≠ 抬起**（键盘自动重复只跟**最后按下**的那个键，被后来者压住的键会彻底安静，松开后来者也不会恢复），到点先问系统（Rust 命令 `is_key_down`，`GetAsyncKeyState`），还按着就再等一轮，只有真抬起才释放；CapsLock 例外（`probe: false`，亮 100ms）。Rust 侧用它按下/抬起时记下来的 `platform_code`（Windows 上是 vkCode）当键码，所以前端要传 rdev 的**原始**键名（`KeyW` / `F5` / `ShiftLeft`）。联机那一侧同一成因有两处：`usePairActivity` 的按住上限（重复事件与 `noteKeysStillDown` 都会续期；上限 = 本机自动释放延迟**多留 1 秒**，否则本机那次「问系统」的往返正好落在裁剪之后，对方猫会缺一帧）和对方猫的 TTL（`usePairState` 的 `sendKeepAlive`：还按着贴图/指针按键时按 `max(250ms, 当前快照间隔)` 重发同一份快照，间隔取自 `pet_state_hz`，不越传输额度；判据在 `utils/keepAlive.ts`，**窗口藏起来时不补**——那会儿页面被冻结、节拍被夹到 1 秒，补发反而让对方猫每秒闪一下）。
- 鼠标位置算比例时的显示器口径（`utils/monitor.ts`）：`monitorFromPoint` 的坐标是**原样**交给 tao 的（Windows = 物理像素 + `MONITOR_DEFAULTTONULL`，macOS = 逻辑点），所以 `monitorQueryPoint` 只在 macOS 上折算；查不到显示器时**退回上一次认到的那块**（其次主显示器），不能返回 null——返回 null 会让那一帧的鼠标比例整个不更新，而多屏高度不一致留下的空档（没人覆盖的那条带子）正好在「鼠标贴着屏幕边缘」的位置上。
- 中继有两份实现：`server-cloudflare/`（Cloudflare Worker + Durable Object，**一个部署只服务一对用户**，忽略 `X-Bongo-Room` 与 `X-Bongo-Server`）与 `server-relay/`（自建 Rust 服务，**一套承载多个双人会话**，按 `X-Bongo-Room` 分组，并要求服务器密码）。线上契约以 `server-cloudflare/README.md` 为准，两侧必须一致（自建版多出 `server.welcome` 里可选的 `limits` / `iceServers` 字段、`/health` 的 `mode` / `passwordRequired` 字段、服务器密码不对时的 HTTP 403、容量满时的 HTTP 503，以及**本版必需**的 `X-Bongo-Room` 与 `X-Bongo-Server` 头——新客户端连两份中继都行，**旧客户端（或没填服务器密码的新客户端）连自建版会被挡下**，所以升级自建版要先升两台设备上的客户端并填好服务器密码，见 `server-relay/README.md` 的兼容矩阵）。`server-relay/` 是**独立 workspace**（不在根 workspace 里），单独用 `cargo test --manifest-path server-relay/Cargo.toml` 跑测试。
- 自建中继**内置 STUN**（`server-relay/src/stun.rs`，默认 UDP 3479，`PAIR_STUN_PORT=0` 关闭）：没配 `PAIR_ICE_SERVERS` 时，`server.welcome` 的 `iceServers` 广告 `stun:<客户端连进来用的 Host>:3479`；配了就以它为准、内置的不启动。没有 STUN 时双方只有内网 host 候选，跨网络的 P2P 一定失败，客户端设置页会显示「暂时连不上，已用服务器」。Cloudflare 版跑不了 UDP，那边的 P2P 只在同一局域网成功。客户端的 STUN 连通性用 `p2p.rs` 里被忽略的用例 `the_relay_builtin_stun_yields_a_srflx_candidate` 验（`BONGO_PAIR_E2E_STUN=stun:<本机局域网 IP>:3479`）。
- `src/`：前端，Vue 3 + TypeScript + Vite + Pinia + UnoCSS + antdv-next。
- `src-tauri/`：Rust 后端；`src-tauri/src/plugins/` 下是本仓库自带的本地插件（`admin-status`、`window`）。
- `src-tauri/assets/models`：内置猫咪模型；`scripts/`：图标生成、发布等脚本。
- `public/`：静态资源；`src/locales/`：多语言文案。

## 仓库边界

- 远端：`origin` = CATMIAOZHI/BongoCat（fork）；上游 = ayangweb/BongoCat，主分支为 `master`（上游没有 `dev` 分支）。
- 从 `feat/pair-desktop-v1` 分支起，本 fork 在上游 `master` 之上有本地定制（双人联机功能，仅 Windows 范围）。
- 除非用户明确要求，不主动同步、对比或合并上游。准备向上游贡献时，先做只读可行性分析（含上游重复 issue/PR 检索），报告需要重新验证的部分，等用户明确许可后再建分支或提 PR。
- 对外仓库链接、下载、反馈和维护者署名统一为 `CATMIAOZHI/BongoCat` / `CATMIAOZHI`。许可证保留原作者版权。`identifier`（`com.ayangweb.BongoCat`）与系统凭据库 SERVICE 保留以兼容已安装客户端数据；它们不是网络地址。`ayangweb/gilrs` 是实际依赖源，不替换成不存在的 fork。
- 对方模型自动同步默认开启，只在 presence 中携带 `model: { name, mode, isPreset }`，不发送路径、随机 ID 或文件。内置模型按模式匹配，自定义模型按 `Model.name` 和模式匹配本地已导入列表；导入时保存原始目录名，旧模型在模型卡片点铅笔补填。存储目录末段是随机 ID，不能用它匹配。缺失、未命名或重名时使用自己的模型并提示。用户关闭自动同步后仍可手动选模型。加载串行执行，避免异步覆盖。

## 构建与验证

- 包管理器只用 pnpm（`preinstall` 里有 `only-allow pnpm`），不要用 npm/yarn 安装依赖。
- 常用命令：`pnpm install`、`pnpm tauri dev`、`pnpm tauri build`（调试加 `--debug`）、`pnpm lint`、`pnpm test`。**本 fork 自用不签名、也不做自动更新**：`tauri.conf.json` 里 `createUpdaterArtifacts: false`，本地打包加不加 `--no-sign` 都能过。这个字段**不能置 `true`**（除非同时把 `plugins.updater`、`updater:default` capability 与 updater 插件注册一起恢复）：它为 `true` 时打包器要先读 `plugins.updater` 才知道公钥，而本 fork 已移除该插件配置，于是在「构建 bundler 设置」那一步直接失败（报 `failed to get updater configuration: plugins > updater doesn't exist`）——这一步在签名之前，`--no-sign` 挡不住，本地与 CI 都会挂。CI 里的 `--no-sign` 只是顺手保险（见 `release.yml`）。
- Windows 安装器自带一份 NSIS 模板 `src-tauri/nsis/installer.nsi`（由 `bundle.windows.nsis.template` 指过去），相对上游只多了一处：`Function PageReinstall` 开头的 `Abort`。升级时不再询问「安装前卸载 / 请勿卸载」，**一律直接覆盖安装**（`Abort` 是 NSIS 跳过页面的官方做法，库存模板在没有旧安装时也这么用；页面被跳过就不会调用 `PageLeaveReinstall`，因此旧版本不会被卸载、用户数据也不会被动）。这份文件是从 `@tauri-apps/cli` 对应版本 tag 的 `crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi` 原样拷来的（纯 ASCII、LF、无 BOM，`{{...}}` 占位符必须保持原样），**升级 CLI 时要按新 tag 重新拷一份、只把这段 `Abort` 补回去**，其余部分不要动，保持与上游的 diff 最小。
- 前端改动至少跑 `pnpm lint`；涉及 Rust 或打包配置时说明是否真的跑过 `pnpm tauri build` 或 `cargo check`，没验证的要标注。
- 前端有 vitest 单测（`pnpm test`，用例在 `src/**/*.spec.ts`，目前覆盖双人联机的纯函数映射），Rust 有 `cargo test --lib` 与 `cargo test --all-targets`。
- 中继的端到端验证：先起一个中继（`server-relay` 或 `server-cloudflare` 的 `pnpm dev`），再设 `BONGO_PAIR_E2E_RELAY` / `BONGO_PAIR_E2E_SECRET` / `BONGO_PAIR_HEARTBEAT_SECS`（自建中继还要 `BONGO_PAIR_E2E_SERVER_PASSWORD`），跑 `cargo test --manifest-path src-tauri/Cargo.toml --lib pair::e2e -- --ignored`。换中继实现时，这套用例必须在两侧都通过。
- `vite build` 不做类型检查，要单独跑 `node node_modules/typescript/bin/tsc --noEmit`；仓库没装 `vue-tsc`，所以这个检查只覆盖 `.ts`，`.vue` 里的类型问题目前只能靠 review 和实际运行发现。
- 报告里区分静态检查、构建和实际运行三种证据。
- WebView2 的浏览器参数统一写在 `src-tauri/tauri.conf.json` 的 `additionalBrowserArgs` 上（四个窗口都要写、值必须一致：WebView2 环境按 data_directory 共享，只有创建环境那一份生效）。这个字段的语义是**替换** wry 的默认参数，所以改动时必须把默认串 `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection` 和 `--autoplay-policy=no-user-gesture-required`（wry 的 autoplay 默认项，丢了会拦下提示音与语音播放）一起带上。不要改用 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 环境变量：应用以管理员权限跑（全局键鼠钩子需要），提权进程会被 WebView2 忽略该变量。以后若在 conf 里配了 `proxyUrl`，wry 还会追加 `--proxy-server=...`，那份也要跟着补进来。核验方式是启动后看 `msedgewebview2.exe` 里带 `--embedded-browser-webview=1` 那条命令行。两个与遮挡有关的项都留着、四处必须一致：`--disable-backgrounding-occluded-windows` 已经让 content 层把 OCCLUDED 当成 VISIBLE（遮挡因此不产生任何后果），`--disable-features=...,CalculateNativeWinOcclusion` 再把 Windows 原生遮挡追踪本身关掉（省掉它的 `SetWinEventHook` 与定时器，也挡住将来 `ApplyNativeOcclusionToCompositor` 被打开时的副作用）。这两项都只是「关掉遮挡优化」，**不是**下面那条卡死的解药。
- 宠物窗口（`main` / `remote-cat`）会出现「画面停在最后一帧、点不动、拖不动、连偏好页里的显示开关都不响应」的状态（`usePairOverlay.ts` 里记过对方猫的先例）：窗口本身没坏（仍是 TOPMOST、可点击、没被禁用也没穿透），是里面那个页面整个不跑了——所以**「点不动」就是判据**（只停绘制的话拖动仍会生效，因为拖动是页面里的 `startDragging()`）。唤醒办法是把那个窗口**隐藏再显示**一次：前端用 `plugin:custom-window` 的 `hide_window_label` / `show_window_label`（主窗口那条是 `hide_window` / `show_window`），Rust 侧最终落到 `webview.show()`（wry 里就是 WebView2 的 `controller.SetIsVisible(true)`）；注意 `show_window_label` 的 `focus` 默认 `false`，只有主窗口那条路必定 `set_focus()`（微软的官方口径见 `windows.rs` 顶部注释）；仍不行才重开应用。别想靠重装 rdev 钩子自愈：句柄在 rdev 的私有模块里拿不到，重复 `listen` 会让事件走两遍。那次现场更像内存压力到 Critical 时的一次系统级降级（同期一批无关进程一起崩），遮挡不是成因。
- 日志级别固定在 `src-tauri/src/lib.rs` 的 `tauri-plugin-log` builder 上（`LevelFilter::Info`）。插件默认是 `Trace`，会把 `tokio-tungstenite` 的逐帧收发连整包 payload 一起写盘，双人联机时是持续的 CPU / IO 源（磁盘占用本来就有上限：插件默认 `KeepOne` + 40KB，这里省的是 CPU / IO）。`.level()` 顺带把全局 `log::set_max_level` 也降到 `Info`，所以噪声在格式化之前就被丢掉；本项目自己的日志只用 `error` / `warn` / `info`，所以 `Info` 不影响排障。临时要查帧就用 `level_for` 单独放开某个模块——它会把全局上限抬回去，前提是别在别处再引入第二处 `set_max_level`（前端日志的 target 是 `webview:<location>`，按模块名放开时注意这点）。

- 日志保留量：`src-tauri/src/lib.rs` 里显式写了 `RotationStrategy::KeepOne` + `max_file_size(2 * 1024 * 1024)`。插件默认是 40KB，而 WebRTC / TURN 的错误每 5 分钟就写两行（实测基线约 60KB/天），40KB 只够半天——「今天几点直连上的」这类问题事后就查不到了。2MB 在平稳时约一个月；持续「打不通」风暴（退避封顶 120 秒一轮、每轮约 10~17 行）时约一天半。改这个值时按「至少够存几天」定，别退回默认。
- 直连（P2P）的现场统一记在 info 级别：`p2p.rs` 的「P2P 开始协商 / 本地候选 / 远端候选 / ICE 状态 / 连接状态 / 通道就绪 / 这一轮没打通，N 秒后重试」，以及 `manager.rs` 的 `publish_route` 里**只在状态真的变了**时写的那行「直连（P2P）状态：…」。候选只记类型 / 协议 / 地址（`describe_candidate`），ICE 服务器只记地址（`describe_ice_servers` 会丢掉 `user:pass@` 之前的部分）——凭据永远不进日志。「已直连」只代表 DataChannel 真的过了数据，绕 TURN 中转也算，所以要看候选里有没有 `relay` 才分得清是哪种。

### 本机 pnpm 注意事项

- 本机 `node_modules` 是用工作区内的 store 装的，pnpm 命令要带 `--store-dir .pnpm-store`，否则报 `ERR_PNPM_UNEXPECTED_STORE`。
- 仓库根的 `pnpm-workspace.yaml` 里声明了 `allowBuilds`（esbuild / @parcel/watcher / simple-git-hooks）。pnpm 11 默认拒绝执行依赖的 build script，而且只要有一条被忽略就让 `pnpm install` 以 `ERR_PNPM_IGNORED_BUILDS` 退出 1；更麻烦的是 `pnpm run` 之前那次依赖检查会**再跑一次 install**，命令行上的 `--config.strict-dep-builds=false` 传不进那一次，于是 `pnpm test`、`pnpm build:icon` 也会跟着失败。所以这个文件是 pnpm 11 要求的正式配置，不是它生成的占位提示文件：不要删，也不要让它退回 `set this to true or false` 的占位内容。
- 提交前仍然自己跑一遍 `node node_modules/eslint/bin/eslint.js --fix src` 与 `commitlint` 校验（`simple-git-hooks` 的钩子装没装取决于本机跑没跑过 `prepare`，别依赖它）。

## 提交与发布

- 提交信息遵循 Conventional Commits（`commitlint` 经 `simple-git-hooks` 的 `commit-msg`、`pre-commit` 钩子校验，pre-commit 会跑 `eslint --fix`）。
- 发布用 `pnpm release`（release-it，标签 `v*`），再由 `.github/workflows/release.yml` 构建 Draft Release。CI **只构建 Windows 三个目标**（x64 / x86 / arm64），macOS 与 Linux 已从矩阵里去掉（双人联机只做 Windows，那些包没人用）；要恢复上游的全平台见 workflow 里矩阵旁的注释。**本 fork 自用版不需要配任何 Secret**：推 `v*` 标签（或手动 Run workflow）就出安装包，用的是 GitHub 自带的 `GITHUB_TOKEN`（workflow 里已声明 `permissions: contents: write`），构建时 `--no-sign` 且 `createUpdaterArtifacts: false`、不生成 `latest.json`。想恢复「签名 + 自动更新分发」，按 `release.yml` 末尾的步骤重新配置自有公钥、私钥与插件（旧公钥已移除）。
- 上游专用的 UpgradeLink 与 Gitee 同步 workflow 已移除；不再访问上游更新服务或携带其 access key。
- 自建中继（`server-relay/`）的预编译二进制走 `.github/workflows/relay-release.yml`：推 `relay-v*` 标签（如 `relay-v1`）出 Release，或在 Actions 手动跑存 Artifact。**runner 必须留在 ubuntu-22.04**（服务器是 `debian:bookworm-slim`，glibc 2.36；用 24.04 编出来的会报 `GLIBC_2.3x not found`），workflow 里那步 `Check glibc requirement` 把这条钉住。包里除 `bongocat-pair-relay` 还带 `generate-pair`，服务器上不用装 Rust 也能生成服务器密码；下载与校验方式见 `server-relay/README.md` 的「预编译二进制」一节。
- 更新检查通过 GitHub API 读取本 fork 正式客户端 `vX.Y.Z` Release，过滤 draft / prerelease / `relay-v*`；发现新版后打开本 fork 下载页供用户手动安装。旧 updater 端点、公钥与运行时插件注册已移除，自用发布不做自动覆盖安装。
- 未经用户明确要求，不提交、不推送、不打标签、不发布 Release。

## 通用安全

- API Key、令牌、Cookie、签名材料和其他凭据不得写入仓库、日志或文档（上游既有的 `UPGRADE_LINK_ACCESS_KEY` 等硬编码常量除外，不要顺手清理以免改变上游行为）。
- 不回退、覆盖或清理他人的未提交改动；遇到直接冲突先询问用户。
- 修改前确认实际运行路径；新增防护或兼容处理前，先用调用链、日志或复现证明必要性。
- README 对外承诺「不收集任何用户数据」，改动不得引入遥测、统计或额外网络上报。
