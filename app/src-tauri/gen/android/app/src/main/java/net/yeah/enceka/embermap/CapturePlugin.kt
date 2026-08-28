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
    var alpha: Double = 0.75
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
        var hidden = false
        try {
            if (ov != null && ov.visibility == View.VISIBLE) {
                activity.runOnUiThread { ov.visibility = View.INVISIBLE }
                hidden = true
                // 等合成器出一帧不含悬浮窗的画面，并丢掉队列里的旧帧
                Thread.sleep(120)
                while (true) {
                    val stale = ir.acquireLatestImage() ?: break
                    stale.close()
                }
            }

            // 取队列中最新的一帧，丢弃积压的旧帧
            var latest = ir.acquireLatestImage()
            var waited = 0
            while (latest == null && waited < 1500) {
                Thread.sleep(30)
                waited += 30
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
            if (hidden && ov != null) {
                activity.runOnUiThread { ov.visibility = View.VISIBLE }
            }
        }
    }

    // -----------------------------------------------------------------------
    // 悬浮窗：叠加显示在游戏之上
    // -----------------------------------------------------------------------

    private var overlayView: OverlayView? = null

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
                if (view == null) {
                    view = OverlayView(activity)
                    wm.addView(view, lp)
                    overlayView = view
                } else {
                    wm.updateViewLayout(view, lp)
                }
                view.update(args)
                invoke.resolve(JSObject())
            } catch (e: Exception) {
                Log.e("EmberMap", "悬浮窗显示失败", e)
                invoke.reject(e.message ?: "悬浮窗显示失败")
            }
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
            } catch (_: Exception) {
            } finally {
                overlayView = null
            }
            invoke.resolve(JSObject())
        }
    }
}
