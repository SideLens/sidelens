package com.sidelens.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.os.BatteryManager
import android.os.Binder
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit

/** Owns the complete camera, encoder, audio, and network session beyond Activity lifetime. */
class SharingForegroundService : Service() {
    private val binder = LocalBinder()
    private val sessionWorker = Executors.newSingleThreadExecutor()
    private val controlWorker = Executors.newSingleThreadExecutor()
    private val drainer = Executors.newSingleThreadExecutor()
    private val stateReporter = Executors.newSingleThreadScheduledExecutor()
    private val videoLock = Any()

    @Volatile private var running = false
    @Volatile private var sessionGeneration = 0L
    @Volatile private var sender: FrameSender? = null
    @Volatile private var capture: CameraCapture? = null
    @Volatile private var encoder: VideoEncoder? = null
    @Volatile private var audioCapture: AudioCapture? = null
    @Volatile private var activeProfile = CaptureProfile(1280, 720, 30)
    @Volatile private var requestedProfile = activeProfile
    @Volatile private var activeCameraId = "0"
    @Volatile private var thermalReconfigureTarget: CaptureProfile? = null
    @Volatile private var audioEnabled = false
    @Volatile private var rawMicrophone = false
    @Volatile private var videoPaused = false
    @Volatile private var availableProfiles: List<CaptureProfile> = emptyList()
    /** 电脑端最近一次下发的升级提示；null 表示无需提示 */
    @Volatile var upgradePrompt: PhoneUpgradePrompt? = null
        private set

    private var maxZoom = 1f
    private var zoom = 1f
    private var torch = false
    private var manualFocus = false
    private var focusDistance = 0f
    private var exposureLocked = false
    private var faceExposure = false
    private var exposureCompensation = 0
    private var whiteBalanceLocked = false
    private var stabilization = false
    private var stateJob: ScheduledFuture<*>? = null
    private var thermalPolicy = ThermalPolicy()

    private data class CaptureProfile(val width: Int, val height: Int, val fps: Int)

    inner class LocalBinder : Binder() {
        fun service(): SharingForegroundService = this@SharingForegroundService
    }

    fun isSessionRunning(): Boolean = running
    fun isAudioEnabled(): Boolean = audioEnabled
    fun isVideoPaused(): Boolean = videoPaused
    fun isRawMicrophone(): Boolean = rawMicrophone

    fun currentStatus(): String = if (running) "SideLens 共享服务运行中" else "接收器已停止"

    override fun onCreate() {
        super.onCreate()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                "摄像头共享",
                NotificationManager.IMPORTANCE_LOW,
            ).apply { description = "SideLens 正在向电脑共享摄像头" }
            getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> {
                audioEnabled = intent.getBooleanExtra(EXTRA_AUDIO, false)
                rawMicrophone = intent.getBooleanExtra(EXTRA_RAW_MICROPHONE, false)
                promoteToForeground()
                startSession(intent)
            }
            ACTION_CONTROL -> {
                val json = intent.getStringExtra(EXTRA_CONTROL).orEmpty()
                if (json.isNotEmpty()) controlWorker.execute { applyControl(JSONObject(json)) }
            }
            ACTION_AUDIO -> controlWorker.execute {
                setAudioEnabled(
                    intent.getBooleanExtra(EXTRA_AUDIO, false),
                    intent.getBooleanExtra(EXTRA_RAW_MICROPHONE, rawMicrophone),
                )
            }
            ACTION_STOP -> {
                stopSession()
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelfResult(startId)
                return START_NOT_STICKY
            }
            ACTION_STATUS_QUERY -> broadcastStatus(currentStatus(), running)
        }
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder = binder

    private fun promoteToForeground() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, buildNotification(), foregroundTypes())
        } else {
            startForeground(NOTIFICATION_ID, buildNotification())
        }
    }

    private fun foregroundTypes(): Int {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return 0
        var types = ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
        if (audioEnabled) types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
        return types
    }

    private fun buildNotification(): Notification {
        val openIntent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        val openPendingIntent = PendingIntent.getActivity(
            this, 1, openIntent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val stopIntent = Intent(this, SharingForegroundService::class.java).setAction(ACTION_STOP)
        val stopPendingIntent = PendingIntent.getService(
            this, 2, stopIntent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.presence_video_online)
            .setContentTitle("SideLens 正在共享摄像头")
            .setContentText(if (audioEnabled) "视频和麦克风已连接到电脑" else "视频已连接到电脑")
            .setContentIntent(openPendingIntent)
            .setOngoing(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .addAction(android.R.drawable.ic_media_pause, "停止共享", stopPendingIntent)
            .build()
    }

    private fun startSession(intent: Intent) {
        if (running) return
        val generation = ++sessionGeneration
        running = true
        activeCameraId = intent.getStringExtra(EXTRA_CAMERA_ID) ?: "0"
        val width = intent.getIntExtra(EXTRA_WIDTH, 1280)
        val height = intent.getIntExtra(EXTRA_HEIGHT, 720)
        val fps = intent.getIntExtra(EXTRA_FPS, 30)
        requestedProfile = CaptureProfile(width, height, fps)
        activeProfile = requestedProfile
        maxZoom = cameraCharacteristics(activeCameraId)
            ?.get(CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM) ?: 1f
        thermalPolicy = ThermalPolicy()
        val host = intent.getStringExtra(EXTRA_HOST).orEmpty().ifBlank { "127.0.0.1" }
        val hello = intent.getStringExtra(EXTRA_HELLO).orEmpty()
        val initialControl = intent.getStringExtra(EXTRA_INITIAL_CONTROL).orEmpty()
        if (initialControl.isNotEmpty()) applyControl(JSONObject(initialControl), updateVideo = false)
        broadcastStatus("连接中…", true)

        sessionWorker.execute {
            try {
                val profiles = supportedProfiles(activeCameraId)
                if (!running || generation != sessionGeneration) return@execute
                availableProfiles = profiles
                val stream = FrameSender()
                stream.connect(host, PORT)
                if (!running || generation != sessionGeneration) {
                    stream.close()
                    return@execute
                }
                sender = stream
                stream.startControlReader { command ->
                    controlWorker.execute {
                        try {
                            applyControl(command)
                        } catch (e: Exception) {
                            Log.w(TAG, "忽略无效的电脑控制消息: ${e.message}")
                        }
                    }
                }
                if (hello.isNotEmpty()) stream.sendHello(hello)

                val profile = requestedProfile
                val videoEncoder = VideoEncoder(profile.width, profile.height, profile.fps)
                videoEncoder.start()
                if (!running || generation != sessionGeneration) {
                    videoEncoder.stop()
                    stream.close()
                    return@execute
                }
                encoder = videoEncoder
                val camera = createCameraCapture(activeCameraId, profile, videoEncoder.inputSurface)
                camera.setPaused(videoPaused)
                camera.start()
                capture = camera
                if (!running || generation != sessionGeneration) {
                    camera.stop()
                    if (capture === camera) capture = null
                    videoEncoder.stop()
                    if (encoder === videoEncoder) encoder = null
                    stream.close()
                    if (sender === stream) sender = null
                    return@execute
                }
                if (audioEnabled) startAudioCapture()
                stateJob?.cancel(false)
                stateJob = stateReporter.scheduleAtFixedRate(
                    { reportPhoneState() }, 0, 2, TimeUnit.SECONDS,
                )
                startDrainer(stream, generation)
                broadcastStatus("已连接 $host，启动采集…", true)
            } catch (e: Exception) {
                Log.e(TAG, "共享会话启动失败", e)
                val errorStatus = "启动失败：${e.message ?: e.javaClass.simpleName}"
                if (generation != sessionGeneration) return@execute
                stopSession(status = null)
                broadcastStatus(errorStatus, false)
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            }
        }
    }

    private fun startDrainer(stream: FrameSender, generation: Long) {
        drainer.execute {
            while (running && generation == sessionGeneration) {
                try {
                    synchronized(videoLock) {
                        encoder?.drainEncoder(
                            { width, height, codecData -> stream.sendConfig(width, height, codecData) },
                            { data, ptsUs, isKey -> if (!videoPaused) stream.send(data, ptsUs, isKey) },
                        )
                    }
                    Thread.sleep(2)
                } catch (e: InterruptedException) {
                    break
                } catch (e: Exception) {
                    if (running) {
                        Log.e(TAG, "编码输出失败", e)
                        broadcastStatus("视频编码失败：${e.message ?: e.javaClass.simpleName}", true)
                    }
                    break
                }
            }
        }
    }

    private fun stopSession(status: String? = "已停止") {
        sessionGeneration += 1
        running = false
        stateJob?.cancel(false)
        stateJob = null
        audioCapture?.stop()
        audioCapture = null
        audioEnabled = false
        rawMicrophone = false
        videoPaused = false
        availableProfiles = emptyList()
        thermalReconfigureTarget = null
        upgradePrompt = null
        synchronized(videoLock) {
            capture?.stop()
            capture = null
            encoder?.stop()
            encoder = null
        }
        sender?.close()
        sender = null
        if (status != null) broadcastStatus(status, false)
    }

    private fun createCameraCapture(
        cameraId: String,
        profile: CaptureProfile,
        surface: android.view.Surface,
    ): CameraCapture {
        lateinit var camera: CameraCapture
        camera = CameraCapture(this, cameraId, profile.width, profile.height, profile.fps, surface) {
            applyCameraControls(camera, cameraId)
            broadcastStatus(
                if (videoPaused) "视频已暂停 · 电脑保留最后画面"
                else "共享中 · ${activeProfile.width}×${activeProfile.height}@${activeProfile.fps}fps",
                true,
            )
        }
        return camera
    }

    private fun applyControl(command: JSONObject, updateVideo: Boolean = true) {
        if (command.has("zoom")) {
            zoom = command.optDouble("zoom", 1.0).toFloat().coerceIn(1f, maxZoom.coerceAtLeast(1f))
            capture?.setZoom(zoom)
        }
        if (command.has("torch")) {
            torch = command.optBoolean("torch")
            capture?.setTorch(torch)
        }
        if (command.has("af")) manualFocus = command.optString("af") == "manual"
        if (command.has("focus")) focusDistance = command.optDouble("focus", 0.0).toFloat().coerceAtLeast(0f)
        if (command.has("af") || command.has("focus")) {
            capture?.setManualFocus(manualFocus, focusDistance)
        }
        if (command.has("ae")) {
            exposureLocked = command.optString("ae") == "lock"
            capture?.setExposureLock(exposureLocked)
        }
        if (command.has("faceExposure")) {
            faceExposure = command.optBoolean("faceExposure")
            if (faceExposure) exposureLocked = false
            capture?.setFaceExposure(faceExposure)
            if (faceExposure) capture?.setExposureLock(false)
        }
        if (command.has("ev")) {
            exposureCompensation = command.optInt("ev")
            capture?.setExposureCompensation(exposureCompensation)
        }
        if (command.has("awb")) {
            whiteBalanceLocked = command.optString("awb") == "lock"
            capture?.setWhiteBalanceLock(whiteBalanceLocked)
        }
        if (command.has("stab")) {
            stabilization = command.optBoolean("stab")
            capture?.setStabilization(stabilization)
        }
        if (command.has("pause")) {
            videoPaused = command.optBoolean("pause")
            capture?.setPaused(videoPaused)
            if (!videoPaused) encoder?.requestSyncFrame()
        }
        // 电脑端按发布清单判定的「手机端该不该升级」。解析失败不影响其它控制指令 ——
        // PhoneUpgradePrompt.fromControl 内部吞掉所有结构问题并返回 null。
        if (command.has("phone_upgrade")) {
            val prompt = PhoneUpgradePrompt.fromControl(command)
            if (prompt != null && prompt != upgradePrompt) {
                upgradePrompt = prompt
                broadcastStatus(prompt.message.ifEmpty { "电脑端提示手机端需要升级" }, true)
            }
        }
        if (command.has("mic")) rawMicrophone = command.optString("mic") == "raw"
        if (command.has("audio")) setAudioEnabled(command.optBoolean("audio"), rawMicrophone)
        if (command.optBoolean("keyframe", false)) encoder?.requestSyncFrame()
        if (updateVideo && running && command.has("pause")) {
            broadcastStatus(
                if (videoPaused) "视频已暂停 · 电脑保留最后画面" else "正在恢复视频…",
                true,
            )
        }
    }

    private fun applyCameraControls(camera: CameraCapture, cameraId: String) {
        val chars = cameraCharacteristics(cameraId)
        maxZoom = camera.maxZoom.coerceAtLeast(1f)
        faceExposure = faceExposure && camera.faceExposureSupported
        camera.setZoom(zoom.coerceIn(1f, maxZoom))
        camera.setTorch(torch)
        camera.setManualFocus(manualFocus, focusDistance)
        camera.setExposureLock(exposureLocked)
        camera.setExposureCompensation(exposureCompensation.coerceIn(
            chars?.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)?.lower ?: 0,
            chars?.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)?.upper ?: 0,
        ))
        camera.setWhiteBalanceLock(whiteBalanceLocked)
        camera.setStabilization(stabilization)
        camera.setFaceExposure(faceExposure)
        camera.setPaused(videoPaused)
    }

    private fun setAudioEnabled(enabled: Boolean, raw: Boolean) {
        audioEnabled = enabled
        rawMicrophone = raw
        promoteToForeground()
        audioCapture?.stop()
        audioCapture = null
        if (enabled && running) startAudioCapture()
    }

    private fun startAudioCapture() {
        val activeSender = sender ?: return
        try {
            val audio = AudioCapture(activeSender, rawMicrophone) { error ->
                Log.w(TAG, "麦克风采集失败: $error")
                controlWorker.execute {
                    setAudioEnabled(false, rawMicrophone)
                    broadcastStatus("麦克风不可用：$error", running)
                }
            }
            audio.start()
            audioCapture = audio
        } catch (e: Exception) {
            Log.e(TAG, "启动麦克风失败", e)
            audioEnabled = false
            promoteToForeground()
            broadcastStatus("启动麦克风失败：${e.message ?: e.javaClass.simpleName}", running)
        }
    }

    private fun reportPhoneState() {
        if (!running) return
        val battery = registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        val temperature = battery?.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, -1)
            ?.takeIf { it >= 0 }?.div(10f)
        val level = battery?.getIntExtra(BatteryManager.EXTRA_LEVEL, -1) ?: -1
        val scale = battery?.getIntExtra(BatteryManager.EXTRA_SCALE, -1) ?: -1
        val batteryPercent = if (level >= 0 && scale > 0) level * 100 / scale else -1
        val batteryStatus = battery?.getIntExtra(BatteryManager.EXTRA_STATUS, BatteryManager.BATTERY_STATUS_UNKNOWN)
        val charging = batteryStatus == BatteryManager.BATTERY_STATUS_CHARGING ||
            batteryStatus == BatteryManager.BATTERY_STATUS_FULL
        val power = getSystemService(POWER_SERVICE) as PowerManager
        val thermalStatus = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) power.currentThermalStatus else 0
        val headroom = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            try { power.getThermalHeadroom(0).takeIf { it.isFinite() && it >= 0f } }
            catch (e: Exception) { Log.w(TAG, "读取热余量失败: ${e.message}"); null }
        } else null

        val limits = thermalPolicy.update(thermalStatus, headroom, temperature)
        val generation = sessionGeneration
        val targetProfile = thermalProfileFor(limits)
        if (targetProfile != activeProfile && targetProfile != thermalReconfigureTarget) {
            thermalReconfigureTarget = targetProfile
            sessionWorker.execute {
                try { reconfigureForThermal(targetProfile, limits, generation) }
                finally { if (thermalReconfigureTarget == targetProfile) thermalReconfigureTarget = null }
            }
        }
        val appliedFps = capture?.let { camera ->
            camera.setFpsLimit(limits.fps)
            camera.currentFpsLimit
        } ?: activeProfile.fps
        encoder?.setBitrate(limits.bitrate)
        val thermal = JSONObject()
            .put("level", limits.level)
            .put("status", thermalStatus)
            .put("throttled", limits.throttled)
            .put("limit", limits.fps)
            .put("fps", appliedFps)
            .put("headroom", headroom ?: JSONObject.NULL)
        val state = JSONObject()
            .put("camera", activeCameraId)
            .put("width", activeProfile.width)
            .put("height", activeProfile.height)
            .put("fps", appliedFps)
            .put("battery", batteryPercent)
            .put("charging", charging)
            .put("audio", audioEnabled)
            .put("paused", videoPaused)
            .put("faceExposure", faceExposure)
            .put("mic", if (rawMicrophone) "raw" else "voice")
            .put("thermal", thermal)
        if (temperature != null) state.put("tempC", temperature)
        // 版本随每一条 STATE 上报：电脑端用它判断该不该给这台手机发升级提示。
        // 单独发一条消息会让"电脑端刚连上就问版本"多一个往返，而 STATE 本来就每 2 秒一条。
        val packageInfo = packageManager.getPackageInfo(packageName, 0)
        // longVersionCode 是 API 28+；minSdk 是 26，所以低版本要回落到 versionCode。
        // 只取一次 packageInfo：每 2 秒调两次 getPackageInfo 是没必要的开销。
        @Suppress("DEPRECATION")
        val versionCode =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) packageInfo.longVersionCode
            else packageInfo.versionCode.toLong()
        val versionName = packageInfo.versionName ?: ""
        sender?.sendStateWithVersion(state, versionCode, versionName)
    }

    private fun thermalProfileFor(limits: ThermalPolicy.Limits): CaptureProfile {
        val requested = requestedProfile
        val fpsLimit = minOf(requested.fps, limits.fps)
        return availableProfiles.asSequence()
            .filter { it.width <= requested.width && it.height <= requested.height }
            .filter { it.width <= limits.maxWidth && it.height <= limits.maxHeight }
            .filter { it.fps <= fpsLimit }
            .maxByOrNull { it.width.toLong() * it.height * 100 + it.fps }
            ?: requested
    }

    private fun supportedProfiles(cameraId: String): List<CaptureProfile> {
        val chars = cameraCharacteristics(cameraId) ?: return emptyList()
        val map = chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
        val sizes = map?.getOutputSizes(android.graphics.SurfaceTexture::class.java)
            ?.map { it.width to it.height }?.toSet().orEmpty()
        val fpsRanges = chars.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES).orEmpty()
        return listOf(640 to 480, 1280 to 720, 1920 to 1080, 2560 to 1440, 3840 to 2160)
            .flatMap { (w, h) -> listOf(30, 60).mapNotNull { fps ->
                if ((w to h) in sizes && fpsRanges.any { it.lower <= fps && it.upper >= fps } &&
                    VideoEncoder.supports(w, h, fps)
                ) CaptureProfile(w, h, fps) else null
            } }
    }

    private fun reconfigureForThermal(profile: CaptureProfile, limits: ThermalPolicy.Limits, generation: Long) {
        synchronized(videoLock) {
            if (!running || generation != sessionGeneration || profile == activeProfile) return
            val oldEncoder = encoder
            capture?.stop()
            try {
                val nextEncoder = VideoEncoder(profile.width, profile.height, profile.fps, limits.bitrate)
                nextEncoder.start()
                val nextCamera = createCameraCapture(activeCameraId, profile, nextEncoder.inputSurface)
                nextCamera.setPaused(videoPaused)
                nextCamera.setFpsLimit(limits.fps)
                nextCamera.start()
                encoder = nextEncoder
                capture = nextCamera
                activeProfile = profile
                oldEncoder?.stop()
                broadcastStatus("热管理切换至 ${profile.width}×${profile.height}@${profile.fps}fps", true)
            } catch (e: Exception) {
                Log.e(TAG, "热管理切换画质失败", e)
                if (running && oldEncoder != null) {
                    val restored = createCameraCapture(activeCameraId, activeProfile, oldEncoder.inputSurface)
                    restored.setPaused(videoPaused)
                    restored.setFpsLimit(limits.fps)
                    restored.start()
                    capture = restored
                }
            }
        }
    }

    private fun cameraCharacteristics(cameraId: String): CameraCharacteristics? = try {
        (getSystemService(CAMERA_SERVICE) as CameraManager).getCameraCharacteristics(cameraId)
    } catch (_: Exception) { null }

    private fun broadcastStatus(status: String, active: Boolean) {
        val intent = Intent(ACTION_STATUS).setPackage(packageName)
            .putExtra(EXTRA_STATUS, status)
            .putExtra(EXTRA_RUNNING, active)
            .putExtra(EXTRA_AUDIO, audioEnabled)
            .putExtra(EXTRA_RAW_MICROPHONE, rawMicrophone)
            .putExtra(EXTRA_PAUSED, videoPaused)
        // 升级提示随每条状态广播一起带出去：Activity 可能在服务收到提示之后
        // 才重建，靠 Intent 传递比重放一次控制消息更可靠。
        upgradePrompt?.let { prompt ->
            intent.putExtra(EXTRA_UPGRADE_LEVEL, prompt.level)
            intent.putExtra(EXTRA_UPGRADE_MESSAGE, prompt.message)
            intent.putExtra(EXTRA_UPGRADE_LATEST, prompt.latestVersionName)
            intent.putExtra(EXTRA_UPGRADE_BLOCKING, prompt.blocking)
        }
        sendBroadcast(intent)
    }

    override fun onDestroy() {
        stopSession(status = null)
        sessionWorker.shutdownNow()
        controlWorker.shutdownNow()
        drainer.shutdownNow()
        stateReporter.shutdownNow()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "SideLens.ShareService"
        const val ACTION_START = "com.sidelens.app.action.START_SHARING"
        const val ACTION_AUDIO = "com.sidelens.app.action.UPDATE_AUDIO"
        const val ACTION_CONTROL = "com.sidelens.app.action.CONTROL"
        const val ACTION_STOP = "com.sidelens.app.action.STOP_SHARING"
        const val ACTION_STATUS_QUERY = "com.sidelens.app.action.STATUS_QUERY"
        const val ACTION_STATUS = "com.sidelens.app.action.STATUS"
        const val EXTRA_AUDIO = "audio_active"
        const val EXTRA_RAW_MICROPHONE = "raw_microphone"
        const val EXTRA_CAMERA_ID = "camera_id"
        const val EXTRA_WIDTH = "width"
        const val EXTRA_HEIGHT = "height"
        const val EXTRA_FPS = "fps"
        const val EXTRA_HOST = "host"
        const val EXTRA_HELLO = "hello"
        const val EXTRA_INITIAL_CONTROL = "initial_control"
        const val EXTRA_CONTROL = "control"
        const val EXTRA_STATUS = "status"
        const val EXTRA_RUNNING = "running"
        const val EXTRA_PAUSED = "paused"
        /** 升级提示：级别（must / recommended / unsupported） */
        const val EXTRA_UPGRADE_LEVEL = "upgrade_level"
        /** 升级提示：给用户看的一句话 */
        const val EXTRA_UPGRADE_MESSAGE = "upgrade_message"
        /** 升级提示：最新版本名（空表示电脑端没给） */
        const val EXTRA_UPGRADE_LATEST = "upgrade_latest"
        /** 升级提示：是否阻塞（true 时无法正常共享） */
        const val EXTRA_UPGRADE_BLOCKING = "upgrade_blocking"
        private const val CHANNEL_ID = "sidelens_sharing"
        private const val NOTIFICATION_ID = 27300
        private const val PORT = 27300
    }
}
