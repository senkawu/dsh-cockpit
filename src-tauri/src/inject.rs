//! 宿主注入清单（P4 2.5.3）：版本化 JS 注入层。
//! 每个条目：脚本 id + 版本 + 注入时机（Started=页面脚本执行前 / Finished=加载完成后）。
//! 白名单内才注入；`--safe-mode` 时整体关闭；单条注入失败静默降级（不阻塞页面）。

use tauri::webview::PageLoadEvent;
use tauri::WebviewWindow;

/// 注入条目
pub struct Injection {
    pub id: &'static str,
    pub version: u32,
    pub at: PageLoadEvent,
    pub script: &'static str,
}

/// 轨迹面板字段汉化（P4 2.5.2）：dsh 上游若已支持 i18n 则由官方文案接管，
/// 此处仅兜底做 DOM 文案替换。MutationObserver 处理动态渲染；
/// 映射表版本化，随版本更新集中维护。
const TRACE_ZH_LOCALIZE_JS: &str = r#"
  (function(){
    if (window.__dshCockpitTraceZhInjected) return;
    window.__dshCockpitTraceZhInjected = true;
    var MAP = {
      'Duration': '耗时',
      'Turns': '轮次',
      'Calls': '调用次数',
      'Trace': '轨迹',
      'Trace Panel': '轨迹面板',
      'Tokens': 'Token 数',
      'Cost': '费用',
      'Latency': '延迟',
      'Status': '状态',
      'Model': '模型',
      'Context': '上下文',
      'Prompt': '提示词',
      'Response': '响应',
      'Elapsed': '已用时间',
      'Retries': '重试次数',
      'Success': '成功',
      'Failed': '失败'
    };
    var replaced = {};
    function replaceIn(node) {
      if (!node || node.nodeType !== 1) return;
      // 跳过输入框与已有标记
      if (node.dataset && node.dataset.dshZhDone) return;
      if (/^(INPUT|TEXTAREA|SELECT|SCRIPT|STYLE)$/.test(node.tagName)) return;
      var walker = document.createTreeWalker(node, NodeFilter.SHOW_TEXT, null);
      var textNode;
      while ((textNode = walker.nextNode())) {
        var t = textNode.nodeValue;
        if (!t) continue;
        var hit = false;
        for (var en in MAP) {
          if (!MAP.hasOwnProperty(en)) continue;
          var re = new RegExp('\\b' + en + '\\b', 'g');
          if (re.test(t)) {
            t = t.replace(re, MAP[en]);
            hit = true;
          }
        }
        if (hit) textNode.nodeValue = t;
      }
      if (node.dataset) node.dataset.dshZhDone = '1';
    }
    replaceIn(document.body);
    var mo = new MutationObserver(function(muts){
      for (var i = 0; i < muts.length; i++) {
        var added = muts[i].addedNodes;
        for (var j = 0; j < added.length; j++) {
          replaceIn(added[j]);
        }
      }
    });
    mo.observe(document.body, { childList: true, subtree: true });
    // 供宿主探测（面板可显示注入状态）
    window.__dshCockpitInjections = window.__dshCockpitInjections || {};
    window.__dshCockpitInjections['trace-zh'] = '1.0';
  })();
"#;

/// 长对话导航条（P4 2.5.1）：会话右侧刻度轨道。
/// 壳层直接注入（不依赖 dsh 插件安装——插件需 npm 发布，普通用户拿不到，
/// 故改为随外壳注入，任何 dmg 开箱即用）。选择器基于 dsh web 实测 DOM：
///   消息单元 [class*="flowItem"]，用户消息 = 单元内含 [class*="userStack" i]。
const NAVIGATOR_RAIL_JS: &str = r#"
  (function(){
    if (window.__dshNavigatorInjected) return;
    window.__dshNavigatorInjected = true;
    var THRESHOLD = 15;
    function isUserFlowItem(el) {
      if (!el || el.nodeType !== 1) return false;
      if (el.querySelector && el.querySelector('[class*="userStack" i]')) return true;
      var cls = (typeof el.className === 'string' ? el.className : '') || '';
      return cls.indexOf('userStack') !== -1 || cls.indexOf('UserStack') !== -1;
    }
    function previewText(el) {
      var t = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
      return t.length > 60 ? t.slice(0, 60) + '…' : t;
    }
    function makeStyle() {
      var css = [
        '.dsh-nav-rail{position:fixed;right:10px;top:50%;transform:translateY(-50%);z-index:9999;display:flex;flex-direction:column;gap:4px;max-height:70vh;overflow-y:auto;padding:8px 4px;border-radius:12px;background:rgba(15,20,30,.35);backdrop-filter:blur(6px);opacity:.6;transition:opacity .2s;scrollbar-width:none}',
        '.dsh-nav-rail:hover{opacity:1}',
        '.dsh-nav-rail::-webkit-scrollbar{display:none}',
        '.dsh-nav-tick{width:8px;height:8px;min-height:8px;border-radius:4px;background:rgba(120,160,255,.75);cursor:pointer;transition:all .15s;position:relative;flex:none}',
        '.dsh-nav-tick:hover{background:rgba(90,140,255,1);transform:scale(1.35)}',
        '.dsh-nav-tick.dsh-nav-active{background:#4c8dff;box-shadow:0 0 6px rgba(76,141,255,.8)}',
        '.dsh-nav-tip{position:fixed;z-index:10000;max-width:280px;padding:6px 10px;border-radius:8px;background:rgba(10,14,22,.92);color:#e6edf7;font-size:12px;line-height:1.4;pointer-events:none;white-space:normal;word-break:break-all;box-shadow:0 4px 16px rgba(0,0,0,.4);display:none}',
        '[data-theme="light"] .dsh-nav-rail{background:rgba(240,244,252,.55)}',
        '[data-theme="light"] .dsh-nav-tip{background:rgba(255,255,255,.95);color:#1a2332;box-shadow:0 4px 16px rgba(0,0,0,.15)}'
      ].join('');
      var style = document.createElement('style');
      style.textContent = css;
      document.head.appendChild(style);
    }
    var rail = null, tip = null, ticks = new Map();
    function flowItems() { return document.querySelectorAll('[class*="flowItem"]'); }
    function scrollContainer() {
      var first = flowItems()[0];
      if (!first) return null;
      var p = first.parentElement, depth = 0;
      while (p && depth < 8) {
        if (p.scrollHeight > p.clientHeight + 50) return p;
        p = p.parentElement; depth++;
      }
      return null;
    }
    function makeTick(msg) {
      var tick = document.createElement('div');
      tick.className = 'dsh-nav-tick';
      var summary = previewText(msg);
      tick.title = summary;
      tick.addEventListener('mouseenter', function (ev) {
        if (!tip) return;
        tip.textContent = summary || '（空消息）';
        tip.style.display = 'block';
        var r = ev.clientX, b = ev.clientY;
        tip.style.left = Math.min(r + 14, window.innerWidth - 300) + 'px';
        tip.style.top = Math.min(b - 8, window.innerHeight - 40) + 'px';
      });
      tick.addEventListener('mouseleave', function () { if (tip) tip.style.display = 'none'; });
      tick.addEventListener('click', function () {
        msg.scrollIntoView({ behavior: 'smooth', block: 'center' });
        ticks.forEach(function (t) { t.classList.remove('dsh-nav-active'); });
        tick.classList.add('dsh-nav-active');
      });
      return tick;
    }
    function rebuild() {
      if (!rail) return;
      var items = flowItems();
      var users = Array.prototype.filter.call(items, isUserFlowItem);
      if (!users.length || users.length < THRESHOLD) { rail.style.display = 'none'; return; }
      rail.style.display = 'flex';
      ticks.forEach(function (t) { if (t.parentNode) t.parentNode.removeChild(t); });
      ticks.clear();
      var container = scrollContainer();
      users.forEach(function (msg) {
        var tick = makeTick(msg);
        if (container) {
          var top = 0, el = msg;
          while (el && el !== container && el !== document.body) { top += el.offsetTop || 0; el = el.offsetParent; }
          var ratio = Math.min(1, Math.max(0, top / (container.scrollHeight || 1)));
          tick.style.marginTop = (ratio * 100) + '%';
          tick.style.marginBottom = '0';
        }
        rail.appendChild(tick);
        ticks.set(msg, tick);
      });
    }
    function start() {
      makeStyle();
      if (!rail) {
        rail = document.createElement('div');
        rail.className = 'dsh-nav-rail';
        rail.style.display = 'none';
        document.body.appendChild(rail);
        tip = document.createElement('div');
        tip.className = 'dsh-nav-tip';
        document.body.appendChild(tip);
      }
      rebuild();
      var mo = new MutationObserver(function () {
        clearTimeout(window.__dshNavTimer);
        window.__dshNavTimer = setTimeout(rebuild, 300);
      });
      mo.observe(document.body, { childList: true, subtree: true });
      window.addEventListener('scroll', function () {
        if (!ticks.size) return;
        var best = null, bestDist = Infinity, mid = window.innerHeight / 2;
        ticks.forEach(function (tick, msg) {
          var r = msg.getBoundingClientRect();
          if (!r.height) return;
          var d = Math.abs(r.top + r.height / 2 - mid);
          if (d < bestDist) { bestDist = d; best = tick; }
        });
        ticks.forEach(function (t) { t.classList.remove('dsh-nav-active'); });
        if (best) best.classList.add('dsh-nav-active');
      }, { passive: true });
      window.__dshNavigatorState = 'active';
    }
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
    else start();
  })();
"#;

/// 注入清单（版本化；新增脚本在此登记，白名单生效）
pub const INJECTION_MANIFEST: &[Injection] = &[
    Injection {
        id: "trace-zh",
        version: 1,
        at: PageLoadEvent::Finished, // 轨迹面板是动态渲染，Finished 后由 MutationObserver 接管
        script: TRACE_ZH_LOCALIZE_JS,
    },
    Injection {
        id: "dsh-cockpit-bridge",
        version: 1,
        at: PageLoadEvent::Finished,
        script: crate::host_ext::DSH_COCKPIT_BRIDGE_JS,
    },
    Injection {
        id: "navigator-rail",
        version: 1,
        at: PageLoadEvent::Finished, // 会话页动态渲染，Finished 后由 MutationObserver 接管
        script: NAVIGATOR_RAIL_JS,
    },
];

/// 对一次页面加载执行清单注入。safe_mode 时整体关闭。
pub fn run_injections(win: &WebviewWindow, event: &PageLoadEvent, safe_mode: bool) {
    if safe_mode {
        return;
    }
    for inj in INJECTION_MANIFEST {
        let same_phase = match (inj.at, event) {
            (PageLoadEvent::Started, PageLoadEvent::Started) => true,
            (PageLoadEvent::Finished, PageLoadEvent::Finished) => true,
            _ => false,
        };
        if !same_phase {
            continue;
        }
        if let Err(e) = win.eval(inj.script) {
            // 静默降级：注入失败不影响页面（记录日志便于排查）
            log::debug!("注入脚本 {} 失败: {e}", inj.id);
        } else {
            log::debug!("注入脚本 {} v{} 完成", inj.id, inj.version);
        }
    }
}
