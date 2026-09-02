pluginManagement {
    repositories {
        google()
        gradlePluginPortal()
        mavenCentral()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "apppilotkit-android"
include(":semantic-registry")
include(":protocol-runtime")
include(":target-transport-internal")
include(":transport-smoke-host")
