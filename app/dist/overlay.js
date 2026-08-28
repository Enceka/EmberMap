// 覆盖窗口渲染：只画手绘层（黑底按亮度转 alpha）+ 门位，背景全透明。
const { listen, emit } = window.__TAURI__.event;

const canvas = document.getElementById("ov");
const ctx = canvas.getContext("2d");

function loadImg(b64) {
  return new Promise((res, rej) => {
    const im = new Image();
    im.onload = () => res(im);
    im.onerror = rej;
    im.src = "data:image/png;base64," + b64;
  });
}

async function render(p) {
  const im = await loadImg(p.draw_png);
  canvas.width = p.panel_w;   // 物理像素，与匹配变换同坐标系
  canvas.height = p.panel_h;
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  // 变换绘制手绘图，然后按亮度生成 alpha（黑底 → 透明）
  ctx.setTransform(p.tf.scale, 0, 0, p.tf.scale, p.tf.tx, p.tf.ty);
  ctx.drawImage(im, 0, 0);
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  const id = ctx.getImageData(0, 0, canvas.width, canvas.height);
  const d = id.data;
  const alpha = p.alpha ?? 0.45;
  for (let i = 0; i < d.length; i += 4) {
    const luma = 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
    d[i + 3] = Math.min(255, (luma * 255) / 70) * alpha;
  }
  ctx.putImageData(id, 0, 0);
  // 门位（不透明红圈+描边文字）
  const r = Math.max(8, canvas.height * 0.014);
  ctx.font = `bold ${Math.max(15, canvas.height * 0.026)}px "PingFang SC", sans-serif`;
  for (const door of p.doors) {
    ctx.strokeStyle = "#ff5050";
    ctx.lineWidth = 4;
    ctx.beginPath();
    ctx.arc(door.x, door.y, r, 0, Math.PI * 2);
    ctx.stroke();
    ctx.fillStyle = "#ff7878";
    ctx.strokeStyle = "#000";
    ctx.lineWidth = 3;
    ctx.strokeText(door.label, door.x + r + 4, door.y + 5);
    ctx.fillText(door.label, door.x + r + 4, door.y + 5);
  }
}

listen("overlay-data", (ev) => render(ev.payload));
// 首次加载时主窗口可能已 emit 过，通知它重发一帧
emit("overlay-ready", {});
