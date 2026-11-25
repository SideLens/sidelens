package com.sidelens.app

import android.annotation.SuppressLint
import android.content.Context
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.params.StreamConfigurationMap
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface

/**
 * Camera2 采集。
 *
 * # 为什么直接输出到 MediaCodec 的 input Surface
 *
 * 摄像头帧的默认路径是 `ImageReader` → CPU 内存 → 编码器。
 * 那条路要走一次 YUV 拷贝（1280x720 NV12 约 1.3 MB/帧，30fps = 40 MB/s），
 * 而且会触发 CPU-GPU 同步。
 *
 * 这里用 `createCaptureSession` 直接把 `MediaCodec.createInputSurface()`
 * 当作输出目标 —— 帧从摄像头 HAL 直接进硬件编码器，**零拷贝**。
 * 这是 Android 相机录视频的标准做法。
 *
 * 代价：拿不到 YUV 数据（编码前无法做图像处理）。当前不需要。
 */
class CameraCapture(
    private val context: Context,
    private val cameraId: String,
    private val width: Int,
    private val height: Int,
    private val fps: Int,
    private val outputSurface: Surface,
    private val onSessionReady: () -> Unit = {},
) {

    private var device: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var thread: HandlerThread? = null
    private var handler: Handler? = null

    @SuppressLint("MissingPermission")
    fun start() {
        val t = HandlerThread("camera").also { it.start() }
        thread = t
        handler = Handler(t.looper)

        val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
        val chars = manager.getCameraCharacteristics(cameraId)

        // 确认这个尺寸真的被支持，否则 createCaptureSession 会静默失败
        val map = chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
            as? StreamConfigurationMap
        val sizes = map?.getOutputSizes(android.graphics.SurfaceTexture::class.java) ?: emptyArray()
        Log.i(TAG, "摄像头 $cameraId 支持尺寸: ${sizes.joinToString { "${it.width}x${it.height}" }}")

        manager.openCamera(cameraId, object : CameraDevice.StateCallback() {
            override fun onOpened(cam: CameraDevice) {
                device = cam
                createSession(cam)
            }

            override fun onDisconnected(cam: CameraDevice) {
                Log.w(TAG, "摄像头断开")
                cam.close()
                device = null
            }

            override fun onError(cam: CameraDevice, error: Int) {
                Log.e(TAG, "摄像头错误: $error")
                cam.close()
                device = null
            }
        }, handler)
    }

    private fun createSession(cam: CameraDevice) {
        val request = cam.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
            addTarget(outputSurface)
            set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO)
            set(CaptureRequest.CONTROL_AE_MODE, CaptureRequest.CONTROL_AE_MODE_ON)
            // 30fps 固定，避免自动帧率在暗光下降到 15fps 导致接收端抖动缓冲反复调整
            set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, android.util.Range(fps, fps))
        }

        cam.createCaptureSession(
            listOf(outputSurface),
            object : CameraCaptureSession.StateCallback() {
                override fun onConfigured(s: CameraCaptureSession) {
                    session = s
                    s.setRepeatingRequest(request.build(), null, handler)
                    Log.i(TAG, "采集已启动 ${width}x${height}@${fps}")
                    onSessionReady()
                }

                override fun onConfigureFailed(s: CameraCaptureSession) {
                    Log.e(TAG, "createCaptureSession 失败")
                }
            },
            handler,
        )
    }

    fun stop() {
        session?.close(); session = null
        device?.close(); device = null
        thread?.quitSafely(); thread = null
        handler = null
    }

    companion object {
        private const val TAG = "SideLens.Camera"
    }
}
