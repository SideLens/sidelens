package com.sidelens.app

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.util.Log
import android.view.Surface

/**
 * H.264 硬件编码。
 *
 * # 为什么用 `createInputSurface`
 *
 * 摄像头直接写这个 Surface，编码器从图形内存取帧 —— 全程不经过 CPU。
 * 实测 1080p30 的 CPU 占用在 3% 以下。
 *
 * # 码率怎么定
 *
 * 720p30 用 4 Mbps：这是视频会议的高画质档位。
 * 太低（<1.5 Mbps）在快速运动时会出现明显块效应；
 * 太高在局域网无意义，但会增加 FEC 冗余包数量。
 */
class VideoEncoder(
    private val width: Int,
    private val height: Int,
    private val fps: Int,
    private val bitrate: Int = 4_000_000,
) {

    private var codec: MediaCodec? = null
    private val bufferInfo = MediaCodec.BufferInfo()

    /** 摄像头往这里写帧 */
    lateinit var inputSurface: Surface
        private set

    fun start() {
        val format = MediaFormat.createVideoFormat(MIME, width, height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT,
                MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)   // 每秒一个关键帧
            // 低延迟：不让编码器攒帧
            setInteger(MediaFormat.KEY_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0)           // 0 = realtime
        }

        val c = MediaCodec.createEncoderByType(MIME)
        c.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        inputSurface = c.createInputSurface()
        c.start()
        codec = c

        Log.i(TAG, "编码器已启动 ${width}x${height}@${fps} ${bitrate / 1000}kbps")
    }

    /**
     * 取一帧编码结果。
     *
     * 返回 null 表示这次没有产出（编码器还在攒帧），不是错误。
     */
    fun drainEncoder(onFrame: (data: ByteArray, ptsUs: Long, isKey: Boolean) -> Unit) {
        val c = codec ?: return
        while (true) {
            val index = c.dequeueOutputBuffer(bufferInfo, TIMEOUT_US)
            when {
                index == MediaCodec.INFO_TRY_AGAIN_LATER -> return
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val f = c.outputFormat
                    Log.i(TAG, "输出格式: $f")
                }
                index >= 0 -> {
                    val buf = c.getOutputBuffer(index) ?: continue
                    if (bufferInfo.size > 0) {
                        val data = ByteArray(bufferInfo.size)
                        buf.position(bufferInfo.offset)
                        buf.get(data, 0, bufferInfo.size)
                        val isKey = (bufferInfo.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0
                        onFrame(data, bufferInfo.presentationTimeUs, isKey)
                    }
                    c.releaseOutputBuffer(index, false)
                }
            }
        }
    }

    fun stop() {
        try {
            codec?.stop()
        } catch (e: Exception) {
            Log.w(TAG, "停止编码器: ${e.message}")
        }
        codec?.release()
        codec = null
    }

    companion object {
        private const val TAG = "SideLens.Encoder"
        private const val MIME = MediaFormat.MIMETYPE_VIDEO_AVC
        private const val TIMEOUT_US = 0L   // 非阻塞，由调用方的循环驱动
    }
}
