# Android transport smoke host

This Debug/Internal-only application exists only for the #62 Android transport journey.
Its Debug variant exports `dev.apppilotkit.smokehost.AppPilotKitBootstrapActivity`, accepts
only `dev.apppilotkit.transport.DESCRIPTOR`, and publishes the single read-only
`smoke.ready` Resource. The Release variant has no internal transport dependency, bootstrap
Activity, JNI library, or Noise transport marker.

Run the local checks with JDK 17:

```shell
JAVA_HOME=$(/usr/libexec/java_home -v 17) ./gradlew --no-daemon --max-workers=1 \
  :transport-smoke-host:assembleDebug \
  :transport-smoke-host:verifyReleaseExcludesInternalTransport
```
