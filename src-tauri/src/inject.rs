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
