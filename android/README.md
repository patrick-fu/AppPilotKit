# Android

This directory will contain the AppPilotKit Android debug SDK, transport adapters, tests, and sample app.

The first vertical slice should prove connection, screenshot capture, compact Android View hierarchy inspection, element lookup, and tap on both Emulator and a physical device. Compose semantics support follows as an explicit provider rather than a lossy conversion.

## Semantic registry

`semantic-registry` is the framework-free Kotlin core for App-owned Semantic Resource and
Semantic Action registration. Apps construct it in a Debug/Internal composition root and
freeze it once per process generation. A later runtime module can consume its immutable
declarations, schemas, resource-query boundary, and internal prepared-action seam without
introducing View, Compose, ADB, or transport dependencies.

Run its JVM unit tests with one worker:

```shell
JAVA_HOME=$(/usr/libexec/java_home -v 17) ./gradlew --no-daemon --max-workers=1 :semantic-registry:test
```
