#!/usr/bin/env node
// 发布自动更新到 GitHub Releases：
//   1. 读取版本（src-tauri/Cargo.toml）与仓库（git remote / --repo）
//   2. 收集构建产物：安装包 + .sig 签名（tauri bundler 生成）
//   3. 生成/更新 latest.json（tauri bundler 只产出 .sig，manifest 由本脚本生成）：
//      platforms.<平台>.signature = .sig 文件内容，url = GitHub Release 资产地址
//   4. 用 gh CLI（或 GITHUB_TOKEN + REST API）建 release 并上传全部资产
//
// 前置：先做带签名的构建，例如 Windows：
//   $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content .tauri/updater.key -Raw
//   cargo tauri build --bundles nsis --ci
// 用法：node scripts/publish-update.mjs [--tag vX.Y.Z] [--notes "更新说明"] [--repo owner/repo] [--artifacts-dir DIR] [--manifest-only]
//   --repo owner/repo   指定仓库（默认从 git remote 解析；CI 里可传 ${{ github.repository }}）
//   --artifacts-dir DIR 从该目录递归收集安装包 + .sig（默认 src-tauri/target）
//   --manifest-only     只生成 latest.json，不创建 release/上传（CI 的 merge 任务用）
import { execSync } from 'node:child_process';
import { existsSync, readFileSync, writeFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname, basename } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const args = process.argv.slice(2);
const flag = (name, dflt) => {
  // 同时支持 --name=value 与 --name value（CI 里用空格分隔）
  const i = args.indexOf(name);
  if (i !== -1 && args[i + 1] !== undefined) return args[i + 1];
  const hit = args.find((a) => a.startsWith(name + '='));
  return hit ? hit.slice(name.length + 1) : dflt;
};
const has = (name) => args.includes(name);

// ---- 1. 版本与仓库 ----
const cargoToml = readFileSync(join(root, 'src-tauri', 'Cargo.toml'), 'utf8');
const version = (cargoToml.match(/^version = "([^"]+)"/m) || [])[1];
if (!version) { console.error('无法从 Cargo.toml 读取版本'); process.exit(1); }
const tag = flag('--tag', 'v' + version);

function git(cmd) {
  return execSync(cmd, { encoding: 'utf8', cwd: root }).trim();
}
let owner, repo;
const repoFlag = flag('--repo', '');
if (repoFlag) {
  [owner, repo] = repoFlag.split('/');
} else {
  let remote;
  try { remote = git('git remote get-url github'); } catch { remote = git('git remote get-url origin'); }
  const m = remote.match(/(?:github\.com|gitee\.com)[:/]([^/]+)\/([^/]+?)(?:\.git)?$/);
  if (!m) { console.error('无法解析 remote（可用 --repo owner/repo 指定）:', remote); process.exit(1); }
  [, owner, repo] = m;
}
const downloadBase = `https://github.com/${owner}/${repo}/releases/download/${tag}`;
console.log(`发布 ${tag} -> ${owner}/${repo}`);

// ---- 2. 收集构建产物（默认 src-tauri/target，可用 --artifacts-dir 覆盖，递归查找）----
const artifactsRoot = flag('--artifacts-dir', join(root, 'src-tauri', 'target'));
const installerExts = ['.exe', '.msi', '.dmg', '.app.tar.gz', '.deb', '.AppImage', '.rpm'];
const installers = [];
// 递归收集所有安装包扩展名文件（CI 下载的 artifacts 可能没有 bundle 层级，如 artifacts/nsis/*.exe）
// 最终以「旁边有 .sig」为准；噪音（cargo 编译产物）会在 withSig 过滤时静默排除
function walk(dir) {
  if (!existsSync(dir)) return;
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const st = existsSync(p) ? statSync(p) : null;
    if (st?.isDirectory()) walk(p);
    else if (installerExts.some((e) => name.endsWith(e))) installers.push(p);
  }
}
walk(artifactsRoot);
// 排除 webview2 离线安装器（offlineInstaller 内置的资源，不是本应用安装包），
// 且只保留带 .sig 的安装包（无签名的无法用于自动更新，跳过并提示）
const withSig = installers.filter((p) => !/webview2/i.test(p) && existsSync(p + '.sig'));
const skipped = installers.filter((p) => !/webview2/i.test(p) && !existsSync(p + '.sig'));
// 只提示像真实安装包的跳过项（避免 build_script_build-*.exe 等噪音刷屏）
for (const s of skipped) {
  if (/dsh-desktop/i.test(basename(s))) console.warn('跳过（无 .sig，未签名构建）:', basename(s));
}
if (!withSig.length) {
  console.error('未找到带 .sig 的安装包 —— 请用 TAURI_SIGNING_PRIVATE_KEY 环境变量做带签名构建（cargo tauri build --ci）');
  process.exit(1);
}
console.log('安装包:', withSig.map((p) => basename(p)).join(', '));

// ---- 3. 生成 latest.json（manifest）----
// 安装包文件名 → 更新平台 key（Tauri v2 updater 约定）
function platformKey(name) {
  const n = name.toLowerCase();
  if (n.endsWith('.exe') || n.endsWith('.msi')) return 'windows-x86_64';
  if (n.endsWith('.dmg') || n.endsWith('.app.tar.gz')) {
    if (n.includes('aarch64') || n.includes('arm64')) return 'darwin-aarch64';
    if (n.includes('universal')) return 'darwin-universal';
    return 'darwin-x86_64';
  }
  return 'linux-x86_64';
}
// 同平台多产物时按优先级取（Windows: NSIS > MSI；Linux: AppImage > deb/rpm；macOS: dmg 优先），
// 且优先选与当前版本匹配的安装包（避免旧版本残留产物污染 manifest）
const PRIORITY = { '.exe': 3, '.msi': 2, '.dmg': 3, '.app.tar.gz': 2, '.appimage': 3, '.deb': 2, '.rpm': 2 };
const extOf = (name) => {
  const n = name.toLowerCase();
  for (const e of ['.app.tar.gz', '.appimage', '.exe', '.msi', '.dmg', '.deb', '.rpm']) if (n.endsWith(e)) return e;
  return '';
};

const platforms = {};
for (const installer of withSig) {
  const name = basename(installer);
  const key = platformKey(name);
  const prio = PRIORITY[extOf(name)] || 1;
  const versionMatch = name.includes('_' + version + '_');
  if (platforms[key]) {
    const cur = platforms[key];
    const curVersionMatch = cur._name.includes('_' + version + '_');
    // 旧版本产物 > 新版本：若已有匹配当前版本的就保留；否则按优先级
    if (curVersionMatch && !versionMatch) { console.warn('跳过（旧版本产物）:', name); continue; }
    if (!curVersionMatch && versionMatch) { /* 新版本覆盖旧版本 */ }
    else if (cur._prio > prio) { console.warn('同平台多产物，保留高优先级:', name); continue; }
  }
  const sig = readFileSync(installer + '.sig', 'utf8').trim();
  platforms[key] = { _prio: prio, _name: name, signature: sig, url: `${downloadBase}/${encodeURIComponent(name)}` };
}
for (const k of Object.keys(platforms)) { delete platforms[k]._prio; delete platforms[k]._name; }

const manifest = {
  version,
  notes: flag('--notes', 'auto update release'),
  pub_date: new Date().toISOString(),
  platforms,
};
const manifestPath = join(artifactsRoot, 'latest.json');
writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
console.log('latest.json 已生成:', manifestPath);
console.log('platforms:', Object.keys(platforms).join(', '));

if (has('--manifest-only')) {
  console.log('--manifest-only：跳过发布。');
  process.exit(0);
}

// ---- 4. 发布 ----
// 只上传与当前版本匹配的安装包 + 它的 .sig（避免历史版本产物污染 release 资产），
// manifest（latest.json）始终包含。
const versionAssets = withSig.filter((p) => basename(p).includes('_' + version + '_'));
const assetsForRelease = versionAssets.length ? versionAssets : withSig; // 兜底：都无版本标记才退回全量
const sigFiles = assetsForRelease.map((p) => p + '.sig');
const assets = [...assetsForRelease, ...sigFiles, manifestPath];
if (versionAssets.length === 0) console.warn('警告：未找到与当前版本匹配的安装包，退回全量（可能含旧版本）');

function sh(cmd) {
  console.log('> ' + cmd);
  execSync(cmd, { stdio: 'inherit', cwd: root });
}

const gh = (() => { try { return execSync('gh --version', { encoding: 'utf8' }).includes('gh version'); } catch { return false; } })();
if (gh) {
  sh(`gh release create ${tag} ${assets.map((a) => '"' + a + '"').join(' ')} --repo ${owner}/${repo} --title ${tag} --notes "${manifest.notes}"`);
} else {
  const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
  if (!token) { console.error('未安装 gh CLI，也未设置 GITHUB_TOKEN'); process.exit(1); }
  const api = 'https://api.github.com';
  const headers = { Authorization: `token ${token}`, 'User-Agent': 'publish-update.mjs' };
  const create = await fetch(`${api}/repos/${owner}/${repo}/releases`, {
    method: 'POST',
    headers: { ...headers, 'Content-Type': 'application/json' },
    body: JSON.stringify({ tag_name: tag, name: tag, body: manifest.notes }),
  });
  if (!create.ok) { console.error('创建 release 失败:', create.status, await create.text()); process.exit(1); }
  const rel = await create.json();
  for (const asset of assets) {
    const name = basename(asset);
    const body = readFileSync(asset);
    const up = await fetch(`${api}/repos/${owner}/${repo}/releases/${rel.id}/assets?name=${encodeURIComponent(name)}`, {
      method: 'POST',
      headers: { ...headers, 'Content-Type': 'application/octet-stream' },
      body,
    });
    if (!up.ok) console.error('上传失败', name, up.status, await up.text());
    else console.log('已上传', name);
  }
}
console.log('发布完成:', `${downloadBase}/latest.json`);