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
    private var reader: ImageReader? = null
    private var display: VirtualDisplay? = null

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

            val metrics = resources.displayMetrics
            val w = metrics.widthPixels
            val h = metrics.heightPixels
            val ir = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, 2)
            display = mp.createVirtualDisplay(
                "EmberMapCapture", w, h, metrics.densityDpi,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                ir.surface, null, null
            )
            projection = mp
            reader = ir
            instance = this
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
