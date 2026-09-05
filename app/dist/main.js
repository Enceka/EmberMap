// EmberMap 前端：调用后端匹配，canvas 合成叠加（截图 + 手绘 screen 混合 + 门位）。
// 自动监测为自适应连续循环：分析完歇一小会儿即下一轮，连续未检出则放缓省电；
// 歇多久由用户设的「监测间隔」按比例派生（见 idles()）。
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const $ = (id) => document.getElementById(id);
const statusEl = $("status");
const canvas = $("view");
const ctx = canvas.getContext("2d");

// 监测节奏：用户只调一个「基准间隔」，四档由它按比例派生。
// 真正吃 CPU 的是每轮的识别本身（桌面 1.3-2.2s、手机 1-2s），
// 间歇只决定两轮之间歇多久，所以间歇越短≠越快，只是越费电。
const IDLE_DEFAULT = 800;
let idleBase = Number(localStorage.getItem("em_idle") ?? IDLE_DEFAULT);
const idles = () => ({
  fast: Math.max(150, Math.round(idleBase * 0.4)), // 地图刚出现/结果在变
  stable: idleBase,                                 // 结果稳定
  slow: Math.round(idleBase * 2),                   // 连续没检出，进一步省电
  peek: Math.min(idleBase, 500),                    // 后端判定画面没动，这轮几乎不花钱
});
let lastAnalyzeMs = 0;   // 上一轮识别实际耗时，用于估算占用与保活时限

/// 叠加层保活时限：这么久没拿到「有效」的一轮就强制收起。
/// 兜底而非主路径——正常路径是 no_panel 立即收、连续两帧不置信收。
/// 必须随监测节奏走：用户把间隔调到 5 秒时，一轮就要 6 秒以上，
/// 写死 6 秒会把好端端的叠加层每轮误收一次。
const overlayTtl = () => Math.max(6000, Math.round((lastAnalyzeMs + idleBase) * 2.5));

let lastPayload = null;
let pending = { key: null, n: 0 };  // 低分新结果的 2 帧确认
let shownKey = null;
let watching = false;
let busy = false;
let overlayMode = false;
let overlayVisible = false;
let lowConfMiss = 0;       // 面板在但低置信的连续帧数
let lastPushed = null;     // 上次推给叠加层的指纹，避免重复推送
let androidOverlay = false; // Android 悬浮窗由 Kotlin 绘制，参数形状与桌面不同
let capturing = false;      // Android 投屏是否在进行（授权可被用户随时撤销）
let captureStopped = null;  // Android：投屏中止时的收尾（由平台分支注入）
let overlayGoodAt = 0;      // 上次确认叠加层仍然有效的时刻，供保活时限判定

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
  // tf 与门位用的是「显示区域局部的屏幕物理像素」，而截图可能被取帧端缩过
  // （Android 按长边 2000 封顶）。按 view 宽 / 截图宽还原，预览才与悬浮窗一致——
  // 这也让预览成为排查叠加错位的可信参照。
  const k = shot.width > 0 ? p.view[2] / shot.width : 1;
  renderGeom(p, shot.width);
  const w = Math.max(1, Math.round(shot.width * k));
  const h = Math.max(1, Math.round(shot.height * k));
  // 连续预览时保留用户的缩放/平移（内容尺寸没变就不重置视图）
  setScene(w, h, (g) => {
    g.drawImage(shot, 0, 0, w, h);
    g.save();
    g.setTransform(p.tf.scale, 0, 0, p.tf.scale, p.tf.tx, p.tf.ty);
    g.globalCompositeOperation = "screen";
    g.globalAlpha = alpha;
    g.drawImage(draw, 0, 0);
    g.restore();
    drawDoors(g, p.doors, h);
  }, { keepView: true });
}

/// 排障用：把实际抓到的整帧画进场景，用户截图即可看出抓到的是什么
async function drawFrame(b64) {
  const im = await loadImg(b64);
  setScene(im.width, im.height, (g) => {
    g.drawImage(im, 0, 0);
    g.fillStyle = "rgba(0,0,0,0.55)";
    g.fillRect(0, 0, im.width, 26);
    g.fillStyle = "#e3b341";
    g.font = "16px sans-serif";
    g.fillText("这是识别器实际抓到的画面", 8, 19);
  }, { keepView: true });
}

/// 叠加层几何对账行：view 是悬浮窗矩形（屏幕物理 px），tf 是手绘图→窗口局部的相似变换，
/// k 是取帧缩放还原系数。三者同一坐标系时叠加才可能对准，错位排查先看这行。
function renderGeom(p, shotW) {
  const t = p.tf;
  const k = shotW > 0 ? p.view[2] / shotW : 1;
  $("geom").textContent =
    `view=[${p.view.join(",")}] tf=${t.scale.toFixed(4)}@(${t.tx.toFixed(0)},${t.ty.toFixed(0)}) ` +
    `shot=${shotW} k=${k.toFixed(3)}`;
}

// ---------------------------------------------------------------------------
// 固定视口 + 内部缩放平移。
//
// #view 是一块固定大小的画框（不随内容变尺寸、不带动页面滚动）；
// 内容先画进离屏 scene（原生分辨率），再按 fit×zoom + 平移量投影进画框。
// 缩放：滚轮 / 双指捏合 / 按钮 / 双击，均以指针为锚；平移：框内拖动。
// 这样长图不再撑长页面，缩放也不再和页面滚动纠缠。
// ---------------------------------------------------------------------------
const ZOOM_MIN = 0.2;
const ZOOM_MAX = 8;
const scene = document.createElement("canvas");  // 离屏：合成后的内容，原生分辨率
const sctx = scene.getContext("2d");
let sceneReady = false;
let fitScale = 1;     // 让 scene 宽铺满画框的系数（zoom=1 时）
let zoom = 1;         // 用户在 fit 之上的缩放倍率
let panX = 0, panY = 0;  // scene 左上角在画框内的位置（CSS px）

/// 画框的 CSS 尺寸；顺便把 backing store 调到 CSS×dpr，高分屏不糊
function viewportSize() {
  const dpr = window.devicePixelRatio || 1;
  const cssW = canvas.clientWidth || 1;
  const cssH = canvas.clientHeight || 1;
  if (canvas.width !== Math.round(cssW * dpr) || canvas.height !== Math.round(cssH * dpr)) {
    canvas.width = Math.round(cssW * dpr);
    canvas.height = Math.round(cssH * dpr);
  }
  return { cssW, cssH, dpr };
}

/// 把 scene 投影进画框
function blit() {
  const { cssW, cssH, dpr } = viewportSize();
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, cssW, cssH);
  if (!sceneReady) return;
  const s = fitScale * zoom;
  ctx.imageSmoothingEnabled = true;
  ctx.imageSmoothingQuality = "high";
  ctx.drawImage(scene, panX, panY, scene.width * s, scene.height * s);
  $("zoom-val").textContent = `${Math.round(zoom * 100)}%`;
}

/// 至少留一部分内容在框内，拖不飞
function clampPan() {
  const { cssW, cssH } = viewportSize();
  const s = fitScale * zoom;
  const dw = scene.width * s, dh = scene.height * s;
  const margin = 40;
  panX = Math.min(cssW - margin, Math.max(margin - dw, panX));
  panY = Math.min(cssH - margin, Math.max(margin - dh, panY));
}

/// 回到「适应宽度、顶端对齐」的初始视图
function resetView() {
  const { cssW, cssH } = viewportSize();
  if (!sceneReady) { blit(); return; }
  fitScale = cssW / scene.width;
  zoom = 1;
  const dh = scene.height * fitScale;
  panX = 0;
  panY = dh < cssH ? (cssH - dh) / 2 : 0;  // 矮图居中，高图顶端对齐后可下拉
  blit();
}

/// 用离屏 scene 承接一帧内容。keepView：内容尺寸没变就保留当前缩放/平移
/// （连续识别预览用），否则回到适应视图（切楼层、切到排障帧用）。
function setScene(w, h, paint, opts = {}) {
  const sameSize = scene.width === w && scene.height === h;
  if (!sameSize) { scene.width = w; scene.height = h; }
  sctx.setTransform(1, 0, 0, 1, 0, 0);
  sctx.clearRect(0, 0, w, h);
  paint(sctx);
  sceneReady = true;
  if (opts.keepView && sameSize) blit();
  else resetView();
}

function zoomAt(cx, cy, factor) {
  const ns = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, zoom * factor));
  if (ns === zoom) return;
  const k = ns / zoom;
  // 锚点下的内容保持在指针处：panX' = cx − (cx − panX)·k
  panX = cx - (cx - panX) * k;
  panY = cy - (cy - panY) * k;
  zoom = ns;
  clampPan();
  blit();
}

function initZoomUi() {
  const rectXY = (e) => {
    const r = canvas.getBoundingClientRect();
    return [e.clientX - r.left, e.clientY - r.top];
  };
  const centerZoom = (f) => zoomAt(canvas.clientWidth / 2, canvas.clientHeight / 2, f);
  $("zoom-in").addEventListener("click", () => centerZoom(1.25));
  $("zoom-out").addEventListener("click", () => centerZoom(0.8));
  $("zoom-fit").addEventListener("click", resetView);

  // 画框自己拥有全部手势（touch-action: none），不再和页面滚动抢
  canvas.addEventListener("wheel", (ev) => {
    ev.preventDefault();
    const [cx, cy] = rectXY(ev);
    zoomAt(cx, cy, Math.exp(-ev.deltaY * 0.0025));
  }, { passive: false });

  // 鼠标拖动平移
  let drag = null;
  canvas.addEventListener("mousedown", (ev) => { drag = { x: ev.clientX, y: ev.clientY }; });
  window.addEventListener("mousemove", (ev) => {
    if (!drag) return;
    panX += ev.clientX - drag.x; panY += ev.clientY - drag.y;
    drag = { x: ev.clientX, y: ev.clientY };
    clampPan(); blit();
  });
  window.addEventListener("mouseup", () => { drag = null; });

  // 触屏：单指平移，双指捏合缩放
  let pinch = null, ptouch = null;
  const dist = (t) => Math.hypot(t[0].clientX - t[1].clientX, t[0].clientY - t[1].clientY);
  const mid = (t) => {
    const r = canvas.getBoundingClientRect();
    return [(t[0].clientX + t[1].clientX) / 2 - r.left, (t[0].clientY + t[1].clientY) / 2 - r.top];
  };
  canvas.addEventListener("touchstart", (ev) => {
    if (ev.touches.length === 2) { pinch = { d: dist(ev.touches), m: mid(ev.touches) }; ptouch = null; }
    else if (ev.touches.length === 1) { ptouch = { x: ev.touches[0].clientX, y: ev.touches[0].clientY }; }
  }, { passive: false });
  canvas.addEventListener("touchmove", (ev) => {
    ev.preventDefault();
    if (pinch && ev.touches.length === 2) {
      const d = dist(ev.touches), m = mid(ev.touches);
      zoomAt(m[0], m[1], d / pinch.d);
      pinch = { d, m };
    } else if (ptouch && ev.touches.length === 1) {
      panX += ev.touches[0].clientX - ptouch.x;
      panY += ev.touches[0].clientY - ptouch.y;
      ptouch = { x: ev.touches[0].clientX, y: ev.touches[0].clientY };
      clampPan(); blit();
    }
  }, { passive: false });
  const endTouch = (ev) => { if (ev.touches.length < 2) pinch = null; if (ev.touches.length === 0) ptouch = null; };
  canvas.addEventListener("touchend", endTouch);
  canvas.addEventListener("touchcancel", endTouch);

  // 双击：适应 ⇄ 放大到 250%，以双击点为锚
  canvas.addEventListener("dblclick", (ev) => {
    const [cx, cy] = rectXY(ev);
    if (zoom > 1.05) resetView();
    else zoomAt(cx, cy, 2.5 / zoom);
  });

  // 画框尺寸变了就重新适应。除了窗口缩放/旋转，展开收起「设置」抽屉也会改画框高度，
  // 那种变化不发 resize 事件，所以以 ResizeObserver 为准。
  const refit = () => { if (zoom <= 1.01) resetView(); else { clampPan(); blit(); } };
  window.addEventListener("resize", refit);
  if (window.ResizeObserver) new ResizeObserver(refit).observe(canvas);
}

/// 设置抽屉：不透明度、监测间隔、全局热键都是装一次就不再碰的，收起来给画布腾地方
function initSettingsUi() {
  const btn = $("btn-settings");
  const box = $("settings");
  btn.addEventListener("click", () => {
    const open = box.hidden;
    box.hidden = !open;
    btn.setAttribute("aria-expanded", String(open));
  });
}

// ---------------------------------------------------------------------------
// 锁定地图 + 看整层
//
// 两件互相独立的事，别混为一谈：
//   锁定（pin）  —— 收窄**匹配**范围，治「主界面被认成某张地图」；
//   看整层（view）—— 只管**显示**：把某一层的完整手绘图原样摊开，
//                    不需要跟游戏画面对齐，也不受游戏当前在哪层影响。
// 后者才是「地图放大了看不到别处」「想先看看另一层」的解法。
// ---------------------------------------------------------------------------
let pin = { variant: null, floor: null, name: null };
let viewFloor = null;   // 非空 = 正在看整层，画布不再被实时识别结果覆盖
let viewCache = {};     // 楼层图按 变体|楼层 缓存，切层不用重新过桥

/// 当前可供查看的变体：优先用户锁定的，其次最近一次识别到的
function viewVariant() {
  return pin.variant || lastPayload?.variant || null;
}

function renderPin() {
  $("sel-map").value = pin.variant || "";
  for (const b of document.querySelectorAll("#pinbar button.floor")) {
    b.classList.toggle("on", (b.dataset.floor || null) === viewFloor);
    b.disabled = !!b.dataset.floor && !viewVariant();
  }
  const btn = $("btn-pin");
  btn.textContent = pin.variant ? "解除锁定" : "锁定当前";
  btn.disabled = !pin.variant && !lastPayload;
  const bits = [];
  if (pin.variant) bits.push(`已锁 ${pin.name || pin.variant}`);
  if (viewFloor) bits.push(`正在看${floorCn(viewFloor)}整层`);
  $("pinstate").textContent = bits.join(" · ");
}

async function applyPin(next) {
  pin = await invoke("set_pin", next);
  // 锁定范围变了，上一帧的结果与叠加都不再代表当前设置
  lastPayload = null;
  lastPushed = null;
  shownKey = null;
  renderPin();
}

/// 把某一层的完整手绘图画到画布上（黑底原样显示，再标门位）。
/// 与叠加无关：这里不做任何对齐，就是让用户看清整层长什么样。
async function showFloor(floor) {
  viewFloor = floor;
  renderPin();
  if (!floor) {
    setStatus("已回到跟随游戏显示");
    return;
  }
  const variant = viewVariant();
  if (!variant) return;
  const key = `${variant}|${floor}`;
  // 39 张手绘图共 11 MB，全缓存在手机上太重；留最近几张够用了
  const keys = Object.keys(viewCache);
  if (keys.length > 4 && !viewCache[key]) delete viewCache[keys[0]];
  try {
    viewCache[key] ||= await invoke("floor_map", { variant, floor });
  } catch (e) {
    setStatus(String(e), "warn");
    return;
  }
  const m = viewCache[key];
  const im = await loadImg(m.draw_png);
  // 切了楼层是新内容，回到适应视图（不置 keepView）
  setScene(m.w, m.h, (g) => {
    g.drawImage(im, 0, 0);
    drawDoors(g, m.doors, m.h);
  });
  $("geom").textContent = `整层 ${m.name} · ${floorCn(m.floor)}　${m.w}×${m.h}`;
  setStatus(`看整层：${m.name} · ${floorCn(m.floor)}（可缩放拖动；不随游戏画面变化）`);
  renderCandidates([]);
}

async function initPinUi() {
  const sel = $("sel-map");
  let maps = [];
  try { maps = await invoke("list_maps"); } catch (e) { setStatus(String(e), "warn"); }
  sel.innerHTML =
    `<option value="">自动识别地图</option>` +
    maps.map((m) => `<option value="${m.variant}">锁定：${m.name}</option>`).join("");
  sel.addEventListener("change", async () => {
    const v = sel.value || null;
    await applyPin({
      variant: v,
      floor: null,
      name: v ? sel.selectedOptions[0].text.slice(3) : null,
    });
    // 换了地图，正在看的那一层要换成新地图的同一层
    if (viewFloor) showFloor(viewFloor);
  });
  for (const b of document.querySelectorAll("#pinbar button.floor")) {
    b.addEventListener("click", () => showFloor(b.dataset.floor || null));
  }
  // 主按钮：一键钉住当前识别结果，不用自己在 13 个名字里找
  $("btn-pin").addEventListener("click", () => {
    if (pin.variant) return applyPin({ variant: null, floor: null, name: null });
    if (!lastPayload) return;
    return applyPin({ variant: lastPayload.variant, floor: null, name: lastPayload.name });
  });
  try { pin = await invoke("get_pin"); } catch { /* 旧版后端 */ }
  renderPin();
}

/// 监测节奏与它的实测代价。
///
/// 不写死一句「可能影响性能」，而是拿本机真实测到的单轮耗时算给用户看：
/// 占用 = 识别耗时 /（识别耗时 + 间歇）——这一轮跑完立刻又开下一轮时，
/// 这个比例就是识别线程持续忙碌的时间占比。
function renderIdle() {
  // 每轮识别完都会刷新这里；正在拖滑块时别回写同一个值去打断拖拽
  if (Number($("rng-idle").value) !== idleBase) $("rng-idle").value = idleBase;
  $("idle-val").textContent = idleBase === 0 ? "不歇" : `${(idleBase / 1000).toFixed(1)}s`;
  if (!lastAnalyzeMs) {
    $("perf").textContent = "监测间隔＝两轮识别之间歇多久；识别本身的耗时不受它影响。";
    return;
  }
  const cycle = lastAnalyzeMs + idleBase;
  const duty = Math.round((lastAnalyzeMs / cycle) * 100);
  $("perf").innerHTML =
    `本机单轮识别 <b>${(lastAnalyzeMs / 1000).toFixed(1)}s</b>，` +
    `当前约每 <b>${(cycle / 1000).toFixed(1)}s</b> 测一次，` +
    `识别持续占用约 <b>${duty}%</b> 的时间（多核并行，占的是这段时间里的算力）。` +
    `<br>调短＝叠加层跟手但更费电，调长＝省电但地图开合、拖动会慢半拍；` +
    (androidOverlay
      ? "手机上画面没动的那些轮次会被廉价采样跳过，几乎不花钱，所以调长主要影响「画面在动时」的跟随。"
      : "识别本身的耗时不随间隔变化，间隔调到 0 也不会更快，只是不停地测。");
}

function initIdleUi() {
  renderIdle();
  $("rng-idle").addEventListener("input", () => {
    idleBase = Number($("rng-idle").value);
    localStorage.setItem("em_idle", String(idleBase));
    renderIdle();
  });
}

// ---------------------------------------------------------------------------
// Android 控制悬浮窗
//
// 手机上没有键盘，桌面那套全局热键在这里等价于悬浮窗上的按钮；
// 而且默认**不开自动监测**——手机上每轮识别 1-2 秒，一直转确实卡，
// 由用户按一下再识别更合适，代价是必须把进度显示清楚。
// ---------------------------------------------------------------------------
const BURST_MAX = 6;   // 一次「抓屏匹配」最多连拍几帧（够攒到锁定，又不至于卡太久）
let ctrl = { shown: false, mapOn: false, floor: null, busy: false, status: "" };
// 控制悬浮窗是否要显示由用户决定（记到 localStorage），不再授权投屏后强制弹出——
// 强制弹出既越权，也是「有时候崩溃」时用户唯一的直接接触面。
let controlWanted = localStorage.getItem("em_control") === "1";

/// 打开控制悬浮窗：先确保悬浮窗权限，再显示并启动按钮轮询
async function openControl() {
  try {
    const ok = await invoke("request_overlay_permission");
    if (!ok) throw new Error("未获得悬浮窗权限");
  } catch (e) {
    controlWanted = false;
    $("chk-control").checked = false;
    localStorage.setItem("em_control", "0");
    setStatus(String(e), "warn");
    return;
  }
  ctrl = { shown: true, mapOn: false, floor: null, busy: false,
           status: "切到游戏打开地图，按「抓屏匹配」" };
  await syncControl();
  controlLoop();
  setStatus("控制悬浮窗已显示——拖到屏幕边缘可收起成小柄，点小柄展开", "ok");
}

async function syncControl() {
  if (!ctrl.shown) return;
  const key = ctrl.mapOn && ctrl.floor ? `${viewVariant()}|${ctrl.floor}` : null;
  const m = key ? viewCache[key] : null;
  try {
    await invoke("control_update", {
      args: {
        show: true,
        status: ctrl.status,
        overlayOn: overlayMode,
        mapOn: ctrl.mapOn,
        busy: ctrl.busy,
        floor: ctrl.floor,
        // 传路径不传 base64：整层图有几百 KB，走 JSON 桥不划算，
        // 与叠加层同一套理由
        mapPath: m ? m.draw_path : null,
        mapDoors: m ? m.doors : [],
      },
    });
  } catch (e) {
    setStatus(String(e), "warn");
  }
}

/// 轮询控制悬浮窗上按下的按钮。
///
/// 只在控制窗显示时转，一轮就是个空 JSON 往返，与真正吃 CPU 的识别
/// （手机上 1-2 秒）比可以忽略；用户嫌卡的是后者，不是这个。
/// 之所以轮询而非推送：Kotlin 的 trigger 要求 JS 侧 addPluginListener，
/// 那条命令被 Tauri 的 ACL 拦下（应用内联插件没有权限清单可放行）。
const CTRL_POLL = 250;
let ctrlPolling = false;

/// 只要投屏还开着就转——不能只在控制条显示时转：
/// 通知栏的「开关叠加层」正是为「控制条被收起或被挡住」准备的兜底入口，
/// 控制条一关就停轮询的话，那个入口就成了摆设。
async function controlLoop() {
  if (ctrlPolling) return;
  ctrlPolling = true;
  try {
    while (capturing) {
      try {
        const r = await invoke("poll_control");
        ctrl.shown = !!r.controlShown;
        for (const a of r.actions || []) await onControlAction(a.action, a.value);
      } catch { /* 投屏停了之类，下一轮自然退出 */ }
      await sleep(CTRL_POLL);
    }
  } finally {
    ctrlPolling = false;
  }
}

async function hideControl() {
  ctrl.shown = false;
  try {
    await invoke("control_update", {
      args: { show: false, status: "", overlayOn: false, mapOn: false,
              busy: false, floor: null, mapPath: null, mapDoors: [] },
    });
  } catch { /* 已经没了就算了 */ }
}

/// 手动识别：连拍若干帧直到锁定。
/// 单帧往往攒不够证据（跟踪器要多帧印证），一次只测一帧会让用户以为没反应，
/// 所以按一下就连拍到锁定为止，每帧刷新进度。
async function burstCapture() {
  if (ctrl.busy) return;
  ctrl.busy = true;
  let ok = false;
  for (let i = 1; i <= BURST_MAX && !ok; i++) {
    ctrl.status = `识别中 ${i}/${BURST_MAX}…`;
    await syncControl();
    const r = await analyzeOnce(false);
    if (r === "hit") ok = true;
    else if (r === "miss") {
      ctrl.status = "画面里没找到地图，把游戏地图完整打开再试";
      break;
    }
  }
  if (ok && lastPayload) {
    const who = `${lastPayload.name} · ${floorCn(lastPayload.floor)}`;
    // 叠加层被关着时，只报「识别成功」会让人以为功能坏了——说清楚该点哪儿
    ctrl.status = overlayMode
      ? `${who}　置信 ${lastPayload.score.toFixed(2)}`
      : `${who}　已识别，但叠加层是关的——点「叠加层 关」打开`;
    // 认出来了才知道是哪张图，整层视图这时才有内容可显示
    if (ctrl.mapOn && !ctrl.floor) ctrl.floor = lastPayload.floor;
    if (ctrl.mapOn) await ensureFloorCached(ctrl.floor);
  } else if (ctrl.status.startsWith("识别中")) {
    ctrl.status = "证据不足，再按一次或把地图放大些";
  }
  ctrl.busy = false;
  await syncControl();
}

/// 整层图取回来放进缓存，控制窗从缓存里拿路径
async function ensureFloorCached(floor) {
  const variant = viewVariant();
  if (!variant || !floor) return false;
  const key = `${variant}|${floor}`;
  const keys = Object.keys(viewCache);
  if (keys.length > 4 && !viewCache[key]) delete viewCache[keys[0]];
  try {
    viewCache[key] ||= await invoke("floor_map", { variant, floor });
    return true;
  } catch (e) {
    ctrl.status = String(e);
    return false;
  }
}

async function onControlAction(action, value) {
  switch (action) {
    case "capture":
      await burstCapture();
      return;
    case "toggle_overlay": {
      // 走界面上那个复选框，保证应用内与悬浮窗、通知栏三处状态始终一致
      const box = $("chk-overlay");
      box.checked = !box.checked;
      box.dispatchEvent(new Event("change"));
      // change 处理是异步的，等一拍再回读，免得按钮态反着显示
      await sleep(0);
      ctrl.status = overlayMode ? "叠加层开" : "叠加层关";
      break;
    }
    case "toggle_map":
      ctrl.mapOn = !ctrl.mapOn;
      if (ctrl.mapOn) {
        if (!viewVariant()) {
          ctrl.mapOn = false;
          ctrl.status = "还没认出是哪张图，先按「抓屏匹配」";
        } else {
          ctrl.floor = ctrl.floor || lastPayload?.floor || "1f";
          if (!(await ensureFloorCached(ctrl.floor))) ctrl.mapOn = false;
        }
      }
      break;
    case "floor":
      ctrl.floor = value;
      if (!(await ensureFloorCached(value))) ctrl.mapOn = false;
      break;
    case "reset":
      await invoke("reset_lock");
      shownKey = null;
      ctrl.status = "已清除锁定，下次全库重扫";
      break;
    case "close":
      // ✕ 只收控制条本身；叠加层有自己的开关（应用内勾选框 / 通知栏），不连坐
      await hideControl();
      controlWanted = false;
      $("chk-control").checked = false;
      localStorage.setItem("em_control", "0");
      return;
  }
  await syncControl();
}

// ---------------------------------------------------------------------------
// 全局热键自定义（仅桌面）
//
// 后端用的加速键写法是「修饰键在前、键码在后」，键码名与浏览器
// KeyboardEvent.code 完全一致（KeyM / Digit1 / F5 / ArrowUp…），
// 所以这里可以把用户按下的组合直接拼成后端认得的字符串。
// ---------------------------------------------------------------------------
let hotkeys = null;
let capturing_hk = null;

/// "Shift+Alt+KeyM" → "⇧⌥M"，按钮上显示得下
const HK_SYM = { Shift: "⇧", Alt: "⌥", Control: "⌃", Super: "⌘" };
function hkLabel(s) {
  if (!s) return "…";
  const parts = s.split("+");
  const code = parts.pop();
  const key = code.replace(/^Key|^Digit/, "");
  return parts.map((m) => HK_SYM[m] || m + "+").join("") + key;
}

function renderHotkeys() {
  for (const b of document.querySelectorAll("#hotkeybar button.hk")) {
    b.querySelector("b").textContent = hkLabel(hotkeys?.[b.dataset.key]);
    b.classList.toggle("capturing", capturing_hk === b.dataset.key);
  }
  // 只在真的等按键时占那一行；平时的说明写在设置抽屉里（改键的人才看得到）
  $("hint").textContent = capturing_hk
    ? "请按下新的组合键（需含 ⇧⌃⌥⌘ 之一；Esc 取消）"
    : "";
}

async function commitHotkeys(next) {
  try {
    hotkeys = await invoke("set_hotkeys", next);
  } catch (e) {
    setStatus(String(e), "warn");
  }
  capturing_hk = null;
  renderHotkeys();
}

async function initHotkeyUi() {
  $("hotkeybar").hidden = false;
  try { hotkeys = await invoke("get_hotkeys"); } catch { return; }
  renderHotkeys();

  for (const b of document.querySelectorAll("#hotkeybar button.hk")) {
    b.addEventListener("click", () => {
      capturing_hk = capturing_hk === b.dataset.key ? null : b.dataset.key;
      renderHotkeys();
    });
  }
  $("hk-reset").addEventListener("click", () =>
    commitHotkeys({
      toggleOverlay: "Shift+Alt+KeyM",
      resetLock: "Shift+Alt+KeyR",
      captureNow: "Shift+Alt+KeyC",
    })
  );

  window.addEventListener("keydown", (ev) => {
    if (!capturing_hk) return;
    ev.preventDefault();
    if (ev.code === "Escape") {
      capturing_hk = null;
      return renderHotkeys();
    }
    // 只按住修饰键时先不作数，等真正的主键
    if (/^(Shift|Control|Alt|Meta)(Left|Right)$/.test(ev.code)) return;
    const mods = [];
    if (ev.ctrlKey) mods.push("Control");
    if (ev.altKey) mods.push("Alt");
    if (ev.shiftKey) mods.push("Shift");
    if (ev.metaKey) mods.push("Super");
    if (!mods.length) {
      // 不带修饰键的全局热键会把整个系统的这个按键抢走
      setStatus("全局热键必须带至少一个修饰键（⇧⌃⌥⌘）", "warn");
      return;
    }
    const combo = mods.concat(ev.code).join("+");
    const next = {
      toggleOverlay: hotkeys.toggle_overlay,
      resetLock: hotkeys.reset_lock,
      captureNow: hotkeys.capture_now,
    };
    next[{ toggle_overlay: "toggleOverlay", reset_lock: "resetLock", capture_now: "captureNow" }[capturing_hk]] = combo;
    commitHotkeys(next);
  });
}

/// 门位画进 scene（h = 场景高，用来定圆圈/字号）
function drawDoors(g, doors, h) {
  const r = Math.max(8, h * 0.014);
  g.font = `bold ${Math.max(15, h * 0.026)}px "PingFang SC", sans-serif`;
  for (const d of doors) {
    g.strokeStyle = "#ff5050";
    g.lineWidth = 4;
    g.beginPath();
    g.arc(d.x, d.y, r, 0, Math.PI * 2);
    g.stroke();
    g.fillStyle = "#ff7878";
    g.strokeStyle = "#000";
    g.lineWidth = 3;
    g.strokeText(d.label, d.x + r + 4, d.y + 5);
    g.fillText(d.label, d.x + r + 4, d.y + 5);
  }
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

// 指纹：结果/几何没变就不重推叠加层
function fingerprint(p) {
  const t = p.tf;
  return [p.name, p.floor, ...p.view, t.scale.toFixed(3),
          t.tx.toFixed(0), t.ty.toFixed(0), $("rng-alpha").value].join("|");
}

/// 把前端的关键判断打到后端 stderr，与后端日志汇成一条时间线。
/// 发行版里 WebView 的 console 看不见，缺这一段排障就只能靠猜——
/// 「识别成功却没显示地图」当初就卡在分不清是没推、推了没画、还是画在别处。
/// 同一句话只打一次，避免每帧刷屏。
let lastUiLog = "";
const uiLog = (m) => {
  if (m === lastUiLog) return;
  lastUiLog = m;
  try { invoke("log_line", { msg: m }); } catch { /* 旧后端 */ }
};

async function pushOverlay(force) {
  if (!overlayMode || !lastPayload) {
    uiLog(`叠加层未推送：开关=${overlayMode ? "开" : "关"} 有结果=${!!lastPayload}`);
    return;
  }
  const fp = fingerprint(lastPayload);
  // 走到这里就说明本轮拿到了可信结果，保活时限要续上——哪怕几何没变、
  // 下面因去重直接返回也一样，否则稳态叠加反倒会被兜底逻辑每 5 秒收一次
  overlayGoodAt = Date.now();
  if (!force && overlayVisible && fp === lastPushed) return;
  try {
    await invoke("overlay_update", overlayArgs(lastPayload));
    overlayVisible = true;
    overlayGoodAt = Date.now();
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

/// 返回 'hit' | 'miss' | 'skip'（供自适应循环决定节奏）
async function analyzeOnce(auto) {
  if (busy) return "miss";
  busy = true;
  $("btn-capture").disabled = true;
  if (!auto) setStatus("抓屏匹配中…");
  try {
    // 手动点按钮时强制真抓一次：跳帧优化不该让按钮看起来像坏了
    const t0 = performance.now();
    const p = await invoke("analyze_screen", { force: !auto });
    // 只统计真跑了识别的轮次；skip 轮几乎不花钱，计进去会低估占用
    if (p.status !== "skip") {
      lastAnalyzeMs = Math.round(performance.now() - t0);
      renderIdle();
    }
    // 本轮不出结果，不移动叠加层。hold=true 表示画面逐像素没变，
    // 那上一帧的叠加仍然成立，可以续上保活时限；hold=false 是「没能验证」，
    // 不能拿来续命，否则叠加层会永远赖在屏幕上（关掉地图也不消失）。
    if (p.status === "skip") {
      if (p.hold) overlayGoodAt = Date.now();
      else if (!viewFloor) setStatus(p.reason, "warn"); // 说清楚为什么这一轮没结果
      return "skip";
    }
    if (p.status === "no_panel") {
      // 面板整体消失 = 地图关了，立即隐藏
      pending = { key: null, n: 0 };
      lowConfMiss = 0;
      await hideOverlay();
      // 看整层时用户是在读地图，识别的动静不该抢走画布和状态栏
      if (viewFloor) return "miss";
      // 自动模式下也要更新状态：否则用户只看到停留不动的旧文字，
      // 无法判断它到底在不在工作（真机排障时踩过这个坑）
      setStatus(`未识别到地图（抓到 ${p.frame_w}×${p.frame_h}）：${p.reason}`, "warn");
      renderCandidates([]);
      $("geom").textContent = "";
      if (p.frame_png) await drawFrame(p.frame_png);
      return "miss";
    }
    const key = `${p.name}|${p.floor}`;
    if (!p.confident) {
      // 证据不足（后端仍在多帧投票）：面板在就继续攒证据，不显示叠加
      lowConfMiss += 1;
      if (lowConfMiss >= 2) await hideOverlay();
      if (viewFloor) return "hit";
      const pct = Math.min(99, Math.round((p.evidence / p.evidence_need) * 100));
      setStatus(
        `识别中 ${pct}%（当前最像 ${p.name}·${floorCn(p.floor)} ${p.score.toFixed(2)}）`,
        "warn"
      );
      renderCandidates(p.candidates);
      // weak：面板在但证据不够。对循环而言与命中同义（保持快节奏继续攒证据），
      // 但手动突发识别要靠它区分「还得再来一帧」和「可以收手了」
      return "weak";
    }
    lowConfMiss = 0;
    shownKey = key;
    const variantChanged = lastPayload?.variant !== p.variant;
    lastPayload = p;
    // 只在变体变化时刷新锁定条：每轮都改写下拉框会打断用户正在进行的选择
    if (variantChanged) renderPin();
    // 看整层时只推叠加层（悬浮窗照旧跟着游戏走），画布与状态栏归整层视图
    if (!viewFloor) {
      const tag = { tracking: "跟踪", pinned: "手动锁定" }[p.phase] || "已锁定";
      // 叠加层默认是关的，只报「识别成功」会让人以为该出现的没出现
      setStatus(
        `${p.name} · ${floorCn(p.floor)}　置信 ${p.score.toFixed(2)}　` +
        `领先次佳 ${p.advantage.toFixed(3)}　${tag}` +
        (overlayMode ? "" : "（叠加层未开，勾选「叠加层」贴到游戏上）"),
        "ok"
      );
      renderCandidates(p.candidates);
      await render(p);
    }
    uiLog(`命中 ${p.name} ${p.floor}　叠加层开关=${overlayMode ? "开" : "关"}`);
    await pushOverlay(false);
    return "hit";
  } catch (e) {
    const msg = String(e);
    // 投屏被用户或系统停掉：立刻收尾，别让循环空转刷错误
    if (captureStopped && /尚未授权投屏|投屏/.test(msg)) {
      await captureStopped("投屏已停止，点「授权投屏」重新开始");
    } else {
      setStatus(msg, "warn");
    }
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
    // skip 既不算命中也不算落空：画面没动，上一轮的判断依然成立
    if (r !== "skip") misses = r === "hit" || r === "weak" ? 0 : misses + 1;
    const iv = idles();
    let idle = iv.fast;
    if (r === "skip") idle = iv.peek;
    else if (misses >= 3) idle = iv.slow;
    else if (r === "hit" && shownKey && shownKey === prevKey) idle = iv.stable;
    if (r !== "skip") prevKey = shownKey;
    // 保活兜底：太久没确认过叠加层仍然有效就收起来。
    // 上一版把「无法验证」的帧也当成保持，结果地图关了叠加层还赖在屏幕上。
    if (overlayVisible && Date.now() - overlayGoodAt > overlayTtl()) {
      console.warn("[em] 叠加层超过保活时限未获确认，收起");
      await hideOverlay();
    }
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
$("chk-watch").addEventListener("change", async (ev) => {
  watching = ev.target.checked;
  if (watching) {
    setStatus("自动监测中——打开游戏内地图即自动识别");
    watchLoop();
  } else {
    // 停了监测就没人再更新叠加层了，留着只会是一张停在错位置的旧图
    await hideOverlay();
    setStatus("已停止自动监测");
  }
});
$("chk-overlay").addEventListener("change", async (ev) => {
  overlayMode = ev.target.checked;
  if (overlayMode) {
    setStatus("叠加层开——识别到地图后自动贴上去（点击穿透，不挡操作）");
    await pushOverlay(true);
  } else {
    await hideOverlay();
  }
  localStorage.setItem("em_overlay", overlayMode ? "1" : "0");
  await syncControl(); // 控制条上的「叠加层」按钮态跟着走
});
$("chk-top").addEventListener("change", (ev) => appWindow.setAlwaysOnTop(ev.target.checked));
$("rng-alpha").addEventListener("input", () => {
  $("alpha-val").textContent = `${$("rng-alpha").value}%`;
  if (lastPayload && !viewFloor) render(lastPayload);
  pushOverlay(false); // 不透明度变化会改变指纹，自动重推
});
listen("overlay-ready", () => pushOverlay(true));

// 全局热键（焦点在游戏里也生效）：默认 ⇧⌥C 抓屏匹配、⇧⌥M 开关叠加层、⇧⌥R 重新识别
listen("hotkey", async (ev) => {
  if (ev.payload === "toggle_overlay") {
    const box = $("chk-overlay");
    box.checked = !box.checked;
    box.dispatchEvent(new Event("change"));
  } else if (ev.payload === "reset_lock") {
    $("btn-reset").click();
  } else if (ev.payload === "capture_now") {
    // 与点按钮同一条路径：force=true，绕开跳帧直接真抓一次
    analyzeOnce(false);
  }
});

// 按平台能力决定开工方式：Android 需先授权投屏，且暂无悬浮窗
(async () => {
  let caps = { screen_capture: true, overlay: true, needs_capture_permission: false };
  try { caps = await invoke("capabilities"); } catch { /* 旧版后端，按桌面处理 */ }
  // 提前定平台：renderIdle 的说明文案要按平台分岔
  androidOverlay = !!caps.needs_capture_permission && !!caps.overlay;
  // 测试构建的版本号带时间戳，回显出来才知道手里是哪一次编的
  if (caps.version) $("ver").textContent = `EmberMap ${caps.version}`;

  await initPinUi();
  initIdleUi();
  initZoomUi();
  initSettingsUi();
  if (caps.hotkeys) await initHotkeyUi();
  else $("hint").textContent = "悬浮窗需「显示在其他应用上层」权限，勾选时会跳转授权";

  if (caps.needs_capture_permission) {
    // Android：悬浮窗由系统权限管控，勾选时按需申请
    $("chk-top").disabled = true;
    $("chk-top").closest("label")?.style.setProperty("opacity", "0.4");
    // 控制悬浮窗开关：默认不显示，用户随时可在应用内收放
    controlWanted = localStorage.getItem("em_control") === "1";
    $("chk-control").checked = controlWanted;
    $("lbl-control").hidden = false;
    $("chk-control").addEventListener("change", async (ev) => {
      controlWanted = ev.target.checked;
      localStorage.setItem("em_control", controlWanted ? "1" : "0");
      if (!controlWanted) {
        await hideControl();
        setStatus("控制悬浮窗已收起；通知栏的「开关叠加层」仍可用");
        return;
      }
      if (!capturing) {
        // 控制条上的按钮全部依赖投屏画面；没有投屏，开了也只是个空壳
        ev.target.checked = false;
        controlWanted = false;
        localStorage.setItem("em_control", "0");
        setStatus("控制悬浮窗要先有投屏画面：请先点「授权投屏」", "warn");
        return;
      }
      await openControl();
    });
    overlayMode = localStorage.getItem("em_overlay") === "1";
    $("chk-overlay").checked = overlayMode;
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
    // Android：先授权投屏才能取帧；按钮兼作开关，随时可停
    const grant = $("btn-grant");
    grant.hidden = false;
    // 默认不开自动监测：手机上每轮识别 1-2 秒，一直转确实卡，
    // 改由悬浮窗上的「抓屏匹配」按需触发
    $("chk-watch").checked = false;
    watching = false;
    setStatus("请先点「授权投屏」，然后切到游戏打开地图", "warn");


    captureStopped = async (reason) => {
      watching = false;
      await hideOverlay();
      await hideControl();
      $("chk-watch").checked = false;
      grant.textContent = "授权投屏";
      grant.disabled = false;
      capturing = false;
      setStatus(reason, "warn");
    };

    grant.addEventListener("click", async () => {
      grant.disabled = true;
      if (capturing) {
        try { await invoke("stop_capture"); } catch { /* 已停就算了 */ }
        await captureStopped("投屏已停止");
        return;
      }
      setStatus("等待系统授权…");
      try {
        await invoke("request_capture");
        capturing = true;
        grant.textContent = "停止投屏";
        grant.disabled = false;
        // 控制悬浮窗不再自动弹出：用户之前勾了「控制悬浮窗」才恢复显示，
        // 否则留在应用内操作（抓屏匹配按钮一直在）
        if (controlWanted) await openControl();
        else setStatus("投屏已就绪。控制悬浮窗默认不显示，需要时勾选「控制悬浮窗」", "ok");
      } catch (e) {
        grant.disabled = false;
        setStatus(String(e), "warn");
      }
    });

    // 用户也可能从通知栏或系统的投屏提示里停掉，回到应用时要同步过来
    document.addEventListener("visibilitychange", async () => {
      if (document.visibilityState !== "visible" || !capturing) return;
      try {
        if (!(await invoke("capture_active"))) {
          await captureStopped("投屏已被停止，需重新授权");
        }
      } catch { /* 忽略 */ }
    });
    return;
  }

  // 叠加层默认关：它会盖在游戏画面上，开不开由用户定。
  // 记住用户上次的选择，免得每次启动都要重勾。
  overlayMode = localStorage.getItem("em_overlay") === "1";
  $("chk-overlay").checked = overlayMode;
  if ($("chk-watch").checked) {
    watching = true;
    setStatus("自动监测中——打开游戏内地图即自动识别");
    watchLoop();
  }
})();
