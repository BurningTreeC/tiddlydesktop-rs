import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

android {
    compileSdk = 36
    namespace = "com.burningtreec.tiddlydesktop_rs"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.burningtreec.tiddlydesktop_rs"
        minSdk = 24
        targetSdk = 36
        // Android has its own versioning, separate from desktop releases
        // versionCode must increase with each Play Store release
        versionCode = 1
        versionName = "0.0.1"
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
            packaging {
                // Extract native libs to filesystem - required for libnode.so executable
                jniLibs.useLegacyPackaging = true
                // Include versioned .so files (libz.so.1, libssl.so.3, etc.)
                jniLibs.pickFirsts.add("**/libz.so.1")
                jniLibs.pickFirsts.add("**/libcrypto.so.3")
                jniLibs.pickFirsts.add("**/libssl.so.3")
                jniLibs.pickFirsts.add("**/libicui18n.so.78")
                jniLibs.pickFirsts.add("**/libicuuc.so.78")
                jniLibs.pickFirsts.add("**/libicudata.so.78")
            }
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("androidx.documentfile:documentfile:1.0.1")
    implementation("com.google.android.material:material:1.12.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")