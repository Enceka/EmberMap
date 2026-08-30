package net.yeah.enceka.embermap

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.provider.Settings
import android.view.View
import android.view.WindowManager
import android.os.Looper
import android.util.Log
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File

@InvokeArg
class EmptyArgs

@InvokeArg
class DoorArg {
    var label: String = ""
    var x: Double = 0.0
    var y: Double = 0.0
}

@InvokeArg
class ControlArgs {
    /** 是否显示控制悬浮窗 */
    var show: Boolean = true
    /** 状态行文字（识别进度等） */
    var status: String = ""
    var overlayOn: Boolean = false
    var mapOn: Boolean = false
    var busy: Boolean = false
    /** 正在看的楼层（b1/1f/2f），null 表示没在看整层 */
    var floor: String? = null
    /** 整层手绘图路径与门位（已换算到手绘图自身坐标） */
    var mapPath: String? = null
    var mapDoors: List<DoorArg> = emptyList()
}

@InvokeArg
class OverlayArgs {
    /** 手绘楼层图的绝对路径（数据包已解压到 dataDir，Kotlin 可直接读） */
    var drawPath: String = ""
    /** 手绘图 → 显示区域局部坐标的相似变换 */
    var scale: Double = 1.0
    var tx: Double = 0.0
    var ty: Double = 0.0
    /** 显示区域在屏幕上的位置与大小（物理像素） */
    var x: Int = 0
    var y: Int = 0
    var w: Int = 0
    var h: Int = 0
    var alpha: Double = 0.45
    var doors: List<DoorArg> = emptyList()
}

/**
 * Android 取帧插件：MediaProjection 投屏 → ImageReader 取最新一帧 →
 * 缩放并存为 JPEG，把文件路径交给 Rust 侧读取。
 *
 * 为什么走文件而不是直接回传像素：Tauri 移动端插件桥是 JSON 通道，
 * 1080p 原始 RGB 约 6 MB，base64 后更大，每帧走 JSON 代价过高；
 * 存成 JPEG（约 200-400 KB）再由 Rust 用 image 解码更划算，
 * 且识别本来就对 JPEG 压缩不敏感（参考库素材本身就是 JPEG）。
 */
@TauriPlugin
class CapturePlugin(private val activity: Activity) : Plugin(activity) {

    companion object {
        /** 画面变化采样的网格边长 */
        private const val SIG_N = 48
        /** 单个采样点灰度变化超过多少才算「动了」——地图界面透出的 3D 场景一直在微动 */
        private const val SAMPLE_DELTA = 12
        /** 变化点占比超过此值才判为画面有实质变化 */
        private const val CHANGE_RATIO = 0.02
        /** 连续判「没变」这么多次就强制重算一次，防阈值偏钝导致叠加层永远跟不动 */
        private const val FORCE_REFRESH_EVERY = 12
    }

    /** 上次画面采样指纹（含悬浮窗），仅由 peekOnWorker 写 */
    @Volatile private var lastSignature: ByteArray? = null
    private var skipStreak = 0

    /**
     * 控制悬浮窗上按下的按钮，等前端来取。
     *
     * 本来想用 Plugin.trigger + JS 侧 addPluginListener 推送，实测被 Tauri 的 ACL 拦下：
     * 「Command plugin:emcapture|registerListener not allowed by ACL」——
     * 应用内联的插件没有权限清单，没法在 capabilities 里放行。
     * 改成前端来轮询：只在控制窗显示时轮，且一次就是个空 JSON 往返，
     * 与真正吃 CPU 的识别（1-2 秒）比可以忽略。
     */
    private val pendingActions = java.util.concurrent.ConcurrentLinkedQueue<Pair<String, String?>>()

    @Command
    fun pollControl(invoke: Invoke) {
        val arr = app.tauri.plugin.JSArray()
        // 通知栏按下的动作与控制条按钮走同一条路径，前端不必分辨来源
        while (true) {
            val a = CaptureService.pendingUiActions.poll() ?: break
            arr.put(JSObject().apply { put("action", a) })
        }
        while (true) {
            val (action, value) = pendingActions.poll() ?: break
            val o = JSObject()
            o.put("action", action)
            if (value != null) o.put("value", value)
            arr.put(o)
        }
        val ret = JSObject()
        ret.put("actions", arr)
        ret.put("controlShown", controlView != null)
        invoke.resolve(ret)
    }

    /** 是否已获得投屏授权（授权在停止投屏后失效，需重新申请） */
    @Command
    fun isCapturing(invoke: Invoke) {
        val ret = JSObject()
        ret.put("capturing", CaptureService.instance != null)
        invoke.resolve(ret)
    }

    /** 拉起系统投屏授权弹窗；用户同意后由前台服务建立取帧管线 */
    @Command
    fun requestCapture(invoke: Invoke) {
        if (CaptureService.instance != null) {
            val ret = JSObject()
            ret.put("granted", true)
            invoke.resolve(ret)
            return
        }
        val mgr = activity.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        startActivityForResult(invoke, mgr.createScreenCaptureIntent(), "onProjectionResult")
    }

    @app.tauri.annotation.ActivityCallback
    fun onProjectionResult(invoke: Invoke, result: androidx.activity.result.ActivityResult) {
        val ret = JSObject()
        if (result.resultCode != Activity.RESULT_OK || result.data == null) {
            ret.put("granted", false)
            ret.put("reason", "用户拒绝了投屏授权")
            invoke.resolve(ret)
            return
        }
        // 交给前台服务去创建投屏：必须先 startForeground 才能 getMediaProjection，
        // 而插件在主线程无法等待服务启动（会把服务自己饿死），故由服务顺序执行。
        CaptureService.lastError = null
        val svc = Intent(activity, CaptureService::class.java)
            .putExtra(CaptureService.EXTRA_CODE, result.resultCode)
            .putExtra(CaptureService.EXTRA_DATA, result.data)
        activity.startForegroundService(svc)

        // 非阻塞轮询等待管线就绪（主线程 Handler，不阻塞服务启动）
        val handler = Handler(Looper.getMainLooper())
        val deadline = System.currentTimeMillis() + 5000
        val poll = object : Runnable {
            override fun run() {
                when {
                    CaptureService.instance != null -> {
                        ret.put("granted", true)
                        invoke.resolve(ret)
                    }
                    CaptureService.lastError != null -> {
                        ret.put("granted", false)
                        ret.put("reason", CaptureService.lastError)
                        invoke.resolve(ret)
                    }
                    System.currentTimeMillis() > deadline -> {
                        ret.put("granted", false)
                        ret.put("reason", "投屏管线启动超时")
                        invoke.resolve(ret)
                    }
                    else -> handler.postDelayed(this, 100)
                }
            }
        }
        handler.postDelayed(poll, 100)
    }

    /** 停止投屏并释放资源；授权随之失效 */
    @Command
    fun stopCapture(invoke: Invoke) {
        activity.stopService(Intent(activity, CaptureService::class.java))
        invoke.resolve(JSObject())
    }

    /**
     * 取最新一帧存为 JPEG，返回文件路径与缩放系数。
     * 缩放上限保护：手机分辨率高，过大帧会拖慢传输与解码。
     */
    @Command
    fun grabFrame(invoke: Invoke) {
        val ir = CaptureService.instance?.imageReader()
        if (ir == null) {
            invoke.reject("尚未授权投屏")
            return
        }
        // 放到后台线程：下面要等待合成器出新帧，阻塞主线程会让画面根本不更新
        Thread { grabOnWorker(ir, invoke) }.start()
    }

    /**
     * 画面相对上次采样有没有实质变化。
     *
     * 取一帧干净画面必须先把悬浮窗藏起来，藏的这一瞬间用户就看得见闪烁；
     * 而地图开着不动时（正是用户在看叠加层的时刻）根本无需重算。
     * 于是先用「不隐藏悬浮窗」的廉价采样探一下：没变就整轮跳过，稳态零闪烁。
     *
     * 采样直接读 ImageReader 的 ByteBuffer，48×48 个点，不建 Bitmap 不解码。
     */
    @Command
    fun peekChanged(invoke: Invoke) {
        val ir = CaptureService.instance?.imageReader()
        if (ir == null) {
            invoke.reject("尚未授权投屏")
            return
        }
        Thread { peekOnWorker(ir, invoke) }.start()
    }

    private fun peekOnWorker(ir: ImageReader, invoke: Invoke) {
        var img: android.media.Image? = null
        try {
            val ret = JSObject()
            img = ir.acquireLatestImage()
            if (img == null) {
                // 合成器没产出新帧 = 屏幕一个像素都没动
                ret.put("changed", false)
                ret.put("ratio", 0.0)
                invoke.resolve(ret)
                return
            }
            val sig = signature(img)
            val prev = lastSignature
            lastSignature = sig
            val ratio = if (prev == null) 1.0 else changedRatio(prev, sig)
            // 兜底：阈值万一在某台机器上偏钝，别让叠加层从此跟不动；
            // 隔一阵强制重算一次，代价是几十秒才闪一下
            val forced = skipStreak >= FORCE_REFRESH_EVERY
            val changed = prev == null || ratio > CHANGE_RATIO || forced
            skipStreak = if (changed) 0 else skipStreak + 1
            ret.put("changed", changed)
            ret.put("ratio", ratio)
            invoke.resolve(ret)
        } catch (e: Exception) {
            Log.w("EmberMap", "画面采样失败，按「有变化」处理", e)
            val ret = JSObject()
            ret.put("changed", true)
            ret.put("ratio", 1.0)
            invoke.resolve(ret)
        } finally {
            try { img?.close() } catch (_: Exception) {}
        }
    }

    /** 48×48 灰度指纹，直接按 stride 抽样，不建 Bitmap */
    private fun signature(image: android.media.Image): ByteArray {
        val plane = image.planes[0]
        val buf = plane.buffer
        val rowStride = plane.rowStride
        val pixelStride = plane.pixelStride
        val out = ByteArray(SIG_N * SIG_N)
        for (gy in 0 until SIG_N) {
            val y = gy * image.height / SIG_N
            for (gx in 0 until SIG_N) {
                val x = gx * image.width / SIG_N
                val i = y * rowStride + x * pixelStride
                val r = buf.get(i).toInt() and 0xFF
                val g = buf.get(i + 1).toInt() and 0xFF
                val b = buf.get(i + 2).toInt() and 0xFF
                out[gy * SIG_N + gx] = ((r * 3 + g * 6 + b) / 10).toByte()
            }
        }
        return out
    }

    /** 两个指纹里「明显变了」的采样点占比 */
    private fun changedRatio(a: ByteArray, b: ByteArray): Double {
        var n = 0
        for (i in a.indices) {
            val d = kotlin.math.abs((a[i].toInt() and 0xFF) - (b[i].toInt() and 0xFF))
            if (d > SAMPLE_DELTA) n++
        }
        return n.toDouble() / a.size
    }

    /** 内容矩形之外的边框是否近乎全黑（判定「留黑边」而非「拉伸填满」） */
    private fun bandIsBlack(bmp: Bitmap, cx: Int, cy: Int, cw: Int, ch: Int): Boolean {
        var dark = 0
        var total = 0
        val step = 16
        var y = 0
        while (y < bmp.height) {
            var x = 0
            while (x < bmp.width) {
                if (x < cx || y < cy || x >= cx + cw || y >= cy + ch) {
                    val c = bmp.getPixel(x, y)
                    val luma = ((c shr 16 and 0xFF) * 3 + (c shr 8 and 0xFF) * 6 + (c and 0xFF)) / 10
                    if (luma < 12) dark++
                    total++
                }
                x += step
            }
            y += step
        }
        return total > 0 && dark * 100 / total >= 95
    }

    /**
     * 当前屏幕真实尺寸（含旋转）。
     *
     * 不能用 activity 的 metrics：应用退到后台（正是游戏在前台的场景）时它不跟随
     * 旋转，实测在竖屏下返回横屏尺寸，导致裁剪取错区域、画面被压扁而认不出地图。
     * 每次现取一个 window context 才可靠。
     */
    @Suppress("DEPRECATION")
    private fun screenSize(): Pair<Int, Int> {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            try {
                val dm = activity.getSystemService(Context.DISPLAY_SERVICE)
                    as android.hardware.display.DisplayManager
                val d = dm.getDisplay(android.view.Display.DEFAULT_DISPLAY)
                val wc = activity.createDisplayContext(d)
                    .createWindowContext(WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY, null)
                val b = wc.getSystemService(WindowManager::class.java).maximumWindowMetrics.bounds
                if (b.width() > 0 && b.height() > 0) return Pair(b.width(), b.height())
            } catch (e: Exception) {
                Log.w("EmberMap", "window context 取屏幕尺寸失败", e)
            }
        }
        val m = android.util.DisplayMetrics()
        activity.windowManager.defaultDisplay.getRealMetrics(m)
        return Pair(m.widthPixels, m.heightPixels)
    }

    /**
     * 悬浮窗带 FLAG_SECURE，不会进入投屏画面——但代价是它覆盖的区域在抓到的帧里
     * 是黑块，而它盖住的恰是地图，于是下一帧就认不出地图（实测表现为识别结果在
     * 「认出」与「无面板」之间来回震荡）。因此取帧瞬间先隐藏它，取完再恢复。
     */
    private fun grabOnWorker(ir: ImageReader, invoke: Invoke) {
        var image: android.media.Image? = null
        val ov = overlayView
        var hiddenAt = 0L
        // 悬浮窗每藏一次用户就看到一次闪。因此只在「必须干净」的那一小段藏：
        // 拿到原始像素就立刻恢复，后面的裁剪/缩放/JPEG 编码/写盘（合计几百毫秒）
        // 全在自己的内存里做，与屏幕无关。
        val restore = {
            if (hiddenAt != 0L && ov != null) {
                val ms = android.os.SystemClock.uptimeMillis() - hiddenAt
                hiddenAt = 0L
                activity.runOnUiThread { ov.visibility = View.VISIBLE }
                if (ms > 200) Log.i("EmberMap", "悬浮窗隐藏了 ${ms}ms（越短越不闪）")
            }
        }
        try {
            if (ov != null && ov.visibility == View.VISIBLE) {
                hiddenAt = android.os.SystemClock.uptimeMillis()
                activity.runOnUiThread { ov.visibility = View.INVISIBLE }
                // 等合成器出一帧不含悬浮窗的画面，并丢掉队列里的旧帧。
                // 60ms ≈ 3~4 个 vsync；万一没等够，识别侧有 0.97 自拍闸门兜底
                Thread.sleep(60)
                while (true) {
                    val stale = ir.acquireLatestImage() ?: break
                    stale.close()
                }
            }

            // 取队列中最新的一帧，丢弃积压的旧帧
            var latest = ir.acquireLatestImage()
            var waited = 0
            while (latest == null && waited < 1500) {
                Thread.sleep(15)
                waited += 15
                latest = ir.acquireLatestImage()
            }
            if (latest == null) {
                invoke.reject("取帧超时")
                return
            }
            image = latest

            val plane = image.planes[0]
            val rowStride = plane.rowStride
            val pixelStride = plane.pixelStride
            val width = image.width
            val height = image.height
            // rowStride 可能带 padding，需按 stride 建 Bitmap 再裁掉
            val padded = rowStride / pixelStride
            val bmp = Bitmap.createBitmap(padded, height, Bitmap.Config.ARGB_8888)
            bmp.copyPixelsFromBuffer(plane.buffer)
            val full = if (padded != width) {
                Bitmap.createBitmap(bmp, 0, 0, width, height)
            } else {
                bmp
            }
            // 像素已复制进自己的 Bitmap，投屏缓冲与屏幕都不再需要
            image.close()
            image = null
            restore()

            // 2000 而非更低：实测缩到 1200 时多次重采样叠加 JPEG 压缩，
            // 会把识别分数压到置信门槛之下（0.73 vs 桌面 0.84）。
            val maxEdge = 2000
            val (sw, sh) = screenSize()

            // 方形缓冲区里屏幕内容的呈现方式，实测在两种之间摇摆：
            //   a) 留黑边（内容按原比例居中，四周纯黑）
            //   b) 拉伸填满（横屏内容被垂直拉伸 2.2 倍）
            // 匹配器只允许等比缩放，(b) 不纠正必然认错（分数虚高 0.86、分差仅 0.004）。
            // 因此不猜系统行为：按屏幕尺寸算出「若留黑边则内容应在的矩形」，
            // 检查该矩形外是否确实全黑，是则裁掉黑边，否则按拉伸处理整体还原比例。
            val left = ((width - sw) / 2).coerceAtLeast(0)
            val top = ((height - sh) / 2).coerceAtLeast(0)
            val hasBars = (left >= 4 || top >= 4) &&
                bandIsBlack(full, left, top, minOf(sw, width - left), minOf(sh, height - top))
            val base = if (hasBars) {
                Bitmap.createBitmap(full, left, top, minOf(sw, width - left), minOf(sh, height - top))
            } else {
                full
            }
            val k = minOf(1.0, maxEdge.toDouble() / maxOf(sw, sh))
            val outW = (sw * k).toInt().coerceAtLeast(1)
            val outH = (sh * k).toInt().coerceAtLeast(1)
            val out = if (base.width == outW && base.height == outH) base
                      else Bitmap.createScaledBitmap(base, outW, outH, true)
            // 截图 px = 屏幕物理 px × scale
            val scale = k

            val file = File(activity.cacheDir, "em_frame.jpg")
            file.outputStream().use { out.compress(Bitmap.CompressFormat.JPEG, 88, it) }

            val ret = JSObject()
            ret.put("path", file.absolutePath)
            ret.put("width", out.width)
            ret.put("height", out.height)
            // Rust 侧需要据此把结果换算回屏幕物理坐标
            ret.put("scale", scale)
            invoke.resolve(ret)
        } catch (e: Exception) {
            Log.e("EmberMap", "取帧失败", e)
            invoke.reject(e.message ?: "取帧失败")
        } finally {
            try { image?.close() } catch (_: Exception) {}
            restore()
        }
    }

    // -----------------------------------------------------------------------
    // 悬浮窗：叠加层（不可触摸，穿透到游戏）+ 控制条（可触摸）
    // -----------------------------------------------------------------------

    private var overlayView: OverlayView? = null
    private var controlView: ControlView? = null
    private var controlLp: WindowManager.LayoutParams? = null
    /** 控制条收起态：贴边小柄。0=展开 1=贴左 2=贴右 */
    private var controlCollapsed = false
    private var controlDock = 0

    /**
     * 控制悬浮窗：手机上没有键盘，桌面端那套全局热键在这里等价于几个按钮。
     * 按钮事件经 trigger 送回前端（JS 侧 addPluginListener("emcapture", "control")），
     * 由前端复用与桌面完全相同的那套动作逻辑，不在 Kotlin 里另起一套。
     */
    @Command
    fun showControl(invoke: Invoke) {
        val args = invoke.parseArgs(ControlArgs::class.java)
        if (!Settings.canDrawOverlays(activity)) {
            invoke.reject("尚未授予悬浮窗权限")
            return
        }
        activity.runOnUiThread {
            try {
                val wm = activity.getSystemService(Context.WINDOW_SERVICE) as WindowManager
                if (!args.show) {
                    removeControl(wm)
                    invoke.resolve(JSObject())
                    return@runOnUiThread
                }
                val view = ensureControlView(wm)
                view.setStatus(args.status)
                view.setFlags(args.overlayOn, args.mapOn, args.busy)
                view.setFloor(args.floor)
                val (sw, sh) = screenSize()
                // 整层图等比缩放的上限：宽不超过屏幕的 2/3，高不超过一半，
                // 免得把游戏画面整个盖住（用户可再捏合放大，见 FloorMapView）
                view.setMap(args.mapPath, args.mapDoors, sw * 2 / 3, sh / 2)
                if (!controlCollapsed) clampControl(wm, sw, sh)
                invoke.resolve(JSObject())
            } catch (t: Throwable) {
                Log.e("EmberMap", "控制悬浮窗失败", t)
                invoke.reject(t.message ?: "控制悬浮窗失败")
            }
        }
    }

    /** 取现有控制条；不存在或已被系统摘除（窗口失效类异常路径）就重建。 */
    private fun ensureControlView(wm: WindowManager): ControlView {
        controlView?.let { v ->
            if (v.isAttachedToWindow) return v
            runCatching { wm.removeView(v) }
            controlView = null
            controlLp = null
        }
        val lp = WindowManager.LayoutParams(
            WindowManager.LayoutParams.WRAP_CONTENT,
            WindowManager.LayoutParams.WRAP_CONTENT,
            WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
            // 只加 NOT_FOCUSABLE：这个窗口要收下自己范围内的触摸
            //（这正是它与叠加层必须分成两个窗口的原因），
            // 但不抢焦点，游戏的其余部分照常操作
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
                WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
            PixelFormat.TRANSLUCENT
        )
        lp.gravity = android.view.Gravity.TOP or android.view.Gravity.START
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            lp.layoutInDisplayCutoutMode =
                WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        }
        val (sw, sh) = screenSize()
        lp.x = sw / 20
        lp.y = sh / 12
        val v = ControlView(activity) { action, value ->
            pendingActions.add(action to value)
        }
        // 拖动全程把窗口钳在屏内：此前可以整条拖出屏幕外找不回来
        v.onDrag = { dx, dy ->
            val p = controlLp
            if (p != null) {
                val (w, h) = screenSize()
                p.x = (p.x + dx).coerceIn(0, (w - v.width).coerceAtLeast(0))
                p.y = (p.y + dy).coerceIn(0, (h - v.height).coerceAtLeast(0))
                runCatching { wm.updateViewLayout(v, p) }
            }
        }
        // 松手：离边缘足够近（或收起态拖完了）就贴边收起
        v.onDragEnd = {
            val p = controlLp
            if (p != null && v.isAttachedToWindow) {
                if (controlCollapsed) {
                    val (w, _) = screenSize()
                    dockControl(if (p.x + v.width / 2 < w / 2) 1 else 2)
                } else {
                    val (w, _) = screenSize()
                    val edge = (20 * v.resources.displayMetrics.density).toInt()
                    when {
                        p.x <= edge -> dockControl(1)
                        p.x + v.width >= w - edge -> dockControl(2)
                    }
                }
            }
        }
        v.onExpandRequest = { expandControl() }
        wm.addView(v, lp)
        controlView = v
        controlLp = lp
        controlCollapsed = false
        controlDock = 0
        return v
    }

    private fun removeControl(wm: WindowManager) {
        controlView?.let { runCatching { wm.removeView(it) } }
        controlView = null
        controlLp = null
        controlCollapsed = false
        controlDock = 0
    }

    /** 旋转/改尺寸后把控制条钳回屏内（每次 showControl 顺手做） */
    private fun clampControl(wm: WindowManager, sw: Int, sh: Int) {
        val v = controlView ?: return
        val p = controlLp ?: return
        if (!v.isAttachedToWindow) return
        val nx = p.x.coerceIn(0, (sw - v.width).coerceAtLeast(0))
        val ny = p.y.coerceIn(0, (sh - v.height).coerceAtLeast(0))
        if (nx != p.x || ny != p.y) {
            p.x = nx
            p.y = ny
            runCatching { wm.updateViewLayout(v, p) }
        }
    }

    /** 收起成贴边小柄：side 1=贴左 2=贴右。等待重新测量后再定位到边缘。 */
    private fun dockControl(side: Int) {
        val v = controlView ?: return
        val p = controlLp ?: return
        controlCollapsed = true
        controlDock = side
        v.setCollapsed(true)
        v.post {
            val wm = activity.getSystemService(Context.WINDOW_SERVICE) as WindowManager
            val (w, h) = screenSize()
            p.x = if (side == 1) 0 else (w - v.width).coerceAtLeast(0)
            p.y = p.y.coerceIn(0, (h - v.height).coerceAtLeast(0))
            runCatching { wm.updateViewLayout(v, p) }
        }
    }

    /** 点贴边小柄展开回完整控制条，并把窗口钳回屏内。 */
    private fun expandControl() {
        val v = controlView ?: return
        val p = controlLp ?: return
        controlCollapsed = false
        controlDock = 0
        v.setCollapsed(false)
        v.post {
            val wm = activity.getSystemService(Context.WINDOW_SERVICE) as WindowManager
            val (w, h) = screenSize()
            p.x = p.x.coerceIn(0, (w - v.width).coerceAtLeast(0))
            p.y = p.y.coerceIn(0, (h - v.height).coerceAtLeast(0))
            runCatching { wm.updateViewLayout(v, p) }
        }
    }

    @Command
    fun hasOverlayPermission(invoke: Invoke) {
        val ret = JSObject()
        ret.put("granted", Settings.canDrawOverlays(activity))
        invoke.resolve(ret)
    }

    /** 跳到系统设置页让用户授予「显示在其他应用上层」 */
    @Command
    fun requestOverlayPermission(invoke: Invoke) {
        if (Settings.canDrawOverlays(activity)) {
            val ret = JSObject()
            ret.put("granted", true)
            invoke.resolve(ret)
            return
        }
        val intent = Intent(
            Settings.ACTION_MANAGE_OVERLAY_PERMISSION,
            android.net.Uri.parse("package:" + activity.packageName)
        )
        startActivityForResult(invoke, intent, "onOverlayPermissionResult")
    }

    @app.tauri.annotation.ActivityCallback
    fun onOverlayPermissionResult(invoke: Invoke, result: androidx.activity.result.ActivityResult) {
        val ret = JSObject()
        ret.put("granted", Settings.canDrawOverlays(activity))
        invoke.resolve(ret)
    }

    @Command
    fun showOverlay(invoke: Invoke) {
        val args = invoke.parseArgs(OverlayArgs::class.java)
        Log.i(
            "EmberMap",
            "悬浮窗 窗口=${args.w}x${args.h}@${args.x},${args.y} " +
                "手绘尺度=${"%.4f".format(args.scale)} 平移=${args.tx.toInt()},${args.ty.toInt()}"
        )
        if (!Settings.canDrawOverlays(activity)) {
            invoke.reject("尚未授予悬浮窗权限")
            return
        }
        activity.runOnUiThread {
            try {
                val wm = activity.getSystemService(Context.WINDOW_SERVICE) as WindowManager
                val lp = WindowManager.LayoutParams(
                    args.w, args.h,
                    WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
                    // NOT_TOUCHABLE = 触摸完全穿透到游戏；
                    // SECURE = 本层不进入投屏画面，避免自己识别自己
                    // 不加 FLAG_SECURE：取帧时本来就会先隐藏悬浮窗（见 grabOnWorker），
                    // 不带 SECURE 才能让用户截图核对叠加位置
                    WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
                        WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or
                        WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
                    PixelFormat.TRANSLUCENT
                )
                lp.gravity = android.view.Gravity.TOP or android.view.Gravity.START
                // 不声明就绕开挖孔区，窗口会被系统整体推开——横屏时刘海在侧边，
                // 偏移量可达上百像素，叠加层就对不准了
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                    lp.layoutInDisplayCutoutMode =
                        WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
                }
                lp.x = args.x
                lp.y = args.y

                var view = overlayView
                if (view != null && !view.isAttachedToWindow) {
                    // 窗口已被系统摘除（异常路径）；旧引用不能 updateViewLayout，重建
                    runCatching { wm.removeView(view) }
                    overlayView = null
                    view = null
                }
                if (view == null) {
                    view = OverlayView(activity)
                    wm.addView(view, lp)
                    overlayView = view
                } else {
                    wm.updateViewLayout(view, lp)
                }
                view.update(args)
                invoke.resolve(JSObject())
            } catch (t: Throwable) {
                Log.e("EmberMap", "悬浮窗显示失败", t)
                invoke.reject(t.message ?: "悬浮窗显示失败")
            }
        }
    }

    /** 应用被销毁时把两个悬浮窗都摘掉，否则会留在屏幕上摘不掉 */
    override fun onDestroy() {
        activity.runOnUiThread {
            val wm = activity.getSystemService(Context.WINDOW_SERVICE) as WindowManager
            overlayView?.let { runCatching { wm.removeView(it) } }
            removeControl(wm)
            overlayView = null
        }
    }

    @Command
    fun hideOverlay(invoke: Invoke) {
        activity.runOnUiThread {
            try {
                overlayView?.let {
                    val wm = activity.getSystemService(Context.WINDOW_SERVICE) as WindowManager
                    wm.removeView(it)
                }
            } catch (_: Throwable) {
            } finally {
                overlayView = null
            }
            invoke.resolve(JSObject())
        }
    }
}
