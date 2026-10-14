// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.media.MediaCodec
import android.media.MediaFormat
import android.net.LocalServerSocket
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.Build
import android.util.Log
import android.view.Surface
import java.io.DataInputStream
import java.io.IOException
import java.io.OutputStream
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * Accepts one host connection at a time on an abstract Unix socket, decodes the
 * H.264 stream, and renders to whatever surface is currently attached.
 *
 * The host reaches this socket via `adb forward tcp:27183 localabstract:<name>`,
 * so nothing here touches Android's TCP stack or `netd`. The daemon picks a
 * fresh name per session, and only adbd's own connections are accepted.
 *
 * **Exactly one instance may exist per process.** The socket name is a
 * process-wide resource: a second instance cannot bind it and will spin on
 * `Address already in use` while the first — possibly holding a stale surface —
 * keeps answering connections. So the surface is a *property* of this object,
 * swapped as the view's surface comes and goes, rather than a constructor
 * argument that would tie the stream's lifetime to the surface's.
 */
class VideoStream(private val socketName: String) {

    private companion object {
        const val TAG = "Moreland"
        const val INPUT_TIMEOUT_MS = 2000L
        const val SURFACE_WAIT_MS = 5000L
        const val MAX_QUEUED_MOVES = 256
    }

    @Volatile private var running = false
    @Volatile private var surface: Surface? = null
    /** The live decoder, so its output surface can be swapped in place. */
    @Volatile private var codec: MediaCodec? = null
    private var serverSocket: LocalServerSocket? = null
    private var client: LocalSocket? = null
    private var thread: Thread? = null
    private var ackThread: Thread? = null

    private val freeInputs = LinkedBlockingQueue<Int>()
    /** Encoded device-to-host messages, written by one thread so they never interleave. */
    private val outgoing = LinkedBlockingQueue<ByteArray>()
    /** Latest display size, resent at the start of every connection. */
    @Volatile private var displaySize: Pair<Int, Int>? = null

    /** Contacts the host believes are down, so they can be lifted if the view goes away. */
    private val activeTouches = BooleanArray(Protocol.MAX_TOUCH_SLOTS)

    /** True only while a connection is being served; touches are dropped otherwise. */
    @Volatile private var connected = false

    @Volatile var framesDecoded: Long = 0; private set
    @Volatile var lastError: String? = null; private set

    /**
     * Attach or detach the render target. Safe to call at any time.
     *
     * Surfaces churn far more than you would expect — immersive-sticky mode
     * recreates them whenever the user touches the screen and the system bars
     * flash in and out. Tearing the session down for that meant a single tap on
     * the tablet killed the stream, so instead the decoder's output surface is
     * swapped in place with `setOutputSurface`, and the session survives.
     *
     * Detaching does **not** close the socket. Frames still arrive and are
     * decoded; they are simply discarded instead of rendered until a surface
     * comes back.
     */
    fun setSurface(surface: Surface?) {
        this.surface = surface
        val codec = this.codec ?: return
        if (surface != null && surface.isValid) {
            runCatching { codec.setOutputSurface(surface) }
                .onFailure { Log.w(TAG, "setOutputSurface failed: ${it.message}") }
        }
    }

    /**
     * Forward one contact to the host. Safe from any thread; never blocks.
     *
     * [x] and [y] are already normalised to `0..Protocol.TOUCH_EXTENT` across
     * the streamed image.
     */
    fun sendTouch(action: Int, slot: Int, x: Int, y: Int) {
        if (!connected || slot !in 0 until Protocol.MAX_TOUCH_SLOTS) return
        synchronized(activeTouches) {
            when (action) {
                Protocol.TOUCH_DOWN -> activeTouches[slot] = true
                Protocol.TOUCH_UP -> {
                    if (!activeTouches[slot]) return
                    activeTouches[slot] = false
                }
                else -> if (!activeTouches[slot]) return
            }
        }
        // A move is superseded by the next one. Shed them rather than let a
        // stalled socket build a backlog that then plays back as lag.
        if (action == Protocol.TOUCH_MOVE && outgoing.size > MAX_QUEUED_MOVES) return
        outgoing.offer(Protocol.encodeTouch(action, slot, x, y))
    }

    /** Lift every contact the host still thinks is down. */
    fun releaseTouches() {
        synchronized(activeTouches) {
            for (slot in activeTouches.indices) {
                if (activeTouches[slot]) {
                    activeTouches[slot] = false
                    outgoing.offer(Protocol.encodeTouch(Protocol.TOUCH_UP, slot, 0, 0))
                }
            }
        }
    }

    /**
     * Tell the host the display is now this size, so it can resize the virtual
     * output to match. Safe to call at any time; the latest size is also sent
     * when a host connects.
     */
    fun reportDisplaySize(width: Int, height: Int) {
        displaySize = width to height
        if (client != null) outgoing.offer(Protocol.encodeOrientation(width, height))
    }

    fun start() {
        if (running) return
        running = true
        thread = Thread(::acceptLoop, "moreland-stream").apply { start() }
    }

    fun stop() {
        running = false
        // Closing a LocalServerSocket does not reliably wake a thread parked
        // in accept(), and a thread left there keeps the socket name bound,
        // so a later instance in this process could never listen. A throwaway
        // connection does wake it: it fails the peer check, and the loop then
        // sees `running` is false. Connect before closing, or there is
        // nothing left to connect to.
        runCatching { LocalSocket().use { it.connect(LocalSocketAddress(socketName)) } }
        runCatching { client?.close() }
        runCatching { serverSocket?.close() }
        thread?.interrupt()
        ackThread?.interrupt()
        thread = null
        ackThread = null
    }

    private fun acceptLoop() {
        while (running) {
            try {
                LocalServerSocket(socketName).use { server ->
                    serverSocket = server
                    Log.i(TAG, "listening on localabstract:$socketName")
                    while (running) {
                        val socket = server.accept()
                        val uid = runCatching { socket.peerCredentials.uid }.getOrDefault(-1)
                        if (!Protocol.isTrustedPeer(uid)) {
                            Log.w(TAG, "refusing connection from uid $uid; only adb may connect")
                            runCatching { socket.close() }
                            continue
                        }
                        Log.i(TAG, "host connected")
                        client = socket
                        runCatching { session(socket) }
                            .onFailure {
                                lastError = it.message
                                Log.w(TAG, "session ended: ${it.message}")
                            }
                        runCatching { socket.close() }
                        client = null
                    }
                }
            } catch (e: Exception) {
                if (!running) return
                lastError = e.message
                Log.w(TAG, "accept loop error: ${e.message}")
                runCatching { Thread.sleep(500) }
            } finally {
                serverSocket = null
            }
        }
        Log.i(TAG, "accept loop exited")
    }

    /** Wait briefly for a usable surface; the host may connect before the view is ready. */
    private fun awaitSurface(): Surface {
        val deadline = System.currentTimeMillis() + SURFACE_WAIT_MS
        while (System.currentTimeMillis() < deadline) {
            val current = surface
            if (current != null && current.isValid) return current
            Thread.sleep(50)
        }
        throw IOException(
            "no valid surface after ${SURFACE_WAIT_MS}ms — " +
                "is the screen on and this app in the foreground?"
        )
    }

    private fun session(socket: LocalSocket) {
        val input = DataInputStream(socket.inputStream.buffered(256 * 1024))
        val output = socket.outputStream

        val header = Protocol.readStreamHeader(input)
        Log.i(TAG, "stream ${header.width}x${header.height}@${header.framerate} ${header.mime}")

        val target = awaitSurface()
        freeInputs.clear()
        outgoing.clear()
        // A new connection means a new injector on the host, with nothing down.
        synchronized(activeTouches) { activeTouches.fill(false) }

        var codec = configureCodec(header, target)
        this.codec = codec
        displaySize?.let { (w, h) -> outgoing.offer(Protocol.encodeOrientation(w, h)) }
        startAckWriter(output)
        connected = true

        try {
            var scratch = ByteArray(256 * 1024)
            while (running) {
                val frame = when (val next = Protocol.readMessage(input)) {
                    is Protocol.Message.Frame -> next.header
                    is Protocol.Message.Header -> {
                        // The host resized the picture; the next frame it
                        // sends is a keyframe from a new encoder.
                        val h = next.header
                        Log.i(TAG, "stream ${h.width}x${h.height}@${h.framerate} ${h.mime}")
                        this.codec = null
                        runCatching { codec.stop() }
                        runCatching { codec.release() }
                        freeInputs.clear()
                        codec = configureCodec(h, awaitSurface())
                        this.codec = codec
                        continue
                    }
                }
                if (frame.length > scratch.size) {
                    scratch = ByteArray(frame.length)
                }
                Protocol.readFully(input, scratch, frame.length)

                val index = freeInputs.poll(INPUT_TIMEOUT_MS, TimeUnit.MILLISECONDS)
                if (index == null) {
                    Log.w(TAG, "decoder starved of input buffers; dropping frame")
                    continue
                }
                val inputBuffer = codec.getInputBuffer(index) ?: continue
                inputBuffer.clear()
                inputBuffer.put(scratch, 0, frame.length)

                val flags = if (frame.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                // MediaCodec timestamps are microseconds; the wire uses nanoseconds.
                codec.queueInputBuffer(index, 0, frame.length, frame.ptsNs / 1000, flags)
            }
        } finally {
            connected = false
            this.codec = null
            runCatching { codec.stop() }
            runCatching { codec.release() }
            ackThread?.interrupt()
            ackThread = null
        }
    }

    /**
     * Low-latency hints, most to least aggressive. Some decoders reject a
     * format outright (`Error 0xfffffc0e`, ERROR_UNSUPPORTED) when a hint they
     * do not implement is present, so each attempt drops one and the first that
     * configures wins. A stream with no hints is slower but still correct.
     */
    private enum class Hints { LOW_LATENCY_AND_VENDOR, REALTIME_ONLY, NONE }

    private fun configureCodec(header: Protocol.StreamHeader, target: Surface): MediaCodec {
        var failure: Exception? = null
        for (hints in Hints.values()) {
            try {
                return configureCodec(header, target, hints)
            } catch (e: Exception) {
                Log.w(TAG, "decoder config with $hints failed: ${e.message}")
                failure = e
            }
        }
        throw failure ?: IOException("no decoder configuration succeeded")
    }

    private fun configureCodec(
        header: Protocol.StreamHeader,
        target: Surface,
        hints: Hints,
    ): MediaCodec {
        val format = MediaFormat.createVideoFormat(header.mime, header.width, header.height)

        if (hints == Hints.LOW_LATENCY_AND_VENDOR && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        if (hints != Hints.NONE) {
            // Realtime priority and an operating rate let decoders without a
            // low-latency mode (e.g. OMX.IMG.MSVDX, which rejects
            // KEY_LOW_LATENCY) stop scheduling for throughput over latency.
            format.setInteger(MediaFormat.KEY_PRIORITY, 0)
            format.setInteger(MediaFormat.KEY_OPERATING_RATE, header.framerate)
        }
        // This device exposes OMX.qcom decoders and declares no `low-latency`
        // feature, so KEY_LOW_LATENCY may be ignored. The Qualcomm vendor
        // extension is the fallback.
        if (hints == Hints.LOW_LATENCY_AND_VENDOR) {
            runCatching { format.setInteger("vendor.qti-ext-dec-low-latency.enable", 1) }
        }

        val codec = MediaCodec.createDecoderByType(header.mime)
        try {
            codec.setCallback(object : MediaCodec.Callback() {
                override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                    freeInputs.offer(index)
                }

                override fun onOutputBufferAvailable(
                    codec: MediaCodec,
                    index: Int,
                    info: MediaCodec.BufferInfo,
                ) {
                    // Render only while a surface is attached; otherwise release
                    // without rendering so the buffer is recycled rather than the
                    // session failing. Render first, account after; the
                    // acknowledgement is queued so this callback never blocks.
                    val render = surface?.isValid == true
                    runCatching { codec.releaseOutputBuffer(index, render) }
                    if (render) {
                        framesDecoded++
                        outgoing.offer(Protocol.encodeAck(info.presentationTimeUs * 1000))
                    }
                }

                override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                    lastError = e.message
                    Log.e(TAG, "codec error: ${e.message}", e)
                }

                override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
                    Log.i(TAG, "output format: $format")
                }
            })

            codec.configure(format, target, null, 0)
            codec.start()
        } catch (e: Exception) {
            runCatching { codec.release() }
            throw e
        }
        Log.i(TAG, "decoder started: ${codec.name} ($hints)")
        return codec
    }

    /**
     * Sends acknowledgements and touches on a dedicated thread so a slow or
     * blocked write can never stall a decoder callback or the UI thread.
     */
    private fun startAckWriter(output: OutputStream) {
        ackThread = Thread({
            try {
                while (running && !Thread.currentThread().isInterrupted) {
                    val message = outgoing.poll(500, TimeUnit.MILLISECONDS) ?: continue
                    output.write(message)
                    output.flush()
                }
            } catch (e: Exception) {
                Log.w(TAG, "ack writer stopped: ${e.message}")
            }
        }, "moreland-ack").apply { start() }
    }
}
