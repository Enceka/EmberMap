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

function renderCandidates(list) {
  $("candidates").innerHTML = list
    .map((c, i) => `<li class="${i === 0 ? "best" : ""}">${i + 1}. ${c.name} · ${floorCn(c.floor)}　${c.score.toFixed(3)}</li>`)
    .join("");
}

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

// 指纹：结果/几何没变就不重推覆盖层
function fingerprint(p) {
  const t = p.tf;
  return [p.name, p.floor, ...p.panel, t.scale.toFixed(3),
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
      if (!auto) setStatus(p.reason, "warn");
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

// 启动即按勾选状态开工（默认自动监测 + 覆盖模式）
overlayMode = $("chk-overlay").checked;
if ($("chk-watch").checked) {
  watching = true;
  setStatus("自动监测中——打开游戏内地图即自动识别");
  watchLoop();
}
