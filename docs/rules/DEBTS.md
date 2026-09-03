# 欠账登记（DEBTS）

> gate 三态：PASS / PASS_WITH_DEBT / BLOCK。欠账必须可见，每 3 次提交或每周 review 还一次债。

| 日期 | 欠账 | 影响 | 偿还计划 |
|---|---|---|---|
| 2026-08-14 | ~~GitHub 远程仓库未建（缺 PAT / gh 未装），仅推了 Gitee~~ | 已偿还（2026-08-15 已推送 GitHub） | — |
| 2026-08-15 | ~~自动更新端到端发布未实跑~~ | 已偿还（2026-08-15 v0.2.0 已发布 GitHub + Gitee 双发行版，latest.json 双端点可用） | — |
| 2026-08-15 | Gitee 附件 100MB 上限，AppImage(123MB) 无法上传 Gitee | Gitee 端 Linux 更新用 deb 替代（已在 publish-gitee.mjs 加超限跳过逻辑） | 后续若需 Gitee 上 AppImage，可考虑 Gitee 付费/OSS 托管 |
| 2026-08-15 | CI 的 gitee job 依赖 GITEE_TOKEN secret（本次手动完成发布，secret 未配） | 未来发版时 gitee job 会失败 | 在 GitHub 仓库配 GITEE_TOKEN secret 后即可全自动 |
| 2026-08-14 | 未产出 Windows 安装包（NSIS/MSI，`cargo tauri build` release） | 需源码环境才能运行 | 后续补 `just release` 与 WebView2/NSIS 说明 |
| 2026-08-14 | 首次 debug 构建耗时过长（本机高负载 132 分钟） | 首次体验差 | 文档已注明；后续在干净环境重新测量增量构建耗时 |
| 2026-08-14 | Linux（Ubuntu）构建未在真机验证（本机仅 Windows） | Linux 用户可能缺依赖 | 在 Ubuntu 实机跑通 `just setup-linux && just dist` 后转 PASS |
| 2026-09-03 | ~~`cargo test` 因遗留坏测试 `version_selector_tests` 引用不存在的 `parse_npm_json_progress` 无法编译~~ | 已偿还（2026-09-03 删除 4 个测已移除函数的死测试；并抽出 `remove_bundle_entries_from` 纯函数补 3 个单测，`cargo test` 10 passed） | — |
| 2026-09-03 | 自动移除坏 bundle 插件改动已在单测层验证；但「桌面壳启动→dsh 崩溃→自动移除→恢复」完整链路未实机复现 | 本机真实坏 bundle（agent-teams-pixel：bundles 登记但依赖缺失 → dsh fail-loud 崩溃无画面）已于 2026-09-03 按同语义从 bundles 移除、profile 恢复可用；载入后 bad_plugins 为空、移除逻辑无从触发端到端复现 | 下次出现「无法解析 profile bundle」崩溃时观察 stderr 是否出现「已自动从 bundles 移除并重启」，确认后删除本条 |
