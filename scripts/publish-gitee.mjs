#!/usr/bin/env node
// 发布自动更新到 Gitee 发行版（与 GitHub 双源互备，供国内用户走 Gitee）
//   1. 收集构建产物：安装包 + .sig（同 publish-update.mjs 的 bundle-only 逻辑）
//   2. 确保 Gitee release 存在（tag 需已推送到 Gitee 仓库）
//   3. 上传全部安装包 + .sig + latest.json 附件，拿真实 browser_download_url
//   4. 生成 Gitee 版 latest.json（url 指向 Gitee 附件下载地址）
//   5. 用 Gitee contents API 把 latest.json 写进 gitee 分支（固定端点 raw/gitee/latest.json）
//
// 前置：
//   $env:GITEE_TOKEN = "你的 Gitee 私人令牌"（Settings -> 私人令牌 -> 勾选 projects/releases）
//   node scripts/publish-gitee.mjs --tag v0.2.0 --artifacts-dir artifacts --repo eternalnight996/dsh-desktop
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

const token = process.env.GITEE_TOKEN || process.env.GITEE_ACCESS_TOKEN;
if (!token) { console.error('未设置 GITEE_TOKEN（Gitee 私人令牌）'); process.exit(1); }

const version = (readFileSync(join(root, 'src-tauri', 'Cargo.toml'), 'utf8').match(/^version = "([^"]+)"/m) || [])[1];
if (!version) { console.error('无法从 Cargo.toml 读取版本'); process.exit(1); }
const tag = flag('--tag', 'v' + version);
const [owner, repo] = flag('--repo', 'eternalnight996/dsh-desktop').split('/');
const artifactsDir = flag('--artifacts-dir', join(root, 'src-tauri', 'target'));
const giteeRawBranch = flag('--raw-branch', 'gitee');
console.log(`Gitee 发布 ${tag} -> ${owner}/${repo}`);

const api = 'https://gitee.com/api/v5';
const headers = { 'User-Agent': 'publish-gitee.mjs' };
async function apiJson(path, opts = {}) {
  const url = opts.params ? `${api}${path}?` + new URLSearchParams(opts.params) : `${api}${path}`;
  const res = await fetch(url, { method: opts.method || 'GET', headers, body: opts.body, signal: AbortSignal.timeout(600_000) });
  const text = await res.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch {}
  return { status: res.status, json, text };
}

// ---- 1. 收集产物（CI 下载的 artifacts 可能没有 bundle 层级，递归收集后按 .sig 过滤）----
const installerExts = ['.exe', '.msi', '.dmg', '.app.tar.gz', '.deb', '.AppImage', '.rpm'];
const installers = [];
function walk(dir) {
  if (!existsSync(dir)) return;
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const st = existsSync(p) ? statSync(p) : null;
    if (st?.isDirectory()) walk(p);
    else if (installerExts.some((e) => name.endsWith(e))) installers.push(p);
  }
}
walk(artifactsDir);
const withSig = installers.filter((p) => !/webview2/i.test(p) && existsSync(p + '.sig'));
if (!withSig.length) { console.error('未找到带 .sig 的安装包'); process.exit(1); }
console.log('安装包:', withSig.map((p) => basename(p)).join(', '));

// ---- 2. 确保 Gitee release 存在（注意：Gitee 对不存在的 tag 返回 200+null，需兜底）----
let releaseId = null;
const existing = await apiJson(`/repos/${owner}/${repo}/releases/tags/${tag}`, { params: { access_token: token } });
if (existing.status === 200 && existing.json && existing.json.id) {
  releaseId = existing.json.id;
  console.log('Gitee release 已存在，id=', releaseId);
} else {
  const form = new URLSearchParams({
    access_token: token,
    tag_name: tag,
    name: tag,
    body: flag('--notes', 'auto update release'),
    target_commitish: 'master',
  });
  const created = await apiJson(`/repos/${owner}/${repo}/releases`, { method: 'POST', body: form });
  if (created.status >= 200 && created.status < 300 && created.json && created.json.id) {
    releaseId = created.json.id;
    console.log('Gitee release 已创建，id=', releaseId);
  } else {
    // 兜底：创建失败（如已存在）时从发行版列表按 tag 查找
    const list = await apiJson(`/repos/${owner}/${repo}/releases`, { params: { access_token: token, per_page: 100 } });
    const hit = (list.json || []).find((r) => r.tag_name === tag);
    if (hit && hit.id) {
      releaseId = hit.id;
      console.log('从列表找到已有 Gitee release，id=', releaseId);
    } else {
      console.error('创建/查找 Gitee release 失败:', created.status, created.text);
      process.exit(1);
    }
  }
}

// ---- 3. 上传附件，收集真实下载 URL（Gitee 附件上限 100MB，超限跳过）----
const GITEE_MAX_FILE = 100 * 1024 * 1024;
const uploadable = [];
for (const f of withSig) {
  const size = statSync(f).size;
  if (size > GITEE_MAX_FILE) {
    console.warn('跳过（超过 Gitee 100MB 上限）:', basename(f), Math.round(size / 1024 / 1024) + 'MB');
    continue;
  }
  uploadable.push(f);
}
const files = [...uploadable, ...uploadable.map((p) => p + '.sig')];
const downloadUrls = {}; // 文件名 -> browser_download_url
// 上传单文件：带 10 分钟超时 + 3 次退避重试，失败返回 null（不中断整个发布）
const UPLOAD_RETRIES = 3;
async function uploadOne(f) {
  const name = basename(f);
  for (let attempt = 1; attempt <= UPLOAD_RETRIES; attempt++) {
    try {
      const body = new FormData();
      body.append('access_token', token);
      body.append('file', new Blob([readFileSync(f)]), name);
      const up = await fetch(`${api}/repos/${owner}/${repo}/releases/${releaseId}/attach_files`, {
        method: 'POST',
        headers,
        body,
        signal: AbortSignal.timeout(600_000),   // 10 分钟超时，防 Gitee 慢导致 HeadersTimeoutError
      });
      const text = await up.text();
      let json = null;
      try { json = JSON.parse(text); } catch {}
      if (up.status >= 200 && up.status < 300 && json?.browser_download_url) return json.browser_download_url;
      console.warn(`上传 ${name} 失败（HTTP ${up.status}，第 ${attempt}/${UPLOAD_RETRIES} 次）: ${text.slice(0, 120)}`);
    } catch (e) {
      console.warn(`上传 ${name} 异常（第 ${attempt}/${UPLOAD_RETRIES} 次）: ${String((e && e.message) || e)}`);
    }
    if (attempt < UPLOAD_RETRIES) await new Promise((r) => setTimeout(r, 2000 * attempt));   // 退避重试
  }
  return null;
}
for (const f of files) {
  const name = basename(f);
  const url = await uploadOne(f);
  if (url) {
    downloadUrls[name] = url;
    console.log('已上传', name, '->', url);
  } else {
    console.error('跳过（多次重试仍失败）:', name);
  }
}
if (!Object.keys(downloadUrls).length) {
  console.error('没有任何安装包上传成功，无法生成 latest.json');
  process.exit(1);
}

// ---- 4. 生成 Gitee 版 latest.json ----
const PRIORITY = { '.exe': 3, '.msi': 2, '.dmg': 3, '.app.tar.gz': 2, '.appimage': 3, '.deb': 2, '.rpm': 2 };
const extOf = (name) => {
  const n = name.toLowerCase();
  for (const e of ['.app.tar.gz', '.appimage', '.exe', '.msi', '.dmg', '.deb', '.rpm']) if (n.endsWith(e)) return e;
  return '';
};
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
const platforms = {};
for (const installer of uploadable) {
  const name = basename(installer);
  if (!downloadUrls[name]) { console.warn('跳过（未上传成功，不写入 latest.json）:', name); continue; }
  const key = platformKey(name);
  const prio = PRIORITY[extOf(name)] || 1;
  const versionMatch = name.includes('_' + version + '_');
  if (platforms[key]) {
    const cur = platforms[key];
    const curVersionMatch = cur._name.includes('_' + version + '_');
    if (curVersionMatch && !versionMatch) continue;
    if (!curVersionMatch && versionMatch) { /* 覆盖旧版 */ }
    else if (cur._prio > prio) continue;
  }
  platforms[key] = {
    _prio: prio,
    _name: name,
    signature: readFileSync(installer + '.sig', 'utf8').trim(),
    url: downloadUrls[name],
  };
}
for (const k of Object.keys(platforms)) { delete platforms[k]._prio; delete platforms[k]._name; }
const manifest = { version, notes: flag('--notes', 'auto update release'), pub_date: new Date().toISOString(), platforms };
const manifestLocal = join(artifactsDir, 'latest-gitee.json');
writeFileSync(manifestLocal, JSON.stringify(manifest, null, 2) + '\n');
console.log('Gitee latest.json 已生成:', manifestLocal);
console.log('platforms:', Object.keys(platforms).join(', '));

// ---- 5. 上传 latest.json 附件 + 写进 gitee 分支（固定端点 raw/gitee/latest.json）----
const mfName = 'latest.json';
const mfBody = new FormData();
mfBody.append('access_token', token);
mfBody.append('file', new Blob([readFileSync(manifestLocal)]), mfName);
const mfUp = await fetch(`${api}/repos/${owner}/${repo}/releases/${releaseId}/attach_files`, { method: 'POST', headers, body: mfBody, signal: AbortSignal.timeout(600_000) });
if (mfUp.status >= 200 && mfUp.status < 300) console.log('latest.json 已上传为附件');
else console.error('latest.json 附件上传失败:', mfUp.status, await mfUp.text());

const content64 = Buffer.from(readFileSync(manifestLocal)).toString('base64');
const rawForm = new URLSearchParams({
  access_token: token,
  content: content64,
  message: 'release: update latest.json (' + tag + ')',
  branch: giteeRawBranch,
});
const rawUp = await apiJson(`/repos/${owner}/${repo}/contents/latest.json`, { method: 'POST', body: rawForm });
if (rawUp.status >= 200 && rawUp.status < 300) console.log(`latest.json 已写入 ${giteeRawBranch} 分支（raw 端点固定可用）`);
else console.error('写入分支失败（可能需先删除旧文件）:', rawUp.status, rawUp.text);

console.log('Gitee 发布完成:', `https://gitee.com/${owner}/${repo}/releases/tag/${tag}`);