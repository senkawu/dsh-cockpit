#!/usr/bin/env node
/**
 * fetch-node.mjs — Tauri 资源准备脚本（beforeBuildCommand 调用）
 *
 * P3 起不再把 Node 打包进安装包（安装包从 ~150MB 瘦身到 ~30MB）：
 *   - Node 运行时改为运行时按需解析（process_mgr::ensure_runtime_node）：
 *     系统 Node ≥24 优先 → <app_data>/node-runtime 缓存 → 镜像下载（sha256 校验）。
 *   - 本脚本只负责把 pnpm（纯 JS CLI）放进 src-tauri/pnpm/ 资源目录。
 *
 * 幂等：pnpm 已就绪且版本标记一致时跳过（不重复下载）。
 * 镜像：默认 npmmirror（国内 CDN），可用 DSH_REGISTRY 覆盖。
 */
import { createWriteStream, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { get } from 'node:https';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const SRC_TAURI = path.join(ROOT, 'src-tauri');
const PNPM_DIR = path.join(SRC_TAURI, 'pnpm');
const MARKER = path.join(PNPM_DIR, '.pnpm-version');

const PNPM_VERSION = process.env.DSH_PNPM_VERSION || '11.24.0';
const PNPM_REGISTRY = (process.env.DSH_REGISTRY || 'https://registry.npmmirror.com').replace(/\/$/, '');

function download(url, dest) {
  return new Promise((resolve, reject) => {
    const req = get(url, { headers: { 'user-agent': 'dsh-desktop-build' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        download(new URL(res.headers.location, url).toString(), dest).then(resolve, reject);
        return;
      }
      if (res.statusCode !== 200) {
        res.resume();
        reject(new Error(`HTTP ${res.statusCode} ${url}`));
        return;
      }
      const out = createWriteStream(dest);
      out.on('error', reject);
      res.on('error', reject);
      out.on('finish', () => out.close(resolve));
      res.pipe(out);
    });
    req.setTimeout(120000, () => req.destroy(new Error('下载超时')));
    req.on('error', reject);
  });
}

function extract(archive, destDir) {
  mkdirSync(destDir, { recursive: true });
  if (process.platform === 'win32') {
    try {
      execFileSync('tar.exe', ['-xf', archive, '-C', destDir], { stdio: 'inherit', timeout: 300000 });
      return;
    } catch {
      /* 老系统无 tar.exe，回退 PowerShell */
    }
    execFileSync('powershell.exe', [
      '-NoProfile', '-NonInteractive', '-Command',
      `Expand-Archive -LiteralPath '${archive}' -DestinationPath '${destDir}' -Force`,
    ], { stdio: 'inherit', timeout: 600000 });
  } else {
    execFileSync('/usr/bin/tar', ['-xzf', archive, '-C', destDir], { stdio: 'inherit', timeout: 300000 });
  }
}

/** 复制目录：先建目标目录，避免 cp -R src/. dest/ 在 dest 缺失时挂起/报错 */
function copyDir(src, dest) {
  mkdirSync(dest, { recursive: true });
  execFileSync('cp', ['-R', src + '/.', dest + '/'], { stdio: 'inherit', timeout: 300000 });
}

async function main() {
  const pnpmOk = existsSync(path.join(PNPM_DIR, 'bin', 'pnpm.cjs'));
  if (pnpmOk && existsSync(MARKER) && readFileSync(MARKER, 'utf8').trim() === PNPM_VERSION) {
    console.log(`[fetch-node] pnpm ${PNPM_VERSION} 已就绪，跳过下载`);
  } else {
    const work = path.join(SRC_TAURI, '.node-build');
    const archive = path.join(work, `pnpm-${PNPM_VERSION}.tgz`);
    const url = `${PNPM_REGISTRY}/pnpm/-/pnpm-${PNPM_VERSION}.tgz`;
    console.log(`[fetch-node] 下载 pnpm ${PNPM_VERSION} <- ${url}`);
    mkdirSync(work, { recursive: true });
    rmSync(archive, { force: true });
    await download(url, archive);
    const extractDir = path.join(work, 'pnpm-extracted');
    rmSync(extractDir, { recursive: true, force: true });
    extract(archive, extractDir);
    const pkg = path.join(extractDir, 'package');
    if (!existsSync(path.join(pkg, 'bin', 'pnpm.cjs'))) {
      throw new Error(`pnpm 包缺少 bin/pnpm.cjs: ${pkg}`);
    }
    rmSync(PNPM_DIR, { recursive: true, force: true });
    mkdirSync(PNPM_DIR, { recursive: true });
    copyDir(pkg, PNPM_DIR);
    rmSync(path.join(PNPM_DIR, 'node_modules'), { recursive: true, force: true });
    writeFileSync(MARKER, PNPM_VERSION + '\n');
    rmSync(work, { recursive: true, force: true });
    console.log(`[fetch-node] 完成: pnpm=${PNPM_DIR} (${PNPM_VERSION})`);
  }

  // 清理 P3 前的遗留产物（node sidecar + npm 资源），避免误入安装包
  const leftovers = [path.join(SRC_TAURI, 'binaries'), path.join(SRC_TAURI, 'npm')];
  for (const p of leftovers) {
    if (existsSync(p)) {
      console.log(`[fetch-node] 清理遗留产物: ${p}`);
      rmSync(p, { recursive: true, force: true });
    }
  }
}

main().catch((err) => {
  console.error('[fetch-node] 失败:', err);
  process.exit(1);
});
