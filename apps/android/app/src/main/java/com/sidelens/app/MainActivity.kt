package com.sidelens.app

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import android.util.Log
import android.view.Surface
import android.widget.Button
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import java.util.concurrent.Executors

/**
 * SideLens 主界面。
 *
 * 职责很窄：申请权限、开关采集、显示状态。
 * 真正的采集与编码在 [CameraCapture] 和 [VideoEncoder] 里。
 *
 * # 为什么所有网络操作都在后台线程
 *
 * Android 从 3.0 起禁止在主线程做网络操作，违反会抛
 * `NetworkOnMainThreadException`。第一版把 `FrameSender.connect()`
 * 直接放在点击回调里，一点就崩 —— 见 progress.md 第二十一轮。
 */
class MainActivity : AppCompatActivity() {

    private lateinit var statusText: TextView
    private lateinit var toggleButton: Button

    private var capture: CameraCapture? = null
    private var encoder: VideoEncoder? = null
    private var sender: FrameSender? = null

    /** 网络与采集的生命周期线程 */
    private val worker = Executors.newSingleThreadExecutor()
    /** 编码结果轮询线程 */
    private val drainer = Executors.newSingleThreadExecutor()

    @Volatile private var running = false

    private val requiredPermissions = arrayOf(
        Manifest.permission.CAMERA,
        Manifest.permission.RECORD_AUDIO,
    )

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        statusText = findViewById(R.id.statusText)
        toggleButton = findViewById(R.id.toggleButton)

        toggleButton.setOnClickListener {
            if (!running) startSharing() else stopSharing()
        }

        if (!hasPermissions()) {
            ActivityCompat.requestPermissions(this, requiredPermissions, REQ_PERMS)
        } else {
            setStatus("就绪")
        }
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
        }
    }

    private fun startSharing() {
        if (running) return
        running = true
        toggleButton.text = getString(android.R.string.cancel)
        setStatus("连接中…")

        // 全部放到后台：Socket.connect 在主线程会抛 NetworkOnMainThreadException
        worker.execute {
            try {
                val s = FrameSender()
                s.connect(HOST, PORT)
                sender = s
                runOnUiThread { setStatus("已连接，启动采集…") }

                val enc = VideoEncoder(WIDTH, HEIGHT, FPS)
                enc.start()
                encoder = enc

                val inputSurface: Surface = enc.inputSurface
                val cam = CameraCapture(
                    this, CAMERA_ID, WIDTH, HEIGHT, FPS, inputSurface,
                ) {
                    runOnUiThread {
                        setStatus("共享中 · ${WIDTH}x${HEIGHT}@${FPS}fps")
                    }
                }
                cam.start()
                capture = cam

                // 轮询编码输出
                drainer.execute {
                    while (running) {
                        try {
                            enc.drainEncoder { data, ptsUs, isKey ->
                                s.send(data, ptsUs, isKey)
                            }
                        } catch (e: Exception) {
                            Log.e(TAG, "编码轮询失败", e)
                            break
                        }
                        // 编码器没产出时稍等，避免空转烧 CPU
                        Thread.sleep(2)
                    }
                }
            } catch (e: Exception) {
                Log.e(TAG, "启动失败", e)
                runOnUiThread {
                    setStatus("启动失败：${e.javaClass.simpleName} ${e.message ?: ""}")
                    stopSharing()
                }
            }
        }
    }

    private fun stopSharing() {
        running = false
        capture?.stop(); capture = null
        encoder?.stop(); encoder = null
        sender?.close(); sender = null
        toggleButton.text = "开始共享"
        setStatus("已停止")
    }

    private fun setStatus(s: String) {
        statusText.text = s
        Log.i(TAG, s)
    }

    override fun onDestroy() {
        stopSharing()
        worker.shutdownNow()
        drainer.shutdownNow()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "SideLens"
        private const val REQ_PERMS = 1001
        private const val CAMERA_ID = "0"        // 0 = 后置
        private const val WIDTH = 1280
        private const val HEIGHT = 720
        private const val FPS = 30
        private const val HOST = "127.0.0.1"     // adb reverse 后指向本机
        private const val PORT = 27300
    }
}
