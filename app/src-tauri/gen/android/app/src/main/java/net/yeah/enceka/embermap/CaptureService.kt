package net.yeah.enceka.embermap

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.util.DisplayMetrics
import android.view.Display
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.IBinder
import android.util.Log

/**
 * 投屏取帧的前台服务，同时持有整条取帧管线。
 *
 * Android 14 起 MediaProjection 必须在 foregroundServiceType="mediaProjection"
 * 的前台服务中创建，并显示常驻通知——系统强制的知情要求。
 *
 * 关键顺序：必须先 startForeground() 让服务真正进入前台，之后才能
 * getMediaProjection()。因此把投屏创建放在服务内部而不是插件里：
 * 插件所在的主线程无法「等待」服务启动（onStartCommand 同在主线程，
 * 阻塞等待会把服务自己饿死），交给服务自己顺序执行才可靠。
 */
class CaptureService : Service() {
    private var projection: MediaProjection? = null
    @Volatile private var reader: ImageReader? = null
    private var display: VirtualDisplay? = null
    private var dpi = 0
    private var curW = 0
    private var curH = 0
    private var displayListener: DisplayManager.DisplayListener? = null
    private val swapLock = Any()

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForegroundNotification()

        val resultCode = intent?.getIntExtra(EXTRA_CODE, Int.MIN_VALUE) ?: Int.MIN_VALUE
        val data: Intent? = if (Build.VERSION.SDK_INT >= 33) {
            intent?.getParcelableExtra(EXTRA_DATA, Intent::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent?.getParcelableExtra(EXTRA_DATA)
        }
        if (resultCode == Int.MIN_VALUE || data == null) {
            return START_NOT_STICKY
        }

        try {
            val mgr = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            val mp = mgr.getMediaProjection(resultCode, data)
                ?: throw IllegalStateException("系统未返回 MediaProjection")
            mp.registerCallback(object : MediaProjection.Callback() {
                override fun onStop() {
                    Log.i("EmberMap", "投屏已被系统或用户停止")
                    teardown()
                }
            }, null)

            val (w, h, density) = realDisplaySize()
            dpi = density
            curW = w
            curH = h
            val ir = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, 2)
            display = mp.createVirtualDisplay(
                "EmberMapCapture", w, h, dpi,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                ir.surface, null, null
            )
            projection = mp
            reader = ir
            instance = this
            watchRotation()
            Log.i("EmberMap", "投屏管线已建立 ${w}x${h}")
        } catch (e: Exception) {
            Log.e("EmberMap", "建立投屏失败", e)
            lastError = e.message
            teardown()
            stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun startForegroundNotification() {
        val channelId = "embermap_capture"
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            nm.createNotificationChannel(
                NotificationChannel(channelId, "地图识别", NotificationManager.IMPORTANCE_LOW)
            )
        }
        val notification: Notification = Notification.Builder(this, channelId)
            .setContentTitle("EmberMap 正在识别地图")
            .setContentText("仅读取屏幕画面用于识别，可随时停止")
            .setSmallIcon(android.R.drawable.ic_menu_mapmode)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                NOTIFICATION_ID, notification,
                android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    fun imageReader(): ImageReader? = reader

    /**
     * 屏幕真实尺寸（含旋转）。
     *
     * 不能直接信 getRealMetrics 的宽高：服务这种非可视上下文拿到的是显示器的
     * 「基础信息」，实测旋转后仍是 1080×2400（dumpsys 里 mBaseDisplayInfo 不变，
     * 只有 mOverrideDisplayInfo 变成 2400×1080）。因此按 rotation 自己换算：
     * 面板物理尺寸固定，横屏就是长边在前。
     */
    @Suppress("DEPRECATION")
    private fun realDisplaySize(): Triple<Int, Int, Int> {
        val dm = getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
        val d = dm.getDisplay(Display.DEFAULT_DISPLAY)
        val m = DisplayMetrics()
        d.getRealMetrics(m)

        // API 30+：用 window context 取 maximumWindowMetrics。
        // 服务是非可视上下文，其 Display 拿到的是显示器「基础信息」，
        // 实测旋转后 rotation 仍报 0、尺寸仍是 1080×2400
        // （dumpsys 里只有 mOverrideDisplayInfo 变成 2400×1080）。
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            try {
                val wc = createDisplayContext(d).createWindowContext(
                    android.view.WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY, null
                )
                val wm = wc.getSystemService(android.view.WindowManager::class.java)
                val b = wm.maximumWindowMetrics.bounds
                if (b.width() > 0 && b.height() > 0) {
                    return Triple(b.width(), b.height(), m.densityDpi)
                }
            } catch (e: Exception) {
                Log.w("EmberMap", "window context 取尺寸失败，回退基础信息", e)
            }
        }
        return Triple(m.widthPixels, m.heightPixels, m.densityDpi)
    }

    /**
     * 跟随屏幕旋转调整虚拟显示器尺寸。
     *
     * 必须做：VirtualDisplay 的尺寸在创建时固定，用户在竖屏的本应用里授权后
     * 切到横屏游戏，横屏画面会被等比缩放塞进竖屏缓冲区——实测面板从 846×1176
     * 缩到 232×282，13 个变体分数挤在 0.761-0.762、分差 0.001，完全无法识别。
     */
    private fun watchRotation() {
        val dm = getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
        val listener = object : DisplayManager.DisplayListener {
            override fun onDisplayAdded(id: Int) {}
            override fun onDisplayRemoved(id: Int) {}
            override fun onDisplayChanged(id: Int) {
                if (id != Display.DEFAULT_DISPLAY) return
                val (w, h, density) = realDisplaySize()
                Log.i("EmberMap", "显示变化：${w}x${h}（当前投屏 ${curW}x${curH}）")
                if (w == curW && h == curH) return
                synchronized(swapLock) {
                    try {
                        val old = reader
                        val ir = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, 2)
                        display?.resize(w, h, density)
                        display?.surface = ir.surface
                        reader = ir
                        curW = w
                        curH = h
                        dpi = density
                        // 换过 surface 才关旧的，避免取帧线程读到已关闭的 reader
                        old?.close()
                        Log.i("EmberMap", "屏幕旋转，投屏尺寸改为 ${w}x${h}")
                    } catch (e: Exception) {
                        Log.e("EmberMap", "调整投屏尺寸失败", e)
                    }
                }
            }
        }
        dm.registerDisplayListener(listener, null)
        displayListener = listener
    }

    private fun teardown() {
        try {
            displayListener?.let {
                (getSystemService(Context.DISPLAY_SERVICE) as DisplayManager)
                    .unregisterDisplayListener(it)
            }
        } catch (_: Exception) {}
        displayListener = null
        try { display?.release() } catch (_: Exception) {}
        try { reader?.close() } catch (_: Exception) {}
        try { projection?.stop() } catch (_: Exception) {}
        display = null
        reader = null
        projection = null
        instance = null
    }

    override fun onDestroy() {
        teardown()
        super.onDestroy()
    }

    companion object {
        private const val NOTIFICATION_ID = 1001
        const val EXTRA_CODE = "resultCode"
        const val EXTRA_DATA = "resultData"

        /** 就绪后由服务自身登记，供插件取帧；未就绪为 null */
        @Volatile
        var instance: CaptureService? = null
            private set

        /** 建立失败时的原因，供插件回报给前端 */
        @Volatile
        var lastError: String? = null
    }
}
