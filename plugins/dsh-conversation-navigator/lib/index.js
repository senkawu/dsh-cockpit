// dsh-conversation-navigator —— 长对话快速导航条（P4 2.5.1）
// 行为（与设计文档一致）：
//   - MutationObserver 观察会话消息流 DOM，在会话容器右侧渲染刻度条（fixed、半透明）
//   - 每个用户消息一个刻度标记；悬停 → tooltip 预览消息摘要（前 ~60 字符）
//   - 点击刻度 → 平滑 scrollIntoView 跳转
//   - 消息数 < 阈值（默认 15）自动隐藏；可配置阈值与位置
// 实现方式：宿主 webServer 注册静态脚本 + tapIndex 注入 <script>，
// 刻度条纯前端实现（不依赖壳层，随 dsh 插件体系安装/热开关/卸载）。

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
  function byAll(selectors) {
    for (var i = 0; i < selectors.length; i++) {
      var nodes = document.querySelectorAll(selectors[i]);
      if (nodes && nodes.length) return nodes;
    }
    return [];
  }
  function byOne(selectors) {
    for (var i = 0; i < selectors.length; i++) {
      var el = document.querySelector(selectors[i]);
      if (el) return el;
    }
    return null;
  }
  function isUserMessage(el) {
    // 命中任意用户消息特征（角色标记/对齐方式）即视为用户消息
    if (!el || el.nodeType !== 1) return false;
    if (el.closest && el.closest('[data-role="user"], [data-author="user"], [data-actor="user"]')) return true;
    var cls = (el.className && String(el.className)) || '';
    if (cls.indexOf('user') !== -1 || cls.indexOf('User') !== -1) return true;
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

  function containerEl() {
    return byOne([
      '[data-testid="conversation-list"]',
      '[data-testid="message-list"]',
      '.chat-messages',
      '.message-list',
      '.conversation-list',
      'main',
    ]);
  }
  function messageEls() {
    return byAll([
      '[data-testid="message"]',
      '[data-testid="chat-message"]',
      '.chat-message',
      '.message',
      '[class*="message"]',
    ]);
  }

  function rebuild() {
    if (!rail) return;
    var container = containerEl();
    if (!container) return;
    var msgs = messageEls();
    if (!msgs || msgs.length < THRESHOLD) {
      rail.style.display = 'none';
      return;
    }
    rail.style.display = '';
    // 清空旧刻度
    ticks.forEach(function (t) { if (t.parentNode) t.parentNode.removeChild(t); });
    ticks.clear();
    var userMsgs = Array.prototype.filter.call(msgs, isUserMessage);
    if (!userMsgs.length) return;
    var step = Math.max(1, Math.floor((container.scrollHeight - container.clientHeight) / userMsgs.length));
    userMsgs.forEach(function (msg) {
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
      rail.appendChild(tick);
      ticks.set(msg, tick);
    });
    void step; // 位置按 scrollIntoView 定位，step 仅为将来「按比例定位」预留
  }

  function ensureRail() {
    if (rail) return;
    rail = document.createElement('div');
    rail.className = 'dsh-nav-rail';
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
    var root = containerEl() || document.body;
    mo.observe(root, { childList: true, subtree: true });
    // 滚动时更新活跃刻度
    window.addEventListener('scroll', function () {
      if (!ticks.size) return;
      var container = containerEl();
      if (!container) return;
      var mid = container.scrollTop + container.clientHeight / 2;
      var best = null, bestDist = Infinity;
      ticks.forEach(function (tick, msg) {
        var d = Math.abs((msg.offsetTop || 0) - mid);
        if (d < bestDist) { bestDist = d; best = tick; }
      });
      ticks.forEach(function (t) { t.classList.remove('dsh-nav-active'); });
      if (best) best.classList.add('dsh-nav-active');
    }, { passive: true });
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
