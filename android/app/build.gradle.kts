import java.io.File
import java.util.Properties
import javax.inject.Inject

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "dev.skywave.android"
    compileSdk = 37

    defaultConfig {
        applicationId = "dev.skywave.android"
        minSdk = 26
        targetSdk = 37
        versionCode = 1
        versionName = "0.1.0"

        // D19: devices and the emulator only. Also drops the 32-bit copies of
        // JNA's libjnidispatch.so from the APK.
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    buildFeatures {
        compose = true
    }

    testOptions {
        // Robolectric renders Compose in JVM unit tests and needs the merged resources.
        unitTests.isIncludeAndroidResources = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.process)
    implementation(libs.kotlinx.coroutines.android)
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.ui.tooling.preview)
    implementation(libs.compose.material3)
    implementation(libs.compose.material.icons.core)
    debugImplementation(libs.compose.ui.tooling)
    debugImplementation(libs.compose.ui.test.manifest)
    implementation(libs.jna) { artifact { type = "aar" } }

    testImplementation(libs.junit)
    testImplementation(libs.kotlinx.coroutines.test)
    testImplementation(platform(libs.compose.bom))
    testImplementation(libs.compose.ui.test.junit4)
    testImplementation(libs.robolectric)
}

// --- Rust core -------------------------------------------------------------
//
// Two tasks per variant, both thin wrappers over the crate's own scripts:
//   cargoBuildAndroid<Variant>       build-android.sh -> jniLibs (arm64-v8a, x86_64)
//   generateUniffiBindings<Variant>  gen-kotlin.sh    -> Kotlin sources (dev.skywave.core)
// They are wired in through the Variant API, so AGP knows the directories they
// produce and runs them before merging native libs / compiling Kotlin.
//
// Up-to-date checks: the inputs are every .rs / Cargo.toml / .toml under
// crates/ plus the workspace Cargo.toml and Cargo.lock, so an unchanged tree
// skips cargo entirely; on any change cargo itself rebuilds incrementally.

val repoRoot: Directory = rootProject.layout.projectDirectory.dir("..")
val ffiCrate: Directory = repoRoot.dir("crates/skywave-core-ffi")

// Optional `skywave.cargo=/path/to/cargo` in local.properties; otherwise
// `cargo` is taken from PATH.
val cargoOverride: Provider<String> =
    providers.fileContents(rootProject.layout.projectDirectory.file("local.properties"))
        .asText
        .map { text -> Properties().apply { load(text.reader()) }.getProperty("skywave.cargo") ?: "" }
        .orElse("")

val rustInputs: FileTree =
    fileTree(repoRoot.dir("crates")) {
        include("**/*.rs", "**/Cargo.toml", "**/*.toml", "**/*.sh")
        exclude("**/target/**")
    } + fileTree(repoRoot) { include("Cargo.toml", "Cargo.lock") }

/** Runs one of the crate's shell scripts as `sh <script> <outputDir> <profile>`. */
abstract class RustScriptTask @Inject constructor(
    private val execOps: ExecOperations,
) : DefaultTask() {
    @get:InputFile
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val script: RegularFileProperty

    @get:Input
    abstract val profile: Property<String>

    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val rustSources: ConfigurableFileCollection

    /** Path to a cargo binary, or empty to use `cargo` from PATH. */
    @get:Input
    abstract val cargo: Property<String>

    /** The NDK cargo-ndk picks up; tracked so switching NDKs rebuilds. */
    @get:Input
    @get:Optional
    abstract val ndkHome: Property<String>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @TaskAction
    fun run() {
        val out = outputDir.get().asFile
        out.deleteRecursively()
        out.mkdirs()
        execOps.exec {
            commandLine("sh", script.get().asFile.absolutePath, out.absolutePath, profile.get())
            val cargoPath = cargo.get()
            if (cargoPath.isNotEmpty()) {
                val dir = File(cargoPath).absoluteFile.parent
                environment("PATH", dir + File.pathSeparator + System.getenv("PATH").orEmpty())
            }
        }
    }
}

androidComponents {
    onVariants { variant ->
        val suffix = variant.name.replaceFirstChar { it.uppercase() }
        // Debug builds use cargo's `dev` profile for speed; everything else the
        // size-optimised `release-android` profile from the workspace Cargo.toml.
        val nativeProfile = if (variant.buildType == "debug") "dev" else "release-android"

        val cargoBuild = tasks.register<RustScriptTask>("cargoBuildAndroid$suffix") {
            group = "rust"
            description = "Cross-compiles skywave-core-ffi ($nativeProfile) for the ${variant.name} variant."
            script.set(ffiCrate.file("build-android.sh"))
            profile.set(nativeProfile)
            rustSources.from(rustInputs)
            cargo.set(cargoOverride)
            ndkHome.set(providers.environmentVariable("ANDROID_NDK_HOME"))
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(cargoBuild, RustScriptTask::outputDir)

        // The bindings do not depend on the profile; the host `dev` build is
        // the cheapest one for the bindgen to read.
        val bindings = tasks.register<RustScriptTask>("generateUniffiBindings$suffix") {
            group = "rust"
            description = "Generates the UniFFI Kotlin bindings for the ${variant.name} variant."
            script.set(ffiCrate.file("gen-kotlin.sh"))
            profile.set("dev")
            rustSources.from(rustInputs)
            cargo.set(cargoOverride)
        }
        variant.sources.kotlin?.addGeneratedSourceDirectory(bindings, RustScriptTask::outputDir)
    }
}
