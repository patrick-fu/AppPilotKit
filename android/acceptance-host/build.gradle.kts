import org.gradle.api.DefaultTask
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.ListProperty
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputDirectory
import org.gradle.api.tasks.TaskAction
import java.util.zip.ZipFile

abstract class VerifyReleaseExcludesAcceptanceTransport : DefaultTask() {
    @get:InputDirectory
    abstract val apkDirectory: DirectoryProperty

    @get:Input
    abstract val releaseDependencyNames: ListProperty<String>

    @TaskAction
    fun verify() {
        check(releaseDependencyNames.get().none { it.contains("target-transport-internal") }) {
            "Release runtime classpath contains target-transport-internal"
        }

        val apk = apkDirectory.get().asFile.listFiles()
            ?.singleOrNull { candidate -> candidate.extension == "apk" }
            ?: error("Expected exactly one release APK")
        val forbiddenMarkers = listOf(
            "AppPilotKitBootstrapActivity",
            "TargetTransportBootstrap",
            "TargetTransport",
            "NativeTransport",
            "dev.apppilotkit.targettransport",
            "dev.apppilotkit.transport",
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
                "Release APK contains an internal transport, bootstrap, or native marker"
            }
        }
    }
}

plugins {
    id("com.android.application")
}

android {
    namespace = "dev.apppilotkit.acceptancehost"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.apppilotkit.acceptancehost"
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
    testDebugImplementation(kotlin("test"))
    testDebugImplementation("junit:junit:4.13.2")
}

val verifyReleaseExcludesAcceptanceTransport by tasks.registering(VerifyReleaseExcludesAcceptanceTransport::class) {
    dependsOn("assembleRelease")
    apkDirectory.set(layout.buildDirectory.dir("outputs/apk/release"))
    releaseDependencyNames.set(configurations.named("releaseRuntimeClasspath").map { configuration ->
        configuration.incoming.resolutionResult.allComponents.map { component -> component.id.displayName }
    })
}

val verifyReleaseExcludesInternalTransport by tasks.registering {
    dependsOn(verifyReleaseExcludesAcceptanceTransport)
}

val verifyAcceptanceHostArtifacts by tasks.registering {
    dependsOn("assembleDebug", verifyReleaseExcludesAcceptanceTransport)
}

tasks.named("check") {
    dependsOn(verifyReleaseExcludesAcceptanceTransport)
}
