/* 设备树：用户 → 分组 → 设备。数据全部由服务端按可见性裁剪后下发。 */
(function () {
  "use strict";

  var LS_KEY = "ups.tree.collapsed";
  var collapsed = new Set();
  try {
    JSON.parse(localStorage.getItem(LS_KEY) || "[]").forEach(function (k) { collapsed.add(k); });
  } catch (e) { /* 忽略损坏的本地状态 */ }

  var state = {
    forest: [],
    filter: "all",
    query: "",
    selected: "",
    booted: false
  };

  var treeEl = document.getElementById("deviceTree");
  var stampEl = document.getElementById("treeStamp");

  function saveCollapsed() {
    try { localStorage.setItem(LS_KEY, JSON.stringify(Array.from(collapsed))); } catch (e) {}
  }

  // 文案：服务端注入 window.I18N，缺失时回退中文（与服务端词典一致）
  function T(key, fallback) {
    if (window.I18N && window.I18N[key]) return window.I18N[key];
    return fallback;
  }

  function esc(s) {
    return String(s == null ? "" : s)
      .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function relTime(ms) {
    if (!ms) return "—";
    var diff = Date.now() - ms;
    if (diff < 0) diff = 0;
    var s = Math.floor(diff / 1000);
    if (s < 60) return T("js.rel_secs", "{n}s ago").replace("{n}", s);
    var m = Math.floor(s / 60);
    if (m < 60) return T("js.rel_mins", "{n} 分钟前").replace("{n}", m);
    var h = Math.floor(m / 60);
    if (h < 24) return T("js.rel_hours", "{n} 小时前").replace("{n}", h);
    return T("js.rel_days", "{n} 天前").replace("{n}", Math.floor(h / 24));
  }

  function deviceVisible(d) {
    if (state.filter === "online" && !d.online) return false;
    if (state.filter === "offline" && d.online) return false;
    if (state.query) {
      var hay = (d.sn + " " + d.mdvr_id + " " + d.owner + " " + d.group).toLowerCase();
      if (hay.indexOf(state.query) < 0) return false;
    }
    return true;
  }

  function renderDevice(d) {
    var cls = d.online ? "online" : "offline";
    var text = d.online ? (d.busy ? T("dashboard.badge_busy", "升级中") : T("js.node_online", "在线")) : T("js.node_offline", "离线");
    var stateCls = d.busy ? "busy" : cls;
    var battery = d.battery > 0 ? d.battery + "%" : "—";
    var fw = d.firmware ? "FW " + d.firmware : "FW —";
    var sel = d.sn === state.selected ? " selected" : "";
    return '' +
      '<div class="device-item' + sel + '" data-sn="' + esc(d.sn) + '">' +
        '<div class="dev-name"><span class="status-dot ' + cls + '"></span>' + esc(d.sn) + '</div>' +
        '<div class="dev-meta">' +
          '<span class="dev-state ' + stateCls + '">' + text + '</span>' +
          '<span>电量 ' + battery + '</span>' +
          '<span>' + esc(fw) + '</span>' +
          '<span>' + relTime(d.last_seen_ms) + '</span>' +
        '</div>' +
      '</div>';
  }

  function renderGroup(g) {
    var devices = (g.devices || []).filter(deviceVisible);
    if (!devices.length) return "";
    var open = !collapsed.has(g.key);
    var html = '<div class="tree-group" data-key="' + esc(g.key) + '">' +
      '<span class="caret' + (open ? " open" : "") + '">▶</span>' +
      '<span>' + esc(g.name) + '</span>' +
      '<span class="g-count">' + g.online + '/' + g.total + '</span>' +
      '</div>';
    if (open) html += devices.map(renderDevice).join("");
    return html;
  }

  function renderUser(u, depth) {
    var groups = (u.groups || []).map(renderGroup).join("");
    var children = (u.children || []).map(function (c) { return renderUser(c, depth + 1); }).join("");
    var hasContent = groups || children;
    // 搜索/过滤后无命中，且不是顶级用户时隐藏整枝
    if (!hasContent && (state.query || state.filter !== "all") && depth > 0) return "";
    var open = !collapsed.has(u.key);
    var role = u.role && u.role !== "—" ? '<span class="u-role">' + esc(u.role) + '</span>' : "";
    var html = '<div class="tree-user" data-key="' + esc(u.key) + '">' +
      '<span class="caret' + (open ? " open" : "") + '">▶</span>' +
      '<span>' + esc(u.label || u.name) + '</span>' + role +
      '<span class="tree-badge">' + u.online + '<span class="total"> / ' + u.total + '</span></span>' +
      '</div>';
    if (open) html += groups + children;
    return html;
  }

  function render() {
    var html = state.forest.map(function (u) { return renderUser(u, 0); }).join("");
    if (!html) {
      treeEl.innerHTML = '<div class="empty-hint">' + esc(T("js.no_result", "没有匹配的设备")) + '</div>';
      return;
    }
    treeEl.innerHTML = html;
  }

  function loadPanel(sn) {
    state.selected = sn;
    document.querySelectorAll(".device-item").forEach(function (el) {
      el.classList.toggle("selected", el.getAttribute("data-sn") === sn);
    });
    if (window.htmx) {
      window.htmx.ajax("GET", "/devices/" + encodeURIComponent(sn) + "/panel", {
        target: "#detailPane",
        swap: "innerHTML"
      });
    }
    try {
      history.replaceState(null, "", "/monitor?sn=" + encodeURIComponent(sn));
    } catch (e) {}
  }

  function firstDevice() {
    var found = "";
    (function walk(nodes) {
      nodes.forEach(function (u) {
        (u.groups || []).forEach(function (g) {
          (g.devices || []).forEach(function (d) {
            if (!found && deviceVisible(d)) found = d.sn;
          });
        });
        walk(u.children || []);
      });
    })(state.forest);
    return found;
  }

  function apply(data, initial) {
    if (!data || !data.tree) return;
    state.forest = data.tree;
    if (window.__applyTreeStats) window.__applyTreeStats(data.stats);
    render();
    if (stampEl) {
      var d = new Date();
      stampEl.textContent = T("js.sync_now", "已同步") + " " + ("0" + d.getHours()).slice(-2) + ":" +
        ("0" + d.getMinutes()).slice(-2) + ":" + ("0" + d.getSeconds()).slice(-2);
    }
    if (!state.booted) {
      state.booted = true;
      var want = initial || window.__INITIAL_SN__ || "";
      var sn = "";
      if (want && document.querySelector('.device-item[data-sn="' + want + '"]')) sn = want;
      if (!sn) sn = firstDevice();
      if (sn) loadPanel(sn);
    } else if (state.selected) {
      // 树刷新后保持选中高亮
      document.querySelectorAll(".device-item").forEach(function (el) {
        el.classList.toggle("selected", el.getAttribute("data-sn") === state.selected);
      });
    }
  }

  document.addEventListener("DOMContentLoaded", function () {
    var initial = null;
    var raw = document.getElementById("initialTree");
    if (raw) {
      try { initial = JSON.parse(raw.textContent); } catch (e) { initial = null; }
    }
    if (initial) apply({ tree: initial, stats: null }, window.__INITIAL_SN__);

    window.addEventListener("tree-data", function (ev) {
      apply(ev.detail, null);
    });

    treeEl.addEventListener("click", function (ev) {
      var device = ev.target.closest(".device-item");
      if (device) {
        loadPanel(device.getAttribute("data-sn"));
        return;
      }
      var group = ev.target.closest(".tree-group");
      if (group) {
        var gk = group.getAttribute("data-key");
        collapsed.has(gk) ? collapsed.delete(gk) : collapsed.add(gk);
        saveCollapsed();
        render();
        return;
      }
      var user = ev.target.closest(".tree-user");
      if (user) {
        var uk = user.getAttribute("data-key");
        collapsed.has(uk) ? collapsed.delete(uk) : collapsed.add(uk);
        saveCollapsed();
        render();
      }
    });

    var search = document.getElementById("treeSearch");
    if (search) {
      search.addEventListener("input", function () {
        state.query = search.value.trim().toLowerCase();
        render();
      });
    }

    var filterBar = document.getElementById("treeFilter");
    if (filterBar) {
      filterBar.addEventListener("click", function (ev) {
        var btn = ev.target.closest(".filter-btn");
        if (!btn) return;
        filterBar.querySelectorAll(".filter-btn").forEach(function (b) { b.classList.remove("active"); });
        btn.classList.add("active");
        state.filter = btn.getAttribute("data-filter");
        render();
      });
    }

    // 归属调整后服务端回 HX-Trigger: tree-refresh
    document.body.addEventListener("tree-refresh", function () {
      if (window.refreshTree) window.refreshTree();
      if (state.selected && window.htmx) {
        window.htmx.ajax("GET", "/devices/" + encodeURIComponent(state.selected) + "/panel", {
          target: "#detailPane",
          swap: "innerHTML"
        });
      }
    });
  });
})();
