(function () {
  "use strict";

  function pad(n) { return n < 10 ? "0" + n : "" + n; }

  function tick() {
    var el = document.getElementById("clock");
    if (!el) return;
    var d = new Date();
    el.textContent = pad(d.getHours()) + ":" + pad(d.getMinutes()) + ":" + pad(d.getSeconds());
  }

  function setText(id, v) {
    var el = document.getElementById(id);
    if (el && v !== undefined && v !== null) el.textContent = v;
  }

  function applyStats(s) {
    if (!s) return;
    setText("statDevices", s.total);
    setText("statOnline", s.online);
    setText("statAbnormal", s.abnormal);
    setText("statUsers", s.users);
  }

  // 设备树轮询间隔。树数据只用于侧栏在线状态与顶部统计，设备实时数值由
  // 设备面板自己的 3s 轮询负责，所以这里可以放得很宽——一台设备量上千时，
  // /api/tree 是全站最大的出站流量来源，间隔翻倍出站就减半。
  var TREE_POLL_MS = 20000;
  var pollTimer = null;

  // ---------------- 空闲锁定 ----------------
  // 持续操作时服务端中间件会滑动续期；停手超过窗口后停止所有轮询并锁定页面，
  // 避免"页面忘了关、后台一直刷"的流量浪费。窗口值由 /api/tree 下发，
  // 与「系统设置 → 空闲超时」保持一致，两边不会写死成不同数字。
  var IDLE_FALLBACK_MS = 15 * 60 * 1000;
  var idleMs = IDLE_FALLBACK_MS;
  var locked = false;
  var lastActive = Date.now();

  function tr(key, fallback) {
    return (window.I18N && window.I18N[key]) || fallback;
  }

  function esc(s) {
    return String(s).replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }

  function markActive() {
    if (locked) return;
    lastActive = Date.now();
  }

  function checkIdle() {
    if (locked) return;
    if (Date.now() - lastActive >= idleMs) lock();
  }

  function lock() {
    if (locked) return;
    locked = true;
    stopPolling();
    // 顺手让服务端会话失效，别让 cookie 在后台继续有效；
    // 离线时忽略——服务端仍会按空闲窗口自然过期。
    try {
      fetch("/logout", { method: "POST", credentials: "same-origin", keepalive: true });
    } catch (e) { /* 忽略 */ }
    showLock();
  }

  function showLock() {
    if (document.getElementById("idleLock")) return;
    var minutes = Math.round(idleMs / 60000);
    var next = location.pathname + location.search;
    var wrap = document.createElement("div");
    wrap.className = "idle-lock";
    wrap.id = "idleLock";
    wrap.innerHTML =
      '<div class="idle-card" role="dialog" aria-modal="true">' +
      '<div class="idle-icon">' +
      '<svg viewBox="0 0 24 24" width="24" height="24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">' +
      '<rect x="4" y="10.5" width="16" height="10" rx="2.2"></rect>' +
      '<path d="M8 10.5V7.6a4 4 0 0 1 8 0v2.9"></path>' +
      '<circle cx="12" cy="15.4" r="1.15" fill="currentColor" stroke="none"></circle>' +
      "</svg></div>" +
      "<h3>" + esc(tr("js.idle_title", "会话已锁定")) + "</h3>" +
      "<p>" + esc(tr("js.idle_body", "已超过 {min} 分钟没有操作，页面已停止自动刷新以节省流量。").replace("{min}", String(minutes))) + "</p>" +
      '<p class="idle-note">' + esc(tr("js.idle_note", "后台任务（如 OTA 升级）仍在继续运行，重新登录后即可查看最新状态。")) + "</p>" +
      '<a class="idle-btn" href="/login?next=' + encodeURIComponent(next) + '">' +
      esc(tr("js.idle_button", "重新登录")) +
      '<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h13M13 6l6 6-6 6"></path></svg>' +
      "</a></div>";
    document.body.appendChild(wrap);
  }

  function refresh() {
    if (locked) return;
    fetch("/api/tree", { headers: { Accept: "application/json" }, credentials: "same-origin" })
      .then(function (r) {
        // 令牌已过期（例如标签页被浏览器节流、定时器没跑到）：直接锁定
        if (r.status === 401) { lock(); return null; }
        return r.ok ? r.json() : null;
      })
      .then(function (data) {
        if (!data) return;
        if (data.idle_secs > 0) idleMs = data.idle_secs * 1000;
        applyStats(data.stats);
        window.__TREE__ = data.tree;
        window.dispatchEvent(new CustomEvent("tree-data", { detail: data }));
      })
      .catch(function () { /* 断线时保持上一帧数据 */ });
  }

  function startPolling() {
    if (pollTimer) return;
    pollTimer = setInterval(refresh, TREE_POLL_MS);
  }

  function stopPolling() {
    if (!pollTimer) return;
    clearInterval(pollTimer);
    pollTimer = null;
  }

  window.refreshTree = refresh;
  window.__applyTreeStats = applyStats;

  // ---------------- 功能菜单收起 / 展开 ----------------
  var MENU_KEY = "ups.menu.collapsed";

  function menuCollapsed() {
    return document.documentElement.classList.contains("menu-collapsed");
  }

  function applyMenu(collapsed) {
    document.documentElement.classList.toggle("menu-collapsed", collapsed);
    var btn = document.querySelector(".menu-collapse");
    var tip = collapsed ? "展开菜单" : "收起菜单";
    if (window.I18N && window.I18N["menu.collapse_short"]) {
      tip = window.I18N["menu.collapse_short"];
    }
    if (btn) btn.title = tip;
    // 侧栏宽度变化后让图表等自适应容器重排
    window.dispatchEvent(new Event("resize"));
  }

  function toggleMenu() {
    var next = !menuCollapsed();
    applyMenu(next);
    try { localStorage.setItem(MENU_KEY, next ? "1" : "0"); } catch (e) { /* 隐私模式忽略 */ }
  }

  try {
    var saved = localStorage.getItem(MENU_KEY);
    if (saved === "1") applyMenu(true);
    else if (saved === null && window.innerWidth < 980) applyMenu(true);
  } catch (e) { /* localStorage 不可用时保持展开 */ }

  document.addEventListener("click", function (e) {
    var trigger = e.target.closest ? e.target.closest("[data-menu-toggle]") : null;
    if (!trigger) return;
    e.preventDefault();
    toggleMenu();
  });

  document.addEventListener("keydown", function (e) {
    // Alt+M 快速切换菜单
    if (e.altKey && (e.key === "m" || e.key === "M")) {
      e.preventDefault();
      toggleMenu();
    }
  });

  tick();
  setInterval(tick, 1000);
  setInterval(checkIdle, 1000);

  // 任何真实操作都算"在用"：鼠标、键盘、滚轮、触摸、滚动
  ["mousemove", "mousedown", "pointerdown", "keydown", "wheel", "touchstart", "scroll"].forEach(
    function (name) {
      document.addEventListener(name, markActive, { passive: true, capture: true });
    }
  );

  // 锁定后掐断 HTMX 的轮询（设备实时指标 3s、OTA 进度 900ms 都在这一层）
  document.addEventListener("htmx:configRequest", function (ev) {
    if (locked) ev.preventDefault();
  });

  document.addEventListener("DOMContentLoaded", function () {
    refresh();
    if (!document.hidden) startPolling();
    // 切到后台标签页就停轮询，切回来立刻补一次并恢复——没人看的页面不该产生流量。
    // 回来时先判定是否已超时（后台定时器会被浏览器节流，不能只靠 checkIdle）。
    document.addEventListener("visibilitychange", function () {
      if (document.hidden) {
        stopPolling();
        return;
      }
      checkIdle();
      if (locked) return;
      refresh();
      startPolling();
    });
  });
})();
