//! DeepSeek Harness Desktop 主进程。
//!
//! 自包含桌面壳，内置 Node sidecar。启动时按顺序选择 dsh 的来源：
//!   1. 环境变量 `DSH_BIN`（开发/调试覆盖）
//!   2. 打包内置的 dsh（方案③「离线」，客户零依赖离线即用）
//!   3. 在线方案（方案②「拉取」）：直接使用**全局安装**（%APPDATA%\npm，与 dsh 命令
//!      同源）里的 dsh，首次联网安装到全局前缀后由内置 node 直跑 `lib/bin.js` web。
//!      版本更新不自动做：后台只检查版本（有新版发事件提示），由用户手动触发更新。
//!
//! 就绪后把窗口导航到 dsh 的 Web UI；退出时回收 dsh 子进程。
//! 跨平台：Windows / macOS / Linux，差异仅在 sidecar 二进制与系统 WebView。


use std::collections::VecDeque;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_updater::UpdaterExt;

/// 工作栏「更新配置」按钮是否已成功注入。注入脚本成功时会经 `log_workbar_inject` 置位，
/// 供 `spawn_dsh` 的重复注入循环判断停止（避免固定 sleep 造成的额外等待）。
static WORKBAR_INJECTED: AtomicBool = AtomicBool::new(false);

// ===== dsh 看门狗状态 =====
// dsh 上游对插件是 fail-loud 设计：任一 bundle 导入/激活失败整个进程 exit(1)。
// 桌面壳据此做自愈：崩溃→退避重启；连续快速崩溃→停止重启，加载页显示原因。

/// 桌面壳代数：每次拉起新 dsh 或主动杀 dsh 前递增。事件消费线程以
///「自己持有的代数 == 当前代数」区分「自己看护的进程意外退出（崩溃，需自愈）」
/// 与「过期事件流（主动杀/已被新实例替换，忽略）」。
static DSH_GENERATION: AtomicU64 = AtomicU64::new(0);
/// 连续快速崩溃计数（稳定运行满 5 分钟自动归零；≥3 次停止自动重启转错误页）。
static DSH_CRASHES: AtomicUsize = AtomicUsize::new(0);
/// 当前 dsh 就绪时刻（用于「稳定运行」判定，崩溃间隔大于它即视为偶发）。
static DSH_READY_AT: Mutex<Option<Instant>> = Mutex::new(None);
/// 最近日志尾（stdout+stderr 各保留若干行），崩溃时截末尾作为诊断文本。
static DSH_LOG_TAIL: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
/// 错误态文本：Some(原因) = 处于错误态（加载页据此渲染原因与重试按钮）。
static DSH_ERROR: Mutex<Option<String>> = Mutex::new(None);
/// 主窗口初始加载页 URL（进入错误态时导航回此页）。
static LOADER_URL: Mutex<Option<String>> = Mutex::new(None);

/// 稳定运行阈值：dsh 存活满此时长后崩溃计数归零（视为新一轮偶发故障）。
const STABLE_UPTIME: Duration = Duration::from_secs(300);
/// 连续崩溃 ≥ 此值停止自动重启，转错误页等用户重试。
const MAX_CRASHES: usize = 3;
/// 崩溃退避序列：第 1/2/3 次崩溃后分别等 1s/5s/30s 再重启。
const CRASH_BACKOFFS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];
/// 日志尾容量（行数）。
const LOG_TAIL_LINES: usize = 60;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Windows：隐藏子进程控制台窗口（CREATE_NO_WINDOW）。
/// GUI 应用直接拉起 powershell / taskkill 等控制台程序时，不加此标志会闪出 cmd 窗口。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 在线方案首次安装 dsh 允许的总耗时（npm 下载依赖可能较慢）。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
/// dsh 启动后等待就绪的超时时间。
const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// 就绪探测轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// dsh Web UI 固定端口：保持 origin（127.0.0.1:<port>）跨重启稳定，localStorage 才能持久
/// （否则随机端口导致 origin 每次变化、localStorage 被清空）。
/// 可用环境变量 `DSH_PORT` 覆盖（仅供隔离测试，不改产品默认行为）。
fn dsh_port() -> u16 {
    std::env::var("DSH_PORT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(5399)
}

/// 持有 dsh 子进程句柄，退出时统一 kill。
struct DshChild(Mutex<Option<CommandChild>>);

/// 杀 dsh 进程树：Windows 用 taskkill /T /F（node 拉起的 dsh 可能还有子进程，
/// 只 kill 直接子进程会让真正的 dsh 残留成孤儿），其它平台直接 kill。
fn kill_process_tree(child: CommandChild) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["/PID", &child.pid().to_string(), "/T", "/F"])
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
    }
}

/// 主动杀当前 dsh（更新安装/在线升级/退出时调用）：先递增代数再看护规则杀进程。
/// 代数递增使旧事件消费线程把随后的 Terminated 视为「过期事件」而非崩溃，
/// 避免看门狗把主动杀误判成崩溃而与更新流程打架。
fn kill_dsh_intentionally(app: &tauri::AppHandle) {
    DSH_GENERATION.fetch_add(1, Ordering::Relaxed);
    if let Some(state) = app.try_state::<DshChild>() {
        if let Some(child) = state.0.lock().unwrap().take() {
            kill_process_tree(child);
        }
    }
}

/// 进入错误态：记录原因并把主窗口导航回加载页（加载页据此显示原因 + 重试）。
/// reason 用 textContent 渲染，不经 innerHTML，无注入面。
fn enter_error_state(app: &tauri::AppHandle, reason: String) {
    eprintln!("[dsh-desktop] 进入错误态: {reason}");
    *DSH_ERROR.lock().unwrap() = Some(reason);
    if let Some(win) = app.get_webview_window("main") {
        if let Some(u) = LOADER_URL.lock().unwrap().clone() {
            if let Ok(parsed) = url::Url::parse(&u) {
                let _ = win.navigate(parsed);
            }
        }
    }
}

/// 追加一行日志到尾部环形缓冲。
fn push_log_tail(line: &str) {
    let mut tail = DSH_LOG_TAIL.lock().unwrap();
    tail.push_back(line.to_string());
    while tail.len() > LOG_TAIL_LINES {
        tail.pop_front();
    }
}

/// 取日志末尾若干行（崩溃诊断文本）。
fn take_log_tail(lines: usize) -> String {
    let tail = DSH_LOG_TAIL.lock().unwrap();
    let start = tail.len().saturating_sub(lines);
    tail.iter().skip(start).cloned().collect::<Vec<_>>().join("\n")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(DshChild(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            check_app_update,
            install_app_update,
            check_dsh_update,
            update_online_dsh,
            get_dsh_version,
            get_update_prompt_pref,
            set_update_prompt_pref,
            get_auto_check,
            set_auto_check,
            open_settings,
            log_workbar_inject,
            get_dsh_cli_status,
            unify_dsh_cli,
            get_boot_state,
            retry_dsh_start,
            report_client_error,
            remove_plugin_bundles
        ])
        .setup(|app| {
            // 记录主窗口初始加载页 URL：进入错误态时导航回此页显示原因。
            if let Some(win) = app.get_webview_window("main") {
                if let Ok(u) = win.url() {
                    *LOADER_URL.lock().unwrap() = Some(u.to_string());
                }
            }
            // 托盘图标：主窗口被 dsh UI 占据后，更新/设置入口常驻系统托盘。
            if let Err(e) = build_tray(app) {
                eprintln!("[dsh-desktop] 创建托盘失败: {e}");
            }
            // 不阻塞主线程：后台线程拉起 dsh，就绪后再切窗口。
            spawn_dsh(app.handle());
            // 后台检查 dsh 版本（只查不装，有新版发事件提示），不阻塞启动。
            let handle = app.handle().clone();
            std::thread::spawn(move || background_check_dsh_version(&handle));
            // 启动后台检查桌面壳更新：有新版且未关闭提醒/未跳过该版本 → 打开设置窗口弹"是否更新"。
            let handle = app.handle().clone();
            std::thread::spawn(move || startup_check_app_update(&handle));
            // 后台统一终端 dsh 命令路径：检测 PATH 抢占/缺失并自动修复（装了 dsh-desktop 即同源）。
            let handle = app.handle().clone();
            std::thread::spawn(move || auto_unify_dsh_cli(&handle));
            Ok(())
        })
        .on_window_event(|window, event| {
            // 主窗口关闭时隐藏到托盘（应用与 dsh 常驻后台，托盘打开秒开）；
            // 设置窗口关闭则真正销毁，下次打开重新加载。完全退出走托盘「退出」。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // 应用退出时回收 dsh 子进程，避免残留后台进程。
            // 走 kill_dsh_intentionally：递增代数，防看门狗在退出竞态窗口里重启 dsh。
            if let RunEvent::Exit = event {
                kill_dsh_intentionally(app_handle);
            }
        });
}


// ===== 托盘与设置窗口（更新入口，参考 lencx/ChatGPT 的交互） =====

/// 创建托盘图标：主窗口加载 dsh UI 后，更新/设置入口常驻系统托盘。
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "show", "打开主窗口", true, None::<&str>)?;
    let settings_i = MenuItem::with_id(app, "open_settings", "设置…", true, None::<&str>)?;
    let check_i = MenuItem::with_id(app, "check_update", "更新配置", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &settings_i, &check_i, &quit_i])?;
    let icon = app
        .default_window_icon()
        .cloned()
        .expect("缺少窗口图标，无法创建托盘");
    let _tray = TrayIconBuilder::with_id("dsh-tray")
        .icon(icon)
        .tooltip("DeepSeek Harness Desktop")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "open_settings" | "check_update" => {
                // 托盘事件回调会阻塞主窗口消息循环直到返回；窗口 build/show 需主线程处理消息，
                // 直接在回调里同步建窗会造成死锁卡死。故异步到独立线程执行。
                let app2 = app.clone();
                std::thread::spawn(move || {
                    let _ = open_settings_window(&app2);
                });
                // 通知设置窗口立即执行一次更新检查（窗口可能已存在）。
                let _ = app.emit("check-update-request", ());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// 显示并聚焦主窗口。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// 供 dsh UI 工作栏「更新配置」按钮调用的 command：打开（或聚焦）桌面壳设置窗口。
/// async：Tauri async command 运行在 async runtime（非主线程），这样其中再
/// run_on_main_thread 建窗才能从非主线程正常投递，避免同步主线程 command 里的自死锁。
#[tauri::command]
async fn open_settings(app: tauri::AppHandle) -> Result<(), String> {
    open_settings_window(&app).map_err(|e| format!("打开设置窗口失败: {e}"))
}

/// 供注入脚本回传诊断，打印到桌面壳日志。
/// 「已插入」→ 置位工作栏标志，停止注入循环。
#[tauri::command]
fn log_workbar_inject(message: String) {
    eprintln!("[dsh-desktop] 注入: {message}");
    if message.contains("已插入") {
        WORKBAR_INJECTED.store(true, Ordering::Relaxed);
    }
}

/// 客户端模块加载失败回调：注入脚本检测到 SPA 模块 import 失败后调用。
/// async command 拿到 AppHandle，走与进程崩溃相同的看门狗路径。
#[tauri::command]
async fn report_client_error(app: tauri::AppHandle, message: String) {
    eprintln!("[dsh-desktop] 客户端模块加载失败: {message}");
    let n = DSH_CRASHES.fetch_add(1, Ordering::Relaxed) + 1;
    if n >= MAX_CRASHES {
        enter_error_state(&app, format!("dsh 插件模块加载失败（客户端侧，连续 {n} 次）：\n{message}\n\n修复或移除问题插件后点「重试」。"));
        return;
    }
    let backoff = CRASH_BACKOFFS[n.min(CRASH_BACKOFFS.len()) - 1];
    eprintln!("[dsh-desktop] {backoff:?} 后重启 dsh…");
    // 先杀掉活着但页面已坏的 dsh（主动杀，代数递增）。
    kill_dsh_intentionally(&app);
    std::thread::sleep(backoff);
    spawn_dsh(&app);
}

/// 注入到 dsh Web UI 的脚本：等待其底部工作栏（launcher）渲染后，找到「设置」按钮，
/// 在其隔壁插入一个「更新配置」按钮；点击后打开桌面壳设置窗口（内含分别检查
/// dsh 与 dsh-desktop 的入口）。dsh 的 CSS 类名是 hashed 的，故运行时按其可见语义
/// （aria-label/title/文本）自省工作栏与设置按钮，而不是硬编码类名。
const DSH_WORKBAR_INJECT_JS: &str = r#"
(function () {
  if (window.__dshWorkbarInjected__) return;
  window.__dshWorkbarInjected__ = true;
  function log(msg) {
    try {
      if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
        window.__TAURI_INTERNALS__.invoke('log_workbar_inject', { message: String(msg) });
      }
    } catch (_) {}
  }
  function norm(s) { return (s || '').toString().toLowerCase().trim(); }
  // 设置入口：优先精确匹配 title/aria-label 为「设置」（对应工作栏底部 ⚙️ 图标按钮），
  // 其次文本含「设置」（左侧/其它 tab）。优先命中底部 ⚙️，契合「设置按钮隔壁」。
  function settingsScore(el) {
    const title = norm(el.getAttribute('title'));
    const aria = norm(el.getAttribute('aria-label'));
    const text = norm(el.textContent);
    if (title === '设置' || aria === '设置') return 3; // 底部 ⚙️
    if (/设置/.test(title) || /设置/.test(aria)) return 2;
    if (/^设置$/.test(text)) return 1; // 文本「设置」
    return 0;
  }
  // 判断按钮是否位于插件浮层内（如像素办公室 agents-pixe-office 之类）：按容器 id / class 标识辨别，
  // 不能用 position 判定——dsh 主工作栏本身也可能在 absolute 容器里（不能误排）。
  function inOverlay(el) {
    let n = el;
    while (n && n.nodeType === 1) {
      const id = (n.id || '') + ' ' + (n.className && n.className.toString ? n.className.toString() : '');
      if (/pixe|office|agents-pixe|overlay/i.test(id)) return true;
      n = n.parentElement;
    }
    return false;
  }
  function findSettingsButton() {
    const cands = Array.prototype.slice.call(
      document.querySelectorAll('button, [role="button"], [role="tab"]'),
    );
    let best = null, bestScore = 0;
    for (const el of cands) {
      if (inOverlay(el)) continue;
      const s = settingsScore(el);
      if (s > bestScore) { best = el; bestScore = s; }
    }
    return best;
  }
  function insertBtn(settingBtn) {
    const btn = document.createElement('button');
    btn.setAttribute('type', 'button');
    btn.setAttribute('title', '更新配置：检查并更新 dsh 与 dsh-desktop');
    btn.style.cssText =
      'all:unset;cursor:pointer;display:flex;align-items:center;gap:4px;' +
      'padding:5px 9px;margin-left:4px;border-radius:8px;vertical-align:middle;' +
      'font:500 12px/1 inherit;color:inherit;width:100%;box-sizing:border-box;' +
      'background:transparent;transition:background-color 0.2s ease;';
    // 悬浮效果
    btn.addEventListener('mouseenter', function () {
      btn.style.backgroundColor = 'rgba(255, 255, 255, 0.1)';
    });
    btn.addEventListener('mouseleave', function () {
      btn.style.backgroundColor = 'transparent';
    });
    // 刷新图标（内联 SVG，可随大小缩放、颜色继承）
    const ic = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    ic.setAttribute('width', '15');
    ic.setAttribute('height', '15');
    ic.setAttribute('viewBox', '0 0 24 24');
    ic.setAttribute('fill', 'none');
    ic.setAttribute('stroke', 'currentColor');
    ic.setAttribute('stroke-width', '2');
    ic.setAttribute('stroke-linecap', 'round');
    ic.setAttribute('stroke-linejoin', 'round');
    const p1 = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    p1.setAttribute('d', 'M21 2v6h-6');
    const p2 = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    p2.setAttribute('d', 'M3 12a9 9 0 1 1 15 6.7L21 17');
    const p3 = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    p3.setAttribute('d', 'M3 22v-6h6');
    ic.appendChild(p1); ic.appendChild(p2); ic.appendChild(p3);
    const label = document.createElement('span');
    label.textContent = '更新配置';
    btn.appendChild(ic);
    btn.appendChild(label);
    // 点击直接打开更新配置（设置）窗口。
    btn.addEventListener('click', function (e) {
      e.preventDefault();
      e.stopPropagation();
      try {
        if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
          window.__TAURI_INTERNALS__.invoke('open_settings');
        }
      } catch (_) {}
    });
    // 把「更新配置」插到「设置」之前：让底部工作栏显示为 记忆 → 更新配置 → 设置。
    settingBtn.parentNode.insertBefore(btn, settingBtn);
    log('更新配置按钮已插入（位于设置入口之前）');
    return true;
  }
  let tries = 0;
  let mo = null;
  function tryInject() {
    if (tries > 120) { log('未能定位工作栏设置按钮（' + tries + ' 次仍无）'); return; }
    const settingBtn = findSettingsButton();
    if (!settingBtn) return;
    if (settingBtn.__dshInjected__) return;
    settingBtn.__dshInjected__ = true;
    if (insertBtn(settingBtn)) {
      if (mo) { try { mo.disconnect(); } catch (_) {} }
      clearInterval(poll);
    }
  }
  // 首选 MutationObserver：dsh 工作栏一旦挂载立即注入（比纯轮询更即时）；轮询作兜底。
  try {
    mo = new MutationObserver(function () { tryInject(); });
    mo.observe(document.body, { childList: true, subtree: true });
  } catch (_) {}
  // 轮询等待 dsh 工作栏（SPA 异步挂载）渲染。
  var poll = setInterval(function () {
    tries += 1;
    tryInject();
    if (tries > 130) { clearInterval(poll); if (mo) { try { mo.disconnect(); } catch (_) {} } }
  }, 500);
})();
"#;

/// 把工作栏注入脚本执行到主窗口（dsh UI）。脚本内部自行轮询定位工作栏 ⚙️ 设置按钮，
/// 在旁插入「更新配置」按钮，并把插入/失败结果经 log_workbar_inject 回报到桌面壳日志。
fn inject_workbar_button(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.eval(DSH_WORKBAR_INJECT_JS);
    }
}

/// 注入客户端错误检测脚本：SPA 模块加载失败（Vite 预加载 import 失败、dsh fail-loud 的
/// 客户端侧表现）时通知桌面壳触发看门狗。脚本幂等（window 标志防重入）。
const DSH_CLIENT_ERROR_DETECT_JS: &str = r#"
(function () {
  if (window.__dshClientErrorWatched__) return;
  window.__dshClientErrorWatched__ = true;
  function log(msg) {
    try {
      if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
        window.__TAURI_INTERNALS__.invoke('log_workbar_inject', { message: String(msg) });
      }
    } catch (_) {}
  }
  function report(reason) {
    if (window.__dshClientErrorReported__) return;
    window.__dshClientErrorReported__ = true;
    log('客户端模块加载失败: ' + reason);
    try {
      if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
        window.__TAURI_INTERNALS__.invoke('report_client_error', { message: String(reason) });
      }
    } catch (_) {}
  }
  window.addEventListener('unhandledrejection', function (e) {
    var m = (e.reason && e.reason.message) || String(e.reason || '');
    if (/bundle script.*failed to load|Failed to fetch dynamically imported module/i.test(m)) report(m);
  });
  window.addEventListener('error', function (e) {
    var m = e.message || '';
    if (/bundle script.*failed to load|Failed to dynamically import|Loading chunk.*failed/i.test(m)) report(m);
  });
  setInterval(function () {
    if (window.__dshClientErrorReported__) return;
    try {
      var entries = Array.from(window.__dsh_loader_entries && window.__dsh_loader_entries() || []);
      var failed = entries.filter(function (e) { return e.fiber && e.fiber.state === 3 && !e.disabled; });
      if (failed.length > 0) {
        report(failed.map(function (e) { return (e.options && e.options.name) || 'unknown'; }).join(', '));
      }
    } catch (_) {}
  }, 1000);
})();
"#;

fn inject_client_error_watchdog(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.eval(DSH_CLIENT_ERROR_DETECT_JS);
    }
}

/// 打开（或显示）设置窗口。统一调度到主线程执行，避免从托盘回调 / async command / 后台线程
/// 直接同步建窗导致主线程消息循环阻塞（死锁、卡死界面）。
fn open_settings_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    let app2 = app.clone();
    app.run_on_main_thread(move || {
        open_settings_window_on_main(&app2);
    })
}

/// 在主线程执行的设置窗口建窗/显示逻辑。
fn open_settings_window_on_main(app: &tauri::AppHandle) {
    // 若设置窗口已存在（未被关闭）则显示并聚焦；否则新建（默认可见，直接弹出）。
    // 设置窗口关闭即真正销毁（on_window_event 只对 main 隐藏），下次打开重建干净内容。
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return;
    }
    match tauri::WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("设置")
        .inner_size(460.0, 660.0)
        .resizable(false)
        .center()
        // 设置不透明背景，避免 WebView2 首帧透明白屏导致「窗口有但画面空白」。
        .background_color(tauri::utils::config::Color(0x0f, 0x17, 0x2a, 255))
        .build()
    {
        Ok(_win) => eprintln!("[dsh-desktop] 设置窗口已创建"),
        Err(e) => eprintln!("[dsh-desktop] 创建设置窗口失败: {e}"),
    }
}

/// 启动时后台检查桌面壳更新：有新版且「自动检查」开启、且未跳过该版本时，
/// 打开设置窗口，由前端弹「是否更新」（下次不提醒由前端记录到偏好）。
fn startup_check_app_update(app: &tauri::AppHandle) {
    std::thread::sleep(Duration::from_secs(2)); // 等主窗口/托盘就绪
    if !load_prefs(app).auto_check {
        eprintln!("[dsh-desktop] 已关闭启动自动检查更新");
        return;
    }
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[dsh-desktop] 初始化更新器失败: {e}");
            return;
        }
    };
    match tauri::async_runtime::block_on(updater.check()) {
        Ok(Some(update)) => {
            let latest = update.version.to_string();
            if latest == load_prefs(app).skip_update_version {
                eprintln!("[dsh-desktop] 该版本已选择「下次不提醒」（v{latest}），跳过弹窗");
                return;
            }
            eprintln!("[dsh-desktop] 启动检查：发现新版本 v{latest}，打开设置窗口询问");
            if let Err(e) = open_settings_window(app) {
                eprintln!("[dsh-desktop] 打开设置窗口失败: {e}");
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("[dsh-desktop] 启动检查更新失败: {e}"),
    }
}

/// 清理残留的 dsh web 实例（命令行匹配 node 进程中的 dsh bin.js web）。
/// 防止 dev 反复启动 / 异常退出留下的孤儿进程积累成多实例，也用于更新安装前强制清理。
/// 匹配特征：命令行同时含 "dsh" 与 "bin.js ... web" 的 node 进程（即 dsh web 服务本体），
/// 双条件匹配更稳（不限定顺序/路径形式），且不误杀其它 node 应用（Cursor / agentmemory 等）。
#[cfg(windows)]
fn cleanup_stale_dsh() {
    let ps = r#"
Get-CimInstance Win32_Process -Filter "Name='node.exe'" | Where-Object {
  $c = $_.CommandLine
  ($c -like '*bin.js*web*' -and $c -like '*dsh*')
} | ForEach-Object {
  try {
    Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop
    Write-Output ("killed pid=" + $_.ProcessId)
  } catch {}
}
"#;
    match std::process::Command::new("powershell.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", ps])
        .output()
    {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            let killed: Vec<&str> = s.trim().split('\n').filter(|l| !l.trim().is_empty()).collect();
            if !killed.is_empty() {
                eprintln!("[dsh-desktop] 已清理残留 dsh 实例: {}", killed.join(", "));
            }
        }
        Err(e) => eprintln!("[dsh-desktop] 清理残留 dsh 实例失败: {e}"),
    }
}

#[cfg(not(windows))]
fn cleanup_stale_dsh() {
    // macOS / Linux：pkill 匹配 dsh web 进程（仅 node）。
    let _ = std::process::Command::new("pkill")
        .args(["-f", r"dsh.*bin\.js.*web"])
        .status();
}

/// 后台线程拉起 dsh，就绪后把窗口导航到 dsh Web UI，并注入「检查更新」工作栏按钮。
fn spawn_dsh(app: &tauri::AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || match start_dsh(&handle) {
        Some(url) => {
            if let Some(win) = handle.get_webview_window("main") {
                if let Ok(parsed) = url::Url::parse(&url) {
                    let _ = win.navigate(parsed);
                    // dsh 是 SPA，整页导航会替换当前文档；不能在导航前就注入（会被丢弃）。
                    // 改为重复注入：脚本幂等（window.__dshWorkbarInjected__），只有落在 dsh 文档里
                    // 才真正生效；一旦 dsh 工作栏挂载，注入脚本即用 MutationObserver/轮询立即插入
                    // 「更新配置」，去掉了原先固定的 6 秒 sleep 造成的额外等待。成功即停。
                    WORKBAR_INJECTED.store(false, Ordering::Relaxed);
                    let wh = handle.clone();
                    std::thread::spawn(move || {
                        // 重复 eval：脚本用 window 标志幂等，首次命中 dsh 文档即由脚本内部等待
                        // 工作栏挂载后注入；30s 兜底停止。
                        let deadline = Instant::now() + Duration::from_secs(30);
                        while Instant::now() < deadline {
                            if WORKBAR_INJECTED.load(Ordering::Relaxed) { break; }
                            inject_workbar_button(&wh);
                            inject_client_error_watchdog(&wh);
                            std::thread::sleep(Duration::from_millis(1000));
                        }
                    });
                }
            }
        }
        None => {
            // 启动失败：start_dsh 的超时路径已自行进入错误态，此处兜底覆盖「拉起即失败」。
            if DSH_ERROR.lock().unwrap().is_none() {
                let tail = take_log_tail(LOG_TAIL_LINES);
                enter_error_state(
                    &handle,
                    format!("dsh 启动失败（未能拉起服务）。可能原因：全局 dsh 安装损坏、插件解析失败、Node 运行时异常。\n\n最近日志：\n{tail}"),
                );
            }
        }
    });
}

/// 拉起 dsh 并等待就绪，返回可导航的 Web UI 地址。
fn start_dsh(app: &tauri::AppHandle) -> Option<String> {
    // 0. 复用优先：若固定端口已有 dsh 服务在跑（说明桌面壳上次未回收、服务常驻后台），
    //    直接复用，不清理、不重拉，秒开不重载。
    let port_fixed = dsh_port();
    if wait_ready(port_fixed, Duration::from_secs(1)) {
        eprintln!("[dsh-desktop] 复用已在后台运行的 dsh（{port_fixed}）");
        return Some(format!("http://127.0.0.1:{port_fixed}"));
    }
    // 否则清理历史残留的 dsh web 实例，保证单实例运行。
    cleanup_stale_dsh();
    // 1. 优先用固定端口（保持 localStorage origin 稳定），被占用才退回随机空闲端口。
    let port = if port_free(port_fixed) { port_fixed } else { find_free_port()? };
    let port_arg = port.to_string();

    let resource_dir = app.path().resource_dir().ok()?;

    // 2. 决定 dsh 入口：内置 dsh（③ 离线）优先；否则在线方案（②）——
    //    自动跟随 npm 官方最新版，装到全局 npm 前缀（%APPDATA%\npm）后用 node 直接跑
    //    bin.js，绕开 npm exec 内部 `cmd /c` 拉起 dsh 导致的 cmd 弹窗。
    let dsh_bin = match resolve_dsh_bin(&resource_dir) {
        Some(bin) => {
            eprintln!("[dsh-desktop] 使用内置 dsh（离线模式）: {}", display_path(&bin));
            bin
        }
        None => match ensure_online_dsh(app, &resource_dir) {
            Some(bin) => bin,
            None => {
                eprintln!("[dsh-desktop] 在线拉取 dsh 失败，停留在加载页");
                return None;
            }
        },
    };
    let argv: Vec<String> = vec![
        display_path(&dsh_bin),
        "web".into(),
        "--port".into(),
        port_arg,
    ];

    // 3. 用 node sidecar 拉起 dsh。
    let sidecar = app
        .shell()
        .sidecar("node")
        .map_err(|e| eprintln!("[dsh-desktop] 找不到 node sidecar: {e}"))
        .ok()?;

    let (mut rx, child) = sidecar
        .args(&argv)
        .spawn()
        .map_err(|e| eprintln!("[dsh-desktop] 拉起 dsh 失败: {e}"))
        .ok()?;

    // 记录子进程句柄，供退出时 kill。
    if let Some(state) = app.try_state::<DshChild>() {
        *state.0.lock().unwrap() = Some(child);
    }

    // 看门狗代数：本线程看护这一代 dsh。之后任何主动杀/新拉起都会递增全局代数，
    // 使本线程随后收到的 Terminated 被视为「过期事件」而非崩溃（防看门狗与更新流程打架）。
    let generation = DSH_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    DSH_LOG_TAIL.lock().unwrap().clear();
    eprintln!("[dsh-desktop] 已拉起 dsh（第 {generation} 代，端口 {port}）");

    // 后台消费事件流，避免 stdout 管道满导致 dsh 阻塞；
    // Terminated（意外退出且代数仍当前）→ 崩溃自愈：退避重启或 3 连崩转错误态。
    {
        let app2 = app.clone();
        std::thread::spawn(move || {
            while let Some(event) = rx.blocking_recv() {
                match event {
                    CommandEvent::Stdout(bytes) => {
                        if let Ok(line) = String::from_utf8(bytes) {
                            let line = line.trim();
                            if !line.is_empty() {
                                eprintln!("[dsh:out] {line}");
                                push_log_tail(line);
                            }
                        }
                    }
                    CommandEvent::Stderr(bytes) => {
                        if let Ok(line) = String::from_utf8(bytes) {
                            let line = line.trim();
                            if !line.is_empty() {
                                eprintln!("[dsh:err] {line}");
                                push_log_tail(line);
                            }
                        }
                    }
                    CommandEvent::Terminated(payload) => {
                        let still_current = generation == DSH_GENERATION.load(Ordering::Relaxed);
                        if still_current {
                            handle_dsh_crash(&app2, payload.code);
                        } else {
                            eprintln!(
                                "[dsh-desktop] 忽略过期 dsh 退出事件（第 {generation} 代，主动杀/已替换）"
                            );
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // 4. 等待本地服务就绪。
    let url = format!("http://127.0.0.1:{port}");
    if wait_ready(port, READY_TIMEOUT) {
        *DSH_READY_AT.lock().unwrap() = Some(Instant::now());
        Some(url)
    } else {
        eprintln!("[dsh-desktop] dsh 启动超时（端口 {port}）");
        // 超时回收：杀掉已启动但没就绪的 dsh，避免残留占用端口。
        kill_dsh_intentionally(app);
        enter_error_state(
            app,
            format!("dsh 启动超时（>{READY_TIMEOUT:?}，端口 {port}）。可能原因：插件安装损坏、Node 运行时异常。\n\n最近日志：\n{}", take_log_tail(LOG_TAIL_LINES)),
        );
        None
    }
}

/// dsh 意外退出（看门狗判定为崩溃）的处理：退避重启；连续 3 次快速崩溃转错误态。
fn handle_dsh_crash(app: &tauri::AppHandle, code: Option<i32>) {
    let stable = DSH_READY_AT
        .lock().unwrap()
        .map(|at| at.elapsed() >= STABLE_UPTIME)
        .unwrap_or(false);
    if stable {
        // 上一代稳定运行过：视为新一轮偶发故障，计数从 1 重新开始。
        DSH_CRASHES.store(1, Ordering::Relaxed);
    } else {
        DSH_CRASHES.fetch_add(1, Ordering::Relaxed);
    }
    let n = DSH_CRASHES.load(Ordering::Relaxed);
    eprintln!("[dsh-desktop] dsh 意外退出（exit={code:?}），第 {n} 次连续快速崩溃");
    *DSH_READY_AT.lock().unwrap() = None;

    if n >= MAX_CRASHES {
        // 崩到救不活：停自动重启，回加载页显示原因（截 stderr 日志尾，常见为
        // dsh fail-loud 打出的 "fatal load failure" / "did not activate" 插件名）。
        let tail = take_log_tail(LOG_TAIL_LINES);
        enter_error_state(
            app,
            format!("dsh 连续崩溃 {n} 次，已停止自动重启（退出码 {code:?}）。多为某个插件加载/激活失败（dsh 对插件零容错）。修复或移除问题插件后点「重试」。\n\n最近日志：\n{tail}"),
        );
        return;
    }
    // 退避后重启。注意：本函数运行在旧事件消费线程，spawn_dsh 会开新线程/新代数，
    // 代数递增后旧流的后续事件自动失效。
    let backoff = CRASH_BACKOFFS[n.min(CRASH_BACKOFFS.len()) - 1];
    eprintln!("[dsh-desktop] {backoff:?} 后自动重启 dsh…");
    std::thread::sleep(backoff);
    spawn_dsh(app);
}

/// 启动状态（加载页轮询）：starting=正在启动；error=错误态（附原因文本）。
/// 成功时窗口已被导航去 dsh UI，加载页随之卸载，无需 ok 状态。
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct BootState {
    status: String,
    error: Option<String>,
}

#[tauri::command]
fn get_boot_state() -> BootState {
    match DSH_ERROR.lock().unwrap().clone() {
        Some(error) => BootState { status: "error".into(), error: Some(error) },
        None => BootState { status: "starting".into(), error: None },
    }
}

/// 用户在错误态点「重试」：清错误态与崩溃计数后重新拉起 dsh。
#[tauri::command]
fn retry_dsh_start(app: tauri::AppHandle) {
    eprintln!("[dsh-desktop] 用户触发重试");
    *DSH_ERROR.lock().unwrap() = None;
    DSH_CRASHES.store(0, Ordering::Relaxed);
    spawn_dsh(&app);
}

/// 从 dsh profile 的 `dsh.profile.bundles` 中移除指定插件条目，然后重启 dsh。
/// 等效于手工编辑 `~/.dsh/profiles/web/package.json` 去掉坏插件再重启——
/// dsh 加载 profile 时跳过不在 bundles 里的条目，不再 fail-loud。
#[tauri::command]
async fn remove_plugin_bundles(app: tauri::AppHandle, plugins: Vec<String>) -> Result<(), String> {
    eprintln!("[dsh-desktop] 移除插件: {plugins:?}");
    // profile web 的 package.json 在 ~/.dsh/profiles/web/。
    let home = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(PathBuf::from))
        .map_err(|_| "无法定位用户目录".to_string())?;
    let profile_pkg = home.join(".dsh/profiles/web/package.json");
    if !profile_pkg.exists() {
        return Err("profile package.json 不存在".into());
    }
    let text = std::fs::read_to_string(&profile_pkg)
        .map_err(|e| format!("读取 package.json 失败: {e}"))?;
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut removed = 0usize;
    for plugin in &plugins {
        let needle = format!("\"{plugin}\"");
        let before = lines.len();
        lines.retain(|line| !line.contains(&needle));
        if lines.len() < before { removed += 1; }
    }
    if removed == 0 {
        return Err("未在 bundles 中找到指定插件".into());
    }
    let new_text = lines.join("\n");
    std::fs::write(&profile_pkg, &new_text)
        .map_err(|e| format!("写入 package.json 失败: {e}"))?;
    eprintln!("[dsh-desktop] 已移除 {removed} 个插件条目，重启 dsh");

    // 重启：走看门狗路径，避免与更新流程打架。
    kill_dsh_intentionally(&app);
    DSH_CRASHES.store(0, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(500));
    spawn_dsh(&app);
    Ok(())
}

/// 定位 dsh 入口脚本：环境变量覆盖 → 打包内置 → 无（走在线方案）。
fn resolve_dsh_bin(resource_dir: &Path) -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("DSH_BIN") {
        let p = PathBuf::from(bin);
        if p.exists() {
            return Some(p);
        }
    }
    let bundled = resource_dir.join("dsh-runtime/node_modules/@deepseek-ai/dsh/lib/bin.js");
    if bundled.exists() {
        return Some(bundled);
    }
    None
}

/// 全局 dsh 的入口 bin.js：桌面壳与 dsh 命令（globalize-dsh.mjs 装到用户级 npm 前缀）
/// 共用同一份全局安装，彻底单一来源。Windows 用户级前缀即 `%APPDATA%\npm`；
/// 可用环境变量 DSH_GLOBAL_DIR 覆盖（调试/异构环境）。
fn global_dsh_bin() -> Option<PathBuf> {
    let dir = if let Ok(dir) = std::env::var("DSH_GLOBAL_DIR") {
        PathBuf::from(dir)
    } else {
        #[cfg(windows)]
        {
            std::env::var("APPDATA").ok().map(|a| PathBuf::from(a).join("npm"))?
        }
        #[cfg(not(windows))]
        {
            // POSIX：globalize-dsh.mjs 的用户级回退前缀，与 dsh CLI 同源。
            std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".npm-global"))?
        }
    };
    let bin = dir.join("node_modules/@deepseek-ai/dsh/lib/bin.js");
    bin.exists().then_some(bin)
}

/// 在线方案：直接使用全局安装的 dsh（与终端 dsh 命令同源）。
/// 命中直接运行（不联网、不检查版本、不自动更新，保证快速启动）；
/// 未安装（首次运行）才联网安装官方最新版到全局前缀。
/// 版本更新不在这里做：由后台检查（background_check_dsh_version）发事件提示，
/// 用户手动触发 update_online_dsh 才会真正更新。
fn ensure_online_dsh(app: &tauri::AppHandle, resource_dir: &Path) -> Option<PathBuf> {
    // 全局已装：直接用，不联网、不更新。
    if let Some(bin) = global_dsh_bin() {
        match installed_dsh_version(&bin) {
            Some(ver) => eprintln!("[dsh-desktop] 使用全局 dsh v{ver}: {}", display_path(&bin)),
            None => eprintln!("[dsh-desktop] 使用全局 dsh: {}", display_path(&bin)),
        }
        return Some(bin);
    }
    let dir = global_dsh_dir()?;

    // 首次安装：查 npm 官方最新版并安装（装到全局前缀，与 dsh CLI 共用）。
    let latest = match fetch_latest_dsh_version() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[dsh-desktop] 首次安装失败（查询 npm 最新版失败）: {e}");
            return None;
        }
    };
    eprintln!("[dsh-desktop] 首次运行，全局安装 dsh v{latest}…");
    let npm_cli = resource_dir.join("node-runtime/node_modules/npm/bin/npm-cli.js");
    if run_npm_install(app, &npm_cli, &dir, &latest) {
        match global_dsh_bin() {
            Some(bin) => {
                let ver = installed_dsh_version(&bin).unwrap_or_else(|| latest.clone());
                eprintln!("[dsh-desktop] 全局 dsh v{ver} 安装完成");
                Some(bin)
            }
            None => {
                eprintln!("[dsh-desktop] dsh 安装完成但校验失败");
                None
            }
        }
    } else {
        eprintln!("[dsh-desktop] dsh 安装失败");
        None
    }
}

/// 全局 dsh 安装目录（不含确认 bin 是否存在）。见 global_dsh_bin。
fn global_dsh_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DSH_GLOBAL_DIR") {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        std::env::var("APPDATA").ok().map(|a| PathBuf::from(a).join("npm"))
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".npm-global"))
    }
}

/// 读 dsh 的 package.json 版本。
fn installed_dsh_version(bin: &Path) -> Option<String> {
    read_pkg_version(&bin.parent()?.parent()?.join("package.json"))
}

/// 用内置 node + npm 把指定版本的 dsh 全局安装到 dir 目录。
/// 保证目录存在；`--ignore-scripts`：dsh 无生命周期脚本，避免任何 cmd 子进程。
/// 等待安装结束并返回是否成功。
fn run_npm_install(app: &tauri::AppHandle, npm_cli: &Path, dir: &Path, version: &str) -> bool {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("[dsh-desktop] 创建 dsh 全局目录失败: {e}");
        return false;
    }
    // 全局安装：-g 且显式 --prefix 指到用户级全局前缀，保证与 dsh CLI 同目录。
    let args = vec![
        display_path(npm_cli),
        "install".into(),
        "-g".into(),
        "--prefix".into(),
        display_path(dir),
        format!("@deepseek-ai/dsh@{version}"),
        "--ignore-scripts".into(),
        "--no-audit".into(),
        "--no-fund".into(),
    ];
    let sidecar = match app.shell().sidecar("node") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[dsh-desktop] 找不到 node sidecar: {e}");
            return false;
        }
    };
    let (mut rx, _child) = match sidecar.args(args).spawn() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[dsh-desktop] 拉起 npm install 失败: {e}");
            return false;
        }
    };
    let start = Instant::now();
    while start.elapsed() < INSTALL_TIMEOUT {
        match rx.blocking_recv() {
            Some(CommandEvent::Terminated(payload)) => {
                let ok = payload.code == Some(0);
                eprintln!("[dsh-desktop] npm install 结束（exit={:?}）", payload.code);
                return ok;
            }
            Some(CommandEvent::Stdout(bytes)) | Some(CommandEvent::Stderr(bytes)) => {
                if let Ok(line) = String::from_utf8(bytes) {
                    let line = line.trim();
                    if !line.is_empty() {
                        eprintln!("[dsh:install] {line}");
                        push_log_tail(line);
                    }
                }
            }
            Some(CommandEvent::Error(e)) => eprintln!("[dsh:install] 事件错误: {e}"),
            Some(_) => {} // CommandEvent 为 #[non_exhaustive]，预留其它事件
            None => {
                eprintln!("[dsh-desktop] npm install 事件流中断");
                return false;
            }
        }
    }
    eprintln!("[dsh-desktop] npm install 超时（>{INSTALL_TIMEOUT:?}）");
    false
}

/// 去掉 Windows 长路径前缀 `\\?\`，转成普通路径给子进程用。
fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    match s.strip_prefix(r"\\?\") {
        Some(stripped) => stripped.to_string(),
        None => s,
    }
}


/// 绑定回环地址的临时端口，返回一个大概率空闲的端口号。
fn find_free_port() -> Option<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    listener.local_addr().ok().map(|addr| addr.port())
}

/// 指定端口是否空闲（可绑定）。
fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// 轮询探测本地服务，直到 HTTP 200（真探活）或超时。
/// 只测 TCP 可连不够：进程半死（如插件加载失败后端口仍被占但不再响应）会让
/// 复用/就绪判定误判，窗口导航过去就是空白。HTTP 200 = Web UI 真正可服务。
fn wait_ready(port: u16, timeout: Duration) -> bool {
    let url = format!("http://127.0.0.1:{port}/");
    let start = Instant::now();
    while start.elapsed() < timeout {
        // TCP 先探一层，避免对未监听端口高频发 HTTP（连接层失败在 Windows 上较慢）。
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            if let Ok(resp) = ureq::get(&url).timeout(Duration::from_secs(2)).call() {
                if resp.status() == 200 {
                    return true;
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    false
}

// ===== 自动更新（tauri-plugin-updater）与 dsh 更新检查 =====

/// 桌面壳自身更新信息（发给前端）。
#[derive(Serialize)]
struct AppUpdateInfo {
    has_update: bool,
    current_version: String,
    latest_version: String,
    body: String,
}

/// dsh 更新信息（查询 npm 官方最新版）。
#[derive(Serialize)]
struct DshUpdateInfo {
    has_update: bool,
    current_version: String,
    latest_version: String,
    /// 是否在线方案（无内置 dsh，可一键升级；离线版需重新打包）。
    online: bool,
}

/// 是否离线方案（resources 里内置了 dsh）。
fn bundled_dsh_exists(app: &tauri::AppHandle) -> bool {
    app.path()
        .resource_dir()
        .map(|dir| dir.join("dsh-runtime/node_modules/@deepseek-ai/dsh/lib/bin.js").exists())
        .unwrap_or(false)
}

/// 下载进度事件负载（发给前端）。
#[derive(Clone, Serialize)]
struct UpdateProgress {
    downloaded: u64,
    total: u64,
}

/// 检查桌面壳自身是否有新版本（联网请求 updater endpoint）。
#[tauri::command]
async fn check_app_update(app: tauri::AppHandle) -> Result<AppUpdateInfo, String> {
    let current = app.package_info().version.to_string();
    let updater = app.updater().map_err(|e| format!("初始化更新器失败: {e}"))?;
    match updater.check().await {
        Ok(Some(update)) => Ok(AppUpdateInfo {
            has_update: true,
            current_version: current,
            latest_version: update.version.to_string(),
            body: update.body.clone().unwrap_or_default(),
        }),
        Ok(None) => Ok(AppUpdateInfo {
            has_update: false,
            current_version: current.clone(),
            latest_version: current,
            body: String::new(),
        }),
        Err(e) => Err(format!("检查更新失败: {e}")),
    }
}

/// 下载并安装新版本，完成后自动重启到新版本。
#[tauri::command]
async fn install_app_update(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    let updater = app.updater().map_err(|e| format!("初始化更新器失败: {e}"))?;
    let update = updater
        .check()
        .await
        .map_err(|e| format!("检查更新失败: {e}"))?
        .ok_or_else(|| "当前已是最新版本".to_string())?;

    // 安装前需要清理后台 node/dsh 进程：NSIS 安装器要替换/关闭运行中的文件，
    // node 侧车（dsh）若仍在会锁住资源/端口导致安装失败；同时避免安装后复用旧的 dsh。
    // on_download_finish（第二个回调）在下载完成、install() 执行前触发，正是清理时机。
    let app_for_kill = app.clone();
    update
        .download_and_install(
            |downloaded, total| {
                let _ = window.emit(
                    "update-progress",
                    UpdateProgress {
                        downloaded: downloaded as u64,
                        total: total.unwrap_or(0),
                    },
                );
            },
            || {
                // 1) 杀掉当前 dsh 子进程树（node sidecar + 其拉起的 dsh 子进程）。
                //    走主动杀入口（递增代数），防看门狗把这次杀当成崩溃而自动重启、打架。
                kill_dsh_intentionally(&app_for_kill);
                // 2) 兜底清理任何残留的 dsh web 实例（防孤儿进程占端口/锁文件）。
                cleanup_stale_dsh();
                let _ = window.emit("update-ready", ());
            },
        )
        .await
        .map_err(|e| format!("下载/安装更新失败: {e}"))?;

    // 安装完成：退出并自动重启（NSIS 安装器完成替换后拉起新版本）。
    app.restart()
}

/// 检查官方 dsh（@deepseek-ai/dsh）是否有新版本（查询 npm registry）。
#[tauri::command]
async fn check_dsh_update(app: tauri::AppHandle) -> Result<DshUpdateInfo, String> {
    let current = current_dsh_version(&app);
    let latest = tauri::async_runtime::spawn_blocking(fetch_latest_dsh_version)
        .await
        .map_err(|e| format!("查询 npm 失败: {e}"))??;
    Ok(DshUpdateInfo {
        has_update: latest != current,
        current_version: current,
        latest_version: latest,
        online: !bundled_dsh_exists(&app),
    })
}

/// 当前 dsh 版本：优先读打包内置的 package.json（离线方案），
/// 否则读全局安装（%APPDATA%\npm，与 dsh CLI 同源）里实际安装的版本；都没有则返回"未知"。
fn current_dsh_version(app: &tauri::AppHandle) -> String {
    if let Ok(dir) = app.path().resource_dir() {
        if let Some(v) = read_pkg_version(&dir.join("dsh-runtime/node_modules/@deepseek-ai/dsh/package.json")) {
            return v;
        }
    }
    if let Some(bin) = global_dsh_bin() {
        if let Some(v) = installed_dsh_version(&bin) {
            return v;
        }
    }
    "未知".to_string()
}

/// 读 package.json 的 version 字段。
fn read_pkg_version(pkg: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pkg).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// 查询当前 dsh 版本（不联网，仅供界面展示）。
#[tauri::command]
fn get_dsh_version(app: tauri::AppHandle) -> String {
    current_dsh_version(&app)
}

/// 查询 npm 上 @deepseek-ai/dsh 的最新版本号。
fn fetch_latest_dsh_version() -> Result<String, String> {
    let resp = ureq::get("https://registry.npmjs.org/@deepseek-ai/dsh/latest")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| format!("请求 npm registry 失败: {e}"))?;
    let json: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("解析 npm 返回失败: {e}"))?;
    json.get("version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "npm 返回缺少 version 字段".to_string())
}

// ===== 在线 dsh 版本检查（只查不装）与手动更新 =====

/// dsh 版本检查结果（后台检查 → 发事件给前端）。
#[derive(Clone, Serialize)]
struct DshVersionInfo {
    current_version: String,
    latest_version: String,
}

/// 后台检查 dsh 是否有新版本：只查询不安装，有新版时通过事件 `dsh-update-available` 通知前端。
/// 仅在线方案（无内置 dsh）才检查；离线方案 dsh 更新走构建期重新打包。
fn background_check_dsh_version(app: &tauri::AppHandle) {
    // 稍等片刻，等前端加载页挂好事件监听器再发。
    std::thread::sleep(Duration::from_millis(2000));

    if let Ok(resource_dir) = app.path().resource_dir() {
        let bundled = resource_dir.join("dsh-runtime/node_modules/@deepseek-ai/dsh/lib/bin.js");
        if bundled.exists() {
            eprintln!("[dsh-desktop] 离线方案，跳过 dsh 后台版本检查");
            return;
        }
    }

    let current = current_dsh_version(app);
    match fetch_latest_dsh_version() {
        Ok(latest) => {
            if latest != current {
                eprintln!("[dsh-desktop] 后台检查：dsh 有新版本 v{latest}（当前 v{current}）");
                let _ = app.emit(
                    "dsh-update-available",
                    DshVersionInfo {
                        current_version: current,
                        latest_version: latest,
                    },
                );
            }
        }
        Err(e) => eprintln!("[dsh-desktop] 后台检查 dsh 版本失败: {e}"),
    }
}

/// 手动更新在线 dsh：把全局安装的 dsh 升到官方最新版（npm install -g），
/// 成功后重启 dsh 进程。失败不影响正在运行的 dsh。返回更新后的版本号。
#[tauri::command]
async fn update_online_dsh(app: tauri::AppHandle) -> Result<String, String> {
    let latest = tauri::async_runtime::spawn_blocking(fetch_latest_dsh_version)
        .await
        .map_err(|e| format!("查询 npm 失败: {e}"))??;

    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|e| format!("无法定位资源目录: {e}"))?;

    // 离线版（内置 dsh）不适用在线更新：dsh 打包在 resources 里，只能构建期重新打包。
    if resource_dir
        .join("dsh-runtime/node_modules/@deepseek-ai/dsh/lib/bin.js")
        .exists()
    {
        return Err("当前为离线版（内置 dsh），更新需 just update 后重新打包".to_string());
    }

    let dir = global_dsh_dir().ok_or_else(|| "无法定位全局 dsh 目录".to_string())?;
    let npm_cli = resource_dir.join("node-runtime/node_modules/npm/bin/npm-cli.js");

    // 1. 全局安装最新版（与终端 dsh CLI 共用同一份，升级后两者同步）。
    let app2 = app.clone();
    let dir2 = dir.clone();
    let latest2 = latest.clone();
    let ok = tauri::async_runtime::spawn_blocking(move || {
        run_npm_install(&app2, &npm_cli, &dir2, &latest2)
    })
    .await
    .map_err(|e| format!("安装 dsh 失败: {e}"))?;
    if !ok {
        return Err("安装 dsh 失败".to_string());
    }
    // 校验全局安装的版本确实是最新版。
    let bin = global_dsh_bin().ok_or_else(|| "安装完成但找不到全局 dsh 入口".to_string())?;
    if installed_dsh_version(&bin).as_deref() != Some(latest.as_str()) {
        return Err("安装完成但版本校验失败".to_string());
    }

    // 2. 重启 dsh：先停旧进程（主动杀，防看门狗误判），再拉起新的。
    kill_dsh_intentionally(&app);

    // 3. 重新拉起 dsh 并切窗口。
    spawn_dsh(&app);
    Ok(latest)
}

// ===== 终端 dsh 命令路径统一（桌面壳与终端同源） =====
// 背景：端用户可能自己 `npm i -g @deepseek-ai/dsh`（管理员终端）装到机器级前缀
// （C:\Program Files\nodejs），系统 PATH 又排在用户级 %APPDATA%\npm 之前，
// 终端 `dsh`/`npx dsh` 会解析到旧副本，与桌面壳管理的副本分裂、版本漂移。
// 目标：装了 dsh-desktop 就自动统一——启动时检测 PATH 抢占/缺失并自动修复，
// 用户零心智负担；需要管理员权限时弹一次 UAC，失败进入 7 天冷却（设置页可手动重试）。

/// 终端 dsh 命令解析状态。
#[derive(Clone, Serialize)]
struct DshCliStatus {
    /// 终端 dsh 是否已与桌面壳管理的副本同源。
    unified: bool,
    /// ok=已同源 / npm-global=其它 npm 全局前缀抢占（可自动修复）/ other-path=非 npm 来源抢占（仅提示）/ missing=终端无 dsh。
    kind: String,
    /// 桌面壳管理的全局前缀（Windows 为 %APPDATA%\npm）。
    app_prefix: String,
    /// 终端实际解析到的 dsh（where 第一命中）。
    terminal_path: Option<String>,
    /// 所有抢占来源（非本前缀的 dsh 入口，含已在阴影中的）。
    foreign_paths: Vec<String>,
}

/// 路径目录归一化：小写、反斜杠、去尾部斜杠、去 \\?\ 前缀，用于跨形式比较。
#[cfg(windows)]
fn normalize_dir(p: &Path) -> String {
    let mut s = display_path(p).to_lowercase().replace('/', "\\");
    while s.ends_with('\\') {
        s.pop();
    }
    s
}

/// 扫描终端 dsh 命令解析：where.exe 按 PATH 顺序列出全部命中，第一命中即终端实际执行者。
#[cfg(windows)]
fn scan_terminal_dsh() -> DshCliStatus {
    let app_prefix = global_dsh_dir().unwrap_or_default();
    let prefix_norm = normalize_dir(&app_prefix);
    let mut hits: Vec<String> = Vec::new();
    if let Ok(out) = std::process::Command::new("where.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("dsh")
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let t = line.trim();
            if !t.is_empty() {
                hits.push(t.to_string());
            }
        }
    }
    let terminal_path = hits.first().cloned();
    let foreign_paths: Vec<String> = hits
        .iter()
        .filter(|h| {
            Path::new(h)
                .parent()
                .map(|d| normalize_dir(d) != prefix_norm)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    let (unified, kind) = match terminal_path.as_deref() {
        None => (false, "missing".to_string()),
        Some(first) => {
            let winner = Path::new(first).parent().map(normalize_dir).unwrap_or_default();
            if winner == prefix_norm {
                (true, "ok".to_string())
            } else if Path::new(first)
                .parent()
                .map(|d| d.join("node_modules/@deepseek-ai/dsh").exists())
                .unwrap_or(false)
            {
                (false, "npm-global".to_string())
            } else {
                (false, "other-path".to_string())
            }
        }
    };
    DshCliStatus {
        unified,
        kind,
        app_prefix: display_path(&app_prefix),
        terminal_path,
        foreign_paths,
    }
}

#[cfg(not(windows))]
fn scan_terminal_dsh() -> DshCliStatus {
    // POSIX：globalize 良好场景下终端与桌面壳共用 ~/.npm-global，暂不做抢占检测。
    DshCliStatus {
        unified: true,
        kind: "ok".to_string(),
        app_prefix: global_dsh_dir().map(|d| display_path(&d)).unwrap_or_default(),
        terminal_path: None,
        foreign_paths: vec![],
    }
}

/// 运行一段 PowerShell 脚本（无窗口），返回 (成功?, stdout+stderr)。
/// 通过环境变量 DSH_PREFIX 传参，彻底规避引号转义问题（同 cleanup_stale_dsh 的教训）。
/// 仅 Windows 使用（powershell.exe + CREATE_NO_WINDOW 均为 Windows 专有），故 cfg(windows)。
#[cfg(windows)]
fn run_ps(script: &str, prefix: &str) -> (bool, String) {
    match std::process::Command::new("powershell.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .env("DSH_PREFIX", prefix)
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", script])
        .output()
    {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).to_string();
            s.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.success(), s)
        }
        Err(e) => (false, format!("{e}")),
    }
}

/// 确保 %APPDATA%\npm 在用户 PATH（读注册表不展开形式，保留 %VAR% 引用；写回原值类型并广播环境变更）。
#[cfg(windows)]
fn ensure_user_path_contains_prefix() -> Result<(), String> {
    let prefix = global_dsh_dir().ok_or("无法定位全局前缀")?;
    let ps = r#"
$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
if ($null -eq $k) { Write-Output 'ERR: 打不开 HKCU\Environment'; exit 1 }
$kind = try { $k.GetValueKind('Path') } catch { 'String' }
$raw = [string]$k.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
$expanded = [Environment]::ExpandEnvironmentVariables($raw)
if ($expanded -split ';' -notcontains $env:DSH_PREFIX) {
  $new = if ($raw.Trim()) { $raw.TrimEnd(';') + ';' + $env:DSH_PREFIX } else { $env:DSH_PREFIX }
  $type = if ($kind -eq 'ExpandString') { 'ExpandString' } else { 'String' }
  $k.SetValue('Path', $new, [Microsoft.Win32.RegistryValueKind]::$type)
  try {
    Add-Type -Namespace W32 -Name NM -MemberDefinition '[DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Auto)] public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);'
    $r = [UIntPtr]::Zero
    [W32.NM]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$r) | Out-Null
  } catch {}
  Write-Output 'ADDED'
} else { Write-Output 'EXISTS' }
"#;
    let (ok, out) = run_ps(ps, &display_path(&prefix));
    if ok { Ok(()) } else { Err(format!("写入用户 PATH 失败: {out}")) }
}

/// 无提权直接移除外来 npm 前缀里的 dsh（目录可写时）。先探测可写性，避免删一半。
#[cfg(windows)]
fn try_remove_foreign_direct(dir: &Path) -> bool {
    let probe = dir.join(".dsh-desktop-write-probe");
    let writable = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    if !writable {
        return false;
    }
    for shim in ["dsh", "dsh.cmd", "dsh.ps1"] {
        let p = dir.join(shim);
        if p.exists() {
            if let Err(e) = std::fs::remove_file(&p) {
                eprintln!("[dsh-desktop] 删除外来 dsh 入口失败 {}: {e}", display_path(&p));
                return false;
            }
        }
    }
    let pkg = dir.join("node_modules/@deepseek-ai/dsh");
    if pkg.exists() {
        if let Err(e) = std::fs::remove_dir_all(&pkg) {
            eprintln!("[dsh-desktop] 删除外来 dsh 包失败 {}: {e}", display_path(&pkg));
            return false;
        }
    }
    true
}

/// 提权卸载机器级 npm 前缀里的 dsh（Program Files 等不可写目录）。会弹一次 UAC。
#[cfg(windows)]
fn elevated_npm_uninstall(dir: &Path) -> Result<(), String> {
    let ps = r#"
try {
  $p = Start-Process -FilePath 'npm.cmd' -ArgumentList @('uninstall','-g','@deepseek-ai/dsh','--prefix', $env:DSH_PREFIX) -Verb RunAs -Wait -PassThru -WindowStyle Hidden
  if ($p.ExitCode -ne 0) { Write-Output ('ERR: npm 退出码 ' + $p.ExitCode); exit 1 }
  Write-Output 'OK'
} catch { Write-Output ('ERR: ' + $_.Exception.Message); exit 2 }
"#;
    let (ok, out) = run_ps(ps, &display_path(dir));
    if ok {
        Ok(())
    } else {
        Err(format!("提权卸载失败（用户取消或 npm 出错）: {out}"))
    }
}

/// 统一终端 dsh：按冲突类型自动修复，返回人话结果。
#[cfg(windows)]
fn unify_terminal_dsh() -> Result<String, String> {
    let st = scan_terminal_dsh();
    if st.unified {
        return Ok("终端 dsh 已与桌面壳同源".into());
    }
    match st.kind.as_str() {
        "missing" => {
            ensure_user_path_contains_prefix()?;
            let after = scan_terminal_dsh();
            if after.unified {
                Ok("已把全局前缀加入用户 PATH，新开终端即生效".into())
            } else {
                Err("已写入 PATH 但仍解析异常，请检查环境变量".into())
            }
        }
        "npm-global" => {
            let winner = Path::new(
                st.terminal_path.as_deref().unwrap_or_default(),
            )
            .parent()
            .ok_or("无法定位抢占来源目录")?
            .to_path_buf();
            eprintln!(
                "[dsh-desktop] 检测到终端 dsh 被另一 npm 全局前缀抢占: {}",
                display_path(&winner)
            );
            if !try_remove_foreign_direct(&winner) {
                elevated_npm_uninstall(&winner)?;
            }
            // 删完外来副本后若终端仍找不到 dsh（用户 PATH 缺前缀），补上。
            let after = scan_terminal_dsh();
            if after.kind == "missing" {
                let _ = ensure_user_path_contains_prefix();
            }
            let final_st = scan_terminal_dsh();
            if final_st.unified {
                Ok(format!(
                    "已移除抢占副本（{}），终端 dsh 现与桌面壳同源",
                    display_path(&winner)
                ))
            } else {
                Err("卸载完成但终端解析仍异常，请重启终端后重试".into())
            }
        }
        _ => Err(format!(
            "检测到非 npm 来源的 dsh 抢占 PATH（{}），请在设置中手动处理",
            st.foreign_paths.join(", ")
        )),
    }
}

#[cfg(not(windows))]
fn unify_terminal_dsh() -> Result<String, String> {
    Ok("此平台暂无需统一".into())
}

/// 启动后台自动统一：失败进入 7 天冷却，避免每次启动都弹 UAC 打扰。
fn auto_unify_dsh_cli(app: &tauri::AppHandle) {
    let st = scan_terminal_dsh();
    if st.unified {
        eprintln!(
            "[dsh-desktop] 终端 dsh 已同源: {}",
            st.terminal_path.as_deref().unwrap_or("(无)")
        );
        let _ = app.emit("dsh-cli-status", st);
        return;
    }
    eprintln!(
        "[dsh-desktop] 终端 dsh 不同源（{}），尝试自动统一…",
        st.kind
    );
    // 冷却：上次失败 7 天内不再自动尝试（设置页按钮不受冷却限制）。
    if let Some(dir) = app.path().app_config_dir().ok() {
        let marker = dir.join("dsh-cli-unify.fail");
        if let Ok(meta) = std::fs::metadata(&marker) {
            if let Ok(mtime) = meta.modified() {
                if mtime.elapsed().map(|d| d.as_secs() < 7 * 86400).unwrap_or(false) {
                    eprintln!("[dsh-desktop] 自动统一冷却中（上次失败未满 7 天），跳过");
                    let _ = app.emit("dsh-cli-status", st);
                    return;
                }
            }
        }
    }
    match unify_terminal_dsh() {
        Ok(msg) => {
            eprintln!("[dsh-desktop] 统一成功: {msg}");
            if let Some(dir) = app.path().app_config_dir().ok() {
                let _ = std::fs::remove_file(dir.join("dsh-cli-unify.fail"));
            }
        }
        Err(e) => {
            eprintln!("[dsh-desktop] 自动统一失败: {e}");
            if let Some(dir) = app.path().app_config_dir().ok() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(dir.join("dsh-cli-unify.fail"), b"");
            }
        }
    }
    let _ = app.emit("dsh-cli-status", scan_terminal_dsh());
}

/// 查询终端 dsh 命令解析状态（设置页展示）。
#[tauri::command]
fn get_dsh_cli_status() -> DshCliStatus {
    scan_terminal_dsh()
}

/// 手动触发统一（设置页按钮，不受冷却限制）。
#[tauri::command]
fn unify_dsh_cli() -> Result<String, String> {
    unify_terminal_dsh()
}

// ===== 更新偏好（启动自动检查开关 + "下次不提醒此版本"） =====

/// 持久化的更新偏好。
#[derive(Serialize, Deserialize)]
struct UpdatePrefs {
    /// 启动时自动检查更新并提醒（默认开启）。
    #[serde(default = "default_true")]
    auto_check: bool,
    /// 用户选择"下次不提醒"的版本号（同一版本不再弹，新版本仍会提醒）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    skip_update_version: String,
}

fn default_true() -> bool {
    true
}

impl Default for UpdatePrefs {
    fn default() -> Self {
        Self {
            auto_check: true,
            skip_update_version: String::new(),
        }
    }
}

/// 偏好文件路径：app_data/update-prefs.json。
fn prefs_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("update-prefs.json"))
}

/// 读取偏好（文件缺失/损坏时用默认值）。
fn load_prefs(app: &tauri::AppHandle) -> UpdatePrefs {
    let Some(path) = prefs_path(app) else {
        return UpdatePrefs::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return UpdatePrefs::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 写回偏好文件。
fn save_prefs(app: &tauri::AppHandle, prefs: &UpdatePrefs) -> Result<(), String> {
    let path = prefs_path(app).ok_or_else(|| "无法定位应用数据目录".to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let text = serde_json::to_string_pretty(prefs).map_err(|e| format!("序列化偏好失败: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入偏好失败: {e}"))
}

/// 读取"下次不提醒"的版本号（未设置返回空串）。
#[tauri::command]
fn get_update_prompt_pref(app: tauri::AppHandle) -> String {
    load_prefs(&app).skip_update_version
}

/// 设置"下次不提醒"的版本号。
#[tauri::command]
fn set_update_prompt_pref(app: tauri::AppHandle, version: String) -> Result<(), String> {
    let mut prefs = load_prefs(&app);
    prefs.skip_update_version = version;
    save_prefs(&app, &prefs)
}

/// 是否启动时自动检查更新。
#[tauri::command]
fn get_auto_check(app: tauri::AppHandle) -> bool {
    load_prefs(&app).auto_check
}

/// 设置启动时自动检查更新开关。
#[tauri::command]
fn set_auto_check(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let mut prefs = load_prefs(&app);
    prefs.auto_check = enabled;
    save_prefs(&app, &prefs)
}
