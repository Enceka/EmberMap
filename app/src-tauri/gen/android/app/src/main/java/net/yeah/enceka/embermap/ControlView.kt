package net.yeah.enceka.embermap

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.drawable.GradientDrawable
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView

/**
 * 可触摸的控制悬浮窗：手机上没有键盘，桌面端那套全局热键在这里等价于几个按钮。
 *
 * 与叠加悬浮窗（OverlayView）分成两个窗口，是因为二者的触摸语义相反：
 * 叠加层必须 FLAG_NOT_TOUCHABLE 让操作穿透到游戏，而控制条恰恰要收下触摸。
 * 同一个窗口做不到「一部分穿透一部分不穿透」。
 *
 * 「整层」展开时地图直接画在本窗口里（等比缩放到卡片宽度），
 * 它与游戏画面无关、不需要对齐，所以放在可触摸的这一侧最合适——
 * 楼层按钮就在地图正上方，切层不用切回应用。
 */
@SuppressLint("ViewConstructor")
class ControlView(
    context: Context,
    private val onAction: (String, String?) -> Unit,
) : LinearLayout(context) {

    private val statusText: TextView
    private val floorRow: LinearLayout
    private val mapView: FloorMapView
    private val btnOverlay: Button
    private val btnMap: Button
    private val btnShoot: Button

    /** 拖动窗口用：由外部（插件）在回调里改 WindowManager.LayoutParams */
    var onDrag: ((dx: Int, dy: Int) -> Unit)? = null

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun mkButton(label: String, onClick: () -> Unit): Button =
        Button(context).apply {
            text = label
            textSize = 12f
            isAllCaps = false
            minWidth = 0
            minimumWidth = 0
            minHeight = dp(34)
            minimumHeight = dp(34)
            setPadding(dp(10), 0, dp(10), 0)
            setTextColor(Color.rgb(0xdd, 0xe3, 0xea))
            background = GradientDrawable().apply {
                cornerRadius = dp(6).toFloat()
                setColor(Color.rgb(0x2c, 0x35, 0x42))
            }
            setOnClickListener { onClick() }
        }

    /** 开关类按钮的选中态：蓝底白字 */
    private fun setOn(b: Button, on: Boolean) {
        (b.background as GradientDrawable).setColor(
            if (on) Color.rgb(0x2d, 0x7f, 0xf9) else Color.rgb(0x2c, 0x35, 0x42)
        )
        b.setTextColor(if (on) Color.WHITE else Color.rgb(0xdd, 0xe3, 0xea))
    }

    init {
        orientation = VERTICAL
        setPadding(dp(8), dp(8), dp(8), dp(8))
        background = GradientDrawable().apply {
            cornerRadius = dp(12).toFloat()
            setColor(Color.argb(0xEE, 0x1b, 0x21, 0x2b))
            setStroke(dp(1), Color.argb(0x80, 0x3a, 0x45, 0x53))
        }

        val row = LinearLayout(context).apply { orientation = HORIZONTAL }
        addView(row, LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT))

        // 拖动把手：按住它挪窗口。放在最左边，误触概率最低
        val handle = TextView(context).apply {
            text = "⋮⋮"
            textSize = 14f
            setTextColor(Color.rgb(0x6f, 0x7b, 0x88))
            gravity = Gravity.CENTER
            setPadding(dp(6), 0, dp(8), 0)
            minimumHeight = dp(34)
        }
        var downX = 0f
        var downY = 0f
        handle.setOnTouchListener { _, ev ->
            when (ev.action) {
                android.view.MotionEvent.ACTION_DOWN -> {
                    downX = ev.rawX; downY = ev.rawY; true
                }
                android.view.MotionEvent.ACTION_MOVE -> {
                    onDrag?.invoke((ev.rawX - downX).toInt(), (ev.rawY - downY).toInt())
                    downX = ev.rawX; downY = ev.rawY; true
                }
                else -> true
            }
        }
        row.addView(handle)

        btnShoot = mkButton("抓屏匹配") { onAction("capture", null) }
        btnOverlay = mkButton("叠加") { onAction("toggle_overlay", null) }
        btnMap = mkButton("整层") { onAction("toggle_map", null) }
        val btnReset = mkButton("重识") { onAction("reset", null) }
        val btnClose = mkButton("✕") { onAction("close", null) }
        for (b in listOf(btnShoot, btnOverlay, btnMap, btnReset, btnClose)) {
            row.addView(b, LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT).apply {
                leftMargin = dp(4)
            })
        }

        statusText = TextView(context).apply {
            textSize = 11f
            setTextColor(Color.rgb(0x9a, 0xa6, 0xb2))
            setPadding(dp(4), dp(6), dp(4), 0)
            text = "点「抓屏匹配」开始识别"
        }
        addView(statusText)

        floorRow = LinearLayout(context).apply {
            orientation = HORIZONTAL
            visibility = GONE
            setPadding(0, dp(6), 0, 0)
        }
        for ((id, label) in listOf("b1" to "地下室", "1f" to "一楼", "2f" to "二楼")) {
            floorRow.addView(
                mkButton(label) { onAction("floor", id) },
                LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT).apply {
                    rightMargin = dp(4)
                }
            )
        }
        addView(floorRow)

        mapView = FloorMapView(context)
        mapView.visibility = GONE
        addView(mapView, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT).apply {
            topMargin = dp(6)
        })
    }

    fun setStatus(text: String) {
        statusText.text = text
    }

    fun setFlags(overlayOn: Boolean, mapOn: Boolean, busy: Boolean) {
        setOn(btnOverlay, overlayOn)
        setOn(btnMap, mapOn)
        btnShoot.isEnabled = !busy
        btnShoot.alpha = if (busy) 0.5f else 1f
        floorRow.visibility = if (mapOn) VISIBLE else GONE
        mapView.visibility = if (mapOn) VISIBLE else GONE
    }

    /** 高亮当前正在看的楼层 */
    fun setFloor(floor: String?) {
        val ids = listOf("b1", "1f", "2f")
        for (i in ids.indices) {
            setOn(floorRow.getChildAt(i) as Button, ids[i] == floor)
        }
    }

    /** 换一张整层图；path 为 null 表示清空 */
    fun setMap(path: String?, doors: List<DoorArg>, maxW: Int, maxH: Int) {
        mapView.load(path, doors, maxW, maxH)
    }

    /**
     * 整层手绘图，等比缩放到卡片宽度以内、且不超过给定高度。
     * 与叠加层不同，这里不做任何对齐——就是把整层原样摊开给用户看，
     * 解决「游戏里地图放大后看不到别处」。
     */
    class FloorMapView(context: Context) : View(context) {
        private var bmp: Bitmap? = null
        private var loaded: String? = null
        private var doors: List<DoorArg> = emptyList()
        private var boxW = 0
        private var boxH = 0
        private var scale = 1f

        private val paint = Paint(Paint.FILTER_BITMAP_FLAG)
        private val doorStroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = 3f
            color = Color.rgb(255, 80, 80)
        }
        private val labelFill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = Color.rgb(255, 140, 140)
            textSize = 22f
        }
        private val labelStroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = 3f
            color = Color.BLACK
            textSize = 22f
        }

        fun load(path: String?, d: List<DoorArg>, maxW: Int, maxH: Int) {
            boxW = maxW
            boxH = maxH
            doors = d
            if (path == null) {
                bmp = null
                loaded = null
            } else if (path != loaded) {
                bmp = BitmapFactory.decodeFile(path)
                loaded = path
            }
            requestLayout()
            invalidate()
        }

        override fun onMeasure(widthSpec: Int, heightSpec: Int) {
            val b = bmp
            if (b == null || boxW <= 0) {
                setMeasuredDimension(0, 0)
                return
            }
            // 等比缩放：宽高各自算一个系数，取小的那个，保证整张都进得来
            scale = minOf(boxW.toFloat() / b.width, boxH.toFloat() / b.height)
            setMeasuredDimension((b.width * scale).toInt(), (b.height * scale).toInt())
        }

        override fun onDraw(canvas: Canvas) {
            val b = bmp ?: return
            canvas.save()
            canvas.scale(scale, scale)
            canvas.drawBitmap(b, 0f, 0f, paint)
            val r = maxOf(6f, b.height * 0.012f)
            for (d in doors) {
                canvas.drawCircle(d.x.toFloat(), d.y.toFloat(), r, doorStroke)
                canvas.drawText(d.label, (d.x + r + 3).toFloat(), (d.y + 7).toFloat(), labelStroke)
                canvas.drawText(d.label, (d.x + r + 3).toFloat(), (d.y + 7).toFloat(), labelFill)
            }
            canvas.restore()
        }
    }
}
