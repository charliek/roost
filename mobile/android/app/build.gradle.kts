import java.util.Properties

plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// The release key, from android/key.properties (gitignored; written by
// tools/mobile/link-signing.sh). Without that file a release build is signed
// with the debug key, so `flutter build apk --release` works in CI and on a
// fresh clone; that APK cannot update an installed release.
val keyPropertiesFile: File = rootProject.file("key.properties")
val releaseKey: Properties? =
    if (keyPropertiesFile.exists()) {
        Properties().apply { keyPropertiesFile.inputStream().use { load(it) } }
    } else {
        null
    }

fun Properties.required(name: String): String =
    getProperty(name)?.takeIf { it.isNotBlank() }
        ?: throw GradleException("$keyPropertiesFile has no $name")

if (releaseKey == null &&
    gradle.startParameter.taskNames.any { it.contains("release", ignoreCase = true) }
) {
    logger.warn("warning: no $keyPropertiesFile; signing the release build with the debug key")
}

android {
    namespace = "ai.stridelabs.roost"
    compileSdk = flutter.compileSdkVersion
    // The NDK CI installs and tools/mobile/android-env.sh builds libghostty-vt with.
    ndkVersion = "28.2.13676358"

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "ai.stridelabs.roost"
        // The API level the native libraries are linked against
        // (ROOST_ANDROID_API in tools/mobile/android-env.sh).
        minSdk = 26
        targetSdk = flutter.targetSdkVersion
        // Uses the version code from pubspec.yaml. When using split APKs, 1000 * ABI_VERSION
        // is added automatically by Flutter. (https://developer.android.com/studio/build/configure-apk-splits#configure-APK-versions)
        // You can force using the value of versionCode by specifying the `-P force-version-code-ignoring-abi=true`
        // flag during build.
        versionCode = flutter.versionCode
        versionName = flutter.versionName
        ndk {
            // libghostty-vt is built for these two ABIs only (`make mobile-deps`).
            // Replaced, not added to: the Flutter plugin has already put its
            // default three (armeabi-v7a too) here by the time this runs.
            abiFilters.clear()
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    signingConfigs {
        if (releaseKey != null) {
            create("release") {
                // A relative storeFile is beside the properties file the
                // link points at (tools/mobile/link-signing.sh).
                storeFile =
                    keyPropertiesFile.toPath().toRealPath().parent
                        .resolve(releaseKey.required("storeFile")).toFile()
                storePassword = releaseKey.required("storePassword")
                keyAlias = releaseKey.required("keyAlias")
                keyPassword = releaseKey.required("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}
