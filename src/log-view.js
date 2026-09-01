// 日志查看器：历史（get_logs 读文件尾部）+ 实时（dsh-log-line 事件）+ 过滤/搜索/诊断包导出
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const logEl = $('log');
const hintEl = $('hint');
const filterEl = $('filter');
const levelEl = $('level');
const realtimeEl = $('realtime');

let history = []; // 磁盘历史（按时间顺序）
let live = [];    // 实时行（去重：只存磁盘尾部之后的部分）
let lastHistoryLen = 0;

function levelOf(line) {
  // tauri-plugin-log 格式形如 [2026-09-01T12:00:00Z INFO  dsh_cockpit] msg
  const m = line.match(/\]?\s*(DEBUG|INFO|WARN|ERROR)\s/);
  return m ? m[1] : '';
}

function render() {
  const kw = filterEl.value.trim().toLowerCase();
  const lv = levelEl.value;
  const all = history.concat(live);
  const shown = all.filter((line) => {
    if (lv && levelOf(line) !== lv) return false;
    if (kw && !line.toLowerCase().includes(kw)) return false;
    return true;
  });
  const tail = shown.slice(-2000); // 渲染上限，防长会话卡顿
  logEl.textContent = tail.join('\n') + (shown.length > 2000 ? `\n…（共 ${shown.length} 行，仅显示末尾 2000 行）` : '');
  logEl.scrollTop = logEl.scrollHeight;
}

function appendLive(text) {
  for (const seg of String(text).split('\n')) {
    const line = seg.trimEnd();
    if (line) live.push(line);
  }
  // 只保留最近 3000 行实时，防内存膨胀
  if (live.length > 3000) live = live.slice(-3000);
  if (realtimeEl.checked) render();
}

async function loadHistory() {
  try {
    history = await invoke('get_logs', { limit: 1000 });
    lastHistoryLen = history.length;
    render();
  } catch (e) {
    hintEl.textContent = '读取历史日志失败: ' + e;
  }
}

async function exportDiagnostics() {
  hintEl.textContent = '正在打包诊断包…';
  try {
    const p = await invoke('collect_diagnostics');
    hintEl.textContent = '诊断包已生成: ' + p;
    await invoke('open_in_finder', { path: p });
  } catch (e) {
    hintEl.textContent = '导出诊断包失败: ' + e;
  }
}

$('btn-export').addEventListener('click', exportDiagnostics);
$('btn-open-dir').addEventListener('click', () => invoke('open_log_dir'));
$('btn-clear').addEventListener('click', () => { logEl.textContent = ''; hintEl.textContent = '已清屏（实时流继续追加）'; });
filterEl.addEventListener('input', render);
levelEl.addEventListener('change', render);
realtimeEl.addEventListener('change', () => { if (realtimeEl.checked) render(); });

// 后端日志流实时追加
listen('dsh-log-line', (ev) => appendLive(ev.payload));

// 状态事件也会刷新历史（服务重启后文件变化）
listen('dsh-status', () => { history = []; live = []; loadHistory(); });

// 展示日志文件位置
(async () => {
  try {
    const s = await invoke('get_status');
    $('log-path').textContent = s.logDir;
  } catch (e) { /* 忽略 */ }
})();

loadHistory();
