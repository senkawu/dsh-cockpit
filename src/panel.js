// 控制面板逻辑：使用 Tauri 全局注入的 API（withGlobalTauri），无打包器
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

async function refresh() {
  try {
    const s = await invoke('get_status');
    $('installed').textContent = s.installed || '未安装';
    $('running').textContent = s.running ? '运行中' : '已停止';
    $('port').textContent = s.port || '—'; // 动态端口未启动时显示 —
    $('registry').textContent = s.registry;
    $('envDir').textContent = s.envDir;
    $('homeDir').textContent = s.homeDir;
    $('logDir').textContent = s.logDir;
    $('auto-check').checked = s.autoCheckUpdate;
    $('app-version').textContent = 'v' + (s.appVersion || '');
    // 安全模式徽章
    const banner = $('safe-banner');
    if (s.safeMode) banner.hidden = false; else banner.hidden = true;
  } catch (e) {
    $('update-result').textContent = '读取状态失败: ' + e;
  }
}

async function checkUpdate() {
  const out = $('update-result');
  out.textContent = '正在查询 npm 源…';
  try {
    const info = await invoke('check_dsh_update');
    $('latest').textContent = info.latest || '—';
    if (info.hasUpdate) {
      out.textContent = `发现新版本 ${info.latest}（当前 ${info.installed}），可点击“立即更新”`;
      $('btn-update').disabled = false;
    } else {
      out.textContent = `已是最新版本（${info.installed}）`;
      $('btn-update').disabled = true;
    }
  } catch (e) {
    out.textContent = '检查失败（网络/registry 异常）: ' + e;
  }
}

async function applyUpdate() {
  const out = $('update-result');
  out.textContent = '更新中（停止服务 → 备份 → npm install → 冒烟测试）…';
  $('btn-update').disabled = true;
  try {
    const v = await invoke('apply_dsh_update');
    out.textContent = `更新完成：${v}，服务已重启`;
  } catch (e) {
    out.textContent = '更新失败，已自动回退: ' + e;
  }
  refresh();
}

async function restart() {
  const out = $('update-result');
  out.textContent = '正在重启 dsh 服务…';
  try {
    await invoke('restart_dsh');
    out.textContent = 'dsh 服务已重启';
  } catch (e) {
    out.textContent = '重启失败: ' + e;
  }
  refresh();
}

async function shellUpdate() {
  const out = $('shell-result');
  out.textContent = '正在检查外壳更新…';
  try {
    out.textContent = await invoke('check_shell_update');
  } catch (e) {
    out.textContent = e;
  }
}

async function exportDiagnostics() {
  const out = $('diag-result');
  out.textContent = '正在打包诊断包…';
  try {
    const p = await invoke('collect_diagnostics');
    out.textContent = '已生成: ' + p;
    await invoke('open_in_finder', { path: p });
  } catch (e) {
    out.textContent = '导出失败: ' + e;
  }
}

async function refreshPlugins() {
  const list = document.getElementById('plugin-list');
  const hint = document.getElementById('plugin-hint');
  try {
    const plugins = await invoke('get_plugins');
    list.innerHTML = '';
    for (const p of plugins) {
      const row = document.createElement('div');
      row.className = 'row';
      const label = document.createElement('span');
      label.textContent = `${p.name}${p.installed ? '' : '（未安装）'}`;
      const sw = document.createElement('input');
      sw.type = 'checkbox';
      sw.checked = p.enabled && p.installed;
      sw.disabled = !p.installed;
      sw.addEventListener('change', async () => {
        hint.textContent = '开关已写入，HMR 约 1 秒生效…';
        try {
          await invoke('set_plugin_enabled', { id: p.id, enabled: sw.checked });
          hint.textContent = sw.checked ? `${p.name} 已启用` : `${p.name} 已禁用`;
        } catch (e) {
          hint.textContent = '设置失败: ' + e;
          sw.checked = !sw.checked;
        }
      });
      row.append(label, sw);
      list.appendChild(row);
    }
  } catch (e) {
    hint.textContent = '读取插件状态失败: ' + e;
  }
}

$('btn-check').addEventListener('click', checkUpdate);
$('btn-update').addEventListener('click', applyUpdate);
$('btn-restart').addEventListener('click', restart);
$('btn-shell-update').addEventListener('click', shellUpdate);
$('btn-logs').addEventListener('click', () => invoke('open_log_viewer'));
$('btn-export-diag').addEventListener('click', exportDiagnostics);
$('btn-quit').addEventListener('click', () => invoke('quit_app'));
$('btn-exit-safe').addEventListener('click', () => invoke('exit_safe_mode'));
$('auto-check').addEventListener('change', (e) => {
  invoke('set_auto_check_update', { enabled: e.target.checked });
});

// 监听后端状态事件（安装/更新/崩溃），自动刷新面板
listen('dsh-status', () => refresh());
listen('settings-changed', () => refresh());
listen('dsh-status', () => refreshPlugins());

refresh();
refreshPlugins();
