// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import java.io.DataInputStream
import java.io.EOFException
import java.io.IOException

/**
 * Mirror of `crates/protocol`.
 *
 * Every multi-byte field on the wire is big-endian, which is precisely why
 * [DataInputStream] can be used directly here — no byte-swapping anywhere.
 */
object Protocol {
    /** Used only when launched by hand; the daemon passes [SOCKET_EXTRA]. */
    const val SOCKET_NAME = "moreland"

    /** Intent extra carrying this session's socket name from the daemon. */
    const val SOCKET_EXTRA = "com.moreland.display.SOCKET"

    private val SOCKET_NAME_PATTERN = Regex("[A-Za-z0-9._-]{1,64}")

    /** The socket named in the launch intent, or the default when absent or malformed. */
    fun socketName(extra: String?): String =
        extra?.takeIf { SOCKET_NAME_PATTERN.matches(it) } ?: SOCKET_NAME

    /**
     * `adb forward` connections arrive from adbd, which runs as `shell`, or
     * as `root` after `adb root`. Any other peer is another app on this
     * device and must not be able to drive the display.
     */
    private const val SHELL_UID = 2000
    private const val ROOT_UID = 0

    fun isTrustedPeer(uid: Int): Boolean = uid == SHELL_UID || uid == ROOT_UID

    private const val MAGIC = 0x4D524C44 // "MRLD"
    const val VERSION = 3

    const val STREAM_HEADER_LEN = 16
    const val FRAME_HEADER_LEN = 16
    const val ACK_LEN = 1 + 8

    const val FLAG_KEYFRAME = 0x01

    /** Refuse absurd frame lengths rather than attempting the allocation. */
    const val MAX_FRAME_BYTES = 8 * 1024 * 1024

    data class StreamHeader(
        val width: Int,
        val height: Int,
        val framerate: Int,
        val mime: String,
    )

    data class FrameHeader(
        val length: Int,
        val ptsNs: Long,
        val keyframe: Boolean,
    )

    /** What the host sent next: a new picture size, or one access unit. */
    sealed class Message {
        data class Header(val header: StreamHeader) : Message()
        data class Frame(val header: FrameHeader) : Message()
    }

    /**
     * Read the next header. A stream header can arrive between frames when the
     * picture size changes; its magic, read as a length, exceeds
     * [MAX_FRAME_BYTES], so the first four bytes tell the two apart.
     */
    @Throws(IOException::class)
    fun readMessage(input: DataInputStream): Message {
        val first = input.readInt()
        return if (first == MAGIC) {
            Message.Header(readStreamHeaderBody(input))
        } else {
            Message.Frame(readFrameHeaderBody(input, first))
        }
    }

    @Throws(IOException::class)
    fun readStreamHeader(input: DataInputStream): StreamHeader {
        val magic = input.readInt()
        if (magic != MAGIC) {
            throw IOException("bad magic 0x%08x".format(magic))
        }
        return readStreamHeaderBody(input)
    }

    private fun readStreamHeaderBody(input: DataInputStream): StreamHeader {
        val version = input.readUnsignedShort()
        if (version != VERSION) {
            throw IOException("unsupported protocol version $version (expected $VERSION)")
        }
        val width = input.readUnsignedShort()
        val height = input.readUnsignedShort()
        val framerate = input.readUnsignedShort()
        val codec = input.readUnsignedByte()
        input.skipBytes(3) // reserved

        val mime = when (codec) {
            0 -> "video/avc"
            1 -> "video/hevc"
            else -> throw IOException("unknown codec id $codec")
        }
        return StreamHeader(width, height, framerate, mime)
    }

    private fun readFrameHeaderBody(input: DataInputStream, length: Int): FrameHeader {
        if (length < 0 || length > MAX_FRAME_BYTES) {
            throw IOException("implausible frame length $length")
        }
        val ptsNs = input.readLong()
        val flags = input.readUnsignedByte()
        input.skipBytes(3) // reserved
        return FrameHeader(length, ptsNs, flags and FLAG_KEYFRAME != 0)
    }

    @Throws(IOException::class)
    fun readFully(input: DataInputStream, buffer: ByteArray, length: Int) {
        var read = 0
        while (read < length) {
            val n = input.read(buffer, read, length - read)
            if (n < 0) throw EOFException("stream closed after $read of $length bytes")
            read += n
        }
    }

    /** Tag byte introducing each device → host message. */
    const val TAG_ACK = 0x01
    const val TAG_TOUCH = 0x02
    const val TAG_ORIENTATION = 0x03

    /** Touch coordinates run 0..TOUCH_EXTENT across the streamed image. */
    const val TOUCH_EXTENT = 65535

    /** Simultaneous contacts the host accepts; higher pointer ids are ignored. */
    const val MAX_TOUCH_SLOTS = 10

    const val TOUCH_DOWN = 0
    const val TOUCH_MOVE = 1
    const val TOUCH_UP = 2

    /** Tag, action, slot, x u16, y u16 — mirrors `protocol::TouchEvent::encode`. */
    fun encodeTouch(action: Int, slot: Int, x: Int, y: Int): ByteArray = byteArrayOf(
        TAG_TOUCH.toByte(),
        action.toByte(),
        slot.toByte(),
        (x ushr 8).toByte(),
        x.toByte(),
        (y ushr 8).toByte(),
        y.toByte(),
    )

    fun encodeAck(ptsNs: Long): ByteArray = byteArrayOf(
        TAG_ACK.toByte(),
        (ptsNs ushr 56).toByte(),
        (ptsNs ushr 48).toByte(),
        (ptsNs ushr 40).toByte(),
        (ptsNs ushr 32).toByte(),
        (ptsNs ushr 24).toByte(),
        (ptsNs ushr 16).toByte(),
        (ptsNs ushr 8).toByte(),
        ptsNs.toByte(),
    )

    /** Tell the host the display is now `width` x `height` (it was rotated). */
    fun encodeOrientation(width: Int, height: Int): ByteArray = byteArrayOf(
        TAG_ORIENTATION.toByte(),
        (width ushr 8).toByte(),
        width.toByte(),
        (height ushr 8).toByte(),
        height.toByte(),
    )
}
