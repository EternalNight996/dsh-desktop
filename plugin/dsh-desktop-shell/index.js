// 桌面壳（host 侧）：纯客户端插件，host 半无职责。
// 按钮 UI 全在 lib/client.js（浏览器半）经 sidebar.footer.action 槽位挂载；
// 点击经 Tauri IPC（window.__TAURI_INTERNALS__.invoke）调桌面壳 open_settings 命令。
export const name = 'dsh-desktop-shell'
export const inject = []

export function apply(_ctx) {}
