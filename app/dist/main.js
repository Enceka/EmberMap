// EmberMap 前端：调用后端匹配，canvas 合成叠加（截图 + 手绘 screen 混合 + 门位）。
// 自动监测为自适应连续循环：分析完歇 IDLE_FAST 即下一轮；连续未检出则放缓省 CPU。
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const $ = (id) => document.getElementById(id);
const statusEl = $("status");
const canvas = $("view");
const ctx = canvas.getContext("2d");

const IDLE_FAST = 300;    // 地图刚出现/结果变化时的轮询间歇 ms
const IDLE_STABLE = 800;  // 结果稳定时放缓，省 CPU
const IDLE_SLOW = 1500;   // 连续未检出后的放缓间歇 ms

let lastPayload = null;
let pending = { key: null, n: 0 };  // 低分新结果的 2 帧确认
let shownKey = null;
let watching = false;
let busy = false;
let overlayMode = false;
let overlayVisible = false;
let lowConfMiss = 0;       // 面板在但低置信的连续帧数
let lastPushed = null;     // 上次推给覆盖层的指纹，避免重复推送
let androidOverlay = false; // Android 悬浮窗由 Kotlin 绘制，参数形状与桌面不同

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const floorCn = (f) => ({ "1f": "一楼", "2f": "二楼", b1: "地下室" }[f] || f);

function setStatus(text, cls) {
  statusEl.textContent = text;
  statusEl.className = cls || "";
}

function loadImg(b64) {
  return new Promise((res, rej) => {
    const im = new Image();
    im.onload = () => res(im);
    im.onerror = rej;
    im.src = "data:image/png;base64," + b64;
  });
}

async function render(p) {
  const [shot, draw] = await Promise.all([loadImg(p.shot_png), loadImg(p.draw_png)]);
  const alpha = $("rng-alpha").value / 100;
  canvas.width = shot.width;
  canvas.height = shot.height;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(shot, 0, 0);
  ctx.save();
  ctx.setTransform(p.tf.scale, 0, 0, p.tf.scale, p.tf.tx, p.tf.ty);
  ctx.globalCompositeOperation = "screen";
  ctx.globalAlpha = alpha;
  ctx.drawImage(draw, 0, 0);
  ctx.restore();
  const r = Math.max(8, canvas.height * 0.014);
  ctx.font = `bold ${Math.max(15, canvas.height * 0.026)}px "PingFang SC", sans-serif`;
  for (const d of p.doors) {
    ctx.strokeStyle = "#ff5050";
    ctx.lineWidth = 4;
    ctx.beginPath();
    ctx.arc(d.x, d.y, r, 0, Math.PI * 2);
    ctx.stroke();
    ctx.fillStyle = "#ff7878";
    ctx.strokeStyle = "#000";
    ctx.lineWidth = 3;
    ctx.strokeText(d.label, d.x + r + 4, d.y + 5);
    ctx.fillText(d.label, d.x + r + 4, d.y + 5);
  }
}

/// 排障用：把实际抓到的整帧画到 canvas 上，用户截图即可看出抓到的是什么
async function drawFrame(b64) {
  const im = await loadImg(b64);
  canvas.width = im.width;
  canvas.height = im.height;
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(im, 0, 0);
  ctx.fillStyle = "rgba(0,0,0,0.55)";
  ctx.fillRect(0, 0, canvas.width, 26);
  ctx.fillStyle = "#e3b341";
  ctx.font = "16px sans-serif";
  ctx.fillText("这是识别器实际抓到的画面", 8, 19);
}

function renderCandidates(list) {
  $("candidates").innerHTML = list
    .map((c, i) => `<li class="${i === 0 ? "best" : ""}">${i + 1}. ${c.name} · ${floorCn(c.floor)}　${c.score.toFixed(3)}</li>`)
    .join("");
}

function overlayArgs(p) {
  if (androidOverlay) {
    // Android：手绘图已解压在磁盘上，直接传路径由 Kotlin 读，免 base64 过桥
    return {
      drawPath: p.draw_path,
      tf: p.tf,
      doors: p.doors,
      x: p.view[0],
      y: p.view[1],
      w: p.view[2],
      h: p.view[3],
      alpha: $("rng-alpha").value / 100,
    };
  }
  return {
    payload: {
      draw_png: p.draw_png,
      tf: p.tf,
      doors: p.doors,
      panel_w: p.view[2],
      panel_h: p.view[3],
      alpha: $("rng-alpha").value / 100,
    },
    x: p.view[0],
    y: p.view[1],
    w: p.view[2],
    h: p.view[3],
  };
}

// 指纹：结果/几何没变就不重推覆盖层
function fingerprint(p) {
  const t = p.tf;
  return [p.name, p.floor, ...p.view, t.scale.toFixed(3),
          t.tx.toFixed(0), t.ty.toFixed(0), $("rng-alpha").value].join("|");
}

async function pushOverlay(force) {
  if (!overlayMode || !lastPayload) return;
  const fp = fingerprint(lastPayload);
  if (!force && overlayVisible && fp === lastPushed) return;
  try {
    await invoke("overlay_update", overlayArgs(lastPayload));
    overlayVisible = true;
    lastPushed = fp;
  } catch (e) {
    setStatus(String(e), "warn");
  }
}

async function hideOverlay() {
  if (overlayVisible) {
    await invoke("overlay_hide");
    overlayVisible = false;
    lastPushed = null;
  }
}

/// 返回 'hit' | 'miss'（供自适应循环决定节奏）
async function analyzeOnce(auto) {
  if (busy) return "miss";
  busy = true;
  $("btn-capture").disabled = true;
  if (!auto) setStatus("抓屏匹配中…");
  try {
    const p = await invoke("analyze_screen");
    if (p.status === "no_panel") {
      // 面板整体消失 = 地图关了，立即隐藏
      pending = { key: null, n: 0 };
      lowConfMiss = 0;
      await hideOverlay();
      // 自动模式下也要更新状态：否则用户只看到停留不动的旧文字，
      // 无法判断它到底在不在工作（真机排障时踩过这个坑）
      setStatus(`未识别到地图（抓到 ${p.frame_w}×${p.frame_h}）：${p.reason}`, "warn");
      renderCandidates([]);
      if (p.frame_png) await drawFrame(p.frame_png);
      return "miss";
    }
    const key = `${p.name}|${p.floor}`;
    if (!p.confident) {
      // 证据不足（后端仍在多帧投票）：面板在就继续攒证据，不显示叠加
      lowConfMiss += 1;
      if (lowConfMiss >= 2) await hideOverlay();
      const pct = Math.min(99, Math.round((p.evidence / p.evidence_need) * 100));
      setStatus(
        `识别中 ${pct}%（当前最像 ${p.name}·${floorCn(p.floor)} ${p.score.toFixed(2)}）`,
        "warn"
      );
      renderCandidates(p.candidates);
      return "hit"; // 面板在，保持快节奏继续攒证据
    }
    lowConfMiss = 0;
    shownKey = key;
    lastPayload = p;
    const tag = p.phase === "tracking" ? "跟踪" : "已锁定";
    setStatus(
      `${p.name} · ${floorCn(p.floor)}　置信 ${p.score.toFixed(2)}　` +
      `领先次佳 ${p.advantage.toFixed(3)}　${tag}`,
      "ok"
    );
    renderCandidates(p.candidates);
    await render(p);
    await pushOverlay(false);
    return "hit";
  } catch (e) {
    setStatus(String(e), "warn");
    return "miss";
  } finally {
    busy = false;
    $("btn-capture").disabled = false;
  }
}

async function watchLoop() {
  let misses = 0;
  let prevKey = null;
  while (watching) {
    const r = await analyzeOnce(true);
    misses = r === "hit" ? 0 : misses + 1;
    let idle = IDLE_FAST;
    if (misses >= 3) idle = IDLE_SLOW;
    else if (r === "hit" && shownKey && shownKey === prevKey) idle = IDLE_STABLE;
    prevKey = shownKey;
    await sleep(idle);
  }
}

$("btn-capture").addEventListener("click", () => analyzeOnce(false));
$("btn-reset").addEventListener("click", async () => {
  await invoke("reset_lock");
  shownKey = null;
  pending = { key: null, n: 0 };
  setStatus("已清除锁定，下次匹配全库重扫");
});
$("chk-watch").addEventListener("change", (ev) => {
  watching = ev.target.checked;
  if (watching) {
    setStatus("自动监测中——打开游戏内地图即自动识别");
    watchLoop();
  } else {
    setStatus("已停止自动监测");
  }
});
$("chk-overlay").addEventListener("change", async (ev) => {
  overlayMode = ev.target.checked;
  if (overlayMode) {
    setStatus("覆盖模式开——识别到地图后自动贴上去（点击穿透，不挡操作）");
    await pushOverlay(true);
  } else {
    await hideOverlay();
  }
});
$("chk-top").addEventListener("change", (ev) => appWindow.setAlwaysOnTop(ev.target.checked));
$("rng-alpha").addEventListener("input", () => {
  if (lastPayload) render(lastPayload);
  pushOverlay(false); // alpha 变化会改变指纹，自动重推
});
listen("overlay-ready", () => pushOverlay(true));

// 全局热键（焦点在游戏里也生效）：⇧⌥M 切覆盖层，⇧⌥R 重新识别
listen("hotkey", async (ev) => {
  if (ev.payload === "toggle_overlay") {
    const box = $("chk-overlay");
    box.checked = !box.checked;
    box.dispatchEvent(new Event("change"));
  } else if (ev.payload === "reset_lock") {
    $("btn-reset").click();
  }
});

// 按平台能力决定开工方式：Android 需先授权投屏，且暂无悬浮窗
(async () => {
  let caps = { screen_capture: true, overlay: true, needs_capture_permission: false };
  try { caps = await invoke("capabilities"); } catch { /* 旧版后端，按桌面处理 */ }

  if (caps.needs_capture_permission) {
    // Android：悬浮窗由系统权限管控，勾选时按需申请
    androidOverlay = caps.overlay;
    $("chk-top").disabled = true;
    $("chk-top").closest("label")?.style.setProperty("opacity", "0.4");
    $("chk-overlay").checked = false;
    overlayMode = false;
    $("hint").textContent = "悬浮窗需「显示在其他应用上层」权限，勾选时会跳转授权";
    $("chk-overlay").addEventListener("change", async (ev) => {
      if (!ev.target.checked) return;
      try {
        const ok = await invoke("request_overlay_permission");
        if (!ok) {
          ev.target.checked = false;
          overlayMode = false;
          setStatus("未获得悬浮窗权限", "warn");
        }
      } catch (e) {
        ev.target.checked = false;
        overlayMode = false;
        setStatus(String(e), "warn");
      }
    });
  }

  if (caps.needs_capture_permission) {
    // Android：先授权投屏才能取帧；授权在停止投屏后失效，故每次启动都要点
    const grant = $("btn-grant");
    grant.hidden = false;
    $("chk-watch").checked = false;
    setStatus("请先点「授权投屏」，然后切到游戏打开地图", "warn");
    grant.addEventListener("click", async () => {
      grant.disabled = true;
      setStatus("等待系统授权…");
      try {
        await invoke("request_capture");
        grant.textContent = "投屏已授权";
        $("chk-watch").checked = true;
        watching = true;
        setStatus("自动监测中——切到游戏打开地图即自动识别");
        watchLoop();
      } catch (e) {
        grant.disabled = false;
        setStatus(String(e), "warn");
      }
    });
    return;
  }

  overlayMode = $("chk-overlay").checked;
  if ($("chk-watch").checked) {
    watching = true;
    setStatus("自动监测中——打开游戏内地图即自动识别");
    watchLoop();
  }
})();
