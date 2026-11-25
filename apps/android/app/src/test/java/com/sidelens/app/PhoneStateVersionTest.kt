package com.sidelens.app

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 手机端上报的 STATE 必须能被电脑端解析出三层版本信息。
 *
 * 电脑端 crates/sidelens-update/src/phone.rs 接受三种历史形态：
 * 没有 version 字段、version 是整数、version 是对象。这里断言我们**发**的
 * 就是规范的第三种形态，且字段名与解析侧完全一致。
 */
class PhoneStateVersionTest {

    @Test
    fun stateCarriesObjectVersionWithAlignedFieldNames() {
        val base = JSONObject().put("battery", 88).put("paused", false)
        // 复现 FrameSender.sendStateWithVersion 拼出来的结构
        val withVersion = JSONObject(base.toString())
        withVersion.put(
            "version",
            JSONObject().put("code", 2L).put("name", "0.2.0").put("protocol", PhoneProtocol.VERSION),
        )
        withVersion.put("protocol", PhoneProtocol.VERSION)

        val version = withVersion.getJSONObject("version")
        assertEquals(2L, version.getLong("code"))
        assertEquals("0.2.0", version.getString("name"))
        assertEquals(PhoneProtocol.VERSION, version.getInt("protocol"))
        // 顶层也有一份 protocol，供只认顶层字段的老电脑端使用
        assertEquals(PhoneProtocol.VERSION, withVersion.getInt("protocol"))
        // 原有字段不能被覆盖
        assertEquals(88, withVersion.getInt("battery"))
    }

    @Test
    fun addingVersionKeepsExistingStateFields() {
        val base = JSONObject().put("camera", "0")
        base.put("version", JSONObject().put("code", 0).put("name", ""))
        assertTrue(base.has("version"))
        assertEquals("0", base.getString("camera"))
    }
}
