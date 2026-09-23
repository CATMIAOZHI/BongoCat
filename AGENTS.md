# Workspace AGENTS.md

本文件适用于 `C:\Users\CAT\Documents\workspace\bongocat` 工作区；用户当前明确要求优先。协作与设计偏好见工作区根目录的 `taste.md`。

## 用户偏好

- 开始规划或实施任务前，先阅读本文件和 `taste.md`。
- 用简洁易懂的中文交流，先说结论。

## 工作区结构

- 工作区根目录就是仓库本体（BongoCat 的本地 fork），没有子仓库。
- 双人联机功能的设计真相来源分两份：`docs/pair-plan.md`（Phase 1~6：多窗口 / Cloudflare Relay / 对方猫 / 聊天 / 附件 / 语音，修订记录 R1~R19）与 `docs/pair-plan-cloud-p2p.md`（Phase 7~9：自建中继 / P2P / 60Hz，修订记录 R20~R25）。两份都冲突时以各自的修订记录为准；跨文档冲突以 `pair-plan-cloud-p2p.md` 为准。
- 中继有两份实现：`server-cloudflare/`（Cloudflare Worker + Durable Object）与 `server-relay/`（自建 Rust 服务，一对用户一套）。线上契约以 `server-cloudflare/README.md` 为准，两侧必须一致（自建版只多出 `server.welcome` 里可选的 `limits` / `iceServers` 字段，旧客户端忽略）。`server-relay/` 是**独立 workspace**（不在根 workspace 里），单独用 `cargo test --manifest-path server-relay/Cargo.toml` 跑测试。
- `src/`：前端，Vue 3 + TypeScript + Vite + Pinia + UnoCSS + antdv-next。
- `src-tauri/`：Rust 后端；`src-tauri/src/plugins/` 下是本仓库自带的本地插件（`admin-status`、`window`）。
- `src-tauri/assets/models`：内置猫咪模型；`scripts/`：图标生成、发布等脚本。
- `public/`：静态资源；`src/locales/`：多语言文案。

## 仓库边界

- 远端：`origin` = CATMIAOZHI/BongoCat（fork）；上游 = ayangweb/BongoCat，主分支为 `master`（上游没有 `dev` 分支）。
- 从 `feat/pair-desktop-v1` 分支起，本 fork 在上游 `master` 之上有本地定制（双人联机功能，仅 Windows 范围）。
- 除非用户明确要求，不主动同步、对比或合并上游。准备向上游贡献时，先做只读可行性分析（含上游重复 issue/PR 检索），报告需要重新验证的部分，等用户明确许可后再建分支或提 PR。
- `src-tauri/tauri.conf.json` 的 `identifier`（`com.ayangweb.BongoCat`）和 updater 端点仍指向上游。自行打包时自动更新会拉取上游版本，改动这些标识需用户明确要求。

## 构建与验证

- 包管理器只用 pnpm（`preinstall` 里有 `only-allow pnpm`），不要用 npm/yarn 安装依赖。
- 常用命令：`pnpm install`、`pnpm tauri dev`、`pnpm tauri build`（调试加 `--debug`）、`pnpm lint`、`pnpm test`。
- 前端改动至少跑 `pnpm lint`；涉及 Rust 或打包配置时说明是否真的跑过 `pnpm tauri build` 或 `cargo check`，没验证的要标注。
- 前端有 vitest 单测（`pnpm test`，用例在 `src/**/*.spec.ts`，目前覆盖双人联机的纯函数映射），Rust 有 `cargo test --lib` 与 `cargo test --all-targets`。
- 中继的端到端验证：先起一个中继（`server-relay` 或 `server-cloudflare` 的 `pnpm dev`），再设 `BONGO_PAIR_E2E_RELAY` / `BONGO_PAIR_E2E_SECRET` / `BONGO_PAIR_HEARTBEAT_SECS`，跑 `cargo test --manifest-path src-tauri/Cargo.toml --lib pair::e2e -- --ignored`。换中继实现时，这套用例必须在两侧都通过。
- `vite build` 不做类型检查，要单独跑 `node node_modules/typescript/bin/tsc --noEmit`；仓库没装 `vue-tsc`，所以这个检查只覆盖 `.ts`，`.vue` 里的类型问题目前只能靠 review 和实际运行发现。
- 报告里区分静态检查、构建和实际运行三种证据。

### 本机 pnpm 注意事项

- 本机 `node_modules` 是用工作区内的 store 装的，pnpm 命令要带 `--store-dir .pnpm-store`，否则报 `ERR_PNPM_UNEXPECTED_STORE`。
- 仓库根的 `pnpm-workspace.yaml` 里声明了 `allowBuilds`（esbuild / @parcel/watcher / simple-git-hooks）。pnpm 11 默认拒绝执行依赖的 build script，而且只要有一条被忽略就让 `pnpm install` 以 `ERR_PNPM_IGNORED_BUILDS` 退出 1；更麻烦的是 `pnpm run` 之前那次依赖检查会**再跑一次 install**，命令行上的 `--config.strict-dep-builds=false` 传不进那一次，于是 `pnpm test`、`pnpm build:icon` 也会跟着失败。所以这个文件是 pnpm 11 要求的正式配置，不是它生成的占位提示文件：不要删，也不要让它退回 `set this to true or false` 的占位内容。
- 提交前仍然自己跑一遍 `node node_modules/eslint/bin/eslint.js --fix src` 与 `commitlint` 校验（`simple-git-hooks` 的钩子装没装取决于本机跑没跑过 `prepare`，别依赖它）。

## 提交与发布

- 提交信息遵循 Conventional Commits（`commitlint` 经 `simple-git-hooks` 的 `commit-msg`、`pre-commit` 钩子校验，pre-commit 会跑 `eslint --fix`）。
- 发布用 `pnpm release`（release-it，标签 `v*`），再由 `.github/workflows/release.yml` 多平台构建 Draft Release。
- 未经用户明确要求，不提交、不推送、不打标签、不发布 Release。

## 通用安全

- API Key、令牌、Cookie、签名材料和其他凭据不得写入仓库、日志或文档（上游既有的 `UPGRADE_LINK_ACCESS_KEY` 等硬编码常量除外，不要顺手清理以免改变上游行为）。
- 不回退、覆盖或清理他人的未提交改动；遇到直接冲突先询问用户。
- 修改前确认实际运行路径；新增防护或兼容处理前，先用调用链、日志或复现证明必要性。
- README 对外承诺「不收集任何用户数据」，改动不得引入遥测、统计或额外网络上报。
