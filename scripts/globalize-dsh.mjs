// 全局化 dsh CLI：让「dsh」命令在终端里直接可用（项目初始化时执行，幂等）。
// 用法：node scripts/globalize-dsh.mjs [版本]（默认 latest；推荐传 {{dsh_version}} 与内置 dsh 对齐）
//
// 策略（避开管理员权限，并与桌面壳的 global_dsh_dir 保持一致、统一单一来源）：
//   - 目标前缀固定取用户级全局前缀，不再优先取 npm prefix -g（以免指向 node 版本管理器目录造成双源）：
//     Windows → %APPDATA%\npm（npm 官方用户级前缀，通常已在 PATH）；
//     macOS/Linux → ~/.npm-global（自建并写入 shell rc）。
//     可用环境变量 DSH_GLOBAL_DIR 强制覆盖。
//   - 安装：npm install -g @deepseek-ai/dsh@<版本> --prefix <前缀>
//   - 确保前缀在 PATH：Windows 写 HKCU\Environment\Path（保留原值），
//     POSIX 向 ~/.bashrc / ~/.zshrc 追加 export PATH。
import { spawnSync } from 'node:child_process'
import { mkdirSync, readFileSync, appendFileSync, existsSync } from 'node:fs'
import { homedir } from 'node:os'
import { join, dirname } from 'node:path'

const version = process.argv[2] ?? 'latest'
const spec = version === 'latest' ? '@deepseek-ai/dsh@latest' : `@deepseek-ai/dsh@${version}`

function run(cmd, args) {
  // 统一用 node 直接执行 npm-cli.js（shell:false、参数数组化）：既避开 Windows 下 .cmd 的
  // 引号与 Node ≥20 shell:true 的 DEP0190 弃用警告，跨平台行为也一致。
  // cmd 通常为 'npm'，此处解析其真实 cli 脚本位置。
  const npmCli = join(dirname(process.execPath), 'node_modules', 'npm', 'bin', 'npm-cli.js')
  const nodeBin = process.execPath
  const r = spawnSync(nodeBin, [npmCli, ...args], { stdio: 'inherit' })
  if (r.error) throw r.error
  return r.status ?? 1
}

/** Windows：把目录加进用户级 PATH（HKCU\Environment），保留原值与类型。 */
function ensureWindowsPath(dir) {
  const query = spawnSync('reg', ['query', 'HKCU\\Environment', '/v', 'Path'], { encoding: 'utf8' })
  let current = ''
  let type = 'REG_SZ'
  if (query.status === 0) {
    const m = query.stdout.match(/^\s*Path\s+(REG_\w+)\s+(.*)$/m)
    if (m) { type = m[1]; current = m[2] }
  }
  const parts = current.split(';').filter(Boolean)
  const norm = dir.replace(/\\+$/, '')
  if (parts.some(p => p.replace(/\\+$/, '').toLowerCase() === norm.toLowerCase())) {
    console.log(`[globalize-dsh] PATH 已包含 ${dir}，跳过`)
    return
  }
  parts.push(norm)
  const next = parts.join(';')
  const add = spawnSync('reg', ['add', 'HKCU\\Environment', '/v', 'Path', '/t', type, '/d', next, '/f'], { stdio: 'inherit' })
  if (add.status !== 0) throw new Error('reg add 失败，请手动把 ' + dir + ' 加入用户 PATH')
  console.log(`[globalize-dsh] 已把 ${dir} 写入用户 PATH（新开的终端生效）`)
}

/** POSIX：把 bin 目录追加进 shell rc。 */
function ensurePosixPath(binDir) {
  const rcCandidates = process.env.SHELL?.includes('zsh') ? ['.zshrc'] : ['.bashrc', '.zshrc', '.profile']
  for (const rc of rcCandidates) {
    const path = join(homedir(), rc)
    if (!existsSync(path)) continue
    const content = readFileSync(path, 'utf8')
    const line = `export PATH="${binDir}:$PATH"`
    if (content.includes(binDir)) { console.log(`[globalize-dsh] ${path} 已包含 ${binDir}，跳过`); return }
    appendFileSync(path, `\n# dsh CLI（globalize-dsh.mjs 追加）\n${line}\n`)
    console.log(`[globalize-dsh] 已追加到 ${path}：${line}`)
    return
  }
  console.log(`[globalize-dsh] 未找到 shell rc，请手动把 ${binDir} 加入 PATH`)
}

// 1) 决定前缀（与桌面壳 lib.rs 的 global_dsh_dir 保持一致，确保「just update」装的位置
//    正是桌面壳在线方案读取的全局 dsh，统一单一来源）：
//      Windows → %APPDATA%\npm（npm 官方用户级前缀）；
//      macOS/Linux → ~/.npm-global（自建并写入 shell rc）。
//    不再优优先 npm prefix -g：它常指向随 node 版本管理器的目录（如 hermes\node），
//    会造成桌面壳与 CLI 双源不同步。DSH_GLOBAL_DIR 可强制覆盖。
let prefix = process.env.DSH_GLOBAL_DIR
if (!prefix) {
  prefix = process.platform === 'win32'
    ? join(process.env.APPDATA ?? homedir(), 'npm')
    : join(homedir(), '.npm-global')
  mkdirSync(prefix, { recursive: true })
}
console.log(`[globalize-dsh] 目标前缀：${prefix}`)

// 2) 安装
console.log(`[globalize-dsh] 安装 ${spec}（--prefix ${prefix}）...`)
const code = run('npm', ['install', '-g', spec, '--prefix', prefix])
if (code !== 0) {
  console.error(`[globalize-dsh] npm install 失败（exit ${code}）`)
  process.exit(code)
}

// 3) 确保在 PATH
const binDir = process.platform === 'win32' ? prefix : join(prefix, 'bin')
if (process.platform === 'win32') ensureWindowsPath(binDir)
else ensurePosixPath(binDir)

// 4) 验证 shim 存在
const shim = process.platform === 'win32'
  ? join(binDir, 'dsh.cmd')
  : join(binDir, 'dsh')
if (!existsSync(shim)) {
  console.error(`[globalize-dsh] 未找到 shim：${shim}，请检查 npm 安装结果`)
  process.exit(1)
}
const v = process.platform === 'win32'
  // cmd /c + shell:false 执行 .cmd shim，避免 shell:true 的 DEP0190 警告（--version 无可注入参数）
  ? spawnSync('cmd', ['/c', `${shim} --version`], { encoding: 'utf8' })
  : spawnSync(shim, ['--version'], { encoding: 'utf8' })
console.log(`[globalize-dsh] 完成：dsh 已全局化 → ${shim}（版本 ${v.stdout?.trim() ?? '?'}）`)
console.log('[globalize-dsh] 新开终端即可直接使用 dsh 命令（当前终端请先重新打开）')
