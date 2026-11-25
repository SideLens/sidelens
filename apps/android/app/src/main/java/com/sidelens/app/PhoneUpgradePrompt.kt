package com.sidelens.app

import org.json.JSONObject

/**
 * 电脑端下发的「手机端需要升级」提示。
 *
 * # 为什么由电脑端下发，而不是手机端自己去商店查
 *
 * 手机端不知道自己该多新 —— 只有电脑端知道。电脑端从发布清单的 phone 段
 * 读到两个数字（min_supported_version_code / latest_version_code），
 * 按协议自己在 STATE 里上报的 versionCode 判断，结论只有四种：
 * 无需提示、建议升级、必须升级、协议不兼容。
 *
 * 把这个判断放在电脑端还有一个现实好处：手机端可能没联网、可能装的是
 * 内测包、可能来自第三方应用市场，指望它自己检查更新是不可靠的。
 *
 * # 判据与 Rust 侧一一对应
 *
 * 见 crates/sidelens-update/src/decision.rs 的 phone_hint()。两侧的
 * 分支顺序必须一致（协议下限优先），否则会出现"电脑端说是协议问题、
 * 手机端显示的是版本问题"这种自相矛盾。
 */
data class PhoneUpgradePrompt(
    /** must | recommended | unsupported */
    val level: String,
    val latestVersionCode: Int = 0,
    val latestVersionName: String = "",
    val minSupportedCode: Int = 0,
    val minProtocol: Int = 0,
    val message: String = "",
) {
    /** 是否必须马上升级（不升级就没法正常共享） */
    val blocking: Boolean get() = level == "must" || level == "unsupported"

    companion object {
        const val LEVEL_MUST = "must"
        const val LEVEL_RECOMMENDED = "recommended"
        const val LEVEL_UNSUPPORTED = "unsupported"

        /**
         * 从一条电脑端 CONTROL 消息里取升级提示。
         *
         * 返回 null 表示这条消息里没有升级提示（电脑端没配置清单、
         * 或者手机端版本已经够新）。**不抛异常** —— 这个方法在控制消息
         * 回调里执行，抛异常会让后面所有控制指令一起丢掉。
         */
        fun fromControl(command: JSONObject): PhoneUpgradePrompt? {
            val upgrade = command.optJSONObject("phone_upgrade") ?: return null
            val level = upgrade.optString("level").lowercase()
            if (level != LEVEL_MUST && level != LEVEL_RECOMMENDED && level != LEVEL_UNSUPPORTED) {
                return null
            }
            return PhoneUpgradePrompt(
                level = level,
                latestVersionCode = upgrade.optInt("latest_code", 0),
                latestVersionName = upgrade.optString("latest_name", ""),
                minSupportedCode = upgrade.optInt("min_code", 0),
                minProtocol = upgrade.optInt("min_protocol", 0),
                message = upgrade.optString("message", defaultMessage(level)),
            )
        }

        /** 电脑端没给文案时的兜底（两侧文案风格保持一致） */
        fun defaultMessage(level: String): String = when (level) {
            LEVEL_MUST -> "请升级 SideLens 手机端后继续共享"
            LEVEL_UNSUPPORTED -> "手机端协议过旧，请升级后继续共享"
            LEVEL_RECOMMENDED -> "SideLens 手机端有新版本，建议更新"
            else -> ""
        }
    }
}
