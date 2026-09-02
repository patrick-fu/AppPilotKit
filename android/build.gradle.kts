buildscript {
    repositories {
        mavenCentral()
    }
    dependencies {
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:2.4.10")
    }
}

plugins {
    id("com.android.application") version "9.1.0" apply false
    id("com.android.library") version "9.1.0" apply false
    kotlin("android") version "2.4.10" apply false
    kotlin("jvm") version "2.4.10" apply false
}
