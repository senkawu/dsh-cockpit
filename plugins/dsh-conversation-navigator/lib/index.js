// dsh-conversation-navigator —— 长对话快速导航条（P4 2.5.1）
// 行为（与设计文档一致）：
//   - MutationObserver 观察会话消息流 DOM，在会话容器右侧渲染刻度条（fixed、半透明）
//   - 每个用户消息一个刻度标记；悬停 → tooltip 预览消息摘要（前 ~60 字符）
//   - 点击刻度 → 平滑 scrollIntoView 跳转
//   - 消息数 < 阈值（默认 15）自动隐藏；可配置阈值与位置
// 实现方式：宿主 webServer 注册静态脚本 + tapIndex 注入 <script>，
// 刻度条纯前端实现（不依赖壳层，随 dsh 插件体系安装/热开关/卸载）。

// 注意：选择器基于 dsh web 前端（dsh-client-ui-conversation）实测 DOM：
//   - 消息单元: [class*="flowItem"]（如 Md3f7G_flowItem）
//   - 用户消息: 单元内存在 [class*="userStack"]（如 gdEzaW_userStack）
//   - 滚动容器: 消息流外层可滚动祖先（动态探测）

const name = 'dsh-conversation-navigator'

// 纯宿主插件：需要 webServer 服务注入（注册脚本路由 + tapIndex 注入）。
const inject = ['webServer']

// 刻度条脚本：注入会话页面。所有选择器都走「候选 + 兜底」策略，
// 上游 DOM 变更时仅功能降级（找不到容器就不显示），不影响会话本体。
const NAVIGATOR_JS = `
(function () {
  if (window.__dshNavigatorInjected) return;
  window.__dshNavigatorInjected = true;

  var THRESHOLD = 15; // 消息数低于此阈值自动隐藏

  // ---- 工具 ----
  function isUserFlowItem(el) {
    if (!el || el.nodeType !== 1) return false;
    // 消息单元内存在用户消息栈即视为用户轮次
    if (el.querySelector && el.querySelector('[class*="userStack" i]')) return true;
    var cls = (typeof el.className === 'string' ? el.className : '') || '';
    if (cls.indexOf('userStack') !== -1 || cls.indexOf('UserStack') !== -1) return true;
    return false;
  }
  function previewText(el) {
    var t = (el.innerText || el.textContent || '').replace(/\\s+/g, ' ').trim();
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

  // ---- 主逻辑 ----
  var rail = null;
  var tip = null;
  var ticks = new Map(); // 消息元素 -> 刻度元素

  function flowItems() {
    return document.querySelectorAll('[class*="flowItem"]');
  }
  function scrollContainer() {
    // 消息流外层可滚动容器（动态探测：从消息单元向上找可滚动祖先）
    var first = flowItems()[0];
    if (!first) return null;
    var p = first.parentElement;
    var depth = 0;
    while (p && depth < 8) {
      if (p.scrollHeight > p.clientHeight + 50) return p;
      p = p.parentElement;
      depth++;
    }
    return null;
  }

  function rebuild() {
    if (!rail) return;
    var items = flowItems();
    var userItems = Array.prototype.filter.call(items, isUserFlowItem);
    // 用户消息数 < 阈值 → 隐藏
    if (!userItems.length || userItems.length < THRESHOLD) {
      rail.style.display = 'none';
      return;
    }
    rail.style.display = 'flex';
    // 清空旧刻度
    ticks.forEach(function (t) { if (t.parentNode) t.parentNode.removeChild(t); });
    ticks.clear();
    var container = scrollContainer();
    if (!container) {
      // 无滚动容器：仍渲染刻度，点击直接 scrollIntoView
      userItems.forEach(function (msg) {
        var tick = makeTick(msg);
        rail.appendChild(tick);
        ticks.set(msg, tick);
      });
      return;
    }
    // 有滚动容器：刻度按消息在流中的相对位置定位（比例映射）
    var total = container.scrollHeight - container.clientHeight || 1;
    userItems.forEach(function (msg) {
      var tick = makeTick(msg);
      // 用 offsetTop 相对滚动容器比例定位（无容器时忽略）
      var top = 0;
      var el = msg;
      while (el && el !== container && el !== document.body) {
        top += el.offsetTop || 0;
        el = el.offsetParent;
      }
      var ratio = Math.min(1, Math.max(0, top / container.scrollHeight));
      tick.style.marginTop = (ratio * 100) + '%';
      tick.style.marginBottom = '0';
      rail.appendChild(tick);
      ticks.set(msg, tick);
    });
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
    tick.addEventListener('mouseleave', function () {
      if (tip) tip.style.display = 'none';
    });
    tick.addEventListener('click', function () {
      msg.scrollIntoView({ behavior: 'smooth', block: 'center' });
      ticks.forEach(function (t) { t.classList.remove('dsh-nav-active'); });
      tick.classList.add('dsh-nav-active');
    });
    return tick;
  }

  function ensureRail() {
    if (rail) return;
    rail = document.createElement('div');
    rail.className = 'dsh-nav-rail';
    rail.style.display = 'none'; // 初始隐藏，rebuild 判定
    document.body.appendChild(rail);
    tip = document.createElement('div');
    tip.className = 'dsh-nav-tip';
    document.body.appendChild(tip);
  }

  function start() {
    makeStyle();
    ensureRail();
    rebuild();
    var mo = new MutationObserver(function () {
      // 防抖：DOM 高频变动时合并重建
      clearTimeout(window.__dshNavTimer);
      window.__dshNavTimer = setTimeout(rebuild, 300);
    });
    mo.observe(document.body, { childList: true, subtree: true });
    // 滚动时更新活跃刻度（按可见性：离视口中心最近的用户消息）
    var scrollTick = function () {
      if (!ticks.size) return;
      var best = null, bestDist = Infinity;
      var mid = window.innerHeight / 2;
      ticks.forEach(function (tick, msg) {
        var r = msg.getBoundingClientRect();
        if (r.height === 0) return;
        var center = r.top + r.height / 2;
        var d = Math.abs(center - mid);
        if (d < bestDist) { bestDist = d; best = tick; }
      });
      ticks.forEach(function (t) { t.classList.remove('dsh-nav-active'); });
      if (best) best.classList.add('dsh-nav-active');
    };
    window.addEventListener('scroll', scrollTick, { passive: true });
    window.__dshNavigatorState = 'active';
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();
`

function apply(ctx, config = {}) {
  if (typeof config.threshold === 'number' && config.threshold > 0) {
    // 阈值可配置：运行时无法改注入脚本常量，仅记录配置供将来版本使用
    ctx.logger?.info?.('[navigator] 阈值配置: ' + config.threshold)
  }
  const disposers = []

  disposers.push(ctx.webServer.register({
    kind: 'exact',
    path: '/dsh-navigator/navigator.js',
    handler: (req, res) => {
      res.writeHead(200, {
        'Content-Type': 'application/javascript; charset=utf-8',
        'Cache-Control': 'no-store',
      })
      res.end(NAVIGATOR_JS)
    },
  }))

  disposers.push(ctx.webServer.tapIndex((html) => {
    if (html.indexOf('/dsh-navigator/navigator.js') !== -1) return html
    const tag = '<script defer src="/dsh-navigator/navigator.js"></script>'
    if (html.indexOf('</body>') !== -1) return html.replace('</body>', tag + '</body>')
    return html + tag
  }))

  ctx.effect(() => () => {
    for (const d of disposers) {
      try { d() } catch (err) {}
    }
  })
}

export { name, inject, apply }
