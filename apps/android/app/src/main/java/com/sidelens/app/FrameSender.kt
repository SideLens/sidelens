package com.sidelens.app

import android.util.Log
import org.json.JSONObject
import java.io.BufferedOutputStream
import java.net.InetSocketAddress
import java.net.Socket

/** Sends versioned phone-link messages and receives desktop controls on TCP's reverse direction. */
class FrameSender {

    private var socket: Socket? = null
    private var out: BufferedOutputStream? = null
    @Volatile private var controlReader: Thread? = null

    /**
     * 连接电脑。
     *
     * # 为什么要重试
     *
     * 手机点「开始共享」时，电脑端的 `sidelensd --phone-recv` 可能还没启动，
     * 或者 adb reverse 刚建好还没生效。第一次连不上很正常 —— 重试几次比
     * 让用户重开 App 友好。
     *
     * 注意：**必须在后台线程调用**，Android 主线程禁止网络操作。
     */
    fun connect(host: String, port: Int, retries: Int = 15) {
        var lastErr: Exception? = null
        for (i in 0 until retries) {
            try {
                val s = Socket()
                s.tcpNoDelay = true      // 关键：否则 Nagle 会攒 40ms
                s.connect(InetSocketAddress(host, port), 2000)
                socket = s
                out = BufferedOutputStream(s.getOutputStream(), 256 * 1024)
                Log.i(TAG, "已连接 $host:$port（第 ${i + 1} 次尝试）")
                return
            } catch (e: Exception) {
                lastErr = e
                Log.w(TAG, "连接失败（第 ${i + 1}/$retries 次）: ${e.message}")
                Thread.sleep(1000)
            }
        }
        throw lastErr ?: IllegalStateException("连接失败")
    }

    @Synchronized
    fun send(data: ByteArray, ptsUs: Long, isKey: Boolean) {
        val o = out ?: return
        try {
            PhoneProtocol.writeMessage(o, PhoneProtocol.FRAME, PhoneProtocol.framePayload(data, ptsUs, isKey))
            o.flush()
        } catch (e: Exception) {
            Log.e(TAG, "发送失败: ${e.message}")
            close()
        }
    }

    @Synchronized
    fun sendHello(json: String) = sendMessage(PhoneProtocol.HELLO, json.toByteArray(Charsets.UTF_8))

    @Synchronized
    fun sendConfig(width: Int, height: Int, codecData: ByteArray) = sendMessage(
        PhoneProtocol.CONFIG,
        PhoneProtocol.configPayload(width, height, codecData),
    )

    @Synchronized
    fun sendState(json: String) = sendMessage(PhoneProtocol.STATE, json.toByteArray(Charsets.UTF_8))

    /**
     * 把版本信息并进 STATE。
     *
     * 单独一个方法而不是让调用方自己拼：版本字段的**形状**是跨端契约
     * （见 crates/sidelens-update/src/phone.rs 里的三种历史形态），
     * 分散到调用方迟早会有人写成 "version": "0.2.0" 然后电脑端读不到 code。
     */
    @Synchronized
    fun sendStateWithVersion(base: JSONObject, versionCode: Long, versionName: String) {
        val withVersion = JSONObject(base.toString())
        withVersion.put(
            "version",
            JSONObject()
                .put("code", versionCode)
                .put("name", versionName)
                .put("protocol", PhoneProtocol.VERSION),
        )
        // 顶层也放一份 protocol：老电脑端只认顶层字段时仍能判协议兼容性
        withVersion.put("protocol", PhoneProtocol.VERSION)
        sendState(withVersion.toString())
    }

    @Synchronized
    fun sendAudioConfig(codec: Int, sampleRate: Int, channels: Int, codecData: ByteArray) = sendMessage(
        PhoneProtocol.AUDIO_CONFIG,
        PhoneProtocol.audioConfigPayload(codec, sampleRate, channels, codecData),
    )

    @Synchronized
    fun sendAudio(data: ByteArray, ptsUs: Long) = sendMessage(
        PhoneProtocol.AUDIO,
        PhoneProtocol.audioPayload(data, ptsUs),
    )

    private fun sendMessage(type: Int, payload: ByteArray) {
        val o = out ?: return
        try {
            PhoneProtocol.writeMessage(o, type, payload)
            o.flush()
        } catch (e: Exception) {
            Log.e(TAG, "协议消息发送失败: ${e.message}")
            close()
        }
    }

    /** Read newline-delimited desktop camera controls from the TCP reverse channel. */
    fun startControlReader(onControl: (JSONObject) -> Unit) {
        val activeSocket = socket ?: return
        val thread = Thread({
            try {
                activeSocket.getInputStream().use { input ->
                    while (!activeSocket.isClosed) {
                        val message = PhoneProtocol.readMessage(input) ?: break
                        if (message.type == PhoneProtocol.CONTROL) {
                            try {
                                onControl(JSONObject(String(message.payload, Charsets.UTF_8)))
                            } catch (e: Exception) {
                                Log.w(TAG, "忽略无效控制消息: ${e.message}")
                            }
                        }
                    }
                }
            } catch (e: Exception) {
                if (activeSocket.isClosed.not()) Log.w(TAG, "控制通道已断开: ${e.message}")
            }
        }, "SideLens-Control").apply { isDaemon = true }
        controlReader = thread
        thread.start()
    }

    fun close() {
        try {
            out?.close()
            socket?.close()
        } catch (_: Exception) {
        }
        out = null
        socket = null
    }

    companion object {
        private const val TAG = "SideLens.Sender"
    }
}