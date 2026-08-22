#!/usr/bin/env node
// 准备两套方案共用的运行时（在目标平台上运行）：
//   1. node sidecar：复制本机 node（或下载），放到 src-tauri/binaries/node-<triple>[.exe]
//   2. npm 运行时：复制 node 自带 npm，放到 vendor/node-runtime/node_modules/npm
//   3. dsh 运行时：npm install @deepseek-ai/dsh@<version> 到 vendor/dsh-runtime
// 说明：WebView2 属 Tauri bundler 管理的系统依赖（offlineInstaller 模式构建时自动内置），
//       不走 vendor。跨平台：Windows/macOS/Linux 在各自平台上运行本脚本。
// 用法：node scripts/vendor.mjs [dsh版本]  （缺省 latest，自动查 npm 官方最新版）
import { execSync } from 'node:child_process';
import { existsSync, mkdirSync, cpSync, rmSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const args = process.argv.slice(2);
const update = args.includes('--update');
const useLatest = args.includes('--latest');
let dshVersion = args.find((a) => !a.startsWith('--')) || 'latest';
const dshRegistry = process.env.DSH_REGISTRY || 'https://registry.npmjs.org';
if (useLatest || dshVersion === 'latest') {
  try {
    const out = execSync(`npm view @deepseek-ai/dsh versions --json --registry=${dshRegistry}`, { encoding: 'utf8' }).trim();
    const allVersions = JSON.parse(out);
    dshVersion = allVersions[allVersions.length - 1];
    console.log(`最新 dsh 版本: ${dshVersion}（registry: ${dshRegistry}）`);
  } catch {
    console.error(`查询最新 dsh 版本失败，回退到默认 ${dshVersion}`);
  }
}

// 目标三元组：arch 映射 + 平台后缀
const arch = process.arch === 'arm64' ? 'aarch64' : 'x86_64';
const tripleMap = {
  win32: `${arch}-pc-windows-msvc`,
  linux: `${arch}-unknown-linux-gnu`,
  darwin: `${arch}-apple-darwin`,
};
const triple = tripleMap[process.platform];
if (!triple) {
  console.error(`不支持的平台: ${process.platform}`);
  process.exit(1);
}
const exe = process.platform === 'win32' ? '.exe' : '';

function sh(cmd, cwd = root) {
  console.log('> ' + cmd);
  execSync(cmd, { stdio: 'inherit', cwd });
}

// 1. node sidecar：优先本机 node，找不到则提示手动放置
const nodeSrc = (() => {
  if (process.platform === 'win32') return resolve(process.execPath);
  try { return resolve(execSync('command -v node', { encoding: 'utf8' }).trim()); } catch { return null; }
})();
if (!nodeSrc || !existsSync(nodeSrc)) {
  console.error('未找到本机 node，请先安装 Node.js（≥22.19 或 ≥24）。');
  process.exit(1);
}
const sidecarDir = join(root, 'src-tauri', 'binaries');
const sidecarOut = join(sidecarDir, `node-${triple}${exe}`);
mkdirSync(sidecarDir, { recursive: true });
if (!existsSync(sidecarOut)) {
  console.log(`copy node sidecar: ${nodeSrc} -> ${sidecarOut}`);
  cpSync(nodeSrc, sidecarOut);
  // 瘦身：strip 调试符号，减小安装包体积（解决 AppImage/deb 超 Gitee 100MB 上限）
  if (process.platform !== 'win32') {
    try {
      execSync(`strip "${sidecarOut}"`, { stdio: 'ignore' });
      console.log(`node sidecar 已 strip: ${sidecarOut}`);
    } catch (e) { console.warn('strip 失败（忽略，体积可能偏大）'); }
  }
} else {
  console.log(`node sidecar 已就绪: ${sidecarOut}`);
}

// 2. npm 运行时（方案② npx 用）：多路径探测 node 自带 npm
function findNpm() {
  const winNpm = join(dirname(nodeSrc), 'node_modules', 'npm');
  if (existsSync(join(winNpm, 'bin', 'npm-cli.js'))) return winNpm;
  try {
    const gRoot = execSync('npm root -g', { encoding: 'utf8' }).trim();
    const gNpm = join(gRoot, 'npm');
    if (existsSync(join(gNpm, 'bin', 'npm-cli.js'))) return gNpm;
  } catch { /* 忽略 */ }
  return null;
}
const npmDir = join(root, 'vendor', 'node-runtime', 'node_modules', 'npm');
if (!existsSync(join(npmDir, 'bin', 'npm-cli.js'))) {
  const src = findNpm();
  if (!src) {
    console.error('未找到 npm 运行时（方案②需要）；方案③离线可跳过。');
  } else {
    console.log(`copy npm: ${src} -> ${npmDir}`);
    mkdirSync(dirname(npmDir), { recursive: true });
    cpSync(src, npmDir, { recursive: true });
  }
} else {
  console.log(`npm 已就绪: ${npmDir}`);
}

// 3. dsh 运行时（方案③ 离线）
const dshDir = join(root, 'vendor', 'dsh-runtime');
if (update) {
  console.log(`更新 dsh@${dshVersion}（清除旧版后重装）`);
  rmSync(dshDir, { recursive: true, force: true });
}
if (!existsSync(join(dshDir, 'node_modules', '@deepseek-ai', 'dsh', 'lib', 'bin.js'))) {
  console.log(`install dsh@${dshVersion} -> ${dshDir}`);
  mkdirSync(dshDir, { recursive: true });
  // 提高 Node 堆上限，避免 macOS（尤其 arm64）上 npm install 依赖树过大触发 V8 OOM。
  execSync(`npm install --prefix "${dshDir}" --omit=dev --registry=${dshRegistry} @deepseek-ai/dsh@${dshVersion}`, { stdio: 'inherit', cwd: root, env: { ...process.env, NODE_OPTIONS: process.env.NODE_OPTIONS || '--max-old-space-size=4096' } });
} else {
  console.log(`dsh 已就绪: ${join(dshDir, 'node_modules', '@deepseek-ai', 'dsh')}`);
}

console.log('vendor 准备完成');
