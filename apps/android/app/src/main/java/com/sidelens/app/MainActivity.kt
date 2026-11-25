package com.sidelens.app

import android.Manifest
import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.pm.PackageManager
import android.content.Intent
import android.content.IntentFilter
import android.os.Bundle
import android.os.Build
import android.os.IBinder
import android.util.Log
import android.view.View
import android.widget.Button
import android.widget.ArrayAdapter
import android.widget.EditText
import android.widget.SeekBar
import android.widget.Spinner
import android.widget.Switch
import android.widget.TextView
import org.json.JSONObject
import org.json.JSONArray
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import java.util.concurrent.Executors

/**
 * SideLens 主界面。
 *
 * 职责很窄：申请权限、开关采集、显示状态。
 * 相机采集、编码、网络和麦克风由 [SharingForegroundService] 持有，
 * Activity 只负责配置、权限和界面控制。
 *
 * # 为什么所有网络操作都在后台线程
 *
 * Android 从 3.0 起禁止在主线程做网络操作，违反会抛
 * `NetworkOnMainThreadException`。第一版把 `FrameSender.connect()`
 * 直接放在点击回调里，一点就崩 —— 见 progress.md 第二十一轮。
 */
class MainActivity : AppCompatActivity() {

    private lateinit var statusText: TextView
    /** 电脑端下发的升级提示文案；null 表示没有提示。setStatus 用它拼接状态栏。 */
    private var pendingUpgradeNotice: String? = null
    private lateinit var toggleButton: Button
    private lateinit var discoverButton: Button
    private lateinit var hostInput: EditText
    private lateinit var cameraSpinner: Spinner
    private lateinit var profileSpinner: Spinner
    private lateinit var zoomSeekBar: SeekBar
    private lateinit var zoomLabel: TextView
    private lateinit var torchSwitch: Switch
    private lateinit var manualFocusSwitch: Switch
    private lateinit var focusSeekBar: SeekBar
    private lateinit var exposureLockSwitch: Switch
    private lateinit var exposureSeekBar: SeekBar
    private lateinit var exposureLabel: TextView
    private lateinit var faceExposureSwitch: Switch
    private lateinit var awbLockSwitch: Switch
    private lateinit var stabilizationSwitch: Switch
    private lateinit var microphoneSwitch: Switch
    private lateinit var rawMicrophoneSwitch: Switch
    private lateinit var blackoutSwitch: Switch
    private lateinit var pauseSwitch: Switch
    private lateinit var blackoutOverlay: View
    private var cameraIds: List<String> = emptyList()
    private var profiles: List<CaptureProfile> = emptyList()
    @Volatile private var activeCameraId: String? = null
    private var maxZoom = 1f
    private var configuringCameraList = true
    @Volatile private var discoveredHost: String? = null

    @Volatile private var audioEnabled = false
    @Volatile private var rawMicrophone = false
    @Volatile private var videoPaused = false

    private val discoveryWorker = Executors.newSingleThreadExecutor()
    @Volatile private var running = false
    private var sharingService: SharingForegroundService? = null
    private var serviceBound = false

    private val serviceConnection = object : android.content.ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, binder: IBinder?) {
            sharingService = (binder as? SharingForegroundService.LocalBinder)?.service()
            val service = sharingService
            if (service == null) {
                updateSessionUi(false, "接收器已停止")
                return
            }
            val active = service.isSessionRunning()
            if (active) {
                audioEnabled = service.isAudioEnabled()
                videoPaused = service.isVideoPaused()
                rawMicrophone = service.isRawMicrophone()
                microphoneSwitch.isChecked = audioEnabled
                rawMicrophoneSwitch.isChecked = rawMicrophone
                pauseSwitch.isChecked = videoPaused
                rawMicrophoneSwitch.isEnabled = audioEnabled
            }
            updateSessionUi(active, sharingService?.currentStatus() ?: "接收器已停止")
        }

        override fun onServiceDisconnected(name: ComponentName?) {
            sharingService = null
            updateSessionUi(false, "共享服务已断开")
        }
    }

    private val serviceStatusReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            if (intent?.action != SharingForegroundService.ACTION_STATUS) return
            audioEnabled = intent.getBooleanExtra(SharingForegroundService.EXTRA_AUDIO, false)
            rawMicrophone = intent.getBooleanExtra(SharingForegroundService.EXTRA_RAW_MICROPHONE, false)
            videoPaused = intent.getBooleanExtra(SharingForegroundService.EXTRA_PAUSED, false)
            microphoneSwitch.isChecked = audioEnabled
            rawMicrophoneSwitch.isChecked = rawMicrophone
            pauseSwitch.isChecked = videoPaused
            rawMicrophoneSwitch.isEnabled = audioEnabled
            val upgradeLevel = intent.getStringExtra(SharingForegroundService.EXTRA_UPGRADE_LEVEL)
            val upgradeMessage = intent.getStringExtra(SharingForegroundService.EXTRA_UPGRADE_MESSAGE)
            val upgradeLatest = intent.getStringExtra(SharingForegroundService.EXTRA_UPGRADE_LATEST)
            applyUpgradePrompt(
                level = upgradeLevel,
                message = upgradeMessage,
                latest = upgradeLatest,
                blocking = intent.getBooleanExtra(SharingForegroundService.EXTRA_UPGRADE_BLOCKING, false),
            )
            updateSessionUi(
                intent.getBooleanExtra(SharingForegroundService.EXTRA_RUNNING, false),
                intent.getStringExtra(SharingForegroundService.EXTRA_STATUS) ?: "",
            )
        }
    }

    /**
     * 把电脑端下发的升级提示显示出来。
     *
     * # 三档显示策略
     *
     * | level | 显示 | 原因 |
     * | --- | --- | --- |
     * | must / unsupported | banner + 状态栏同文案 | 不升级就没法用，必须显眼 |
     * | recommended | 只在状态栏追加一句 | 不打扰正在用的用户 |
     * | null（无提示） | 清空 banner | 电脑端换了配置要能退回去 |
     *
     * 把"清除"也做成显式分支是必要的：updateSessionUi 每条状态都会调
     * setStatus，如果只在有提示时写、没提示时不写，上一次的提示会
     * 一直挂在界面上，直到用户重启 App。
     */
    private fun applyUpgradePrompt(level: String?, message: String?, latest: String?, blocking: Boolean) {
        val banner = findViewById<TextView>(R.id.upgradeBanner)
        if (level.isNullOrEmpty()) {
            pendingUpgradeNotice = null
            banner.visibility = View.GONE
            return
        }
        val text = buildString {
            append(message?.takeIf { it.isNotBlank() } ?: PhoneUpgradePrompt.defaultMessage(level))
            if (!latest.isNullOrBlank()) append("（最新 $latest）")
        }
        banner.text = text
        banner.visibility = View.VISIBLE
        pendingUpgradeNotice = text
        Log.i(TAG, "电脑端升级提示[$level]: $text blocking=$blocking")
    }

    private val requiredPermissions = arrayOf(Manifest.permission.CAMERA)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        statusText = findViewById(R.id.statusText)
        toggleButton = findViewById(R.id.toggleButton)
        discoverButton = findViewById(R.id.discoverButton)
        hostInput = findViewById(R.id.hostInput)
        cameraSpinner = findViewById(R.id.cameraSpinner)
        profileSpinner = findViewById(R.id.profileSpinner)
        zoomSeekBar = findViewById(R.id.zoomSeekBar)
        zoomLabel = findViewById(R.id.zoomLabel)
        torchSwitch = findViewById(R.id.torchSwitch)
        manualFocusSwitch = findViewById(R.id.manualFocusSwitch)
        focusSeekBar = findViewById(R.id.focusSeekBar)
        exposureLockSwitch = findViewById(R.id.exposureLockSwitch)
        exposureSeekBar = findViewById(R.id.exposureSeekBar)
        exposureLabel = findViewById(R.id.exposureLabel)
        faceExposureSwitch = findViewById(R.id.faceExposureSwitch)
        awbLockSwitch = findViewById(R.id.awbLockSwitch)
        stabilizationSwitch = findViewById(R.id.stabilizationSwitch)
        microphoneSwitch = findViewById(R.id.microphoneSwitch)
        rawMicrophoneSwitch = findViewById(R.id.rawMicrophoneSwitch)
        blackoutSwitch = findViewById(R.id.blackoutSwitch)
        pauseSwitch = findViewById(R.id.pauseSwitch)
        blackoutOverlay = findViewById(R.id.blackoutOverlay)

        loadCameras()
        cameraSpinner.onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
            override fun onNothingSelected(parent: android.widget.AdapterView<*>?) = Unit
            override fun onItemSelected(parent: android.widget.AdapterView<*>?, view: View?, position: Int, id: Long) {
                activeCameraId = cameraIds.getOrNull(position)
                if (!configuringCameraList && !running) {
                    updateCameraCapabilities(cameraIds.getOrNull(position))
                    updateCaptureProfiles(cameraIds.getOrNull(position))
                }
            }
        }
        zoomSeekBar.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(seekBar: SeekBar?, progress: Int, fromUser: Boolean) {
                updateZoomLabel()
                sendServiceControl(JSONObject().put("zoom", zoomForProgress(progress)))
            }
            override fun onStartTrackingTouch(seekBar: SeekBar?) = Unit
            override fun onStopTrackingTouch(seekBar: SeekBar?) = Unit
        })
        torchSwitch.setOnCheckedChangeListener { _, checked ->
            sendServiceControl(JSONObject().put("torch", checked))
        }
        manualFocusSwitch.setOnCheckedChangeListener { _, checked ->
            focusSeekBar.isEnabled = checked
            sendServiceControl(JSONObject()
                .put("af", if (checked) "manual" else "auto")
                .put("focus", focusDistanceForProgress(focusSeekBar.progress)))
        }
        focusSeekBar.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(seekBar: SeekBar?, progress: Int, fromUser: Boolean) {
                sendServiceControl(JSONObject()
                    .put("af", if (manualFocusSwitch.isChecked) "manual" else "auto")
                    .put("focus", focusDistanceForProgress(progress)))
            }
            override fun onStartTrackingTouch(seekBar: SeekBar?) = Unit
            override fun onStopTrackingTouch(seekBar: SeekBar?) = Unit
        })
        exposureLockSwitch.setOnCheckedChangeListener { _, checked ->
            if (checked && faceExposureSwitch.isChecked) faceExposureSwitch.isChecked = false
            sendServiceControl(JSONObject().put("ae", if (checked) "lock" else "auto"))
        }
        faceExposureSwitch.setOnCheckedChangeListener { _, checked ->
            if (checked && exposureLockSwitch.isChecked) exposureLockSwitch.isChecked = false
            sendServiceControl(JSONObject().put("faceExposure", checked))
        }
        exposureSeekBar.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(seekBar: SeekBar?, progress: Int, fromUser: Boolean) {
                val chars = cameraCharacteristics(selectedCameraId())
                val range = chars?.get(android.hardware.camera2.CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)
                val index = (range?.lower ?: 0) + progress
                sendServiceControl(JSONObject().put("ev", index))
                val step = chars?.get(android.hardware.camera2.CameraCharacteristics.CONTROL_AE_COMPENSATION_STEP)
                exposureLabel.text = "曝光补偿 %.1f EV".format(index * (step?.toFloat() ?: 1f))
            }
            override fun onStartTrackingTouch(seekBar: SeekBar?) = Unit
            override fun onStopTrackingTouch(seekBar: SeekBar?) = Unit
        })
        awbLockSwitch.setOnCheckedChangeListener { _, checked ->
            sendServiceControl(JSONObject().put("awb", if (checked) "lock" else "auto"))
        }
        stabilizationSwitch.setOnCheckedChangeListener { _, checked ->
            sendServiceControl(JSONObject().put("stab", checked))
        }
        microphoneSwitch.setOnCheckedChangeListener { _, checked ->
            if (checked && ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
                != PackageManager.PERMISSION_GRANTED
            ) {
                ActivityCompat.requestPermissions(this, arrayOf(Manifest.permission.RECORD_AUDIO), REQ_MIC)
            } else setAudioEnabled(checked)
        }
        rawMicrophoneSwitch.setOnCheckedChangeListener { _, checked ->
            rawMicrophone = checked
            if (audioEnabled && running) setAudioEnabled(true)
        }
        blackoutSwitch.setOnCheckedChangeListener { _, checked ->
            if (checked && running) {
                blackoutOverlay.visibility = View.VISIBLE
                window.addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            } else {
                blackoutOverlay.visibility = View.GONE
            }
        }
        blackoutOverlay.setOnClickListener {
            blackoutSwitch.isChecked = false
        }
        pauseSwitch.setOnCheckedChangeListener { _, checked -> setVideoPaused(checked) }

        toggleButton.setOnClickListener {
            if (!running) startSharing() else stopSharing()
        }
        discoverButton.setOnClickListener {
            discoverButton.isEnabled = false
            setStatus("正在搜索同一 Wi-Fi 下的 SideLens 电脑…")
            discoveryWorker.execute {
                val found = PcDiscovery(this).findReceiver()
                discoveredHost = found
                runOnUiThread {
                    discoverButton.isEnabled = true
                    if (found != null) {
                        hostInput.setText(found)
                        setStatus("已找到电脑 $found，可开始共享")
                    }
                    else setStatus("未找到电脑；USB 模式请先在 Windows 端配置 USB")
                }
            }
        }

        if (!hasPermissions()) {
            ActivityCompat.requestPermissions(this, requiredPermissions, REQ_PERMS)
        } else {
            setStatus("就绪")
        }
        if (intent?.action == SharingForegroundService.ACTION_STOP) stopSharing()
    }

    private fun hasPermissions(): Boolean = requiredPermissions.all {
        ContextCompat.checkSelfPermission(this, it) == PackageManager.PERMISSION_GRANTED
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == REQ_PERMS) {
            if (hasPermissions()) setStatus("就绪") else setStatus("权限被拒绝，无法采集")
        } else if (requestCode == REQ_MIC) {
            if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
                setAudioEnabled(microphoneSwitch.isChecked)
            } else {
                microphoneSwitch.isChecked = false
                setStatus("麦克风权限未授予，摄像头共享仍可使用")
            }
        }
    }

    override fun onStart() {
        super.onStart()
        val filter = IntentFilter(SharingForegroundService.ACTION_STATUS)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(serviceStatusReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("DEPRECATION")
            registerReceiver(serviceStatusReceiver, filter)
        }
        serviceBound = bindService(
            Intent(this, SharingForegroundService::class.java),
            serviceConnection,
            Context.BIND_AUTO_CREATE,
        )
    }

    override fun onStop() {
        if (serviceBound) {
            unbindService(serviceConnection)
            serviceBound = false
        }
        sharingService = null
        try { unregisterReceiver(serviceStatusReceiver) } catch (_: IllegalArgumentException) {}
        super.onStop()
    }

    private fun updateSessionUi(active: Boolean, status: String) {
        running = active
        toggleButton.text = if (active) "停止共享" else "开始共享"
        cameraSpinner.isEnabled = !active
        profileSpinner.isEnabled = !active
        if (!active) {
            audioEnabled = false
            videoPaused = false
            microphoneSwitch.isChecked = false
            rawMicrophoneSwitch.isEnabled = false
            pauseSwitch.isChecked = false
            blackoutOverlay.visibility = View.GONE
            blackoutSwitch.isChecked = false
        }
        setStatus(status)
    }

    private fun startSharing() {
        if (running) return
        if (!hasPermissions()) {
            ActivityCompat.requestPermissions(this, requiredPermissions, REQ_PERMS)
            setStatus("请先授予相机权限")
            return
        }
        if (profiles.isEmpty()) {
            setStatus("当前镜头没有可用的相机/硬件编码器画质组合")
            return
        }
        val host = hostInput.text.toString().trim().ifBlank { discoveredHost ?: HOST }
        val profile = profiles[profileSpinner.selectedItemPosition.coerceIn(0, profiles.lastIndex)]
        val microphoneAllowed = microphoneSwitch.isChecked && ContextCompat.checkSelfPermission(
            this, Manifest.permission.RECORD_AUDIO,
        ) == PackageManager.PERMISSION_GRANTED
        val aeRange = cameraCharacteristics(selectedCameraId())
            ?.get(android.hardware.camera2.CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)
        val initialControl = JSONObject()
            .put("zoom", zoomForProgress(zoomSeekBar.progress))
            .put("torch", torchSwitch.isChecked)
            .put("af", if (manualFocusSwitch.isChecked) "manual" else "auto")
            .put("focus", focusDistanceForProgress(focusSeekBar.progress))
            .put("ae", if (exposureLockSwitch.isChecked) "lock" else "auto")
            .put("ev", (aeRange?.lower ?: 0) + exposureSeekBar.progress)
            .put("awb", if (awbLockSwitch.isChecked) "lock" else "auto")
            .put("stab", stabilizationSwitch.isChecked)
            .put("faceExposure", faceExposureSwitch.isChecked)
            .put("pause", pauseSwitch.isChecked)
            .put("mic", if (rawMicrophoneSwitch.isChecked) "raw" else "voice")
            .put("audio", microphoneAllowed)
        val serviceIntent = Intent(this, SharingForegroundService::class.java)
            .setAction(SharingForegroundService.ACTION_START)
            .putExtra(SharingForegroundService.EXTRA_AUDIO, microphoneAllowed)
            .putExtra(SharingForegroundService.EXTRA_RAW_MICROPHONE, rawMicrophoneSwitch.isChecked)
            .putExtra(SharingForegroundService.EXTRA_CAMERA_ID, selectedCameraId())
            .putExtra(SharingForegroundService.EXTRA_WIDTH, profile.width)
            .putExtra(SharingForegroundService.EXTRA_HEIGHT, profile.height)
            .putExtra(SharingForegroundService.EXTRA_FPS, profile.fps)
            .putExtra(SharingForegroundService.EXTRA_HOST, host)
            .putExtra(SharingForegroundService.EXTRA_HELLO, buildHello(profile).toString())
            .putExtra(SharingForegroundService.EXTRA_INITIAL_CONTROL, initialControl.toString())
        try {
            ContextCompat.startForegroundService(this, serviceIntent)
        } catch (e: Exception) {
            Log.e(TAG, "启动共享前台服务失败", e)
            setStatus("无法启动持续共享服务：${e.message ?: e.javaClass.simpleName}")
            return
        }
        audioEnabled = microphoneAllowed
        videoPaused = pauseSwitch.isChecked
        updateSessionUi(true, "连接中…")
    }

    private fun stopSharing() {
        if (!running && sharingService?.isSessionRunning() != true) return
        running = false
        audioEnabled = false
        videoPaused = false
        pauseSwitch.isChecked = false
        startService(Intent(this, SharingForegroundService::class.java).setAction(SharingForegroundService.ACTION_STOP))
        window.clearFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        blackoutOverlay.visibility = View.GONE
        blackoutSwitch.isChecked = false
        updateSessionUi(false, "正在停止…")
    }

    private fun loadCameras() {
        cameraIds = try {
            val manager = getSystemService(CAMERA_SERVICE) as android.hardware.camera2.CameraManager
            manager.cameraIdList.toList()
        } catch (e: Exception) {
            Log.w(TAG, "读取摄像头列表失败", e)
            emptyList()
        }
        val labels = cameraIds.mapIndexed { index, id ->
            val chars = try {
                (getSystemService(CAMERA_SERVICE) as android.hardware.camera2.CameraManager)
                    .getCameraCharacteristics(id)
            } catch (_: Exception) { null }
            val facing = when (chars?.get(android.hardware.camera2.CameraCharacteristics.LENS_FACING)) {
                android.hardware.camera2.CameraCharacteristics.LENS_FACING_FRONT -> "前置"
                android.hardware.camera2.CameraCharacteristics.LENS_FACING_BACK -> "后置"
                else -> "外接"
            }
            "${facing}镜头 ${index + 1} · ID $id"
        }.ifEmpty { listOf("没有可用摄像头") }
        activeCameraId = cameraIds.firstOrNull()
        cameraSpinner.adapter = ArrayAdapter(this, android.R.layout.simple_spinner_dropdown_item, labels)
        configuringCameraList = false
        updateCaptureProfiles(activeCameraId)
    }

    private fun updateCaptureProfiles(id: String?) {
        val chars = cameraCharacteristics(id)
        val map = chars?.get(android.hardware.camera2.CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
        val sizes = map?.getOutputSizes(android.graphics.SurfaceTexture::class.java)
            ?.map { it.width to it.height }?.toSet().orEmpty()
        val fpsRanges = chars?.get(android.hardware.camera2.CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)
            .orEmpty()
        val candidates = listOf(640 to 480, 1280 to 720, 1920 to 1080, 2560 to 1440, 3840 to 2160)
        profiles = candidates.flatMap { (width, height) ->
            listOf(30, 60).mapNotNull { fps ->
                val cameraSupports = (width to height) in sizes && fpsRanges.any { it.lower <= fps && it.upper >= fps }
                if (cameraSupports && VideoEncoder.supports(width, height, fps)) {
                    CaptureProfile(width, height, fps)
                } else null
            }
        }
        val labels = profiles.map { "${it.width}×${it.height} @ ${it.fps} fps" }
            .ifEmpty { listOf("当前镜头无可用编码模式") }
        profileSpinner.adapter = ArrayAdapter(this, android.R.layout.simple_spinner_dropdown_item, labels)
        profileSpinner.isEnabled = profiles.isNotEmpty() && !running
        toggleButton.isEnabled = profiles.isNotEmpty() || running
        val defaultIndex = profiles.indexOfFirst { it.width == WIDTH && it.height == HEIGHT && it.fps == FPS }
        profileSpinner.setSelection(defaultIndex.coerceAtLeast(0))
    }

    private fun selectedCameraId(): String = activeCameraId ?: CAMERA_ID

    private fun buildHello(profile: CaptureProfile): JSONObject {
        val manager = getSystemService(CAMERA_SERVICE) as android.hardware.camera2.CameraManager
        val appVersion = packageManager.getPackageInfo(packageName, 0).versionName ?: "unknown"
        val cameras = JSONArray()
        for (id in cameraIds) {
            val chars = try { manager.getCameraCharacteristics(id) } catch (_: Exception) { continue }
            val facing = when (chars.get(android.hardware.camera2.CameraCharacteristics.LENS_FACING)) {
                android.hardware.camera2.CameraCharacteristics.LENS_FACING_FRONT -> "front"
                android.hardware.camera2.CameraCharacteristics.LENS_FACING_BACK -> "back"
                else -> "external"
            }
            val streamMap = chars.get(android.hardware.camera2.CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
            val sizes = JSONArray()
            streamMap?.getOutputSizes(android.graphics.SurfaceTexture::class.java)?.forEach { size ->
                sizes.put(JSONArray().put(size.width).put(size.height))
            }
            val fps = JSONArray()
            chars.get(android.hardware.camera2.CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)
                ?.forEach { range -> fps.put(JSONArray().put(range.lower).put(range.upper)) }
            val maxZoom = chars.get(android.hardware.camera2.CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM) ?: 1f
            val minFocus = chars.get(android.hardware.camera2.CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE) ?: 0f
            cameras.put(JSONObject()
                .put("id", id)
                .put("facing", facing)
                .put("sensorOrientation", chars.get(android.hardware.camera2.CameraCharacteristics.SENSOR_ORIENTATION) ?: 0)
                .put("sizes", sizes)
                .put("fps", fps)
                .put("zoom", JSONArray().put(1.0).put(maxZoom))
                .put("flash", chars.get(android.hardware.camera2.CameraCharacteristics.FLASH_INFO_AVAILABLE) == true)
                .put("afManual", minFocus > 0f)
                .put("minFocus", minFocus))
        }
        return JSONObject()
            .put("proto", 1)
            .put("device", "${Build.MANUFACTURER} ${Build.MODEL}")
            .put("version", appVersion)
            .put("cameras", cameras)
            .put("state", JSONObject()
                .put("camera", selectedCameraId())
                .put("width", profile.width)
                .put("height", profile.height)
                .put("fps", profile.fps)
                .put("zoom", zoomForProgress(zoomSeekBar.progress))
                .put("audio", audioEnabled)
                .put("paused", videoPaused)
                .put("mic", if (rawMicrophone) "raw" else "voice"))
    }

    private fun applyDesktopControl(command: JSONObject) {
        if (command.has("zoom")) {
            val zoom = command.optDouble("zoom", 1.0).toFloat()
                .coerceIn(1f, maxZoom.coerceAtLeast(1f))
            zoomSeekBar.progress = (((zoom - 1f) / (maxZoom.coerceAtLeast(1f) - 1f).coerceAtLeast(0.001f)) * 100f)
                .toInt().coerceIn(0, 100)
        }
        if (command.has("torch")) torchSwitch.isChecked = command.optBoolean("torch", false)
        if (command.has("af")) {
            manualFocusSwitch.isChecked = command.optString("af") == "manual"
        }
        if (command.has("focus") && command.optString("af", "manual") != "auto") {
            val maxDistance = cameraCharacteristics(selectedCameraId())
                ?.get(android.hardware.camera2.CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE) ?: 0f
            if (maxDistance > 0f) {
                manualFocusSwitch.isChecked = true
                val distance = command.optDouble("focus", 0.0).toFloat().coerceIn(0f, maxDistance)
                focusSeekBar.progress = (distance / maxDistance * 100f).toInt()
            }
        }
        if (command.has("ae")) exposureLockSwitch.isChecked = command.optString("ae") == "lock"
        if (command.has("faceExposure")) {
            val requested = command.optBoolean("faceExposure", false)
            faceExposureSwitch.isChecked = requested && faceExposureSwitch.isEnabled
            if (requested && !faceExposureSwitch.isEnabled) {
                setStatus("当前摄像头不支持人脸检测测光")
            }
        }
        if (command.has("ev")) {
            val range = cameraCharacteristics(selectedCameraId())
                ?.get(android.hardware.camera2.CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)
            val lower = range?.lower ?: 0
            exposureSeekBar.progress = (command.optInt("ev", 0) - lower).coerceIn(0, exposureSeekBar.max)
        }
        if (command.has("awb")) awbLockSwitch.isChecked = command.optString("awb") == "lock"
        if (command.has("stab")) stabilizationSwitch.isChecked = command.optBoolean("stab", false)
        if (command.has("mic")) rawMicrophoneSwitch.isChecked = command.optString("mic") == "raw"
        if (command.has("audio")) microphoneSwitch.isChecked = command.optBoolean("audio", false)
        if (command.has("pause")) pauseSwitch.isChecked = command.optBoolean("pause", false)
        if (command.optBoolean("keyframe", false)) {
            sendServiceControl(JSONObject().put("keyframe", true))
        }
    }

    private fun setVideoPaused(paused: Boolean) {
        videoPaused = paused
        sendServiceControl(JSONObject().put("pause", paused))
        if (running) {
            setStatus(if (paused) "视频已暂停 · 电脑保留最后画面" else "正在恢复视频…")
        }
    }

    private fun setAudioEnabled(enabled: Boolean) {
        audioEnabled = enabled
        runOnUiThread { rawMicrophoneSwitch.isEnabled = enabled }
        if (!running) return
        startService(
            Intent(this, SharingForegroundService::class.java)
                .setAction(SharingForegroundService.ACTION_AUDIO)
                .putExtra(SharingForegroundService.EXTRA_AUDIO, enabled)
                .putExtra(SharingForegroundService.EXTRA_RAW_MICROPHONE, rawMicrophone),
        )
    }

    private fun sendServiceControl(command: JSONObject) {
        if (!running) return
        startService(
            Intent(this, SharingForegroundService::class.java)
                .setAction(SharingForegroundService.ACTION_CONTROL)
                .putExtra(SharingForegroundService.EXTRA_CONTROL, command.toString()),
        )
    }

    private fun readMaxZoom(id: String?): Float {
        if (id == null) return 1f
        return try {
            val manager = getSystemService(CAMERA_SERVICE) as android.hardware.camera2.CameraManager
            manager.getCameraCharacteristics(id).get(
                android.hardware.camera2.CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM
            ) ?: 1f
        } catch (_: Exception) { 1f }
    }

    private fun cameraCharacteristics(id: String?): android.hardware.camera2.CameraCharacteristics? {
        if (id == null) return null
        return try {
            (getSystemService(CAMERA_SERVICE) as android.hardware.camera2.CameraManager)
                .getCameraCharacteristics(id)
        } catch (_: Exception) { null }
    }

    private fun updateCameraCapabilities(id: String?) {
        val chars = cameraCharacteristics(id)
        maxZoom = chars?.get(android.hardware.camera2.CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM) ?: 1f
        updateZoomLabel()
        torchSwitch.isEnabled = chars?.get(android.hardware.camera2.CameraCharacteristics.FLASH_INFO_AVAILABLE) == true
        val minFocus = chars?.get(android.hardware.camera2.CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE) ?: 0f
        manualFocusSwitch.isEnabled = minFocus > 0f
        focusSeekBar.isEnabled = minFocus > 0f && manualFocusSwitch.isChecked
        exposureLockSwitch.isEnabled = chars?.get(android.hardware.camera2.CameraCharacteristics.CONTROL_AE_LOCK_AVAILABLE) == true
        val faceModes = chars?.get(
            android.hardware.camera2.CameraCharacteristics.STATISTICS_INFO_AVAILABLE_FACE_DETECT_MODES
        )
        val faceSupported = (chars?.get(
            android.hardware.camera2.CameraCharacteristics.CONTROL_MAX_REGIONS_AE
        ) ?: 0) > 0 && faceModes?.any {
            it == android.hardware.camera2.CaptureRequest.STATISTICS_FACE_DETECT_MODE_SIMPLE ||
                it == android.hardware.camera2.CaptureRequest.STATISTICS_FACE_DETECT_MODE_FULL
        } == true
        faceExposureSwitch.isEnabled = faceSupported
        if (!faceSupported) faceExposureSwitch.isChecked = false
        val aeRange = chars?.get(android.hardware.camera2.CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)
        exposureSeekBar.max = ((aeRange?.upper ?: 0) - (aeRange?.lower ?: 0)).coerceAtLeast(0)
        exposureSeekBar.progress = (0 - (aeRange?.lower ?: 0)).coerceIn(0, exposureSeekBar.max)
        awbLockSwitch.isEnabled = chars?.get(android.hardware.camera2.CameraCharacteristics.CONTROL_AWB_LOCK_AVAILABLE) == true
        stabilizationSwitch.isEnabled = chars?.get(
            android.hardware.camera2.CameraCharacteristics.CONTROL_AVAILABLE_VIDEO_STABILIZATION_MODES
        )?.contains(android.hardware.camera2.CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE_ON) == true
    }

    private fun focusDistanceForProgress(progress: Int): Float {
        val maximum = cameraCharacteristics(selectedCameraId())
            ?.get(android.hardware.camera2.CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE) ?: 0f
        return maximum * progress.coerceIn(0, 100) / 100f
    }

    private fun camMaxZoom(): Float = readMaxZoom(selectedCameraId())

    private fun zoomForProgress(progress: Int): Float =
        1f + (maxZoom.coerceAtLeast(1f) - 1f) * progress / 100f

    private fun updateZoomLabel() {
        zoomLabel.text = "变焦 %.1f×".format(zoomForProgress(zoomSeekBar.progress))
    }

    private fun setStatus(s: String) {
        // 有升级提示时把它并进状态栏：服务每 2 秒刷新一次状态，
        // 若只写原始状态，用户会在两秒内看不到那句"请升级"。
        val notice = pendingUpgradeNotice
        statusText.text = if (notice != null) "$s · $notice" else s
        Log.i(TAG, s)
    }

    override fun onDestroy() {
        discoveryWorker.shutdownNow()
        super.onDestroy()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        if (intent.action == SharingForegroundService.ACTION_STOP) stopSharing()
    }

    companion object {
        private const val TAG = "SideLens"
        private const val REQ_PERMS = 1001
        private const val REQ_MIC = 1002
        private const val CAMERA_ID = "0"
        private const val WIDTH = 1280
        private const val HEIGHT = 720
        private const val FPS = 30
        private const val HOST = "127.0.0.1"     // adb reverse 后指向本机
        private const val PORT = 27300
    }

    private data class CaptureProfile(val width: Int, val height: Int, val fps: Int)
}
