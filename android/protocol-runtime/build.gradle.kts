plugins {
    kotlin("jvm")
}

dependencies {
    api(project(":semantic-registry"))
    api("org.jetbrains.kotlinx:kotlinx-serialization-json:1.11.0")
    testImplementation(kotlin("test"))
}

kotlin {
    jvmToolchain(17)
}

tasks.test {
    useJUnitPlatform()
}
