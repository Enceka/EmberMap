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

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // 通知栏「停止投屏」：用户随时能收手，不必回到应用里找开关
        if (intent?.action == ACTION_STOP) {
            Log.i("EmberMap", "用户从通知栏停止投屏")
            teardown()
            stopSelf()
            return START_NOT_STICKY
        }
        // 手机上没有全局热键，通知栏是唯一「在任何界面都够得着」的入口：
        // 悬浮控制条被收起、或被全屏应用挡住时，还能从这里开关叠加层。
        // 事件排进队列，由前端的 pollControl 取走，走与控制条按钮同一条路径。
        if (intent?.action == ACTION_TOGGLE_OVERLAY) {
            Log.i("EmberMap", "用户从通知栏开关叠加层")
            pendingUiActions.add("toggle_overlay")
            return START_NOT_STICKY
        }
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

            // 建成正方形（边长 = 屏幕长边），一次建成永不改动。
            // 原因：屏幕旋转后若 resize()+setSurface()，真机上镜像仍按旧尺寸绘制，
            // 内容被压进新缓冲区一角、其余全黑；而重建 VirtualDisplay 被系统禁止
            // （SecurityException: Don't take multiple captures ... on the same instance），
            // 且尝试重建会连带把投屏停掉。
            // 正方形缓冲区在两种朝向下都按 1:1 镜像（只是留黑边），不损失分辨率。
            val (w, h, density) = realDisplaySize()
            val side = maxOf(w, h)
            dpi = density
            curW = side
            curH = side
            val ir = ImageReader.newInstance(side, side, PixelFormat.RGBA_8888, 2)
            display = mp.createVirtualDisplay(
                "EmberMapCapture", side, side, dpi,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                ir.surface, null, null
            )
            projection = mp
            reader = ir
            instance = this
            Log.i("EmberMap", "投屏管线已建立 ${side}x${side}（屏幕 ${w}x${h}）")
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
        val stopIntent = Intent(this, CaptureService::class.java).setAction(ACTION_STOP)
        val stopPending = android.app.PendingIntent.getService(
            this, 0, stopIntent,
            android.app.PendingIntent.FLAG_IMMUTABLE or android.app.PendingIntent.FLAG_UPDATE_CURRENT
        )
        val toggleIntent = Intent(this, CaptureService::class.java).setAction(ACTION_TOGGLE_OVERLAY)
        // requestCode 必须与 stop 的不同，否则两个 PendingIntent 会被系统视为同一个
        val togglePending = android.app.PendingIntent.getService(
            this, 1, toggleIntent,
            android.app.PendingIntent.FLAG_IMMUTABLE or android.app.PendingIntent.FLAG_UPDATE_CURRENT
        )
        val notification: Notification = Notification.Builder(this, channelId)
            .setContentTitle("EmberMap 正在识别地图")
            .setContentText("仅读取屏幕画面用于识别")
            .setSmallIcon(android.R.drawable.ic_menu_mapmode)
            .setOngoing(true)
            .addAction(
                Notification.Action.Builder(
                    null as android.graphics.drawable.Icon?, "开关叠加层", togglePending
                ).build()
            )
            .addAction(
                Notification.Action.Builder(
                    null as android.graphics.drawable.Icon?, "停止投屏", stopPending
                ).build()
            )
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

    private fun teardown() {
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
        const val ACTION_STOP = "net.yeah.enceka.embermap.STOP_CAPTURE"
        const val ACTION_TOGGLE_OVERLAY = "net.yeah.enceka.embermap.TOGGLE_OVERLAY"

        /** 通知栏按下的动作，等前端 pollControl 取走 */
        val pendingUiActions = java.util.concurrent.ConcurrentLinkedQueue<String>()
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
