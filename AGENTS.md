# Workspace AGENTS.md

本文件适用于 `C:\Users\CAT\Documents\workspace\bongocat` 工作区；用户当前明确要求优先。协作与设计偏好见工作区根目录的 `taste.md`。

## 用户偏好

- 开始规划或实施任务前，先阅读本文件和 `taste.md`。
- 用简洁易懂的中文交流，先说结论。

## 工作区结构

- 工作区根目录就是仓库本体（BongoCat 的本地 fork），没有子仓库。
- 双人联机功能的设计真相来源是 `docs/pair-plan.md`（含实现前的修订记录 R1~R19，冲突以修订记录为准）。
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
- `vite build` 不做类型检查，要单独跑 `node node_modules/typescript/bin/tsc --noEmit`；仓库没装 `vue-tsc`，所以这个检查只覆盖 `.ts`，`.vue` 里的类型问题目前只能靠 review 和实际运行发现。
- 报告里区分静态检查、构建和实际运行三种证据。

### 本机 pnpm 注意事项

- 本机 `node_modules` 是用工作区内的 store 装的，pnpm 命令要带 `--store-dir .pnpm-store`，否则报 `ERR_PNPM_UNEXPECTED_STORE`。
- pnpm 11 默认不执行依赖的 build script，`pnpm add` 会打印 `ERR_PNPM_IGNORED_BUILDS` 并以退出码 1 结束；这时依赖其实已经装好，用 `pnpm test`、`eslint`、`vite build` 复核即可。
- 上面那种情况下 pnpm 会在仓库根目录生成带占位文字（`set this to true or false`）的 `pnpm-workspace.yaml`；它只是提示文件，不要提交。
- `simple-git-hooks` 的 build script 同样被忽略，`.git/hooks` 里没有装钩子；提交前自己跑一遍 `node node_modules/eslint/bin/eslint.js --fix src` 与 `commitlint` 校验。

## 提交与发布

- 提交信息遵循 Conventional Commits（`commitlint` 经 `simple-git-hooks` 的 `commit-msg`、`pre-commit` 钩子校验，pre-commit 会跑 `eslint --fix`）。
- 发布用 `pnpm release`（release-it，标签 `v*`），再由 `.github/workflows/release.yml` 多平台构建 Draft Release。
- 未经用户明确要求，不提交、不推送、不打标签、不发布 Release。

## 通用安全

- API Key、令牌、Cookie、签名材料和其他凭据不得写入仓库、日志或文档（上游既有的 `UPGRADE_LINK_ACCESS_KEY` 等硬编码常量除外，不要顺手清理以免改变上游行为）。
- 不回退、覆盖或清理他人的未提交改动；遇到直接冲突先询问用户。
- 修改前确认实际运行路径；新增防护或兼容处理前，先用调用链、日志或复现证明必要性。
- README 对外承诺「不收集任何用户数据」，改动不得引入遥测、统计或额外网络上报。
