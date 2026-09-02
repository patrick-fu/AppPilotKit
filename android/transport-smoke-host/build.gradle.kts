import org.gradle.api.DefaultTask
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.ListProperty
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputDirectory
import org.gradle.api.tasks.TaskAction
import java.util.zip.ZipFile

abstract class VerifyReleaseExcludesInternalTransport : DefaultTask() {
    @get:InputDirectory
    abstract val apkDirectory: DirectoryProperty

    @get:Input
    abstract val releaseDependencyNames: ListProperty<String>

    @TaskAction
    fun verify() {
        check("target-transport-internal" !in releaseDependencyNames.get()) {
            "Release runtime classpath contains target-transport-internal"
        }

        val apk = apkDirectory.get().asFile.listFiles()
            ?.singleOrNull { candidate -> candidate.extension == "apk" }
            ?: error("Expected exactly one release APK")
        val forbiddenMarkers = listOf(
            "AppPilotKitBootstrapActivity",
            "TargetTransportBootstrap",
            "apppilotkit_transport",
            "Noise_NK_",
            "Noise_NNpsk0_",
        )
        ZipFile(apk).use { archive ->
            val entries = archive.entries().asSequence().toList()
            check(entries.none { entry -> entry.name.endsWith(".so") }) {
                "Release APK contains a native library"
            }
            val payload = buildString {
                entries.forEach { entry ->
                    archive.getInputStream(entry).bufferedReader(Charsets.ISO_8859_1).use { append(it.readText()) }
                }
            }
            check(forbiddenMarkers.none(payload::contains)) {
                "Release APK contains an internal transport marker"
            }
        }
    }
}

plugins {
    id("com.android.application")
}

android {
    namespace = "dev.apppilotkit.smokehost"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.apppilotkit.smokehost"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "1.0"
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
        }
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    debugImplementation(project(":semantic-registry"))
    debugImplementation(project(":protocol-runtime"))
    debugImplementation(project(":target-transport-internal"))
}

val verifyReleaseExcludesInternalTransport by tasks.registering(VerifyReleaseExcludesInternalTransport::class) {
    dependsOn("assembleRelease")
    apkDirectory.set(layout.buildDirectory.dir("outputs/apk/release"))
    releaseDependencyNames.set(configurations.named("releaseRuntimeClasspath").map { configuration ->
        configuration.allDependencies.map { dependency -> dependency.name }
    })
}

tasks.named("check") {
    dependsOn(verifyReleaseExcludesInternalTransport)
}
