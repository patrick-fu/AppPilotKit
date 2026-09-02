import com.android.build.api.variant.LibraryAndroidComponentsExtension
import org.gradle.api.tasks.Exec
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.Sync

plugins {
    id("com.android.library")
}

val androidNdkVersion = "27.3.13750724"
val rustTargetDirectory = layout.buildDirectory.dir("rust")
val generatedJniDirectory = layout.buildDirectory.dir("generated/jniLibs")
val rustCoreDirectory = file("../../transport/crypto-core")
val rustFfiDirectory = rustCoreDirectory.resolve("ffi")
val rustBuildInputs = fileTree(rustCoreDirectory) {
    include(
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "src/**",
        "ffi/Cargo.toml",
        "ffi/Cargo.lock",
        "ffi/rust-toolchain.toml",
        "ffi/build.rs",
        "ffi/src/**",
        "ffi/*.exports",
        "ffi/*.map",
    )
}

android {
    ndkVersion = androidNdkVersion
}

val ndkDirectory = extensions.getByType<LibraryAndroidComponentsExtension>().sdkComponents.ndkDirectory
val hostName = System.getProperty("os.name").lowercase()
val hostArchitecture = System.getProperty("os.arch").lowercase()
val ndkHostTag = when {
    hostName.startsWith("mac") && hostArchitecture in setOf("aarch64", "arm64") -> "darwin-arm64"
    hostName.startsWith("mac") -> "darwin-x86_64"
    hostName.startsWith("linux") && hostArchitecture in setOf("aarch64", "arm64") -> "linux-aarch64"
    hostName.startsWith("linux") -> "linux-x86_64"
    hostName.startsWith("windows") -> "windows-x86_64"
    else -> error("Unsupported Android NDK host: $hostName $hostArchitecture")
}
val ndkLinkerExecutableSuffix = if (ndkHostTag.startsWith("windows")) ".cmd" else ""
val rustFlags = "-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384"

data class RustAbi(
    val androidAbi: String,
    val rustTarget: String,
    val linkerEnvironment: String,
    val linkerBinary: String,
)

val rustAbis = listOf(
    RustAbi(
        androidAbi = "arm64-v8a",
        rustTarget = "aarch64-linux-android",
        linkerEnvironment = "CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER",
        linkerBinary = "aarch64-linux-android26-clang",
    ),
    RustAbi(
        androidAbi = "x86_64",
        rustTarget = "x86_64-linux-android",
        linkerEnvironment = "CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER",
        linkerBinary = "x86_64-linux-android26-clang",
    ),
)

val rustTasks = rustAbis.map { abi ->
    tasks.register<Exec>("buildRust${abi.androidAbi.replace("-", "").replaceFirstChar { it.uppercase() }}") {
        workingDir = rustFfiDirectory
        inputs.files(rustBuildInputs).withPathSensitivity(PathSensitivity.RELATIVE)
        inputs.property("androidNdkVersion", androidNdkVersion)
        inputs.property("ndkDirectory", ndkDirectory.map { it.asFile.absolutePath })
        inputs.property("ndkHostTag", ndkHostTag)
        inputs.property("linkerBinary", abi.linkerBinary)
        inputs.property("linkerExecutableSuffix", ndkLinkerExecutableSuffix)
        inputs.property("rustFlags", rustFlags)
        outputs.file(rustTargetDirectory.map { it.file("${abi.rustTarget}/release/libapppilotkit_transport_ffi.so") })
        environment("CARGO_TARGET_DIR", rustTargetDirectory.get().asFile.absolutePath)
        val ndkHostToolchainDirectory = ndkDirectory.map { ndkRoot ->
            val ndkToolchainDirectory = ndkRoot.asFile.resolve("toolchains/llvm/prebuilt")
            ndkToolchainDirectory.resolve(ndkHostTag)
                .takeIf { it.isDirectory }
                ?: ndkToolchainDirectory.listFiles()?.singleOrNull { it.isDirectory }
                ?: error("No Android NDK host toolchain was found in $ndkToolchainDirectory")
        }
        environment(
            abi.linkerEnvironment,
            ndkHostToolchainDirectory.map {
                it.resolve("bin/${abi.linkerBinary}$ndkLinkerExecutableSuffix").absolutePath
            },
        )
        environment("RUSTFLAGS", rustFlags)
        commandLine("cargo", "build", "--locked", "--release", "--target", abi.rustTarget)
    }
}

val prepareRustJni by tasks.registering(Sync::class) {
    dependsOn(rustTasks)
    rustAbis.forEach { abi ->
        from(rustTargetDirectory.map { it.dir("${abi.rustTarget}/release") }) {
            include("libapppilotkit_transport_ffi.so")
            rename("libapppilotkit_transport_ffi.so", "libapppilotkit_transport.so")
            into(abi.androidAbi)
        }
    }
    into(generatedJniDirectory)
}

android {
    namespace = "dev.apppilotkit.targettransport.internal"
    compileSdk = 36
    ndkVersion = androidNdkVersion

    defaultConfig {
        minSdk = 26
        ndk {
            abiFilters += setOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
        }
        release {
            isMinifyEnabled = false
        }
    }

    sourceSets {
        getByName("debug").jniLibs.srcDir(generatedJniDirectory.get().asFile)
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    implementation(project(":protocol-runtime"))
    testImplementation("junit:junit:4.13.2")
}

tasks.matching { task ->
    task.name.startsWith("merge") && task.name.contains("Debug") &&
        (task.name.endsWith("NativeLibs") || task.name.endsWith("JniLibFolders"))
}.configureEach {
    dependsOn(prepareRustJni)
}
