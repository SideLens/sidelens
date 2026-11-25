package com.sidelens.app

import android.content.Context
import android.net.wifi.WifiManager
import android.util.Log
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/** Small UDP probe used to find a SideLens receiver on the current LAN. */
class PcDiscovery(private val context: Context) {
    fun findReceiver(timeoutMs: Long = 8_000): String? {
        val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        val lock = wifi?.createMulticastLock("SideLensDiscovery")?.apply {
            setReferenceCounted(false)
            acquire()
        }
        try {
            DatagramSocket().use { socket ->
                socket.broadcast = true
                socket.soTimeout = 700
                val probe = "SIDELENS_DISCOVER_V1".toByteArray(Charsets.UTF_8)
                val target = InetAddress.getByName("255.255.255.255")
                val deadline = System.currentTimeMillis() + timeoutMs
                while (System.currentTimeMillis() < deadline) {
                    try {
                        socket.send(DatagramPacket(probe, probe.size, target, DISCOVERY_PORT))
                    } catch (e: Exception) {
                        Log.w(TAG, "发送局域网搜索包失败: ${e.message}")
                    }
                    val buffer = ByteArray(128)
                    val response = DatagramPacket(buffer, buffer.size)
                    try {
                        socket.receive(response)
                        val message = String(response.data, response.offset, response.length, Charsets.UTF_8)
                        if (message.startsWith("SIDELENS_HERE_V1|")) {
                            return response.address.hostAddress
                        }
                    } catch (_: java.net.SocketTimeoutException) {
                        // Resend the probe until timeout so startup order does not matter.
                    }
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "局域网搜索失败", e)
        } finally {
            if (lock?.isHeld == true) lock.release()
        }
        return null
    }

    companion object {
        private const val TAG = "SideLens.Discovery"
        private const val DISCOVERY_PORT = 27301
    }
}
