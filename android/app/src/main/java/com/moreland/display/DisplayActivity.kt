// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.app.Activity
import android.content.Context
import android.content.res.Configuration
import android.graphics.Color
import android.net.wifi.WifiManager
import android.os.Bundle
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView
import kotlin.math.roundToInt

/**
 * Fullscreen host for the decoded stream.
 *
 * The stream is tied to the surface lifecycle rather than the activity's:
 * a surface can be destroyed and recreated without the activity restarting,
 * and decoding into a dead surface is a crash.
 */
class DisplayActivity : Activity(), SurfaceHolder.Callback {

    private lateinit var surfaceView: SurfaceView
    private lateinit var status: TextView
    private var stream: VideoStream? = null
    private var reportedSize: Pair<Int, Int>? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private val refreshStatusTask = Runnable { refreshStatus() }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Only worth holding when the host is reached over Wi-Fi; over USB the
        // radio is irrelevant and the lock would just cost battery.
        if (intent.getBooleanExtra(Protocol.WIRELESS_EXTRA, false)) {
            wifiLock = (applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager)
                ?.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "moreland")
                ?.apply { setReferenceCounted(false) }
        }

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setBackgroundDrawableResource(android.R.color.black)
        // Landscape would otherwise leave a black band beside the cutout.
        window.attributes = window.attributes.apply {
            layoutInDisplayCutoutMode =
                WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
        }

        val root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }

        status = TextView(this).apply {
            text = "Waiting for host…\n\nConnect the tablet and start the daemon."
            setTextColor(Color.parseColor("#888888"))
            textSize = 16f
            val pad = (24 * resources.displayMetrics.density).toInt()
            setPadding(pad, pad, pad, pad)
        }
        surfaceView = SurfaceView(this)

        root.addView(surfaceView, FrameLayout.LayoutParams(MATCH, MATCH))
        root.addView(status, FrameLayout.LayoutParams(MATCH, WRAP))
        setContentView(root)

        val socketName = Protocol.socketName(intent.getStringExtra(Protocol.SOCKET_EXTRA))
        stream = VideoStream(socketName).also { it.start() }
        surfaceView.holder.addCallback(this)
        @Suppress("ClickableViewAccessibility")
        surfaceView.setOnTouchListener { view, event -> forwardTouch(view, event) }
        goImmersive()
    }

    /**
     * Report contacts to the host in coordinates normalised across the view.
     *
     * The decoder output is stretched to fill the SurfaceView, so the view's
     * own bounds are the image's bounds and no letterbox correction applies.
     */
    private fun forwardTouch(view: View, event: MotionEvent): Boolean {
        val stream = stream ?: return false
        val width = view.width
        val height = view.height
        if (width <= 0 || height <= 0) return false

        fun send(action: Int, index: Int) {
            val x = (event.getX(index) / width * Protocol.TOUCH_EXTENT).roundToInt()
            val y = (event.getY(index) / height * Protocol.TOUCH_EXTENT).roundToInt()
            stream.sendTouch(
                action,
                event.getPointerId(index),
                x.coerceIn(0, Protocol.TOUCH_EXTENT),
                y.coerceIn(0, Protocol.TOUCH_EXTENT),
            )
        }

        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN ->
                send(Protocol.TOUCH_DOWN, event.actionIndex)
            MotionEvent.ACTION_MOVE ->
                for (index in 0 until event.pointerCount) send(Protocol.TOUCH_MOVE, index)
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP ->
                send(Protocol.TOUCH_UP, event.actionIndex)
            MotionEvent.ACTION_CANCEL -> stream.releaseTouches()
        }
        return true
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) goImmersive()
    }

    @Suppress("DEPRECATION")
    private fun goImmersive() {
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_FULLSCREEN
                or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
            )
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        // The stream outlives individual surfaces. Only the render target is
        // swapped here — creating a stream per surface races two instances for
        // the single process-wide socket name.
        stream?.setSurface(holder.surface)
        // Surfaces are recreated often (see VideoStream.setSurface); keep a
        // single refresh loop rather than starting another each time.
        status.removeCallbacks(refreshStatusTask)
        status.postDelayed(refreshStatusTask, 1000)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        stream?.setSurface(holder.surface)
        // The surface is laid out at the new size, unlike getRealSize, which
        // can still return the old orientation while the rotation settles.
        reportDisplaySize(width, height)
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        // The system bars come back on rotation and shift the layout.
        goImmersive()
    }

    /** The host sizes its virtual output to this, so it follows rotation. */
    private fun reportDisplaySize(width: Int, height: Int) {
        if (width <= 0 || height <= 0) return
        if (reportedSize == width to height) return
        reportedSize = width to height
        stream?.reportDisplaySize(width, height)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        // The view no longer receives the matching ups for contacts in flight.
        stream?.releaseTouches()
        stream?.setSurface(null)
    }

    override fun onResume() {
        super.onResume()
        // Keeps the radio out of power-save while the stream is on screen.
        // The low-latency mode only takes effect in the foreground.
        wifiLock?.takeIf { !it.isHeld }?.acquire()
    }

    override fun onPause() {
        wifiLock?.takeIf { it.isHeld }?.release()
        super.onPause()
    }

    override fun onDestroy() {
        status.removeCallbacks(refreshStatusTask)
        stream?.stop()
        stream = null
        super.onDestroy()
    }

    private fun refreshStatus() {
        val stream = this.stream ?: return
        if (stream.framesDecoded > 0) {
            status.visibility = View.GONE
        } else {
            stream.lastError?.let { status.text = "Waiting for host…\n\nLast error: $it" }
            status.postDelayed(refreshStatusTask, 1000)
        }
    }

    private companion object {
        const val MATCH = FrameLayout.LayoutParams.MATCH_PARENT
        const val WRAP = FrameLayout.LayoutParams.WRAP_CONTENT
    }
}
