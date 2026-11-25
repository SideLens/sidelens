package com.sidelens.app

import java.io.DataInputStream
import java.io.EOFException
import java.io.InputStream
import java.io.OutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/** Wire framing shared with crates/sidelens-phone-proto. */
object PhoneProtocol {
    const val HELLO = 0x01
    const val CONFIG = 0x02
    const val FRAME = 0x03
    const val STATE = 0x04
    const val AUDIO_CONFIG = 0x05
    const val AUDIO = 0x06
    const val CONTROL = 0x10
    const val PING = 0x11
    const val MAX_MESSAGE_SIZE = 8 * 1024 * 1024

    /**
     * 手机链路协议版本。必须与 crates/sidelens-phone-proto 的 VERSION 一致。
     *
     * 电脑端用清单里的 phone.min_supported_protocol 与它比对：低于下限时
     * 直接提示"协议不兼容，请升级手机端"，而不是让用户面对一个
     * 连上了但画面永远不动的界面。
     *
     * 一致性由两侧测试守住：
     *   - Rust: crates/sidelens-phone-proto/tests/protocol_version.rs
     *   - Android: app/src/test/.../PhoneProtocolTest.kt
     */
    const val VERSION = 1

    data class Message(val type: Int, val payload: ByteArray)

    fun writeMessage(output: OutputStream, type: Int, payload: ByteArray) {
        require(payload.size <= MAX_MESSAGE_SIZE) { "phone protocol message too large" }
        val header = ByteBuffer.allocate(5).order(ByteOrder.LITTLE_ENDIAN)
            .put(type.toByte())
            .putInt(payload.size)
            .array()
        output.write(header)
        output.write(payload)
    }

    fun readMessage(input: InputStream): Message? {
        val stream = if (input is DataInputStream) input else DataInputStream(input)
        val type = try {
            stream.readUnsignedByte()
        } catch (_: EOFException) {
            return null
        }
        val lenBytes = ByteArray(4)
        stream.readFully(lenBytes)
        val length = ByteBuffer.wrap(lenBytes).order(ByteOrder.LITTLE_ENDIAN).int
        require(length in 0..MAX_MESSAGE_SIZE) { "invalid phone message length $length" }
        val payload = ByteArray(length)
        stream.readFully(payload)
        return Message(type, payload)
    }

    fun configPayload(width: Int, height: Int, codecData: ByteArray): ByteArray {
        require(width in 1..65535 && height in 1..65535)
        val header = ByteBuffer.allocate(5).order(ByteOrder.LITTLE_ENDIAN)
            .put(0.toByte()) // H.264
            .putShort(width.toShort())
            .putShort(height.toShort())
            .array()
        return header + codecData
    }

    fun framePayload(data: ByteArray, ptsUs: Long, isKey: Boolean): ByteArray {
        val header = ByteBuffer.allocate(9).order(ByteOrder.LITTLE_ENDIAN)
            .putLong(ptsUs)
            .put(if (isKey) 1.toByte() else 0.toByte())
            .array()
        return header + data
    }

    fun audioConfigPayload(codec: Int, sampleRate: Int, channels: Int, codecData: ByteArray): ByteArray {
        val header = ByteBuffer.allocate(6).order(ByteOrder.LITTLE_ENDIAN)
            .put(codec.toByte())
            .putInt(sampleRate)
            .put(channels.toByte())
            .array()
        return header + codecData
    }

    fun audioPayload(data: ByteArray, ptsUs: Long): ByteArray {
        val header = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(ptsUs).array()
        return header + data
    }
}