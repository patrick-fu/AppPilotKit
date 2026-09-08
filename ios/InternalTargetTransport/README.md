# AppPilotKit internal Target transport

This is the non-production SwiftPM package for AppPilotKit's private Target
transport, its narrow Rust FFI boundary, and the transport test broker. It is
not a product or dependency of `ios/Package.swift` and compiles only with the
Debug/Internal build setting. Release compilation fails before it can produce
an executable consumer.

Only Debug/Internal evidence applications such as `TransportSmokeHost` and
`AcceptanceHost` explicitly depend on this package through its SPI import.

Run its native transport tests (with the accepted Rust FFI) using:

```text
Scripts/run-with-rust-ffi.sh test
```
