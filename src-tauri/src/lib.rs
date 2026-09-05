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
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_updater::UpdaterExt;

// ===== i18n：跟随 dsh 系统语言 =====
// 真源：~/.dsh/settings.yaml → locale.preference（en/zh，与 dsh Web UI 语言设置同源，
// 同 @eternalnight/dsh-theme 的中英跟随模式）。
// - Rust 侧用户可见文案（托盘/窗口标题/错误态/命令消息）：启动读一次缓存，重启生效；
// - 设置窗口/加载页：经 get_ui_locale 每次打开时实时读取，dsh 里切语言即时跟随。
// 缺文件/缺字段/解析失败 → zh（本项目中文为主）。

/// dsh 系统配置文件路径（~/.dsh/settings.yaml）。
fn dsh_settings_yaml_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(PathBuf::from))
        .ok()?;
    Some(home.join(".dsh").join("settings.yaml"))
}

/// 从 settings.yaml 文本定向提取 `locale.preference`（en → Some(true)）。
/// 不引 YAML 依赖：dsh 写出的是顶层 `locale:` 块 + 缩进 `preference:` 键，
/// 逐行扫描足够；块结束于下一个顶层键。找不到/无法识别 → None（调用方兜底 zh）。
fn parse_locale_preference(text: &str) -> Option<bool> {
    let mut in_locale = false;
    for raw in text.lines() {
        let top = !raw.starts_with(' ') && !raw.starts_with('\t');
        let line = raw.trim();
        if top {
            in_locale = line == "locale:";
            continue;
        }
        if !in_locale || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("preference:") {
            let v = rest
                .split('#').next().unwrap_or("")
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            return Some(v.starts_with("en"));
        }
    }
    None
}

/// 实时读取 dsh 语言（true = en）；读不到按 zh。
fn read_dsh_locale() -> bool {
    dsh_settings_yaml_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| parse_locale_preference(&t))
        .unwrap_or(false)
}

/// Rust 侧文案用的语言缓存：首次使用（建托盘）时读一次，此后不再变化（重启生效）。
static UI_LOCALE_EN: OnceLock<bool> = OnceLock::new();

/// Rust 侧双语文案：按缓存语言二选一。
fn tr<'a>(en: &'a str, zh: &'a str) -> &'a str {
    let is_en = *UI_LOCALE_EN.get_or_init(read_dsh_locale);
    if is_en { en } else { zh }
}

/// 查询 dsh 系统语言（设置窗口/加载页每次打开时调用，切语言即时跟随）。
#[tauri::command]
fn get_ui_locale() -> String {
    if read_dsh_locale() { "en".into() } else { "zh".into() }
}

#[cfg(test)]
mod i18n_tests {
    use super::parse_locale_preference;

    #[test]
    fn preference_en() {
        assert_eq!(parse_locale_preference("locale:\n  preference: en\n"), Some(true));
    }

    #[test]
    fn preference_zh() {
        assert_eq!(parse_locale_preference("locale:\n  preference: zh\n"), Some(false));
    }

    #[test]
    fn missing_block_yields_none() {
        assert_eq!(parse_locale_preference("ui-theme:\n  preference: dark\n"), None);
    }

    #[test]
    fn block_ends_at_next_top_level_key() {
        let text = "locale:\n  preference: en\nother:\n  preference: zh\n";
        assert_eq!(parse_locale_preference(text), Some(true));
    }

    #[test]
    fn quoted_value_with_comment() {
        assert_eq!(parse_locale_preference("locale:\n  preference: 'en' # web ui\n"), Some(true));
    }

    #[test]
    fn file_like_real_settings() {
        let text = "beast-tamer:\n  lang: zh\npermission:\n  defaultPreset: danger-full-access\nlocale:\n  preference: en\n";
        assert_eq!(parse_locale_preference(text), Some(true));
    }
}

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
/// 升级锁：dsh / 桌面壳升级期间为 true。
/// 期间拦截：主窗口 CloseRequested、托盘退出、RunEvent::ExitRequested，
/// 防止用户中途关闭导致半安装状态（npm install 半途 / 桌面壳更新半下载）。
static IS_UPDATING: AtomicBool = AtomicBool::new(false);

/// 当前代 dsh 打出的 Web UI 完整 URL（可能带 token）。
/// dsh alpha 起在 web 子命令加进程级 token 认证：根路径不带头 token 一律 401，
/// 只有启动日志 `dsh web: http://127.0.0.1:<port>/?token=…` 里的 URL 才是能进页面的
/// 地址。stdout 消费线程解析到后写入；`wait_ready` 用它判就绪并返回给 navigate。
/// None = 尚无有效 URL（含稳定版未带 token 时的退化路径，此时退回 `127.0.0.1:port/`）。
static DSH_WEB_URL: Mutex<Option<String>> = Mutex::new(None);

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

// ===== 升级锁 =====
// 升级期间统一检查 IS_UPDATING：true 时拦截主窗口关闭 / 托盘退出 / RunEvent::ExitRequested，
// 防止用户中途退出导致半安装状态（npm 全局包半截、桌面壳更新半下载）。
// 任何入口（成功 / 失败）都必须在 finally 里 set_updating(false)。

/// 进入升级态。返回 RAII guard，drop 时自动还原（即便中途 panic/早退也安全）。
struct UpdatingGuard {
    active: bool,
}
impl UpdatingGuard {
    fn enter() -> Self {
        IS_UPDATING.store(true, Ordering::SeqCst);
        Self { active: true }
    }
    fn disarm(mut self) {
        self.active = false;
        IS_UPDATING.store(false, Ordering::SeqCst);
    }
}
impl Drop for UpdatingGuard {
    fn drop(&mut self) {
        if self.active {
            IS_UPDATING.store(false, Ordering::SeqCst);
        }
    }
}

fn is_updating() -> bool {
    IS_UPDATING.load(Ordering::SeqCst)
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
            list_app_versions,
            install_app_version,
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
            get_ui_locale,
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
                // 升级中：所有窗口（含设置窗口）的关闭都拦截，避免半安装状态。
                if is_updating() {
                    api.prevent_close();
                    let _ = window.emit("update-blocked", tr("An update is in progress. Please wait for it to finish.", "升级进行中，请等待完成"));
                    return;
                }
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // 系统级退出（macOS Cmd+Q / Linux SIGTERM / Windows 任务管理器结束任务*部分情况）。
            // 升级中：拦截，防止半安装状态。
            // 注意：Windows 任务管理器「结束任务」走的是硬杀，无法拦截 —— 这条防线只在能拦的入口生效。
            if let RunEvent::ExitRequested { api, .. } = &event {
                if is_updating() {
                    api.prevent_exit();
                    let _ = app_handle.emit("update-blocked", tr("An update is in progress. Please wait for it to finish.", "升级进行中，请等待完成"));
                    return;
                }
            }
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
    let show_i = MenuItem::with_id(app, "show", tr("Show Main Window", "打开主窗口"), true, None::<&str>)?;
    let settings_i = MenuItem::with_id(app, "open_settings", tr("Settings…", "设置…"), true, None::<&str>)?;
    let check_i = MenuItem::with_id(app, "check_update", tr("Check for Updates", "更新配置"), true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", tr("Quit", "退出"), true, None::<&str>)?;
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
            "quit" => {
                // 升级中：拦截托盘退出，避免半安装状态。
                if is_updating() {
                    let _ = app.emit("update-blocked", tr("An update is in progress. Please wait for it to finish.", "升级进行中，请等待完成"));
                    return;
                }
                app.exit(0);
            }
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

/// 供 dsh UI 设置页左侧栏「更新设置」入口调用的 command：打开（或聚焦）桌面壳设置窗口。
/// async：Tauri async command 运行在 async runtime（非主线程），这样其中再
/// run_on_main_thread 建窗才能从非主线程正常投递，避免同步主线程 command 里的自死锁。
#[tauri::command]
async fn open_settings(app: tauri::AppHandle) -> Result<(), String> {
    open_settings_window(&app).map_err(|e| format!("打开设置窗口失败: {e}"))
}

/// 供注入脚本（客户端错误看门狗）回传诊断，打印到桌面壳日志。
#[tauri::command]
fn log_workbar_inject(message: String) {
    eprintln!("[dsh-desktop] 注入: {message}");
}

/// 客户端模块加载失败回调：注入脚本检测到 SPA 模块 import 失败后调用。
/// async command 拿到 AppHandle，走与进程崩溃相同的看门狗路径。
#[tauri::command]
async fn report_client_error(app: tauri::AppHandle, message: String) {
    eprintln!("[dsh-desktop] 客户端模块加载失败: {message}");
    let n = DSH_CRASHES.fetch_add(1, Ordering::Relaxed) + 1;
    if n >= MAX_CRASHES {
        enter_error_state(
            &app,
            format!(
                "{head}\n{message}\n\n{tail}",
                head = tr(
                    &format!("Failed to load dsh plugin module (client side, {n} consecutive failures):"),
                    &format!("dsh 插件模块加载失败（客户端侧，连续 {n} 次）："),
                ),
                tail = tr("Fix or remove the problematic plugin, then click Retry.", "修复或移除问题插件后点「重试」。"),
            ),
        );
        return;
    }
    let backoff = CRASH_BACKOFFS[n.min(CRASH_BACKOFFS.len()) - 1];
    eprintln!("[dsh-desktop] {backoff:?} 后重启 dsh…");
    // 先杀掉活着但页面已坏的 dsh（主动杀，代数递增）。
    kill_dsh_intentionally(&app);
    std::thread::sleep(backoff);
    spawn_dsh(&app);
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
        .title(tr("Settings", "设置"))
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

/// 后台线程拉起 dsh，就绪后把窗口导航到 dsh Web UI，并注入客户端错误看门狗。
/// （「更新设置」入口已改由桌面壳客户端插件 dsh-desktop-shell 经官方槽位挂载，
/// 不再走 eval 注入——见 ensure_shell_plugin。）
fn spawn_dsh(app: &tauri::AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || match start_dsh(&handle) {
        Some(url) => {
            if let Some(win) = handle.get_webview_window("main") {
                if let Ok(parsed) = url::Url::parse(&url) {
                    let _ = win.navigate(parsed);
                    // dsh 是 SPA，整页导航会替换当前文档；不能在导航前就注入（会被丢弃）。
                    // 重复注入：看门狗脚本幂等（window.__dshClientErrorWatched__），只有落在 dsh
                    // 文档里才真正生效；30s 兜底停止。
                    let wh = handle.clone();
                    std::thread::spawn(move || {
                        let deadline = Instant::now() + Duration::from_secs(30);
                        while Instant::now() < deadline {
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
                    format!(
                        "{}{tail}",
                        tr(
                            "Failed to start dsh (service did not come up). Possible causes: corrupted global dsh install, plugin resolution failure, or Node runtime error.\n\nRecent logs:\n",
                            "dsh 启动失败（未能拉起服务）。可能原因：全局 dsh 安装损坏、插件解析失败、Node 运行时异常。\n\n最近日志：\n",
                        )
                    ),
                );
            }
        }
    });
}

/// 桌面壳自带 dsh 客户端插件（resources 内随包分发）：经官方 sidebar.footer.action
/// 槽位在 DSH 侧边栏底部渲染「更新设置」按钮（memory-eternal 的「记忆」按钮同款机制），
/// 点击经 Tauri IPC 打开桌面壳设置窗口。
const SHELL_PLUGIN_NAME: &str = "dsh-desktop-shell";

/// 递归复制目录（安装插件包用；std 无现成递归复制）。
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// 把桌面壳客户端插件安装进 dsh profile（~/.dsh/profiles/web），幂等：
/// 1) 复制 resources/plugin/dsh-desktop-shell → profile node_modules（版本一致即跳过）；
/// 2) 在 profile package.json 的 dsh.profile.bundles 登记条目（serde_json 改写，UTF-8 无 BOM）。
/// dsh 启动时按 bundles 挂载插件：node 半注册 loader entry，client 半由 dsh-client-modules
/// 扫描 dsh.client 声明后经 /plugins/<id>/client.js 分发进浏览器。
/// profile 未初始化（dsh 首次运行还没建 profile）时跳过，下次启动自动生效。
/// 任何失败只记日志，不阻断 dsh 启动。
fn ensure_shell_plugin(resource_dir: &Path) {
    let bundled = match resource_dir.join("plugin").join(SHELL_PLUGIN_NAME).canonicalize() {
        Ok(p) if p.join("package.json").exists() => p,
        _ => {
            eprintln!("[dsh-desktop] 未找到内置客户端插件 {SHELL_PLUGIN_NAME}，跳过安装");
            return;
        }
    };
    let home = match std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(PathBuf::from))
    {
        Ok(h) => h,
        Err(_) => {
            eprintln!("[dsh-desktop] 无法定位用户目录，跳过客户端插件安装");
            return;
        }
    };
    let profile_dir = home.join(".dsh/profiles/web");
    let profile_pkg = profile_dir.join("package.json");
    let installed_pkg = profile_dir.join("node_modules").join(SHELL_PLUGIN_NAME).join("package.json");
    // 版本一致且已登记 bundles → 无需任何写入（避免动运行中 dsh 的文件）。
    let bundled_version = std::fs::read_to_string(bundled.join("package.json")).ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("version").and_then(|v| v.as_str()).map(String::from));
    let installed_version = std::fs::read_to_string(&installed_pkg).ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("version").and_then(|v| v.as_str()).map(String::from));
    let already_listed = std::fs::read_to_string(&profile_pkg).ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.pointer("/dsh/profile/bundles").and_then(|b| b.as_array()).map(|a| {
            a.iter().filter_map(|x| x.as_str()).any(|s| s == SHELL_PLUGIN_NAME)
        }))
        .unwrap_or(false);
    if already_listed && installed_version.is_some() && installed_version == bundled_version {
        return;
    }
    if !profile_pkg.exists() {
        eprintln!("[dsh-desktop] dsh profile 尚未初始化（{} 不存在），客户端插件下次启动生效", profile_pkg.display());
        return;
    }
    // 1) 复制插件包（版本变化即重装；文件被运行中的 dsh 锁住时本次跳过，下次生效）。
    if installed_version != bundled_version {
        let target = profile_dir.join("node_modules").join(SHELL_PLUGIN_NAME);
        if let Err(e) = copy_dir_recursive(&bundled, &target) {
            eprintln!("[dsh-desktop] 客户端插件复制失败（不影响本次启动）: {e}");
            return;
        }
        eprintln!("[dsh-desktop] 客户端插件已安装到 {}", target.display());
    }
    // 2) bundles 登记。
    let text = match std::fs::read_to_string(&profile_pkg) {
        Ok(t) => t,
        Err(e) => { eprintln!("[dsh-desktop] 读取 profile package.json 失败: {e}"); return; }
    };
    let mut json = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => v,
        Err(e) => { eprintln!("[dsh-desktop] 解析 profile package.json 失败: {e}"); return; }
    };
    if let Some(bundles) = json.pointer_mut("/dsh/profile/bundles").and_then(|b| b.as_array_mut()) {
        if !bundles.iter().filter_map(|x| x.as_str()).any(|s| s == SHELL_PLUGIN_NAME) {
            bundles.push(serde_json::Value::String(SHELL_PLUGIN_NAME.into()));
        } else {
            return; // 已登记且文件已装好，无需写回。
        }
    } else {
        eprintln!("[dsh-desktop] profile package.json 缺少 dsh.profile.bundles，跳过登记");
        return;
    }
    let out = match serde_json::to_string_pretty(&json) {
        Ok(s) => s,
        Err(_) => return,
    };
    if let Err(e) = std::fs::write(&profile_pkg, out + "\n") {
        eprintln!("[dsh-desktop] 写回 profile package.json 失败: {e}");
        return;
    }
    eprintln!("[dsh-desktop] 客户端插件 {SHELL_PLUGIN_NAME} 已登记进 profile bundles");
}

/// 拉起 dsh 并等待就绪，返回可导航的 Web UI 地址。
fn start_dsh(app: &tauri::AppHandle) -> Option<String> {
    // 0. 复用优先：若固定端口已有 dsh 服务在跑（说明桌面壳上次未回收、服务常驻后台），
    //    直接复用，不清理、不重拉，秒开不重载。wait_ready 返回带 token 的完整 URL。
    let port_fixed = dsh_port();
    if let Some(url) = wait_ready(port_fixed, Duration::from_secs(1)) {
        eprintln!("[dsh-desktop] 复用已在后台运行的 dsh（{port_fixed}）");
        return Some(url);
    }
    // 否则清理历史残留的 dsh web 实例，保证单实例运行。
    cleanup_stale_dsh();
    // 1. 优先用固定端口（保持 localStorage origin 稳定），被占用才退回随机空闲端口。
    let port = if port_free(port_fixed) { port_fixed } else { find_free_port()? };
    let port_arg = port.to_string();

    let resource_dir = app.path().resource_dir().ok()?;

    // 1.5 桌面壳客户端插件：幂等装入 profile（失败不阻断启动）。
    ensure_shell_plugin(&resource_dir);

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
    // 清空上一代可能残留的 Web URL（含 token），避免误用旧进程的地址。
    *DSH_WEB_URL.lock().unwrap() = None;
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
                                // dsh alpha 起把 Web UI 地址（含 token）打在 stdout：
                                // `dsh web: http://127.0.0.1:<port>/?token=…`。解析出来供
                                // wait_ready / navigate 用，否则带认证的 dsh 永远 401 进不去。
                                if let Some(start) = line.find("http://") {
                                    let tail = &line[start..];
                                    if let Some(end) = tail.find(|c: char| c.is_whitespace()) {
                                        *DSH_WEB_URL.lock().unwrap() = Some(tail[..end].to_string());
                                    } else {
                                        *DSH_WEB_URL.lock().unwrap() = Some(tail.to_string());
                                    }
                                }
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

    // 4. 等待本地服务就绪。wait_ready 返回带 token 的完整 URL（dsh alpha 起 Web 地址带 token）。
    if let Some(url) = wait_ready(port, READY_TIMEOUT) {
        *DSH_READY_AT.lock().unwrap() = Some(Instant::now());
        Some(url)
    } else {
        eprintln!("[dsh-desktop] dsh 启动超时（端口 {port}）");
        // 超时回收：杀掉已启动但没就绪的 dsh，避免残留占用端口。
        kill_dsh_intentionally(app);
        enter_error_state(
            app,
            format!(
                "{}{}",
                tr(
                    &format!("dsh start timed out (>{READY_TIMEOUT:?}, port {port}). Possible causes: corrupted plugin install or Node runtime error.\n\nRecent logs:\n"),
                    &format!("dsh 启动超时（>{READY_TIMEOUT:?}，端口 {port}）。可能原因：插件安装损坏、Node 运行时异常。\n\n最近日志：\n"),
                ),
                take_log_tail(LOG_TAIL_LINES),
            ),
        );
        None
    }
}

/// dsh 意外退出（看门狗判定为崩溃）的处理：退避重启；连续 3 次快速崩溃转错误态。
fn handle_dsh_crash(app: &tauri::AppHandle, code: Option<i32>) {
    // 0. 尝试自动修复：崩溃根因若是「无法解析 profile bundle」（dsh fail-loud 格式，
    //    常见于 bundles 登记了插件但依赖未安装/被损坏），直接从 bundles 移除该坏条目并重启，
    //    不再等用户手动点「自动修复」。
    //    remove_bundle_entries 成功后该条目已不存在，后续崩溃日志不会再提取到同名插件，
    //    天然防空转死循环；提取到但移除失败则照常走下方退避/错误态。
    let crash_tail = take_log_tail(LOG_TAIL_LINES);
    let bad_plugins = extract_bad_bundle_plugins(&crash_tail);
    if !bad_plugins.is_empty() {
        match remove_bundle_entries(&bad_plugins) {
            Ok(removed) => {
                eprintln!(
                    "[dsh-desktop] 检测到崩溃根因为无法解析 profile bundle：{bad_plugins:?}，已自动从 bundles 移除 {removed} 条并重启"
                );
                DSH_CRASHES.store(0, Ordering::Relaxed);
                *DSH_READY_AT.lock().unwrap() = None;
                // 主动杀（含递增代数），避免旧事件流把这次替换误判为崩溃；随后重启。
                kill_dsh_intentionally(app);
                std::thread::sleep(Duration::from_millis(500));
                spawn_dsh(app);
                return;
            }
            Err(e) => {
                eprintln!("[dsh-desktop] 提取到坏插件 {bad_plugins:?} 但自动移除失败: {e}，走常规退避");
            }
        }
    }

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
            format!(
                "{}{tail}",
                tr(
                    &format!("dsh crashed {n} times in a row; automatic restart stopped (exit code {code:?}). Usually a plugin fails to load or activate (dsh is zero-tolerance for plugins). Fix or remove the problematic plugin, then click Retry.\n\nRecent logs:\n"),
                    &format!("dsh 连续崩溃 {n} 次，已停止自动重启（退出码 {code:?}）。多为某个插件加载/激活失败（dsh 对插件零容错）。修复或移除问题插件后点「重试」。\n\n最近日志：\n"),
                )
            ),
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

/// 定位 dsh 主 profile（web）的 package.json 路径。
fn dsh_profile_pkg_path() -> Result<PathBuf, String> {
    let home = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(PathBuf::from))
        .map_err(|_| "无法定位用户目录".to_string())?;
    Ok(home.join(".dsh/profiles/web/package.json"))
}

/// 从 dsh profile 的 `dsh.profile.bundles` 数组中精确移除指定插件条目（serde_json 改写）。
/// 只动 bundles 数组，不动 dependencies，避免误删同名依赖。返回实际移除的条目数。
fn remove_bundle_entries(plugins: &[String]) -> Result<usize, String> {
    let profile_pkg = dsh_profile_pkg_path()?;
    if !profile_pkg.exists() {
        return Err("profile package.json 不存在".into());
    }
    let text = std::fs::read_to_string(&profile_pkg)
        .map_err(|e| format!("读取 package.json 失败: {e}"))?;
    let (out, removed) = remove_bundle_entries_from(&text, plugins)?;
    std::fs::write(&profile_pkg, out)
        .map_err(|e| format!("写入 package.json 失败: {e}"))?;
    Ok(removed)
}

/// 文本级：在 package.json 的 `dsh.profile.bundles` 数组中精确移除指定插件条目。
/// 只动 bundles 数组，不动 dependencies；找不到任何目标条目时报错（防止空写）。
/// 返回（新文本, 实际移除数）。抽成纯函数便于单测。
fn remove_bundle_entries_from(text: &str, plugins: &[String]) -> Result<(String, usize), String> {
    let mut json: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("解析 package.json 失败: {e}"))?;
    let Some(bundles) = json
        .pointer_mut("/dsh/profile/bundles")
        .and_then(|b| b.as_array_mut())
    else {
        return Err("profile package.json 缺少 dsh.profile.bundles".into());
    };
    let mut removed = 0usize;
    bundles.retain(|v| {
        if let Some(name) = v.as_str() {
            if plugins.iter().any(|p| p == name) {
                removed += 1;
                return false;
            }
        }
        true
    });
    if removed == 0 {
        return Err("未在 bundles 中找到指定插件".into());
    }
    let mut out = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("序列化 package.json 失败: {e}"))?;
    out.push('\n');
    Ok((out, removed))
}

/// 从崩溃日志尾提取 fail-loud 的「无法解析 profile bundle」插件名。
/// 匹配 dsh 的错误格式：`cannot resolve profile bundle "agent-teams-pixel"`。
/// 返回插件名列表（去重）。
fn extract_bad_bundle_plugins(tail: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = "cannot resolve profile bundle ";
    let mut rest = tail;
    while let Some(pos) = rest.find(needle) {
        let after = &rest[pos + needle.len()..];
        // 后面应是 `"name"`；
        let quoted = after.trim_start();
        if let Some(stripped) = quoted.strip_prefix('"') {
            // 取到下一个未转义 `"` 为止。
            let end = stripped.find('"').unwrap_or(stripped.len());
            let name = &stripped[..end];
            if !name.is_empty() && !out.iter().any(|x| x == name) {
                out.push(name.to_string());
            }
            rest = &stripped[end..];
        } else {
            break;
        }
    }
    out
}

/// 手动入口：从 dsh profile 的 `dsh.profile.bundles` 中移除指定插件条目，然后重启 dsh。
/// 等效于手工编辑 `~/.dsh/profiles/web/package.json` 去掉坏插件再重启——
/// dsh 加载 profile 时跳过不在 bundles 里的条目，不再 fail-loud。
#[tauri::command]
async fn remove_plugin_bundles(app: tauri::AppHandle, plugins: Vec<String>) -> Result<(), String> {
    eprintln!("[dsh-desktop] 移除插件: {plugins:?}");
    let removed = remove_bundle_entries(&plugins)?;
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
/// 使用 `--json` 让 npm 输出结构化 JSON；解析 `type:"progress"` 事件发到前端画进度条。
/// 等待安装结束并返回是否成功。
fn run_npm_install(app: &tauri::AppHandle, npm_cli: &Path, dir: &Path, version: &str) -> bool {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("[dsh-desktop] 创建 dsh 全局目录失败: {e}");
        return false;
    }
    // 全局安装：-g 且显式 --prefix 指到用户级全局前缀，保证与 dsh CLI 同目录。
    // `--json`：输出结构化 JSON，用于解析下载/解压进度。
    let args = vec![
        display_path(npm_cli),
        "install".into(),
        "-g".into(),
        "--prefix".into(),
        display_path(dir),
        "--loglevel=http".into(),
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
    // 已下载的包数（npm http fetch 行计数），用于实时进度上报。
    let mut fetched: u64 = 0;
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
                    if line.is_empty() {
                        continue;
                    }
                    push_log_tail(line);
                    eprintln!("[dsh:install] {line}");
                    // npm 10 `--loglevel=http` 会把每个包的下载打在 stderr：
                    // `npm http fetch GET 200 <url> <ms> (cache miss)`。
                    // 这是流式的、随下载逐条输出，可作实时进度（已下载包数）。
                    if line.contains("npm http fetch") && line.contains("GET") {
                        fetched += 1;
                        // total=0 让前端走 indeterminate；downloaded 传 fetch 累计数（单位：个包），
                        // 前端 dsh 进度监听按「包」文案渲染（见 settings.html 的 dsh-install-progress）。
                        let _ = app.emit(
                            "dsh-install-progress",
                            UpdateProgress { downloaded: fetched, total: 0 },
                        );
                    }
                }
            }
            Some(CommandEvent::Error(e)) => eprintln!("[dsh:install] 事件错误: {e}"),
            Some(_) => {}
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

/// 用 tauri.conf.json 的 updater 公钥校验一个安装包的 minisign 签名。
/// 返回 Ok 代表签名有效（文件未被篡改、确由发布者私钥签名）。
/// 校验失败返回 Err——调用方绝不能据此运行安装器，这是「任意版本下载安装」的安全根基。
fn verify_installer_signature(installer_path: &Path, sig_path: &Path) -> Result<(), String> {
    use base64::Engine as _;
    // tauri.conf.json 的 updater.pubkey 是「minisign 公钥文本」整体再做一次 base64 的产物
    // （decode 后才得到 untrusted comment + base64 公钥行），先解码再用 PublicKey::decode 解析。
    let pubkey_b64 = updater_public_key();
    let pk_text = base64::engine::general_purpose::STANDARD
        .decode(pubkey_b64.trim())
        .map_err(|e| format!("公钥 base64 解码失败: {e}"))?;
    let pk_text = String::from_utf8(pk_text).map_err(|e| format!("公钥文本非法 UTF-8: {e}"))?;
    let pk = minisign_verify::PublicKey::decode(&pk_text)
        .map_err(|e| format!("公钥解析失败: {e}"))?;
    let sig_raw = std::fs::read_to_string(sig_path)
        .map_err(|e| format!("读取签名文件失败: {e}"))?;
    // tauri 的 .sig 文件是「minisign 签名文本」整体 base64 成的单行，先解码再解析。
    let sig_text = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(sig_raw.trim())
            .map_err(|e| format!("签名 base64 解码失败: {e}"))?,
    )
    .map_err(|e| format!("签名文本非法 UTF-8: {e}"))?;
    let sig = minisign_verify::Signature::decode(&sig_text)
        .map_err(|e| format!("签名解析失败: {e}"))?;
    let bin = std::fs::read(installer_path)
        .map_err(|e| format!("读取安装包失败: {e}"))?;
    pk.verify(&bin, &sig, false)
        .map_err(|e| format!("签名校验失败: {e}"))
}

/// 读取 tauri.conf.json 里 updater 的公钥（minisign 公钥，base64）。
fn updater_public_key() -> String {
    // 从编译期内嵌的 tauri 配置读取最可靠，但 Tauri 不直接暴露 updater pubkey 字符串；
    // 这里直接读 tauri.conf.json（发布时该值恒定，与私钥配对）。
    // 开发运行时代码在 target/debug/，tauri.conf.json 在 src-tauri/。逐级向上查找。
    let mut cur = std::env::current_dir().unwrap_or_default();
    for _ in 0..6 {
        let cand = cur.join("src-tauri").join("tauri.conf.json");
        if cand.exists() {
            if let Ok(txt) = std::fs::read_to_string(&cand) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                    if let Some(pk) = v.pointer("/plugins/updater/pubkey").and_then(|x| x.as_str()) {
                        return pk.to_string();
                    }
                }
            }
        }
        if !cur.pop() { break; }
    }
    // 兜底：编译期内置的常量（与 tauri.conf.json 一致，发布时若改动需同步）。
    "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEI1QzQwRTcwRDdFN0RBRUIKUldUcjJ1ZlhjQTdFdFJVVk1wOW9MK0pXMnY1Mkx5TmJ5MGZ4Nk1ZZlBBNkxvNlNlTU5QR2FaMk0K".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verify_real_sig() {
        // 用 0.6.1 的实际安装包 + .sig 验证签名校验能通过（依赖 target 下真实产物）。
        let exe = Path::new("target/x86_64-pc-windows-msvc/release/bundle/nsis/dsh-desktop_0.6.1_x64-setup.exe");
        let sig = Path::new("target/x86_64-pc-windows-msvc/release/bundle/nsis/dsh-desktop_0.6.1_x64-setup.exe.sig");
        if exe.exists() && sig.exists() {
            match verify_installer_signature(exe, sig) {
                Ok(_) => println!("VERIFY_OK"),
                Err(e) => panic!("VERIFY_FAIL: {e}"),
            }
        } else {
            eprintln!("跳过（构建产物不存在）");
        }
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
/// 等待 dsh 本地服务就绪。返回能进 Web UI 的完整 URL（可能带 token），未就绪返回 None。
/// dsh alpha 起根路径不带 token 一律 401；stdout 消费线程把 `dsh web: <url>` 里的
/// 等待 dsh 本地服务就绪：TCP 可达后，只要 HTTP 层能响应即视为就绪。
/// 返回能进 Web UI 的完整 URL（可能带 token）。
///
/// 关键坑（dsh alpha 起的 token 认证）：带 token 访问 `/?token=…` 返回 **303**，
/// 重定向到不带 token 的 `/` 并种 HttpOnly cookie；无 cookie 的请求访问 `/` 返回 **401**。
/// 服务就绪的表现既可能是 200，也可能是 303/401（认证层已就绪）。ureq 2 把非 2xx
/// 作为 `Err(Error::Status(code, _))` 返回，这里必须同时认 `Ok(200)` 与 `Err(Status(401|303))`，
/// 否则永远探测不到就绪。画面进入交给 WebView：导航到带 token 的 URL，由浏览器完成 cookie 交换。
fn wait_ready(port: u16, timeout: Duration) -> Option<String> {
    let plain = format!("http://127.0.0.1:{port}/");
    let start = Instant::now();
    while start.elapsed() < timeout {
        // TCP 先探一层，避免对未监听端口高频发 HTTP（连接层失败在 Windows 上较慢）。
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            // 服务是否响应（200/303/401 均表示服务在跑且认证层就绪）。
            let responsive = |code: u16| code == 200 || code == 303 || code == 401;
            if let Some(url) = DSH_WEB_URL.lock().unwrap().clone() {
                match ureq::get(&url).timeout(Duration::from_secs(2)).call() {
                    Ok(resp) if responsive(resp.status()) => return Some(url),
                    Err(ureq::Error::Status(code, _)) if responsive(code) => return Some(url),
                    _ => {}
                }
            }
            match ureq::get(&plain).timeout(Duration::from_secs(2)).call() {
                Ok(resp) if responsive(resp.status()) => return Some(plain),
                Err(ureq::Error::Status(code, _)) if responsive(code) => return Some(plain),
                _ => {}
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    None
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

/// 单条可选版本（前端下拉框的渲染单元）。
#[derive(Serialize)]
struct VersionEntry {
    version: String,
    /// true = 按 SemVer 后缀规则判定为预发布（前端用黄色渲染）；false = 正式版（绿色）。
    prerelease: bool,
    /// 是否为当前安装版本（前端用于「已安装」标记）。
    is_current: bool,
    /// 是否为 npm dist-tags 指向（latest/alpha/next...），前端用于标签展示。
    tags: Vec<String>,
}

/// dsh 更新信息（查询 npm 官方最新版）。
#[derive(Serialize)]
struct DshUpdateInfo {
    has_update: bool,
    current_version: String,
    /// latest tag（稳定通道）。
    latest_version: String,
    /// alpha tag（预发布通道）；无该 tag 或与稳定版相同时为 None。
    prerelease_version: Option<String>,
    /// 预发布通道名（固定 "alpha"），前端用其渲染标签；None 时前端不显示预发布块。
    prerelease_kind: Option<String>,
    /// 是否在线方案（无内置 dsh，可一键升级；离线版需重新打包）。
    online: bool,
    /// npm 上发布的所有版本（semver 倒序），供前端下拉框使用。
    /// 仅在线方案返回；离线版 npm 网络不通时为空数组。
    versions: Vec<VersionEntry>,
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

/// GitHub owner/repo（从 `github` remote 解析，失败退回 gitee remote）。
fn github_repo() -> Result<(String, String), String> {
    let is_gh = |u: &str| u.contains("github.com");
    let mut url = git_remote_url("github").or_else(|_| git_remote_url("origin")).map_err(|e| e)?;
    if !is_gh(&url) { url = git_remote_url("github").map_err(|_| "无 github remote".to_string())?; }
    // 支持 git@github.com:owner/repo.git 与 https://github.com/owner/repo.git
    let m = regex_lite_url_parse(&url).ok_or_else(|| format!("无法解析 remote: {url}"))?;
    Ok((m.0, m.1))
}

/// 从 git remote URL 提取 owner/repo（粗糙但足够）。
fn regex_lite_url_parse(url: &str) -> Option<(String, String)> {
    // 去掉协议部分与 .git 后缀，取最后的 owner/repo 两段
    let no_proto = url.split("://").last()?;
    let no_suffix = no_proto.strip_suffix(".git").unwrap_or(no_proto);
    // 形如 github.com/owner/repo 或 git@github.com:owner/repo
    let body = no_suffix.rsplit(':').next()?;
    let mut parts = body.split('/').filter(|s| !s.is_empty());
    let repo = parts.next_back()?.to_string();
    let owner = parts.next_back()?.to_string();
    if owner.is_empty() || repo.is_empty() { return None; }
    Some((owner, repo))
}

fn git_remote_url(name: &str) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", name])
        .output()
        .map_err(|e| format!("git 不可用: {e}"))?;
    if !out.status.success() {
        return Err(format!("无 remote `{name}`"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 列出桌面壳所有已发布版本（含 setup.exe 资产的 tag）。
/// 优先用 Gitee tags API（国内可达、版本全，含最新），GitHub releases 作为回退。
/// tags 比 releases 完整（Gitee 的旧 release 资产不完整、最新版未同步到 releases）；
/// 下载时由 install_app_version 对 Gitee/GitHub 双源兜底，选到缺安装包的版本会明确报错。
#[tauri::command]
async fn list_app_versions(app: tauri::AppHandle) -> Result<Vec<VersionEntry>, String> {
    let current = app.package_info().version.to_string();
    // 先试 Gitee tags（版本最全、国内可达）；失败再试 GitHub releases。
    let gitee_api = "https://gitee.com/api/v5/repos/eternalnight996/dsh-desktop/tags?per_page=60";
    let gh = github_repo().unwrap_or_else(|_| ("EternalNight996".into(), "dsh-desktop".into()));
    let gh_api = format!("https://api.github.com/repos/{}/{}/releases?per_page=60", gh.0, gh.1);
    let apis = vec![gitee_api.to_string(), gh_api];
    let mut rels: Option<Vec<serde_json::Value>> = None;
    for api in &apis {
        let a = api.clone();
        let attempt = tauri::async_runtime::spawn_blocking(move || {
            ureq::get(&a)
                .set("User-Agent", "dsh-desktop")
                .timeout(std::time::Duration::from_secs(10))
                .call()
        })
        .await;
        let body = match attempt {
            Ok(Ok(r)) => if let Ok(s) = r.into_string() { s } else { continue },
            _ => continue,
        };
        // Gitee tags 返回 [{name,commit,...}]；GitHub releases 返回 [{tag_name,assets,...}]。
        if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
            rels = Some(parsed);
            break;
        }
    }
    let rels = rels.ok_or_else(|| "无法获取发布版本列表（GitHub/Gitee 均不可达）".to_string())?;
    let mut versions = Vec::new();
    for rel in &rels {
        // 兼容 tags（name 字段）与 releases（tag_name 字段）；再兼容取 tag 前缀 v。
        let tag = rel.get("name").or_else(|| rel.get("tag_name")).and_then(|v| v.as_str()).unwrap_or("");
        let ver = tag.strip_prefix('v').unwrap_or(tag);
        if ver.is_empty() { continue; }
        // 只保留语义化版本号（数字.数字.数字）的 tag，避免把其它 tag 混进来。
        if !ver.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) { continue; }
        versions.push(VersionEntry {
            version: ver.to_string(),
            prerelease: is_prerelease_version(ver),
            is_current: ver == current,
            tags: vec![],
        });
    }
    // semver 倒序（最新在前）
    versions.sort_by(|a, b| semver_desc_cmp(&a.version, &b.version));
    Ok(versions)
}

/// 下载并安装桌面壳的指定版本安装包。
/// 安全流程：从 GitHub release 下载该版本 `setup.exe` + `.sig` → 用 tauri.conf.json 公钥
/// 做 minisign 签名校验（verify_installer_signature）→ **只有校验通过才运行安装器**，
/// 否则绝不执行（校验失败时安装进程不会被启动，仅清理临时文件）。
#[tauri::command]
async fn install_app_version(
    app: tauri::AppHandle,
    version: String,
) -> Result<(), String> {
    let _guard = UpdatingGuard::enter();
    let tag = if version.starts_with('v') { version.clone() } else { format!("v{version}") };
    let asset_name = format!("dsh-desktop_{version}_x64-setup.exe");
    let sig_name = format!("{asset_name}.sig");
    // 下载源：Gitee 优先（国内可达），GitHub 回退。
    let bases = vec![
        format!("https://gitee.com/eternalnight996/dsh-desktop/releases/download/{tag}"),
        format!("https://github.com/EternalNight996/dsh-desktop/releases/download/{tag}"),
    ];

    // 临时目录下载 .sig 与安装包。
    let tmp = std::env::temp_dir().join("dsh-desktop-install");
    std::fs::create_dir_all(&tmp).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let exe_path = tmp.join(&asset_name);
    let sig_path = tmp.join(&sig_name);

    // 1) 下载 .sig（先，体积小）。依次尝试下载源。
    let mut sig_ok = false;
    for base in &bases {
        let u = format!("{base}/{sig_name}");
        if let Ok(bytes) = http_get_bytes(&u).await {
            if std::fs::write(&sig_path, &bytes).is_ok() {
                sig_ok = true;
                break;
            }
        }
    }
    if !sig_ok { return Err("下载签名文件失败（GitHub/Gitee 均不可达）".into()); }

    // 2) 下载安装包。同一下载源优先。
    let mut exe_ok = false;
    for base in &bases {
        let u = format!("{base}/{asset_name}");
        if let Ok(bytes) = http_get_bytes(&u).await {
            if std::fs::write(&exe_path, &bytes).is_ok() {
                exe_ok = true;
                break;
            }
        }
    }
    if !exe_ok { return Err("下载安装包失败（GitHub/Gitee 均不可达）".into()); }

    // 3) 签名校验（安全关键）。校验通过后才允许运行安装器。
    verify_installer_signature(&exe_path, &sig_path)?;

    // 4) 停掉运行中的 dsh（避免安装器替换文件时被锁）。
    kill_dsh_intentionally(&app);

    // 5) 运行 NSIS 安装器（静默 /S）。校验已通过，这里才真正启动安装进程。
    let exe_str = display_path(&exe_path);
    let spawned = std::process::Command::new(&exe_str)
        .arg("/S")
        .spawn()
        .map_err(|e| format!("启动安装器失败: {e}"))?;
    let _ = spawned;
    Ok(())
}

/// 下载一个 URL 的字节内容（https）。
async fn http_get_bytes(url: &str) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let url_owned = url.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let resp = ureq::get(&url_owned)
            .set("User-Agent", "dsh-desktop")
            .timeout(std::time::Duration::from_secs(600))
            .call()
            .map_err(|e| format!("下载失败 {url_owned}: {e}"))?;
        let mut buf = Vec::new();
        resp.into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| format!("读取下载流失败 {url_owned}: {e}"))?;
        Ok(buf)
    })
    .await
    .map_err(|e| format!("下载任务失败: {e}"))?
}

/// 下载并安装新版本，完成后自动重启到新版本。
#[tauri::command]
async fn install_app_update(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    let _guard = UpdatingGuard::enter();
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

/// 检查官方 dsh（@deepseek-ai/dsh）是否有新版本（同时取稳定 latest + alpha tag + 全量版本列表）。
#[tauri::command]
async fn check_dsh_update(app: tauri::AppHandle) -> Result<DshUpdateInfo, String> {
    let current = current_dsh_version(&app);
    let snap = tauri::async_runtime::spawn_blocking(fetch_npm_version_snapshot)
        .await
        .map_err(|e| format!("查询 npm 失败: {e}"))??;
    let stable = snap.stable.clone().unwrap_or_default();
    // 预发布版：与稳定版不同且非空才上报，避免重复提示。
    let prerelease = snap
        .prerelease
        .as_ref()
        .filter(|v| !v.is_empty() && Some(v.as_str()) != snap.stable.as_deref())
        .cloned();
    let prerelease_kind = if prerelease.is_some() { Some("alpha".to_string()) } else { None };
    let has_update = stable != current || prerelease.as_deref() != Some(current.as_str());

    // 组装版本列表（semver 倒序），并标记每个版本的 dist-tag 与是否为当前。
    // 仅在线方案返回；离线版 dsh 列表无意义、给空数组。
    let mut versions: Vec<VersionEntry> = if bundled_dsh_exists(&app) {
        vec![]
    } else {
        snap.versions
            .iter()
            .map(|v| {
                let mut tags: Vec<String> = Vec::new();
                if Some(v.as_str()) == snap.stable.as_deref() {
                    tags.push("latest".to_string());
                }
                if Some(v.as_str()) == snap.prerelease.as_deref() {
                    tags.push("alpha".to_string());
                }
                VersionEntry {
                    version: v.clone(),
                    prerelease: is_prerelease_version(v),
                    is_current: *v == current,
                    tags,
                }
            })
            .collect()
    };
    versions.sort_by(|a, b| semver_desc_cmp(&a.version, &b.version));

    Ok(DshUpdateInfo {
        has_update,
        current_version: current,
        latest_version: stable,
        prerelease_version: prerelease,
        prerelease_kind,
        online: !bundled_dsh_exists(&app),
        versions,
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
/// npm registry 包元数据中与版本相关的最小字段（避免拉整个 document）。
#[derive(Default)]
struct NpmVersionSnapshot {
    /// `latest` tag 指向的版本（稳定通道）。
    stable: Option<String>,
    /// `alpha` tag 指向的版本（预发布通道；没有该 tag 时为 None）。
    prerelease: Option<String>,
    /// 全部历史版本（未排序，调用方自行 semver 倒序）。
    versions: Vec<String>,
}

fn fetch_npm_version_snapshot() -> Result<NpmVersionSnapshot, String> {
    let resp = ureq::get("https://registry.npmjs.org/@deepseek-ai/dsh")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| format!("请求 npm registry 失败: {e}"))?;
    let json: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("解析 npm 返回失败: {e}"))?;
    let mut snap = NpmVersionSnapshot::default();
    if let Some(tags) = json.get("dist-tags").and_then(|t| t.as_object()) {
        snap.stable = tags.get("latest").and_then(|v| v.as_str()).map(String::from);
        snap.prerelease = tags.get("alpha").and_then(|v| v.as_str()).map(String::from);
    }
    if let Some(versions) = json.get("versions").and_then(|v| v.as_object()) {
        snap.versions = versions.keys().cloned().collect();
    }
    if snap.stable.is_none() && snap.prerelease.is_none() && snap.versions.is_empty() {
        return Err("npm 返回缺少 dist-tags / versions".to_string());
    }
    Ok(snap)
}

/// SemVer 预发布后缀判定：含 alpha / beta / pre / canary / rc 任一者即视为预发布。
/// 参考 SemVer 2.0.0 规则 9 + 11.3：含 pre-release 标识符的版本 < 关联 normal version。
/// 主流包管理器（npm / cargo / pip）默认不解析 pre-release 范围。
fn is_prerelease_version(v: &str) -> bool {
    // 提取首个 '-' 之后的部分（即 pre-release 段），按 '-' 拆分后任一标识符命中即黄。
    let pre = v.split_once('-').map(|(_, p)| p).unwrap_or("");
    if pre.is_empty() {
        return false;
    }
    pre.split(|c| c == '-' || c == '.')
        .any(|seg| {
            let s = seg.to_ascii_lowercase();
            s == "alpha" || s == "beta" || s == "pre" || s == "canary" || s == "rc"
        })
}

/// SemVer 倒序比较（按 SemVer 2.0.0 规则 11）。含 pre-release 的版本低于 normal version，
/// pre-release 之间按 ASCII 字典序。
fn semver_desc_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |v: &str| -> (Vec<u64>, Vec<String>) {
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, p),
            None => (v, ""),
        };
        let nums: Vec<u64> = core
            .split('.')
            .filter_map(|s| s.parse::<u64>().ok())
            .collect();
        let pre_idents: Vec<String> = if pre.is_empty() {
            vec![]
        } else {
            pre.split('.').map(String::from).collect()
        };
        (nums, pre_idents)
    };
    let (an, ap) = parse(a);
    let (bn, bp) = parse(b);
    // 数字部分按段比较
    for i in 0..an.len().max(bn.len()) {
        let ai = an.get(i).copied().unwrap_or(0);
        let bi = bn.get(i).copied().unwrap_or(0);
        match ai.cmp(&bi) {
            std::cmp::Ordering::Equal => continue,
            ord => return ord.reverse(), // 倒序
        }
    }
    // 数字部分相等时：含 pre-release 的 < normal（规则 11.3），反映到倒序则为 normal > 含 pre-release。
    // 倒序中 a 更大 → cmp(a, b) 返回 Less。
    match (ap.is_empty(), bp.is_empty()) {
        (true, false) => std::cmp::Ordering::Less, // a 是 normal，b 是 pre-release：a 倒序中更大
        (false, true) => std::cmp::Ordering::Greater, // a 是 pre-release，b 是 normal：a 倒序中更小
        (true, true) => std::cmp::Ordering::Equal,
        (false, false) => {
            // pre-release 之间 ASCII 字典序（规则 11.4），倒序后反向
            for i in 0..ap.len().max(bp.len()) {
                let ai = ap.get(i).cloned().unwrap_or_default();
                let bi = bp.get(i).cloned().unwrap_or_default();
                match ai.cmp(&bi) {
                    std::cmp::Ordering::Equal => continue,
                    ord => return ord.reverse(),
                }
            }
            std::cmp::Ordering::Equal
        }
    }
}

fn fetch_latest_dsh_version() -> Result<String, String> {
    // 兼容历史调用点：默认取稳定版 latest tag。
    fetch_npm_version_snapshot()?
        .stable
        .or_else(|| fetch_npm_version_snapshot().ok().and_then(|s| s.prerelease))
        .ok_or_else(|| "npm latest tag 缺失".to_string())
}

// ===== 在线 dsh 版本检查（只查不装）与手动更新 =====

/// dsh 版本检查结果（后台检查 → 发事件给前端）。
#[derive(Clone, Serialize)]
struct DshVersionInfo {
    current_version: String,
    latest_version: String,
    prerelease_version: Option<String>,
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
    match fetch_npm_version_snapshot() {
        Ok(snap) => {
            let stable = snap.stable.clone().unwrap_or_default();
            let prerelease = snap
                .prerelease
                .as_ref()
                .filter(|v| !v.is_empty() && Some(v.as_str()) != snap.stable.as_deref())
                .cloned();
            if stable != current || prerelease.as_deref() != Some(current.as_str()) {
                eprintln!(
                    "[dsh-desktop] 后台检查：dsh 有新版本 稳定=v{stable} 预发布={:?}（当前 v{current}）",
                    prerelease
                );
                let _ = app.emit(
                    "dsh-update-available",
                    DshVersionInfo {
                        current_version: current,
                        latest_version: stable,
                        prerelease_version: prerelease,
                    },
                );
            }
        }
        Err(e) => eprintln!("[dsh-desktop] 后台检查 dsh 版本失败: {e}"),
    }
}

/// 手动更新在线 dsh：把全局安装的 dsh 升到指定版本（npm install -g），
/// 成功后重启 dsh 进程。失败不影响正在运行的 dsh。返回更新后的版本号。
/// 参数优先级：`version` 精确版本号 > `channel`（stable/prerelease 走对应 dist-tag）> 默认 stable。
#[tauri::command]
async fn update_online_dsh(
    app: tauri::AppHandle,
    channel: Option<String>,
    version: Option<String>,
) -> Result<String, String> {
    let _guard = UpdatingGuard::enter();
    let explicit_version = version.filter(|v| !v.trim().is_empty());
    let want_prerelease = explicit_version.is_none()
        && matches!(channel.as_deref(), Some("prerelease") | Some("alpha"));
    let latest = tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        if let Some(v) = explicit_version {
            // 精确版本号：直接透传，不再查 npm（避免与已选择版本不一致）。
            return Ok(v);
        }
        let snap = fetch_npm_version_snapshot()?;
        if want_prerelease {
            snap.prerelease
                .or(snap.stable)
                .ok_or_else(|| "npm alpha tag 缺失".to_string())
        } else {
            snap.stable
                .or(snap.prerelease)
                .ok_or_else(|| "npm latest tag 缺失".to_string())
        }
    })
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

    // 1. 先停旧 dsh 进程，再全局安装。
    //    原因：运行中的 dsh 会占住 `koffi.node`/`sharp.node` 等原生 .node 文件，
    //    npm 原地替换它们会 EBUSY/EPERM，导致升级半途失败、全局 dsh 依赖树损坏，
    //    之后每次启动都崩（看门狗连续快速崩溃 → 没画面）。
    //    必须让 dsh 完全退出释放文件锁后再装；失败则由下方回滚分支重新拉起旧版。
    kill_dsh_intentionally(&app);

    let app2 = app.clone();
    let dir2 = dir.clone();
    let latest2 = latest.clone();
    let ok = tauri::async_runtime::spawn_blocking(move || {
        run_npm_install(&app2, &npm_cli, &dir2, &latest2)
    })
    .await
    .map_err(|e| format!("安装 dsh 失败: {e}"))?;
    if !ok {
        eprintln!("[dsh-desktop] dsh 升级失败，回滚：重新拉起当前已安装的全局 dsh");
        spawn_dsh(&app);
        return Err("安装 dsh 失败：已在升级前停掉旧实例，现沿用当前版本".to_string());
    }
    // 校验全局安装的版本确实是最新版。
    let bin = global_dsh_bin().ok_or_else(|| "安装完成但找不到全局 dsh 入口".to_string())?;
    if installed_dsh_version(&bin).as_deref() != Some(latest.as_str()) {
        eprintln!("[dsh-desktop] dsh 升级版本校验失败，回滚：重新拉起当前全局 dsh");
        spawn_dsh(&app);
        return Err("安装完成但版本校验失败：沿用当前版本".to_string());
    }

    // 2. 重新拉起新版本 dsh 并切窗口。
    spawn_dsh(&app);
    _guard.disarm();
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
    let prefix = global_dsh_dir().ok_or_else(|| tr("Cannot locate the global npm prefix", "无法定位全局前缀").to_string())?;
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
    if ok { Ok(()) } else { Err(format!("{}{out}", tr("Failed to write the user PATH: ", "写入用户 PATH 失败: "))) }
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
        Err(format!(
            "{}{out}",
            tr("Elevated uninstall failed (user cancelled or npm error): ", "提权卸载失败（用户取消或 npm 出错）: ")
        ))
    }
}

/// 统一终端 dsh：按冲突类型自动修复，返回人话结果。
#[cfg(windows)]
fn unify_terminal_dsh() -> Result<String, String> {
    let st = scan_terminal_dsh();
    if st.unified {
        return Ok(tr("Terminal dsh is already in sync with the desktop shell", "终端 dsh 已与桌面壳同源").into());
    }
    match st.kind.as_str() {
        "missing" => {
            ensure_user_path_contains_prefix()?;
            let after = scan_terminal_dsh();
            if after.unified {
                Ok(tr("Added the global prefix to the user PATH; takes effect in new terminals", "已把全局前缀加入用户 PATH，新开终端即生效").into())
            } else {
                Err(tr("PATH was written but dsh still does not resolve; check your environment variables", "已写入 PATH 但仍解析异常，请检查环境变量").into())
            }
        }
        "npm-global" => {
            let winner = Path::new(
                st.terminal_path.as_deref().unwrap_or_default(),
            )
            .parent()
            .ok_or_else(|| tr("Cannot locate the hijacking source directory", "无法定位抢占来源目录").to_string())?
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
                    "{}",
                    tr(
                        &format!("Removed the hijacking copy ({}); terminal dsh is now in sync with the desktop shell", display_path(&winner)),
                        &format!("已移除抢占副本（{}），终端 dsh 现与桌面壳同源", display_path(&winner)),
                    )
                ))
            } else {
                Err(tr("Uninstalled, but terminal dsh still does not resolve; restart your terminal and try again", "卸载完成但终端解析仍异常，请重启终端后重试").into())
            }
        }
        _ => Err(format!(
            "{}",
            tr(
                &format!("Detected a non-npm dsh taking over PATH ({}); handle it manually in Settings", st.foreign_paths.join(", ")),
                &format!("检测到非 npm 来源的 dsh 抢占 PATH（{}），请在设置中手动处理", st.foreign_paths.join(", ")),
            )
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

#[cfg(test)]
mod version_selector_tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn prerelease_detection_by_suffix() {
        // 含 alpha / beta / pre / canary / rc 任一后缀 → 黄
        assert!(is_prerelease_version("0.1.2-alpha.3"));
        assert!(is_prerelease_version("0.1.2-alpha"));
        assert!(is_prerelease_version("0.1.2-beta.1"));
        assert!(is_prerelease_version("0.1.2-canary.5"));
        assert!(is_prerelease_version("0.1.1-rc.2"));
        assert!(is_prerelease_version("1.0.0-pre"));
        assert!(is_prerelease_version("0.1.2-alpha-rc.1")); // 复合后缀也命中 rc
        // 不含 → 绿
        assert!(!is_prerelease_version("0.1.2"));
        assert!(!is_prerelease_version("0.1.0"));
        assert!(!is_prerelease_version("1.0.0"));
        assert!(!is_prerelease_version("0.0.1"));
        // 大小写不敏感
        assert!(is_prerelease_version("0.1.2-ALPHA"));
        assert!(is_prerelease_version("0.1.2-Rc.1"));
    }

    #[test]
    fn semver_desc_ordering() {
        // 倒序：normal 永远排在含 pre-release 的相同 major.minor.patch 之前
        // 与 SemVer 2.0.0 规则 11.3 一致（normal > pre-release），反向即为倒序
        assert_eq!(semver_desc_cmp("1.0.0", "1.0.0-rc.1"), Ordering::Less);
        assert_eq!(semver_desc_cmp("1.0.0-rc.1", "1.0.0"), Ordering::Greater);
        // 数字部分 major/minor/patch 倒序
        assert_eq!(semver_desc_cmp("1.0.0", "0.9.9"), Ordering::Less);
        assert_eq!(semver_desc_cmp("0.1.1-rc.2", "0.1.0-rc.8"), Ordering::Less);
        // pre-release 内部按 ASCII 字典序（倒序则反向）
        assert_eq!(semver_desc_cmp("1.0.0-beta", "1.0.0-alpha"), Ordering::Less);
        assert_eq!(semver_desc_cmp("1.0.0-rc.1", "1.0.0-beta"), Ordering::Less);
    }

    #[test]
    fn full_version_list_sort_matches_npm() {
        // npm 当前真实版本（与本次查询结果一致）
        let mut vs = vec![
            "0.1.1-rc.2",
            "0.0.1-rc.1",
            "0.1.0-rc.2",
            "0.1.2-alpha.3",
            "0.1.0-rc.7",
            "1.0.0",
            "0.0.1-rc.5",
            "0.1.2-alpha.2",
        ];
        vs.sort_by(|a, b| semver_desc_cmp(a, b));
        // 倒序预期：1.0.0 > 0.1.2-alpha.3 > 0.1.2-alpha.2 > 0.1.1-rc.2 > 0.1.0-rc.7 > 0.1.0-rc.2 > 0.0.1-rc.5 > 0.0.1-rc.1
        assert_eq!(
            vs,
            vec![
                "1.0.0",
                "0.1.2-alpha.3",
                "0.1.2-alpha.2",
                "0.1.1-rc.2",
                "0.1.0-rc.7",
                "0.1.0-rc.2",
                "0.0.1-rc.5",
                "0.0.1-rc.1",
            ]
        );
    }

}

#[cfg(test)]
mod bundle_repair_tests {
    use super::{extract_bad_bundle_plugins, remove_bundle_entries_from};

    /// 模拟真实 `~/.dsh/profiles/web/package.json` 的结构（含 agent-teams-pixel 坏条目）。
    const SAMPLE_PKG: &str = r#"{
  "name": "dsh-profile-web",
  "private": true,
  "dependencies": {
    "@eternalnight/dsh-theme": "git+https://github.com/EternalNight996/dsh-theme.git",
    "dsh-ui-three-body": "^0.2.10",
    "memory-eternal": "^0.1.0"
  },
  "dsh": {
    "profile": {
      "bundles": [
        "@deepseek-ai/dsh-base",
        "@deepseek-ai/dsh-web-app",
        "dshmarket",
        "memory-eternal",
        "agent-teams-pixel"
      ]
    }
  }
}"#;

    #[test]
    fn removes_exact_bundle_entry_only() {
        // 只删 bundles 里的目标条目：dependencies 里的同名/相似名依赖不受影响；
        // 其余 bundle 条目保留且顺序不变。
        let plugins = vec!["agent-teams-pixel".to_string()];
        let (out, removed) = remove_bundle_entries_from(SAMPLE_PKG, &plugins).unwrap();
        assert_eq!(removed, 1);
        assert!(!out.contains("agent-teams-pixel"));
        for keep in ["@deepseek-ai/dsh-base", "dshmarket", "memory-eternal"] {
            assert!(out.contains(keep), "不应误删 {keep}");
        }
        // 序列化结果仍是合法 JSON，且 dependencies 完整保留。
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(parsed.pointer("/dependencies/memory-eternal").is_some());
    }

    #[test]
    fn errors_when_target_absent() {
        // 目标条目本就不存在时报错，避免空写文件。
        let plugins = vec!["no-such-plugin".to_string()];
        assert!(remove_bundle_entries_from(SAMPLE_PKG, &plugins).is_err());
    }

    #[test]
    fn errors_when_bundles_missing() {
        // 结构异常（无 dsh.profile.bundles）时报错而不是 panic。
        let plugins = vec!["agent-teams-pixel".to_string()];
        assert!(remove_bundle_entries_from(r#"{"name":"x"}"#, &plugins).is_err());
        assert!(remove_bundle_entries_from("not json", &plugins).is_err());
    }

    #[test]
    fn extracts_single_bundle_name_from_crash_log() {
        // 复现用户实测崩溃日志 error 行格式：`Error: dsh: cannot resolve profile bundle "agent-teams-pixel" ...`
        let tail = r#"file:///.../dsh-app-boot/lib/index.js:831
Error: dsh: cannot resolve profile bundle "agent-teams-pixel" from the dsh installation or C:\Users\...\.dsh\profiles\web; run 'dsh plugin --profile web install' if its dependency is not installed
    at resolveBundleDir (file:///.../lib/index.js:831:8)"#;
        assert_eq!(extract_bad_bundle_plugins(tail), vec!["agent-teams-pixel"]);
    }

    #[test]
    fn extracts_scoped_name_and_dedupes() {
        // 同时出现多次同名插件（多行堆栈）只返回一项；scoped 包名也能解析。
        let tail = r#"
Error: dsh: cannot resolve profile bundle "@scope/foo" from ...
Error: dsh: cannot resolve profile bundle "@scope/foo" from ...
Error: dsh: cannot resolve profile bundle "plain" from ..."#;
        assert_eq!(extract_bad_bundle_plugins(tail), vec!["@scope/foo", "plain"]);
    }

    #[test]
    fn returns_empty_when_no_bundle_error() {
        // 非「无法解析 bundle」的崩溃（如激活失败、端口占用）不应被自动修复误处理。
        assert!(extract_bad_bundle_plugins("fatal load failure\nplugin did not activate: x").is_empty());
        assert!(extract_bad_bundle_plugins("").is_empty());
    }
}
