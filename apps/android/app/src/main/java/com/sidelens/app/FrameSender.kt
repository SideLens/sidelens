package com.sidelens.app

import android.util.Log
import java.io.BufferedOutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * 把编码后的帧发到电脑。
 *
 * # 为什么先用裸 TCP 而不是复用 UDP 协议
 *
 * 现有 `sidelens-transport` 的 UDP 协议带分片、NACK、FEC，是一整套东西。
 * 但**手机端目前只需要验证「摄像头 → 编码 → 网络 → 电脑」这条链**，
 * 不需要弱网对抗。裸 TCP 能在半天内跑通，而且丢包由内核重传兜住。
 *
 * 等链路验证完，再把这层换成 `sidelens-proto` 的 UDP 分片 —— 那时
 * 才有必要处理 NACK 与 FEC（它们是为丢包设计的，不是为「先跑通」设计的）。
 *
 * # 线格式
 *
 * ```text
 * [4B 长度 BE][8B ptsUs BE][1B flags][N B H.264 数据]
 * ```
 *
 * flags: bit0 = 关键帧。
 */
class FrameSender {

    private var socket: Socket? = null
    private var out: BufferedOutputStream? = null

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
            val header = ByteBuffer.allocate(HEADER_SIZE).order(ByteOrder.BIG_ENDIAN)
            header.putInt(data.size)
            header.putLong(ptsUs)
            header.put(if (isKey) 1 else 0)
            o.write(header.array())
            o.write(data)
            // 每帧 flush：实时视频不能等缓冲满
            o.flush()
        } catch (e: Exception) {
            Log.e(TAG, "发送失败: ${e.message}")
            close()
        }
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
        private const val HEADER_SIZE = 4 + 8 + 1
    }
}
