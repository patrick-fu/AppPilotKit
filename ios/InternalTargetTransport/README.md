# AppPilotKit Target transport (relocated)

The internal Target transport is now a non-public target in
[`../TransportSmokeHost`](../TransportSmokeHost). It has no standalone SwiftPM
package or library product, so a Release/Production consumer cannot declare a
dependency edge to it. The Debug/Internal Smoke Host and its tests remain the
only in-package consumers.
