// EmberMap 前端：调用后端匹配，canvas 合成叠加（截图 + 手绘 screen 混合 + 门位）。
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const $ = (id) => document.getElementById(id);
const statusEl = $("status");
const canvas = $("view");
const ctx = canvas.getContext("2d");

let lastPayload = null;      // 最近一次成功匹配（用于透明度滑杆重绘）
let pending = { key: null, n: 0 };  // 自动模式持久性过滤：连续 2 帧同结果才切换
let shownKey = null;
let watchTimer = null;
let busy = false;
let overlayMode = false;
let missCount = 0;           // 连续未检出计数，≥2 隐藏覆盖窗

function overlayArgs(p) {
  return {
    payload: {
      draw_png: p.draw_png,
      tf: p.tf,
      doors: p.doors,
      panel_w: p.panel[2],
      panel_h: p.panel[3],
      alpha: $("rng-alpha").value / 100,
    },
    x: p.panel[0],
    y: p.panel[1],
    w: p.panel[2],
    h: p.panel[3],
  };
}

async function pushOverlay() {
  if (overlayMode && lastPayload) {
    try { await invoke("overlay_update", overlayArgs(lastPayload)); } catch (e) { setStatus(String(e), "warn"); }
  }
}

async function missOverlay() {
  missCount += 1;
  if (overlayMode && missCount >= 2) await invoke("overlay_hide");
}

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
  // 手绘图黑底：screen 混合 ≈ 黑色透明，结构/文字/路线浮现
  ctx.save();
  ctx.setTransform(p.tf.scale, 0, 0, p.tf.scale, p.tf.tx, p.tf.ty);
  ctx.globalCompositeOperation = "screen";
  ctx.globalAlpha = alpha;
  ctx.drawImage(draw, 0, 0);
  ctx.restore();
  // 门位
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

function renderCandidates(list) {
  $("candidates").innerHTML = list
    .map((c, i) => `<li class="${i === 0 ? "best" : ""}">${i + 1}. ${c.name} · ${floorCn(c.floor)}　${c.score.toFixed(3)}</li>`)
    .join("");
}

const floorCn = (f) => ({ "1f": "一楼", "2f": "二楼", b1: "地下室" }[f] || f);

async function analyzeOnce(auto) {
  if (busy) return;
  busy = true;
  $("btn-capture").disabled = true;
  if (!auto) setStatus("抓屏匹配中…");
  try {
    const p = await invoke("analyze_screen");
    if (p.status === "no_panel") {
      if (!auto) setStatus(p.reason, "warn");
      pending = { key: null, n: 0 };
      await missOverlay();
      return;
    }
    const key = `${p.name}|${p.floor}`;
    if (!p.confident) {
      if (!auto) setStatus(`低置信（${p.score.toFixed(2)}），结果仅供参考：${p.name}·${floorCn(p.floor)}`, "warn");
      await missOverlay();
      return;
    }
    // 自动模式：连续 2 帧同结果才切换显示，滤掉单帧漏网误检
    if (auto && key !== shownKey) {
      if (pending.key === key) pending.n += 1;
      else pending = { key, n: 1 };
      if (pending.n < 2) return;
    }
    shownKey = key;
    lastPayload = p;
    missCount = 0;
    setStatus(`${p.name} · ${floorCn(p.floor)}　置信 ${p.score.toFixed(2)}`, "ok");
    renderCandidates(p.candidates);
    await render(p);
    await pushOverlay();
  } catch (e) {
    setStatus(String(e), "warn");
  } finally {
    busy = false;
    $("btn-capture").disabled = false;
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
  if (ev.target.checked) {
    setStatus("自动监测中——打开游戏内地图即自动识别");
    watchTimer = setInterval(() => analyzeOnce(true), 3000);
  } else {
    clearInterval(watchTimer);
    setStatus("已停止自动监测");
  }
});
$("chk-overlay").addEventListener("change", async (ev) => {
  overlayMode = ev.target.checked;
  if (overlayMode) {
    setStatus("覆盖模式开——识别到地图后自动贴上去（点击穿透，不挡操作）");
    await pushOverlay();
  } else {
    await invoke("overlay_hide");
  }
});
$("chk-top").addEventListener("change", (ev) => appWindow.setAlwaysOnTop(ev.target.checked));
$("rng-alpha").addEventListener("input", () => {
  if (lastPayload) render(lastPayload);
  pushOverlay();
});
// 覆盖窗首次加载完成后补发一帧
listen("overlay-ready", () => pushOverlay());
