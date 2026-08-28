package net.yeah.enceka.embermap

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Matrix
import android.graphics.Paint
import android.view.View

/**
 * 悬浮层绘制：把手绘楼层图按相似变换叠到游戏地图上，并标注门位。
 *
 * 与桌面端前端 canvas 的逻辑一一对应：手绘图是黑底的，按亮度换算 alpha，
 * 于是只有结构、文字、路线浮现，底色透明。
 * 绘制放在 Kotlin 而不是 Rust：这里要画中文标签，用系统字体最省事，
 * Rust 侧渲染中文需要额外引入字体文件。
 */
@SuppressLint("ViewConstructor")
class OverlayView(context: Context) : View(context) {
    private var bitmap: Bitmap? = null
    private var loadedPath: String? = null
    private var args: OverlayArgs? = null

    private val bmpPaint = Paint(Paint.FILTER_BITMAP_FLAG)
    private val doorStroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeWidth = 4f
        color = Color.rgb(255, 80, 80)
    }
    private val textFill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.rgb(255, 120, 120)
    }
    private val textStroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeWidth = 3f
        color = Color.BLACK
    }

    fun update(a: OverlayArgs) {
        if (a.drawPath != loadedPath) {
            bitmap = BitmapFactory.decodeFile(a.drawPath)?.let { src ->
                // 黑底转透明：亮度映射到 alpha，与桌面端 screen 混合等价
                val w = src.width
                val h = src.height
                val px = IntArray(w * h)
                src.getPixels(px, 0, w, 0, 0, w, h)
                for (i in px.indices) {
                    val c = px[i]
                    val luma = 0.299 * Color.red(c) + 0.587 * Color.green(c) + 0.114 * Color.blue(c)
                    val alpha = (luma * 255.0 / 70.0).coerceIn(0.0, 255.0).toInt()
                    px[i] = (alpha shl 24) or (c and 0x00FFFFFF)
                }
                Bitmap.createBitmap(px, w, h, Bitmap.Config.ARGB_8888)
            }
            loadedPath = a.drawPath
        }
        args = a
        invalidate()
    }

    override fun onDraw(canvas: Canvas) {
        val a = args ?: return
        val bmp = bitmap ?: return

        bmpPaint.alpha = (a.alpha * 255).toInt().coerceIn(0, 255)
        val m = Matrix()
        m.setScale(a.scale.toFloat(), a.scale.toFloat())
        m.postTranslate(a.tx.toFloat(), a.ty.toFloat())
        canvas.drawBitmap(bmp, m, bmpPaint)

        // 边框：让用户一眼看出叠加层落在哪，便于核对是否对准
        val border = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = 3f
            color = Color.argb(150, 80, 200, 120)
        }
        canvas.drawRect(1.5f, 1.5f, width - 1.5f, height - 1.5f, border)

        val r = maxOf(8f, height * 0.014f)
        val size = maxOf(15f, height * 0.026f)
        textFill.textSize = size
        textStroke.textSize = size
        for (d in a.doors) {
            val x = d.x.toFloat()
            val y = d.y.toFloat()
            canvas.drawCircle(x, y, r, doorStroke)
            canvas.drawText(d.label, x + r + 4, y + 5, textStroke)
            canvas.drawText(d.label, x + r + 4, y + 5, textFill)
        }
    }
}
