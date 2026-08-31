// 主窗口启动状态页：炫酷进度条 + 阶段文本 + 可折叠日志。
// 就绪后由 Rust 导航到 dsh web UI。
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const bar = $('bar');
const phaseEl = $('phase');
const percentEl = $('percent');
const detailEl = $('detail');
const logEl = $('log');
const btnLog = $('btn-log');

let progress = 0; // 0-100
let logLines = 0;

function setProgress(value, text) {
  progress = Math.max(progress, Math.min(100, value)); // 只进不退
  bar.style.width = progress + '%';
  percentEl.textContent = Math.round(progress) + '%';
  if (text) phaseEl.textContent = text;
}

function appendLog(line) {
  logEl.textContent += String(line) + '\n';
  logEl.scrollTop = logEl.scrollHeight;
  logLines++;
  if (logLines > 0 && logEl.hidden) {
    btnLog.classList.add('has-log'); // 有日志时高亮按钮
  }
}

// 估算下载总规模（dsh 依赖树约 200~450MB），据此换算进度
const TOTAL_MB_EST = 420;

const STATE_TEXT = {
  installing: '正在安装 dsh 内核（隔离 pnpm 环境）',
  updating: '正在更新 dsh 内核',
  starting: '正在启动 dsh web 服务',
  ready: 'dsh 已就绪',
  crashed: 'dsh 服务异常退出',
  stopped: 'dsh 服务已停止',
  error: '出错了',
  'update-available': '发现 DSH 内核新版本',
  'safe-mode': '安全模式',
};

listen('dsh-status', (ev) => {
  const s = ev.payload;
  phaseEl.textContent = STATE_TEXT[s.state] || s.state || '…';
  if (s.message && s.state !== 'ready') detailEl.textContent = s.message;
  if (s.state === 'ready') {
    setProgress(100, '加载完成');
    detailEl.textContent = '正在打开 ' + s.message;
  } else if (s.state === 'error' || s.state === 'crashed') {
    bar.style.background = 'var(--danger)';
  }
  if (s.state === 'installing' || s.state === 'updating') {
    setProgress(5, phaseEl.textContent);
  }
  if (s.state === 'starting') setProgress(88, phaseEl.textContent);
});

// 安装/更新时的进度与 npm/pnpm 输出
listen('dsh-update-progress', (ev) => {
  const text = String(ev.payload);
  appendLog(text);

  // "下载依赖中… 已下载 123 MB（用时 12s）" → 换算进度
  const mbMatch = text.match(/已下载 (\d+) MB/);
  if (mbMatch) {
    const mb = parseInt(mbMatch[1], 10);
    setProgress(5 + (mb / TOTAL_MB_EST) * 80, phaseEl.textContent);
  }
  // "Done in 6.4s using pnpm" / "added N packages" → 接近完成
  if (text.includes('Done in') || text.includes('added ') || text.includes('Packages: +')) {
    setProgress(86, phaseEl.textContent);
  }
});

// 查看日志折叠开关（有日志时才出现，点击展开）
btnLog.addEventListener('click', () => {
  logEl.hidden = !logEl.hidden;
  btnLog.textContent = logEl.hidden ? '查看日志' : '收起日志';
  if (!logEl.hidden) logEl.scrollTop = logEl.scrollHeight;
});

// 开场动画
setProgress(2, '初始化…');

// 关键：启动早期的状态事件可能早于本页监听注册而丢失（尤其安装失败很快发生时），
// 页面加载后先主动拉取一次最近状态回放，保证进度条/阶段永不"卡死在初始化"。
(async () => {
  try {
    const last = await invoke('get_last_status');
    if (last && last.state) {
      const s = last;
      phaseEl.textContent = STATE_TEXT[s.state] || s.state;
      if (s.message && s.state !== 'ready') detailEl.textContent = s.message;
      if (s.state === 'ready') setProgress(100, '加载完成');
      else if (s.state === 'installing' || s.state === 'updating') setProgress(5, phaseEl.textContent);
      else if (s.state === 'starting') setProgress(88, phaseEl.textContent);
      else if (s.state === 'error' || s.state === 'crashed') {
        bar.style.background = 'var(--danger)';
        setProgress(100, phaseEl.textContent);
      }
    }
  } catch (e) {
    /* 页面尚未注入完成时忽略，事件流会接管 */
  }
})();
