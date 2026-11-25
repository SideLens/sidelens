package com.sidelens.app

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 手机端升级提示的解析与判据。
 *
 * # 为什么这些测试值得存在
 *
 * PhoneUpgradePrompt.fromControl 运行在控制消息回调里。它抛异常会让
 * **同一条网络消息里的其它控制指令一起丢掉**（音量、暂停、对焦全都失效），
 * 而症状是「电脑端点了按钮没反应」—— 极难定位到「某条 JSON 少了一个字段」。
 * 因此这里的重点是「什么情况下返回 null 而不是炸」。
 *
 * 判据本身（谁比谁大、协议优先）的权威实现在 Rust 侧
 * crates/sidelens-update/src/decision.rs；这里只保证两侧读同一份字段名。
 */
class PhoneUpgradePromptTest {

    private fun control(payload: String): JSONObject = JSONObject(payload)

    @Test
    fun mustUpgradeIsBlocking() {
        val prompt = PhoneUpgradePrompt.fromControl(
            control(
                """{"phone_upgrade":{"level":"must","min_code":3,"latest_code":5,"latest_name":"0.3.0","min_protocol":1,"message":"请升级到 0.3.0"}}"""
            )
        )
        assertNotNull(prompt)
        assertEquals(PhoneUpgradePrompt.LEVEL_MUST, prompt!!.level)
        assertEquals(5, prompt.latestVersionCode)
        assertEquals("0.3.0", prompt.latestVersionName)
        assertEquals(3, prompt.minSupportedCode)
        assertTrue("must 级别必须阻塞共享", prompt.blocking)
        assertEquals("请升级到 0.3.0", prompt.message)
    }

    @Test
    fun recommendedIsNotBlocking() {
        val prompt = PhoneUpgradePrompt.fromControl(
            control("""{"phone_upgrade":{"level":"recommended","latest_code":4,"latest_name":"0.2.1"}}""")
        )
        assertNotNull(prompt)
        assertFalse(prompt!!.blocking)
        // 电脑端没给文案时用兜底文案，不能是空串
        assertTrue(prompt.message.isNotEmpty())
    }

    @Test
    fun unsupportedProtocolIsBlocking() {
        val prompt = PhoneUpgradePrompt.fromControl(
            control("""{"phone_upgrade":{"level":"unsupported","min_protocol":2}}""")
        )
        assertNotNull(prompt)
        assertEquals(PhoneUpgradePrompt.LEVEL_UNSUPPORTED, prompt!!.level)
        assertTrue(prompt.blocking)
        assertEquals(2, prompt.minProtocol)
    }

    @Test
    fun missingFieldYieldsNull() {
        assertNull(PhoneUpgradePrompt.fromControl(control("""{"zoom":1.5,"torch":true}""")))
    }

    @Test
    fun unknownLevelIsIgnored() {
        // 电脑端将来可能加新级别（比如 critical）。老手机端不认识时
        // 应该「不提示」，而不是把未知字符串当文案显示出来。
        assertNull(
            PhoneUpgradePrompt.fromControl(
                control("""{"phone_upgrade":{"level":"critical","message":"x"}}""")
            )
        )
        assertNull(PhoneUpgradePrompt.fromControl(control("""{"phone_upgrade":{"level":""}}""")))
    }

    @Test
    fun levelIsCaseInsensitiveAndFieldsMayBeMissing() {
        val prompt = PhoneUpgradePrompt.fromControl(
            control("""{"phone_upgrade":{"level":"MUST"}}""")
        )
        assertNotNull("level 应大小写不敏感", prompt)
        // 缺失的数字字段回落成 0，而不是抛异常
        assertEquals(0, prompt!!.latestVersionCode)
        assertEquals(0, prompt.minSupportedCode)
    }

    @Test
    fun malformedStructuresDoNotThrow() {
        // phone_upgrade 是字符串而不是对象
        assertNull(PhoneUpgradePrompt.fromControl(control("""{"phone_upgrade":"must"}""")))
        // phone_upgrade 是 null
        assertNull(PhoneUpgradePrompt.fromControl(control("""{"phone_upgrade":null}""")))
        // 数字类型写成字符串：optInt 会尝试解析，失败回落 0
        val prompt = PhoneUpgradePrompt.fromControl(
            control("""{"phone_upgrade":{"level":"must","latest_code":"5"}}""")
        )
        assertNotNull(prompt)
    }

    @Test
    fun defaultMessagesAreNeverEmpty() {
        for (level in listOf(
            PhoneUpgradePrompt.LEVEL_MUST,
            PhoneUpgradePrompt.LEVEL_RECOMMENDED,
            PhoneUpgradePrompt.LEVEL_UNSUPPORTED,
        )) {
            assertTrue(
                "$level 的默认文案不能为空",
                PhoneUpgradePrompt.defaultMessage(level).isNotEmpty(),
            )
        }
    }

    @Test
    fun protocolVersionConstantIsPositive() {
        // 与 Rust 侧 crates/sidelens-phone-proto 的 VERSION 一致性由
        // 该 crate 的 tests/protocol_version.rs 与这里的值共同守住。
        assertTrue(PhoneProtocol.VERSION >= 1)
    }
}
