//! DeepSeek Harness Desktop 主进程。
//!
//! 自包含桌面壳，内置 Node sidecar。启动时按顺序选择 dsh 的来源：
//!   1. 环境变量 `DSH_BIN`（开发/调试覆盖）
//!   2. 打包内置的 dsh（方案③「离线」，客户零依赖离线即用）
//!   3. 通过内置 npm 执行 `npx @deepseek-ai/dsh@<版本> web`（方案②「拉取」，首次联网）
//!
//! 就绪后把窗口导航到 dsh 的 Web UI；退出时回收 dsh 子进程。
//! 跨平台：Windows / macOS / Linux，差异仅在 sidecar 二进制与系统 WebView。


use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{Emitter, Manager, RunEvent};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_updater::UpdaterExt;

/// 方案②下从 npm 拉取的 dsh 版本。
const DSH_VERSION: &str = "0.1.0-rc.6";
/// dsh 启动后等待就绪的超时时间。
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// 就绪探测轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// dsh Web UI 固定端口：保持 origin（127.0.0.1:<port>）跨重启稳定，localStorage 才能持久（否则随机端口导致 origin 每次变化、localStorage 被清空）。
const DSH_PORT: u16 = 5399;

/// 持有 dsh 子进程句柄，退出时统一 kill。
struct DshChild(Mutex<Option<CommandChild>>);

/// 杀 dsh 进程树：Windows 用 taskkill /T /F（npx 拉起的 dsh 是 壳→npm→cmd→node 多层子链，
/// 只 kill 直接子进程会让真正的 dsh 残留成孤儿），其它平台直接 kill。
fn kill_process_tree(child: CommandChild) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
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
            check_dsh_update
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            // 不阻塞主线程：后台线程拉起 dsh，就绪后再切窗口。
            std::thread::spawn(move || match start_dsh(&handle) {
                Some(url) => {
                    if let Some(win) = handle.get_webview_window("main") {
                        if let Ok(parsed) = url::Url::parse(&url) {
                            let _ = win.navigate(parsed);
                        }
                    }
                }
                None => eprintln!("[dsh-desktop] 启动 dsh 失败，停留在加载页"),
            });
            Ok(())
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

/// 拉起 dsh 并等待就绪，返回可导航的 Web UI 地址。
fn start_dsh(app: &tauri::AppHandle) -> Option<String> {
    // 0. 先清理历史残留的 dsh web 实例，保证单实例运行。
    //    否则端口被占会退回随机端口新拉实例，dev 反复启动会积累一堆孤儿进程。
    cleanup_stale_dsh();
    // 1. 优先用固定端口（保持 localStorage origin 稳定），被占用才退回随机空闲端口。
    let port = if port_free(DSH_PORT) { DSH_PORT } else { find_free_port()? };
    let port_arg = port.to_string();

    let resource_dir = app.path().resource_dir().ok()?;

    // 2. 决定启动命令：内置 dsh（③ 离线）优先，否则 npx 拉取（②）。
    let argv: Vec<String> = match resolve_dsh_bin(&resource_dir) {
        Some(bin) => {
            eprintln!("[dsh-desktop] 使用内置 dsh（离线模式）: {}", display_path(&bin));
            let mut argv = vec![display_path(&bin), "web".into()];
            argv.push("--port".into());
            argv.push(port_arg);
            argv
        }
        None => {
            let npm_cli = resource_dir.join("node-runtime/node_modules/npm/bin/npm-cli.js");
            eprintln!("[dsh-desktop] 使用 npx 拉取 dsh@{DSH_VERSION}");
            vec![
                display_path(&npm_cli),
                "exec".into(),
                "--yes".into(),
                format!("@deepseek-ai/dsh@{DSH_VERSION}"),
                "--".into(),
                "web".into(),
                "--port".into(),
                port_arg,
            ]
        }
    };

    // 3. 用 node sidecar 拉起 dsh。
    let sidecar = app
        .shell()
        .sidecar("node")
        .map_err(|e| eprintln!("[dsh-desktop] 找不到 node sidecar: {e}"))
        .ok()?;

    let (mut rx, child) = sidecar
        .args(argv)
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

/// 定位 dsh 入口脚本：环境变量覆盖 → 打包内置 → 无（走 npx）。
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

/// 去掉 Windows 长路径前缀 `\\?\`，转成普通路径给子进程用。
fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    match s.strip_prefix(r"\\?\") {
        Some(stripped) => stripped.to_string(),
        None => s,
    }
}


/// 递归复制目录。


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
    })
}

/// 当前 dsh 版本：优先读打包内置的 package.json（离线方案），否则用内置常量（npx 拉取方案）。
fn current_dsh_version(app: &tauri::AppHandle) -> String {
    if let Ok(dir) = app.path().resource_dir() {
        let pkg = dir.join("dsh-runtime/node_modules/@deepseek-ai/dsh/package.json");
        if let Ok(text) = std::fs::read_to_string(&pkg) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(v) = json.get("version").and_then(|v| v.as_str()) {
                    return v.to_string();
                }
            }
        }
    }
    DSH_VERSION.to_string()
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
