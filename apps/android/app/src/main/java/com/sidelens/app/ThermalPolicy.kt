package com.sidelens.app

/** Hysteretic thermal policy: heating responds immediately; cooling takes one minute per level. */
class ThermalPolicy {
    data class Limits(
        val level: Int,
        val fps: Int,
        val bitrate: Int,
        val maxWidth: Int,
        val maxHeight: Int,
        val throttled: Boolean,
    )

    @Volatile private var level = 0
    private var coolSinceMs: Long? = null

    @Synchronized
    fun update(status: Int, headroom: Float?, batteryTempC: Float?, nowMs: Long = System.currentTimeMillis()): Limits {
        val target = when {
            status >= 5 || (headroom != null && headroom >= 0.90f) || (batteryTempC != null && batteryTempC >= 46f) -> 4
            status >= 4 || (headroom != null && headroom >= 0.75f) || (batteryTempC != null && batteryTempC >= 43f) -> 3
            status >= 2 || (headroom != null && headroom >= 0.65f) || (batteryTempC != null && batteryTempC >= 41f) -> 2
            else -> 0
        }

        if (target > level) {
            level = target
            coolSinceMs = null
        } else if (target < level) {
            val since = coolSinceMs ?: nowMs.also { coolSinceMs = it }
            if (nowMs - since >= COOL_DOWN_MS) {
                level = when (level) {
                    4 -> 3
                    3 -> 2
                    else -> 0
                }
                coolSinceMs = nowMs
                if (level == target) coolSinceMs = null
            }
        } else {
            coolSinceMs = null
        }

        return when (level) {
            // Camera2 profiles are negotiated in 30/60fps steps; 30 is the lowest
            // mode exposed by the current UI and avoids asking the AE range for 24fps.
            4 -> Limits(4, 30, 1_600_000, 640, 480, true)
            3 -> Limits(3, 30, 2_000_000, 1280, 720, true)
            2 -> Limits(2, 30, 3_000_000, 1920, 1080, true)
            else -> Limits(0, 60, 4_000_000, Int.MAX_VALUE, Int.MAX_VALUE, false)
        }
    }

    companion object {
        private const val COOL_DOWN_MS = 60_000L
    }
}
