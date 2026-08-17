# ADR-003 · dsh-ui-agents-pixe 插件采用「npm 双面包 + 组合补丁层」落地

- 状态：已采纳
- 日期：2026-08-15

## 背景

需求：在主窗口新增「工作角色」页签（从 The Agency（en）/ agency-agents-zh 选角色，每个角色配像素小人），并在对话区显示「像素办公室」浮层。本质是给 dsh 的 Web UI 增加一个**客户端（浏览器）插件**。

考察了四条落地路径：

| 路径 | 结论 |
|---|---|
| 动态 Cordis 插件（`cordis_define`/`cordis_run`） | Client 半边需要审批；当前审批策略 `never`（`danger-full-access` 级联）会**自动拒绝**，且动态插件是**进程内临时**、重启即失 |
| CSS/DOM 注入（`styles.insert` 硬编码选择器） | 脆弱，违反 dsh「不要硬编码产品 DOM 选择器」的约定，升级易失效 |
| 修改 dsh 源码 | 项目铁律：**不改 deepseek-harness 任何代码** |
| **npm 双面包 + 组合补丁层（持久插件）** | ✅ dsh 官方扩展机制，持久、可离线分发 |

## 决策

把 `agents-pixe` 实现为一个 dsh 客户端插件包 `dsh-ui-agents-pixe`（**node 半边 + browser 半边**），再通过**组合补丁层**把它的行插入 web profile：

- 包结构（独立仓库 `dsh-ui-agents-pixe`，npm 包 `dsh-ui-agents-pixe`）：
  - `package.json` 声明 `dsh.client: { platform: "web", inject: [] }`，`exports["./client"]` 指向 browser 半边；
  - `lib/index.js`：node 半边，`export { apply }`（无宿主行为）；
  - `lib/client.js`：browser 半边，自包含 `window.__ModuleLoader__.load({ id, factory })`（`factory` 内 `require("react")`、`exports.apply = apply; return module.exports`）；
  - `cordis.patch.yml`：补丁行 `- insert: [{ id: ui-agents-pixe, name: 'dsh-ui-agents-pixe' }]`（包内 `dsh.bundle.patch` 自挂载）。
- 角色目录（en 255 + zh 253 = 508 条）由 `scripts/gen-roles.mjs` 生成、`scripts/build-client.mjs` 冻结进 `lib/client.js`，运行时零文件系统依赖。

## 关键机制（已实测验证）

1. **包解析**：dsh 的 Loader 以 profile 目录为 `baseUrl`（`$DSH_HOME/profiles/web/`），out-of-tree 插件从 `profiles/web/node_modules`（或父级 `profiles/node_modules`）解析；`healProfilesModuleFallback` 只对 dsh 安装的**依赖闭包**建符号链接，自定义包需**直接落盘**到该 node_modules。
2. **补丁层顺序**：bundle 层 → profile 自身 `cordis.patch.yml` → `$DSH_HOME/cordis.patch.yml` → `--patch` 覆盖层。
3. **`--patch` 是启动器旗标**，必须放在首个非启动器旗标（如 `--port`）**之前**，否则被当作 web app 内层参数报 `unknown option '--patch'`。
4. **UI 落点**：`conversation.view`（list，session 作用域）加「工作角色」页签；`shell.overlay`（list，root 作用域，层 `pointer-events:none` 但直接子元素自动 `pointer-events:auto`）放可拖动/折叠的像素办公室浮层。选中角色状态用模块级 store + `localStorage` 在两者间共享。

## 结果

- 不修改 deepseek-harness 任何代码；插件包独立为 npm 包 `dsh-ui-agents-pixe`（独立仓库维护，包内 `dsh.bundle.patch` 自挂载，`dsh plugin add` 一条命令安装）。
- **2026-08-17 更新**：桌面壳**不再内置**该插件——`scripts/vendor.mjs` 与 `src-tauri/src/lib.rs` 的内置安装/补丁逻辑已摘除；以独立 npm 包发布，用户按需 `dsh plugin add dsh-ui-agents-pixe` 安装（后续可随时重新内置）。

## 否决的备选

| 备选 | 否决原因 |
|---|---|
| 动态插件（进程内临时） | Client 半边被 `never` 审批自动拒绝；且重启即失，非持久 |
| `styles.insert` 字面「背景图」 | 依赖产品 DOM 选择器，脆弱；且 pixel-agents 形态本就用浮层面板而非真背景 |
| 嵌入静态 dist | 缺 `__DSH_BOOT__` 注入，无法启动（见 ADR-001） |
