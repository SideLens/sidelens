package com.sidelens.app

import android.annotation.SuppressLint
import android.content.Context
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.CaptureResult
import android.hardware.camera2.TotalCaptureResult
import android.hardware.camera2.params.MeteringRectangle
import android.hardware.camera2.params.StreamConfigurationMap
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.os.Build
import android.graphics.Rect
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
    private var requestBuilder: CaptureRequest.Builder? = null
    private var characteristics: CameraCharacteristics? = null
    @Volatile private var zoom = 1f
    @Volatile private var torch = false
    @Volatile private var manualFocus = false
    @Volatile private var focusDistance = 0f
    @Volatile private var exposureLocked = false
    @Volatile private var exposureCompensation = 0
    @Volatile private var whiteBalanceLocked = false
    @Volatile private var stabilization = false
    @Volatile private var faceExposure = false
    @Volatile private var faceExposureRegion: MeteringRectangle? = null
    @Volatile private var paused = false
    @Volatile private var fpsLimit = fps
    @Volatile private var appliedFpsLimit = fps
    private var cameraDevice: CameraDevice? = null
    /** 当前被相机接受的请求组（真机上可能小于全集，见 startRepeating） */
    private var acceptedGroups: Set<KeyGroup> = allGroups

    val maxZoom: Float
        get() = characteristics?.get(CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM) ?: 1f
    val maxFocusDistance: Float
        get() = characteristics?.get(CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE) ?: 0f
    val stabilizationSupported: Boolean
        get() = characteristics?.get(CameraCharacteristics.CONTROL_AVAILABLE_VIDEO_STABILIZATION_MODES)
            ?.contains(CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE_ON) == true
    val faceExposureSupported: Boolean
        get() {
            val chars = characteristics ?: return false
            val modes = chars.get(CameraCharacteristics.STATISTICS_INFO_AVAILABLE_FACE_DETECT_MODES)
                ?: return false
            return (chars.get(CameraCharacteristics.CONTROL_MAX_REGIONS_AE) ?: 0) > 0 &&
                (modes.contains(CaptureRequest.STATISTICS_FACE_DETECT_MODE_SIMPLE) ||
                    modes.contains(CaptureRequest.STATISTICS_FACE_DETECT_MODE_FULL))
        }

    @SuppressLint("MissingPermission")
    fun start() {
        val t = HandlerThread("camera").also { it.start() }
        thread = t
        handler = Handler(t.looper)

        val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
        val chars = manager.getCameraCharacteristics(cameraId)
        characteristics = chars

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
        cameraDevice = cam
        cam.createCaptureSession(
            listOf(outputSurface),
            object : CameraCaptureSession.StateCallback() {
                override fun onConfigured(s: CameraCaptureSession) {
                    session = s
                    // onConfigured 跑在相机 HandlerThread 上。这里**必须**兜住所有异常：
                    // 一旦有异常逃出去，线程直接死，整个进程跟着崩 —— 用户看到的是
                    // 「点开始共享后 App 闪退」，而真正的原因只是一条请求被 HAL 拒绝。
                    // 实测：一加 13 / Android 17 上 setRepeatingRequest 抛
                    // IllegalArgumentException，且不带任何 message。
                    try {
                        if (!paused && !startRepeating(s, cam)) {
                            Log.e(TAG, "所有请求组合都被相机拒绝，采集未启动")
                            onSessionReady()
                            return
                        }
                        Log.i(
                            TAG,
                            "采集已启动 ${width}x${height}@$fps" +
                                (if (paused) "（暂停）" else "") +
                                "，请求组 ${acceptedGroups.size}/${allGroups.size}",
                        )
                    } catch (e: Exception) {
                        Log.e(TAG, "启动采集时异常", e)
                    }
                    onSessionReady()
                }

                override fun onConfigureFailed(s: CameraCaptureSession) {
                    Log.e(TAG, "createCaptureSession 失败：${width}x${height}@$fps")
                }
            },
            handler,
        )
    }

    /**
     * 下发 repeating request，失败时逐组降级。
     *
     * # 为什么要降级而不是只报错
     *
     * 不同厂商的 HAL 对「哪些 key 可以一起下发」的容忍度差别很大。一加 13 /
     * Android 17 上，完整的 8 组控制会被 [setRepeatingRequest] 直接拒绝并抛
     * [IllegalArgumentException]（且 message 为空），没有任何可读线索。
     *
     * 先按全集试，被拒绝就按可疑度**逐组摘掉**再试，直到某一组被接受：
     *
     * 1. 用户至少能拿到画面（而不是闪退）；
     * 2. 日志明确写出「被摘掉的是哪几组」，下次直接按这个信息修根因，
     *    而不是靠猜。
     *
     * 降级顺序按「最可能是元凶」排：Face（AE_REGIONS + 人脸检测）、Zoom
     * （ZOOM_RATIO / CROP_REGION）、Flash、Stab、Awb、AeLock、Af、Fps。
     * 最后剩下的是一条只有模板 + outputSurface 的最小请求 —— 这条要是还被拒，
     * 那就是会话本身有问题，只能报错。
     */
    private fun startRepeating(s: CameraCaptureSession, cam: CameraDevice): Boolean {
        for (groups in fallbackOrder()) {
            val request = try {
                buildRequest(cam, groups)
            } catch (e: Exception) {
                Log.w(TAG, "构建请求失败（${describe(groups)}）: ${e.message}")
                continue
            }
            try {
                s.setRepeatingRequest(request, captureCallback, handler)
                if (groups != allGroups) {
                    Log.w(
                        TAG,
                        "完整请求被相机拒绝，已降级：缺少的控制组 = ${(allGroups - groups).joinToString()}",
                    )
                }
                acceptedGroups = groups
                requestBuilder = requestBuilderFor(cam, groups)
                return true
            } catch (e: IllegalArgumentException) {
                // 这就是真机上的那个异常：message 通常为 null，只能靠组合定位
                Log.w(TAG, "请求被拒（${describe(groups)}）: ${e.message ?: "无消息"}")
            } catch (e: Exception) {
                Log.e(TAG, "下发请求失败（${describe(groups)}）", e)
                return false
            }
        }
        return false
    }

    /** 由全集开始，按可疑度逐组摘掉 */
    private fun fallbackOrder(): List<Set<KeyGroup>> {
        val dropOrder = listOf(
            KeyGroup.Face,
            KeyGroup.Zoom,
            KeyGroup.Flash,
            KeyGroup.Stab,
            KeyGroup.Awb,
            KeyGroup.AeLock,
            KeyGroup.Af,
            KeyGroup.Fps,
        )
        val result = mutableListOf(allGroups)
        var current = allGroups
        for (group in dropOrder) {
            current = current - group
            result += current
        }
        return result
    }

    private fun describe(groups: Set<KeyGroup>): String =
        if (groups == allGroups) "全集" else "缺少 ${(allGroups - groups).joinToString()}"

    private fun buildRequest(cam: CameraDevice, groups: Set<KeyGroup>): CaptureRequest {
        val builder = cam.createCaptureRequest(CameraDevice.TEMPLATE_RECORD)
        builder.addTarget(outputSurface)
        applyControls(builder, groups)
        return builder.build()
    }

    /** 供后续 [updateRepeatingRequest] 复用同一个已接受的组合 */
    private fun requestBuilderFor(cam: CameraDevice, groups: Set<KeyGroup>): CaptureRequest.Builder {
        val builder = cam.createCaptureRequest(CameraDevice.TEMPLATE_RECORD)
        builder.addTarget(outputSurface)
        applyControls(builder, groups)
        return builder
    }

    /** 一次请求里可独立开关的控制组。拆开是为了能在真机上逐个排除。 */
    private enum class KeyGroup {
        Fps, Af, AeLock, Face, Awb, Stab, Zoom, Flash
    }

    /** PC/手机界面可动态调整的 Camera2 控件。 */
    @Synchronized
    fun setZoom(value: Float) {
        zoom = value.coerceIn(1f, maxZoom.coerceAtLeast(1f))
        updateRepeatingRequest()
    }

    @Synchronized
    fun setTorch(enabled: Boolean) {
        torch = enabled && (characteristics?.get(CameraCharacteristics.FLASH_INFO_AVAILABLE) == true)
        updateRepeatingRequest()
    }

    @Synchronized
    fun setManualFocus(enabled: Boolean, distance: Float = focusDistance) {
        manualFocus = enabled && maxFocusDistance > 0f
        focusDistance = distance.coerceIn(0f, maxFocusDistance)
        updateRepeatingRequest()
    }

    @Synchronized
    fun setExposureLock(locked: Boolean) {
        exposureLocked = locked
        updateRepeatingRequest()
    }

    @Synchronized
    fun setExposureCompensation(value: Int) {
        exposureCompensation = value
        updateRepeatingRequest()
    }

    @Synchronized
    fun setWhiteBalanceLock(locked: Boolean) {
        whiteBalanceLocked = locked
        updateRepeatingRequest()
    }

    @Synchronized
    fun setStabilization(enabled: Boolean) {
        stabilization = enabled && stabilizationSupported
        updateRepeatingRequest()
    }

    @Synchronized
    fun setFaceExposure(enabled: Boolean) {
        faceExposure = enabled && faceExposureSupported
        faceExposureRegion = null
        updateRepeatingRequest()
    }

    @Synchronized
    fun setFpsLimit(limit: Int) {
        val next = limit.coerceAtLeast(1)
        if (fpsLimit == next) return
        fpsLimit = next
        updateRepeatingRequest()
    }

    @Synchronized
    fun setPaused(value: Boolean) {
        if (paused == value) return
        paused = value
        updateRepeatingRequest()
    }

    val currentFpsLimit: Int
        get() = appliedFpsLimit

    private fun updateRepeatingRequest() {
        val builder = requestBuilder ?: return
        val activeSession = session ?: return
        if (paused) {
            try {
                activeSession.stopRepeating()
            } catch (e: Exception) {
                Log.w(TAG, "暂停相机采集失败: ${e.message}")
            }
            return
        }
        // 用**已经被接受过**的组重建请求：真机上全集是会被拒的，
        // 这里如果再按全集下发，每次用户拖动滑块都会失败一次。
        applyControls(builder, acceptedGroups)
        try {
            activeSession.setRepeatingRequest(builder.build(), captureCallback, handler)
        } catch (e: IllegalArgumentException) {
            // 单次控制变更被拒不影响正在跑的采集（上一帧的请求仍在生效），
            // 所以只记录、不重试 —— 重试会在 HAL 不接受该 key 时变成死循环。
            Log.w(TAG, "更新相机控制被拒（${describe(acceptedGroups)}）: ${e.message ?: "无消息"}")
        } catch (e: Exception) {
            Log.w(TAG, "更新相机控制失败: ${e.message}")
        }
    }

    private fun applyControls(builder: CaptureRequest.Builder, groups: Set<KeyGroup>) {
        val chars = characteristics ?: return
        if (KeyGroup.Fps in groups) {
            val fpsRanges = chars.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)
            val selectedFpsRange = fpsRanges
                ?.filter { it.upper <= fpsLimit }
                ?.maxByOrNull { it.upper }
            if (selectedFpsRange != null) {
                builder.set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, selectedFpsRange)
                appliedFpsLimit = selectedFpsRange.upper
            }
        }
        if (KeyGroup.Af in groups) {
            if (manualFocus && maxFocusDistance > 0f) {
                builder.set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_OFF)
                builder.set(CaptureRequest.LENS_FOCUS_DISTANCE, focusDistance)
            } else {
                builder.set(
                    CaptureRequest.CONTROL_AF_MODE,
                    CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO,
                )
            }
        }
        val aeRange = chars.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)
        if (aeRange != null) {
            if (KeyGroup.AeLock in groups) {
                builder.set(CaptureRequest.CONTROL_AE_LOCK,
                    exposureLocked && !faceExposure &&
                        chars.get(CameraCharacteristics.CONTROL_AE_LOCK_AVAILABLE) == true)
            }
            builder.set(CaptureRequest.CONTROL_AE_EXPOSURE_COMPENSATION,
                exposureCompensation.coerceIn(aeRange.lower, aeRange.upper))
        }
        if (KeyGroup.Face in groups && faceExposureSupported) {
            val modes = chars.get(CameraCharacteristics.STATISTICS_INFO_AVAILABLE_FACE_DETECT_MODES)
                ?: intArrayOf()
            val faceMode = when {
                !faceExposure -> CaptureRequest.STATISTICS_FACE_DETECT_MODE_OFF
                modes.contains(CaptureRequest.STATISTICS_FACE_DETECT_MODE_FULL) ->
                    CaptureRequest.STATISTICS_FACE_DETECT_MODE_FULL
                else -> CaptureRequest.STATISTICS_FACE_DETECT_MODE_SIMPLE
            }
            builder.set(CaptureRequest.STATISTICS_FACE_DETECT_MODE, faceMode)
            builder.set(CaptureRequest.CONTROL_AE_REGIONS,
                faceExposureRegion?.let { arrayOf(it) } ?: emptyArray())
        }
        if (KeyGroup.Awb in groups &&
            chars.get(CameraCharacteristics.CONTROL_AWB_LOCK_AVAILABLE) == true
        ) {
            builder.set(CaptureRequest.CONTROL_AWB_LOCK, whiteBalanceLocked)
        }
        if (KeyGroup.Stab in groups && stabilizationSupported) {
            builder.set(CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE,
                if (stabilization) CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE_ON
                else CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE_OFF)
        }
        if (KeyGroup.Zoom in groups) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
                chars.get(CameraCharacteristics.CONTROL_ZOOM_RATIO_RANGE) != null
            ) {
                builder.set(CaptureRequest.CONTROL_ZOOM_RATIO, zoom)
            } else {
                val active = chars.get(CameraCharacteristics.SENSOR_INFO_ACTIVE_ARRAY_SIZE)
                val max = (chars.get(CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM) ?: 1f)
                    .coerceAtLeast(1f)
                if (active != null) {
                    val ratio = zoom.coerceIn(1f, max)
                    val halfWidth = (active.width() / (2f * ratio)).toInt()
                    val halfHeight = (active.height() / (2f * ratio)).toInt()
                    val centerX = active.centerX()
                    val centerY = active.centerY()
                    builder.set(CaptureRequest.SCALER_CROP_REGION, Rect(
                        centerX - halfWidth, centerY - halfHeight,
                        centerX + halfWidth, centerY + halfHeight,
                    ))
                }
            }
        }
        if (KeyGroup.Flash in groups &&
            chars.get(CameraCharacteristics.FLASH_INFO_AVAILABLE) == true
        ) {
            builder.set(CaptureRequest.FLASH_MODE,
                if (torch) CaptureRequest.FLASH_MODE_TORCH else CaptureRequest.FLASH_MODE_OFF)
        }
    }

    /** 拆成独立函数，方便在日志里指认是哪一组把请求撑炸的 */
    private fun KeyGroup.describe(): String = name;

    private val captureCallback = object : CameraCaptureSession.CaptureCallback() {
        override fun onCaptureCompleted(
            session: CameraCaptureSession,
            request: CaptureRequest,
            result: TotalCaptureResult,
        ) {
            if (!faceExposure) return
            val face = result.get(CaptureResult.STATISTICS_FACES)
                ?.asSequence()
                ?.filter { it.score >= 40 }
                ?.maxByOrNull { it.bounds.width().toLong() * it.bounds.height() }
            updateFaceMeteringRegion(face?.bounds)
        }
    }

    @Synchronized
    private fun updateFaceMeteringRegion(faceBounds: Rect?) {
        if (!faceExposure || !faceExposureSupported) return
        val active = characteristics?.get(CameraCharacteristics.SENSOR_INFO_ACTIVE_ARRAY_SIZE) ?: return
        val next = faceBounds?.let { bounds ->
            val rect = Rect(bounds)
            if (!rect.intersect(active)) return@let null
            val centerX = rect.centerX()
            val centerY = rect.centerY()
            val width = (rect.width() * 1.25f).toInt().coerceAtLeast(1)
            val height = (rect.height() * 1.25f).toInt().coerceAtLeast(1)
            val expanded = Rect(
                centerX - width / 2,
                centerY - height / 2,
                centerX + width / 2,
                centerY + height / 2,
            )
            if (!expanded.intersect(active)) null else MeteringRectangle(expanded, 1000)
        }
        val current = faceExposureRegion?.rect
        val nextRect = next?.rect
        if (current == nextRect) return
        if (current != null && nextRect != null) {
            val toleranceX = (active.width() / 80).coerceAtLeast(1)
            val toleranceY = (active.height() / 80).coerceAtLeast(1)
            if (kotlin.math.abs(current.centerX() - nextRect.centerX()) < toleranceX &&
                kotlin.math.abs(current.centerY() - nextRect.centerY()) < toleranceY &&
                kotlin.math.abs(current.width() - nextRect.width()) < toleranceX * 2 &&
                kotlin.math.abs(current.height() - nextRect.height()) < toleranceY * 2
            ) return
        }
        faceExposureRegion = next
        updateRepeatingRequest()
    }

    fun stop() {
        session?.close(); session = null
        device?.close(); device = null
        requestBuilder = null
        characteristics = null
        thread?.quitSafely(); thread = null
        handler = null
    }

    companion object {
        private const val TAG = "SideLens.Camera"

        /** 全集：一次「功能完整」的请求应有的所有控制组 */
        private val allGroups: Set<KeyGroup> = KeyGroup.entries.toSet()
    }
}