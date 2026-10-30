plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "org.zerolink"
    compileSdk = 34

    defaultConfig {
        applicationId = "org.zerolink"
        minSdk = 29
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"

        ndk {
            // ABIs to include for the native Rust JNI library
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
        debug {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    // The prebuilt libzerolink_jni.so is expected under
    // app/src/main/jniLibs/<abi>/libzerolink_jni.so
    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }
}

dependencies {
    // AndroidX Core
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")

    // Lifecycle
    implementation("androidx.lifecycle:lifecycle-service:2.8.3")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.3")

    // Coroutines
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")

    // WorkManager (Doze-compatible scheduling)
    implementation("androidx.work:work-runtime-ktx:2.9.0")
}
