package com.sidelens.app

import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.AutomaticGainControl
import android.media.audiofx.NoiseSuppressor
import android.os.Build
import android.util.Log
import java.nio.ByteBuffer
import java.nio.ByteOrder

/** Captures 20 ms mono microphone blocks and encodes them with the platform codec. */
class AudioCapture(
    private val sender: FrameSender,
    private val raw: Boolean,
    private val onFailure: (String) -> Unit = {},
) {
    private var record: AudioRecord? = null
    private var codec: MediaCodec? = null
    private var effects = emptyList<android.media.audiofx.AudioEffect>()
    private var thread: Thread? = null
    @Volatile private var running = false
    private var codecId = 0

    fun start() {
        if (running) return
        val useOpus = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q
        codecId = if (useOpus) 0 else 1
        val mime = if (useOpus) MediaFormat.MIMETYPE_AUDIO_OPUS else MediaFormat.MIMETYPE_AUDIO_AAC
        val source = if (raw) MediaRecorder.AudioSource.MIC else MediaRecorder.AudioSource.VOICE_COMMUNICATION
        val minBytes = AudioRecord.getMinBufferSize(SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
        require(minBytes > 0) { "设备不支持 48 kHz 单声道录音" }
        val rec = AudioRecord(
            source, SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT,
            maxOf(minBytes, FRAME_BYTES * 4),
        )
        require(rec.state == AudioRecord.STATE_INITIALIZED) { "初始化麦克风失败" }

        val format = MediaFormat.createAudioFormat(mime, SAMPLE_RATE, CHANNELS).apply {
            setInteger(MediaFormat.KEY_BIT_RATE, if (useOpus) 32_000 else 64_000)
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, FRAME_BYTES * 4)
            setInteger(MediaFormat.KEY_PCM_ENCODING, AudioFormat.ENCODING_PCM_16BIT)
            if (!useOpus) setInteger(MediaFormat.KEY_AAC_PROFILE, MediaCodecInfo.CodecProfileLevel.AACObjectLC)
        }
        val enc = MediaCodec.createEncoderByType(mime)
        enc.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        enc.start()
        rec.startRecording()
        record = rec
        codec = enc
        effects = if (raw) emptyList() else enableVoiceEffects(rec.audioSessionId)
        running = true
        thread = Thread({ captureLoop(rec, enc) }, "SideLens-Microphone").apply {
            isDaemon = true
            start()
        }
    }

    private fun captureLoop(rec: AudioRecord, enc: MediaCodec) {
        val samples = ShortArray(FRAME_SAMPLES)
        val bytes = ByteBuffer.allocate(FRAME_BYTES).order(ByteOrder.LITTLE_ENDIAN)
        val info = MediaCodec.BufferInfo()
        var sampleCount = 0L
        try {
            while (running) {
                val count = rec.read(samples, 0, samples.size, AudioRecord.READ_BLOCKING)
                if (count <= 0) continue
                val inputIndex = enc.dequeueInputBuffer(CODEC_TIMEOUT_US)
                if (inputIndex >= 0) {
                    bytes.clear()
                    for (i in 0 until count) bytes.putShort(samples[i])
                    val input = enc.getInputBuffer(inputIndex)
                    if (input != null) {
                        input.clear()
                        input.put(bytes.array(), 0, count * 2)
                        val ptsUs = sampleCount * 1_000_000L / SAMPLE_RATE
                        sampleCount += count
                        enc.queueInputBuffer(inputIndex, 0, count * 2, ptsUs, 0)
                    } else {
                        enc.queueInputBuffer(inputIndex, 0, 0, 0, 0)
                    }
                }
                drainEncoder(enc, info)
            }
        } catch (e: Exception) {
            if (running) {
                Log.e(TAG, "麦克风采集停止", e)
                onFailure(e.message ?: e.javaClass.simpleName)
            }
        }
    }

    private fun drainEncoder(enc: MediaCodec, info: MediaCodec.BufferInfo) {
        while (true) {
            when (val index = enc.dequeueOutputBuffer(info, 0)) {
                MediaCodec.INFO_TRY_AGAIN_LATER -> return
                MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val format = enc.outputFormat
                    sender.sendAudioConfig(codecId, SAMPLE_RATE, CHANNELS, codecData(format))
                }
                else -> if (index >= 0) {
                    val output = enc.getOutputBuffer(index)
                    val codecConfig = info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0
                    if (output != null && info.size > 0 && !codecConfig) {
                        val packet = ByteArray(info.size)
                        output.position(info.offset)
                        output.get(packet)
                        sender.sendAudio(packet, info.presentationTimeUs)
                    }
                    enc.releaseOutputBuffer(index, false)
                }
            }
        }
    }

    private fun codecData(format: MediaFormat): ByteArray {
        val parts = listOfNotNull(
            format.getByteBuffer("csd-0")?.duplicate(),
            format.getByteBuffer("csd-1")?.duplicate(),
        ).map { buffer -> ByteArray(buffer.remaining()).also { buffer.get(it) } }
        return parts.fold(ByteArray(0)) { all, part -> all + part }
    }

    private fun enableVoiceEffects(sessionId: Int): List<android.media.audiofx.AudioEffect> {
        val active = mutableListOf<android.media.audiofx.AudioEffect>()
        fun attach(available: Boolean, create: () -> android.media.audiofx.AudioEffect?) {
            if (!available) return
            try {
                create()?.also { it.enabled = true; active += it }
            } catch (e: Exception) {
                Log.w(TAG, "启用语音音效失败: ${e.message}")
            }
        }
        attach(AcousticEchoCanceler.isAvailable()) { AcousticEchoCanceler.create(sessionId) }
        attach(NoiseSuppressor.isAvailable()) { NoiseSuppressor.create(sessionId) }
        attach(AutomaticGainControl.isAvailable()) { AutomaticGainControl.create(sessionId) }
        return active
    }

    fun stop() {
        running = false
        try { record?.stop() } catch (_: Exception) {}
        thread?.join(500)
        thread = null
        effects.forEach { try { it.release() } catch (_: Exception) {} }
        effects = emptyList()
        try { codec?.stop() } catch (_: Exception) {}
        codec?.release()
        codec = null
        record?.release()
        record = null
    }

    companion object {
        private const val TAG = "SideLens.Audio"
        private const val SAMPLE_RATE = 48_000
        private const val CHANNELS = 1
        private const val FRAME_SAMPLES = 960
        private const val FRAME_BYTES = FRAME_SAMPLES * 2
        private const val CODEC_TIMEOUT_US = 10_000L
    }
}
