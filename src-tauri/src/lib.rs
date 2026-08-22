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


use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// dsh Web UI 固定端口：保持 origin（127.0.0.1:<port>）跨重启稳定，localStorage 才能持久（否则随机端口导致 origin 每次变化、localStorage 被清空）。
const DSH_PORT: u16 = 5399;

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
            log_workbar_inject
        ])
        .setup(|app| {
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
            if let RunEvent::Exit = event {
                if let Some(state) = app_handle.try_state::<DshChild>() {
                    if let Some(child) = state.0.lock().unwrap().take() {
                        kill_process_tree(child);
                    }
                }
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

/// 供工作栏注入脚本回传诊断（是否找到工作栏/设置按钮、是否插入成功），打印到桌面壳日志。
#[tauri::command]
fn log_workbar_inject(message: String) {
    eprintln!("[dsh-desktop] 工作栏注入: {message}");
    if message.contains("已插入") {
        WORKBAR_INJECTED.store(true, Ordering::Relaxed);
    }
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
      'all:unset;cursor:pointer;display:inline-flex;align-items:center;gap:4px;' +
      'padding:5px 9px;margin-left:4px;border-radius:8px;vertical-align:middle;' +
      'font:500 12px/1 inherit;color:inherit;';
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
/// 防止 dev 反复启动 / 异常退出留下的孤儿进程积累成多实例。
/// 匹配特征：命令行同时含 "dsh"、"bin.js"、"web" 的 node 进程（即 dsh web 服务本体），
/// 不匹配其他 node 应用（Cursor / agentmemory 等）。
#[cfg(windows)]
fn cleanup_stale_dsh() {
    let ps = r#"
Get-CimInstance Win32_Process -Filter "Name = 'node.exe'" |
  Where-Object { $_.CommandLine -like '*dsh*bin.js*web*' } |
  ForEach-Object {
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
                            std::thread::sleep(Duration::from_millis(1000));
                        }
                    });
                }
            }
        }
        None => eprintln!("[dsh-desktop] 启动 dsh 失败，停留在加载页"),
    });
}

/// 拉起 dsh 并等待就绪，返回可导航的 Web UI 地址。
fn start_dsh(app: &tauri::AppHandle) -> Option<String> {
    // 0. 复用优先：若固定端口已有 dsh 服务在跑（说明桌面壳上次未回收、服务常驻后台），
    //    直接复用，不清理、不重拉，秒开不重载。
    if wait_ready(DSH_PORT, Duration::from_secs(1)) {
        eprintln!("[dsh-desktop] 复用已在后台运行的 dsh（{}）", DSH_PORT);
        return Some(format!("http://127.0.0.1:{DSH_PORT}"));
    }
    // 否则清理历史残留的 dsh web 实例，保证单实例运行。
    cleanup_stale_dsh();
    // 1. 优先用固定端口（保持 localStorage origin 稳定），被占用才退回随机空闲端口。
    let port = if port_free(DSH_PORT) { DSH_PORT } else { find_free_port()? };
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

    // 后台消费事件流，避免 stdout 管道满导致 dsh 阻塞。
    std::thread::spawn(move || {
        while let Some(event) = rx.blocking_recv() {
            match event {
                CommandEvent::Stdout(bytes) => {
                    if let Ok(line) = String::from_utf8(bytes) {
                        let line = line.trim();
                        if !line.is_empty() {
                            eprintln!("[dsh:out] {line}");
                        }
                    }
                }
                CommandEvent::Stderr(bytes) => {
                    if let Ok(line) = String::from_utf8(bytes) {
                        let line = line.trim();
                        if !line.is_empty() {
                            eprintln!("[dsh:err] {line}");
                        }
                    }
                }
                _ => {}
            }
        }
    });

    // 4. 等待本地服务就绪。
    let url = format!("http://127.0.0.1:{port}");
    if wait_ready(port, READY_TIMEOUT) {
        Some(url)
    } else {
        eprintln!("[dsh-desktop] dsh 启动超时（端口 {port}）");
        // 超时回收：杀掉已启动但没就绪的 dsh，避免残留占用端口
        if let Some(state) = app.try_state::<DshChild>() {
            if let Some(child) = state.0.lock().unwrap().take() {
                kill_process_tree(child);
            }
        }
        None
    }
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

/// 轮询探测本地端口，直到可连接或超时。
fn wait_ready(port: u16, timeout: Duration) -> bool {
    let addr = format!("127.0.0.1:{port}");
    let start = Instant::now();
    while start.elapsed() < timeout {
        if TcpStream::connect(&addr).is_ok() {
            return true;
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
                if let Some(state) = app_for_kill.try_state::<DshChild>() {
                    if let Some(child) = state.0.lock().unwrap().take() {
                        kill_process_tree(child);
                    }
                }
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

    // 2. 重启 dsh：先停旧进程，再拉起新的。
    if let Some(state) = app.try_state::<DshChild>() {
        if let Some(child) = state.0.lock().unwrap().take() {
            kill_process_tree(child);
        }
    }

    // 3. 重新拉起 dsh 并切窗口。
    spawn_dsh(&app);
    Ok(latest)
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
