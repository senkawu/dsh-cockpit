#!/usr/bin/env node
/**
 * fetch-node.mjs — Tauri sidecar 资源准备脚本（beforeBuildCommand 调用）
 *
 * 职责：把 Node.js 运行时拆成两部分打进 Tauri 安装包：
 *   1. node 可执行文件 → src-tauri/binaries/node-<target-triple>(.exe)
 *      （Tauri externalBin sidecar，运行时经 tauri-plugin-shell 解析）
 *   2. npm 的 JS 源码 → src-tauri/resources/npm/node_modules/npm/**
 *      （普通资源，运行时用 sidecar node 执行 npm-cli.js）
 *
 * 幂等：目标文件已存在且版本标记一致时跳过（不重复下载）。
 * 镜像：默认 npmmirror（国内 CDN），可用 DSH_NODE_MIRROR 覆盖。
 * 版本：默认 v26.5.0（dsh 依赖 node:zlib 的 Zstd API，Node >= 23.7），
 *       可用 DSH_NODE_VERSION 覆盖。
 */
import { execFileSync } from 'node:child_process';
import { createWriteStream, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { get } from 'node:https';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const SRC_TAURI = path.join(ROOT, 'src-tauri');
const BIN_DIR = path.join(SRC_TAURI, 'binaries');
const NPM_DIR = path.join(SRC_TAURI, 'npm');
const MARKER = path.join(BIN_DIR, '.node-version');

const VERSION = process.env.DSH_NODE_VERSION || 'v26.5.0';
const MIRROR = (process.env.DSH_NODE_MIRROR || 'https://cdn.npmmirror.com/binaries/node').replace(/\/$/, '');
// pnpm：隔离环境用 pnpm 安装 dsh（比 npm 快数倍）；以纯 JS 形式打包进资源目录
const PNPM_VERSION = process.env.DSH_PNPM_VERSION || '11.24.0';
const PNPM_REGISTRY = (process.env.DSH_REGISTRY || 'https://registry.npmmirror.com').replace(/\/$/, '');

function triple() {
  // 目标 triple：优先显式覆盖（Windows arm64 交叉构建等场景），否则按宿主推断
  if (process.env.DSH_TARGET_TRIPLE) return process.env.DSH_TARGET_TRIPLE;
  const map = {
    'darwin-arm64': 'aarch64-apple-darwin',
    'darwin-x64': 'x86_64-apple-darwin',
    'win32-x64': 'x86_64-pc-windows-msvc',
    'win32-arm64': 'aarch64-pc-windows-msvc',
    'linux-x64': 'x86_64-unknown-linux-gnu',
    'linux-arm64': 'aarch64-unknown-linux-gnu',
  };
  const key = `${process.platform}-${process.arch}`;
  const t = map[key];
  if (!t) throw new Error(`不支持的构建平台: ${key}（可设 DSH_TARGET_TRIPLE 指定目标）`);
  return t;
}

/** 目标平台/架构（用于 Node 发行包目录名，可被 DSH_TARGET_PLATFORM/ARCH 覆盖）。
 *  注意：Node 官方发行包 Windows 目录名是 `win-x64` 而非 `win32-x64`。 */
function targetPlatformArch() {
  let platform = process.env.DSH_TARGET_PLATFORM;
  if (!platform) {
    platform = process.platform === 'win32' ? 'win' : process.platform;
  }
  const arch = process.env.DSH_TARGET_ARCH || process.arch;
  return { platform, arch };
}

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
  const t = triple();
  const isWin = t.includes('windows'); // 目标平台是否为 Windows（决定 zip/解压/node.exe）
  const nodeSidecar = path.join(BIN_DIR, `node-${t}${isWin ? '.exe' : ''}`);

  // 幂等检查：sidecar 与 npm/pnpm 资源都在且版本一致 → 跳过
  const PNPM_DIR = path.join(SRC_TAURI, 'pnpm');
  const pnpmOk = existsSync(path.join(PNPM_DIR, 'bin', 'pnpm.cjs'));
  const npmOk = existsSync(path.join(NPM_DIR, 'node_modules', 'npm', 'bin', 'npm-cli.js'));
  if (existsSync(MARKER) && readFileSync(MARKER, 'utf8').trim() === VERSION && existsSync(nodeSidecar) && npmOk && pnpmOk) {
    console.log(`[fetch-node] sidecar node ${VERSION} + npm/pnpm 资源已就绪，跳过下载`);
    return;
  }

  const { platform: distPlatform, arch: distArch } = targetPlatformArch();
  const distDir = `node-${VERSION}-${distPlatform}-${distArch}`;
  const archive = `${distDir}${isWin ? '.zip' : '.tar.gz'}`;
  const url = `${MIRROR}/${VERSION}/${archive}`;
  const work = path.join(SRC_TAURI, '.node-build');
  const archivePath = path.join(work, archive);

  console.log(`[fetch-node] 下载 ${url}`);
  mkdirSync(work, { recursive: true });
  rmSync(archivePath, { force: true });
  await download(url, archivePath);

  const extractDir = path.join(work, 'extracted');
  rmSync(extractDir, { recursive: true, force: true });
  console.log(`[fetch-node] 解压 ${archive}`);
  extract(archivePath, extractDir);

  const root = path.join(extractDir, distDir);
  // 1) node 可执行文件 → sidecar
  mkdirSync(BIN_DIR, { recursive: true });
  const nodeSrc = isWin ? path.join(root, 'node.exe') : path.join(root, 'bin', 'node');
  if (!existsSync(nodeSrc)) throw new Error(`解压产物缺少 node 可执行文件: ${nodeSrc}`);
  execFileSync('cp', [nodeSrc, nodeSidecar]);
  if (!isWin) execFileSync('chmod', ['+x', nodeSidecar]);

  // 2) npm JS → resources
  const npmSrc = isWin
    ? path.join(root, 'node_modules', 'npm')
    : path.join(root, 'lib', 'node_modules', 'npm');
  if (!existsSync(path.join(npmSrc, 'bin', 'npm-cli.js'))) {
    throw new Error(`解压产物缺少 npm-cli.js: ${npmSrc}`);
  }
  rmSync(NPM_DIR, { recursive: true, force: true });
  mkdirSync(NPM_DIR, { recursive: true });
  copyDir(npmSrc, path.join(NPM_DIR, 'node_modules', 'npm'));

  // pnpm（纯 JS，node 直接执行）→ resources/pnpm/
  const pnpmDest = path.join(SRC_TAURI, 'pnpm');
  const pnpmUrl = `${PNPM_REGISTRY}/pnpm/-/pnpm-${PNPM_VERSION}.tgz`;
  const pnpmArchive = path.join(work, `pnpm-${PNPM_VERSION}.tgz`);
  console.log(`[fetch-node] 下载 pnpm ${PNPM_VERSION}`);
  rmSync(pnpmArchive, { force: true });
  await download(pnpmUrl, pnpmArchive);
  const pnpmExtract = path.join(work, 'pnpm-extracted');
  rmSync(pnpmExtract, { recursive: true, force: true });
  extract(pnpmArchive, pnpmExtract);
  const pnpmPkg = path.join(pnpmExtract, 'package');
  if (!existsSync(path.join(pnpmPkg, 'bin', 'pnpm.cjs'))) {
    throw new Error(`pnpm 包缺少 bin/pnpm.cjs: ${pnpmPkg}`);
  }
  rmSync(pnpmDest, { recursive: true, force: true });
  mkdirSync(pnpmDest, { recursive: true });
  // pnpm.cjs 相对 require dist/，复制整个包体（跳过其自带 node_modules，pnpm 无运行时依赖）
  copyDir(pnpmPkg, pnpmDest);
  rmSync(path.join(pnpmDest, 'node_modules'), { recursive: true, force: true });

  writeFileSync(MARKER, VERSION + '\n');
  rmSync(work, { recursive: true, force: true });
  console.log(`[fetch-node] 完成: sidecar=${nodeSidecar} npm=${NPM_DIR} pnpm=${PNPM_DIR} (${VERSION})`);
}

main().catch((err) => {
  console.error('[fetch-node] 失败:', err);
  process.exit(1);
});
