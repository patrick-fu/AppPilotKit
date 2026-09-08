@testable import AcceptanceHost
import AppPilotKit
@_spi(AppPilotKitTargetTransportInternal) import AppPilotKitTargetTransportInternal
import XCTest

final class FoundationCompositionTests: XCTestCase {
  func testTerminationDefersActivationUntilThePreviousTransportHasStopped() {
    let lifecycle = AcceptanceHostTransportLifecycle()

    XCTAssertTrue(lifecycle.requestStart())
    XCTAssertTrue(lifecycle.didStart())
    XCTAssertEqual(lifecycle.beginTermination(), .stopActiveTransport)
    XCTAssertFalse(lifecycle.requestStart())
    XCTAssertTrue(lifecycle.didStop())
    XCTAssertTrue(lifecycle.requestStart())
  }

  func testTerminationDuringStartDefersUntilTheRejectedTransportStops() {
    let lifecycle = AcceptanceHostTransportLifecycle()

    XCTAssertTrue(lifecycle.requestStart())
    XCTAssertEqual(lifecycle.beginTermination(), .awaitStartingTransport)
    XCTAssertFalse(lifecycle.requestStart())
    XCTAssertFalse(lifecycle.didStart())
    XCTAssertTrue(lifecycle.didStop())
    XCTAssertTrue(lifecycle.requestStart())
  }

  func testFailedNormalActivationCanConsumeALaterDescriptor() {
    let lifecycle = AcceptanceHostTransportLifecycle()

    XCTAssertTrue(lifecycle.requestStart())
    XCTAssertFalse(lifecycle.startFailed())
    XCTAssertTrue(lifecycle.requestStart())
  }

  func testColdProcessCompositionExposesTheFixedFoundationSeed() async throws {
    let composition = try makeFoundationComposition(processGeneration: 41)
    let declaration = try composition.catalog.declaration(for: "acceptance.foundation.state")

    XCTAssertEqual(AcceptanceHostIdentity.bundleIdentifier, "dev.apppilotkit.acceptancehost.ios")
    XCTAssertEqual(composition.catalog.identity.generation, 41)
    let items = composition.catalog.items
    XCTAssertEqual(items.count, 1)
    XCTAssertEqual(items.first?.id, "acceptance.foundation.state")
    XCTAssertEqual(items.first?.kind, .resource)
    XCTAssertEqual(items.first?.declarationRevision, 1)
    XCTAssertEqual(declaration.kind, .resource)
    XCTAssertEqual(declaration.declarationRevision, 1)
    XCTAssertNil(declaration.inputSchema)
    XCTAssertEqual(declaration.valueSchema?.id, "schema_demo_foundation_state_v1")
    XCTAssertEqual(
      declaration.valueSchema?.digest,
      "sha256:b63382887a98877b06466df4d6aa2a2fd788e89c5c1db39170720fd6c721a08c"
    )

    let schema = try composition.catalog.schema(
      capabilityID: declaration.id,
      declarationRevision: declaration.declarationRevision,
      handle: try XCTUnwrap(declaration.valueSchema)
    )
    XCTAssertEqual(
      schema.document,
      .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://acceptance.foundation.state/value@1"),
        "type": .string("object"),
        "required": .array([.string("scenario"), .string("seed"), .string("platform")]),
        "properties": .object([
          "scenario": .object([
            "type": .string("string"),
            "const": .string("demo.foundation"),
          ]),
          "seed": .object([
            "type": .string("string"),
            "const": .string("foundation-v1"),
          ]),
          "platform": .object(["type": .string("string")]),
        ]),
        "additionalProperties": .bool(false),
      ])
    )

    let value = try await composition.catalog.queryResource(
      SemanticResourceQuery(
        capability: declaration.id,
        declarationRevision: declaration.declarationRevision,
        valueSchema: try XCTUnwrap(declaration.valueSchema)
      ),
      maximumOutputBytes: 1_024
    )

    XCTAssertEqual(
      value.value,
      .object([
        "scenario": .string("demo.foundation"),
        "seed": .string("foundation-v1"),
        "platform": .string("ios"),
      ])
    )
    XCTAssertThrowsError(try composition.catalog.declaration(for: "smoke.ready"))
  }

  func testColdProcessCompositionsRestoreTheSameSeedWithNewGenerations() throws {
    let first = try makeFoundationComposition(processGeneration: 41)
    let restarted = try makeFoundationComposition(processGeneration: 42)

    XCTAssertEqual(first.catalog.identity.generation, 41)
    XCTAssertEqual(restarted.catalog.identity.generation, 42)
    XCTAssertEqual(first.catalog.items, restarted.catalog.items)
  }
}
