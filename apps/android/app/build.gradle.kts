plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.sidelens.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.sidelens.app"
        minSdk = 26          // MediaCodec Surface 输入 + Camera2 稳定
        targetSdk = 35
        // 与发布清单的 phone.latest_version_code / phone.latest_version_name 对齐。
        // 每次发手机包都要同时改这里和 package-release.ps1 的同名参数，
        // tools/verify-release.ps1 会断言两者一致 —— 忘了改会被脚本挡住。
        versionCode = 2
        versionName = "0.2.0"

        ndk {
            abiFilters += listOf("arm64-v8a")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    // 单元测试里直接解析 JSON，需要真实的 org.json 实现
    // （Android SDK 自带的 org.json 在 JVM 测试里是桩，会抛 "not mocked"）
    testOptions {
        unitTests.isReturnDefaultValues = true
    }

    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")
}

dependencies {
    // JVM 单元测试：判据与解析逻辑不碰 Android framework，
    // 因此跑在普通 JVM 上就够了（不需要 Robolectric 或真机）。
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")

    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.constraintlayout:constraintlayout:2.1.4")
}