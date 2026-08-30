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
    /** 一段拖动结束（松手）：插件据此判断是否贴边收起 */
    var onDragEnd: (() -> Unit)? = null
    /** 收起态下点小柄：请求展开 */
    var onExpandRequest: (() -> Unit)? = null

    private var collapsed = false
    private var mapOn = false
    private lateinit var pill: TextView
    private lateinit var row: LinearLayout
    private var downX = 0f
    private var downY = 0f
    private var moved = 0f
    private val touchSlop = android.view.ViewConfiguration.get(context).scaledTouchSlop

    /** 拖动把手 / 收起态小柄共用的触摸处理。
     *  isPill：小柄上松手且几乎没移动 = 点按，请求展开；真正拖动过才报 onDragEnd。 */
    private fun dragTouch(isPill: Boolean) = android.view.View.OnTouchListener { _, ev ->
        when (ev.action) {
            android.view.MotionEvent.ACTION_DOWN -> {
                downX = ev.rawX; downY = ev.rawY; moved = 0f; true
            }
            android.view.MotionEvent.ACTION_MOVE -> {
                val dx = ev.rawX - downX
                val dy = ev.rawY - downY
                onDrag?.invoke(dx.toInt(), dy.toInt())
                moved += kotlin.math.abs(dx) + kotlin.math.abs(dy)
                downX = ev.rawX; downY = ev.rawY
                true
            }
            android.view.MotionEvent.ACTION_UP, android.view.MotionEvent.ACTION_CANCEL -> {
                if (moved >= touchSlop) onDragEnd?.invoke()
                else if (isPill && ev.action == android.view.MotionEvent.ACTION_UP) onExpandRequest?.invoke()
                true
            }
            else -> true
        }
    }

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

        row = LinearLayout(context).apply { orientation = HORIZONTAL }
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
        handle.setOnTouchListener(dragTouch(false))
        row.addView(handle)

        btnShoot = mkButton("抓屏匹配") { onAction("capture", null) }
        // 开关类按钮把「开/关」直接写进文字：只靠蓝色高亮表示状态，
        // 用户容易看不出它是个开关（实测就被当成没有这个功能）
        btnOverlay = mkButton("叠加层 关") { onAction("toggle_overlay", null) }
        btnMap = mkButton("整层 关") { onAction("toggle_map", null) }
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

        // 收起态小柄：控制条被拖到屏幕边缘时缩成它，点一下展开回完整控制条
        pill = TextView(context).apply {
            text = "⋮⋮"
            textSize = 15f
            setTextColor(Color.rgb(0x9f, 0xdc, 0xff))
            gravity = Gravity.CENTER
            setPadding(dp(12), dp(8), dp(12), dp(8))
            minimumWidth = dp(34)
            minimumHeight = dp(34)
            setOnTouchListener(dragTouch(true))
        }
        addView(pill, LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT))
        pill.visibility = GONE
    }

    fun setStatus(text: String) {
        statusText.text = text
    }

    fun setFlags(overlayOn: Boolean, mapOn: Boolean, busy: Boolean) {
        this.mapOn = mapOn
        btnOverlay.text = if (overlayOn) "叠加层 开" else "叠加层 关"
        btnMap.text = if (mapOn) "整层 开" else "整层 关"
        setOn(btnOverlay, overlayOn)
        setOn(btnMap, mapOn)
        btnShoot.isEnabled = !busy
        btnShoot.alpha = if (busy) 0.5f else 1f
        // 收起态下隐藏全部内容行，只留小柄；展开时按 mapOn 恢复
        floorRow.visibility = if (!collapsed && mapOn) VISIBLE else GONE
        mapView.visibility = if (!collapsed && mapOn) VISIBLE else GONE
    }

    /** 贴边收起：只剩一个小柄。拖到屏幕边缘松手即收起（插件侧触发），点小柄展开。 */
    fun setCollapsed(c: Boolean) {
        collapsed = c
        row.visibility = if (c) GONE else VISIBLE
        statusText.visibility = if (c) GONE else VISIBLE
        floorRow.visibility = if (!c && mapOn) VISIBLE else GONE
        mapView.visibility = if (!c && mapOn) VISIBLE else GONE
        pill.visibility = if (c) VISIBLE else GONE
        val pad = if (c) dp(2) else dp(8)
        setPadding(pad, pad, pad, pad)
        (background as GradientDrawable).cornerRadius = dp(if (c) 20 else 12).toFloat()
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
     * 整层手绘图：先等比适配到卡片内（宽 ≤ 屏 2/3、高 ≤ 屏一半），
     * 用户可双指捏合继续放大、拖动查看，双击在适配与放大之间切换——
     * 适配大小对小地图来说字太小，是「整层看得见但看不清」的解法。
     * 与叠加层不同，这里不做任何对齐——就是把整层原样摊开给用户看，
     * 解决「游戏里地图放大后看不到别处」。
     */
    class FloorMapView(context: Context) : View(context) {
        private var bmp: Bitmap? = null
        private var loaded: String? = null
        private var doors: List<DoorArg> = emptyList()
        private var boxW = 0
        private var boxH = 0
        private var fit = 1f      // 适配倍率：整图缩进卡片
        private var zoom = 1f     // 用户倍率：1 = 适配，最大 8
        private var panX = 0f     // 平移（视图像素）：把放大后的内容挪进可视区
        private var panY = 0f

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

        private val touchSlop = android.view.ViewConfiguration.get(context).scaledTouchSlop

        private fun total() = fit * zoom

        private fun clampPan() {
            val b = bmp ?: return
            val t = total()
            panX = panX.coerceIn(0f, (b.width * t - width).coerceAtLeast(0f))
            panY = panY.coerceIn(0f, (b.height * t - height).coerceAtLeast(0f))
        }

        /** 以视图点 (fx, fy) 为锚改缩放：锚点下的内容保持不动 */
        private fun zoomAt(fx: Float, fy: Float, target: Float) {
            val prev = total()
            zoom = target.coerceIn(1f, 8f)
            val now = total()
            if (now == prev) return
            panX = (fx + panX) * now / prev - fx
            panY = (fy + panY) * now / prev - fy
            clampPan()
            invalidate()
        }

        private val scaleDetector = android.view.ScaleGestureDetector(
            context,
            object : android.view.ScaleGestureDetector.SimpleOnScaleGestureListener() {
                override fun onScale(d: android.view.ScaleGestureDetector): Boolean {
                    val prev = total()
                    zoom = (zoom * d.scaleFactor).coerceIn(1f, 8f)
                    val now = total()
                    if (now != prev) {
                        panX = (d.focusX + panX) * now / prev - d.focusX
                        panY = (d.focusY + panY) * now / prev - d.focusY
                        clampPan()
                        invalidate()
                    }
                    return true
                }
            }
        )

        private var panPt: android.graphics.PointF? = null
        private var downX = 0f
        private var downY = 0f
        private var downAt = 0L
        private var lastTapAt = 0L

        @SuppressLint("ClickableViewAccessibility")
        override fun onTouchEvent(ev: android.view.MotionEvent): Boolean {
            scaleDetector.onTouchEvent(ev)
            when (ev.actionMasked) {
                android.view.MotionEvent.ACTION_DOWN -> {
                    downX = ev.x; downY = ev.y
                    downAt = android.os.SystemClock.uptimeMillis()
                    panPt = if (zoom > 1f) android.graphics.PointF(ev.x, ev.y) else null
                }
                android.view.MotionEvent.ACTION_POINTER_DOWN -> panPt = null
                android.view.MotionEvent.ACTION_MOVE -> {
                    val pt = panPt
                    if (pt != null && !scaleDetector.isInProgress) {
                        panX -= ev.x - pt.x
                        panY -= ev.y - pt.y
                        pt.set(ev.x, ev.y)
                        clampPan()
                        invalidate()
                    }
                }
                android.view.MotionEvent.ACTION_UP, android.view.MotionEvent.ACTION_CANCEL -> {
                    val moved = kotlin.math.hypot(ev.x - downX, ev.y - downY)
                    val quick = ev.actionMasked == android.view.MotionEvent.ACTION_UP &&
                        android.os.SystemClock.uptimeMillis() - downAt < 250
                    if (quick && moved < touchSlop) {
                        val now = android.os.SystemClock.uptimeMillis()
                        if (now - lastTapAt < 320) {
                            // 双击：适配 ↔ 放大
                            lastTapAt = 0L
                            if (zoom > 1f) {
                                zoom = 1f; panX = 0f; panY = 0f
                            } else {
                                zoomAt(ev.x, ev.y, 2.5f)
                            }
                            invalidate()
                        } else {
                            lastTapAt = now
                        }
                    }
                    panPt = null
                }
            }
            return true
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
                // 换了图，视图状态回适配；缩放与平移都按新图算
                zoom = 1f
                panX = 0f
                panY = 0f
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
            // 等比适配：宽高各自算一个系数，取小的那个，保证整张都进得来
            fit = minOf(boxW.toFloat() / b.width, boxH.toFloat() / b.height)
            setMeasuredDimension((b.width * fit).toInt(), (b.height * fit).toInt())
        }

        override fun onDraw(canvas: Canvas) {
            val b = bmp ?: return
            canvas.save()
            canvas.translate(-panX, -panY)
            val t = total()
            canvas.scale(t, t)
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
