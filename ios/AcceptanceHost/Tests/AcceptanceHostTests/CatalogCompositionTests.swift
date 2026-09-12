@testable import AcceptanceHost
@testable import AppPilotKit
@_spi(AppPilotKitTargetTransportInternal) import AppPilotKitTargetTransportInternal
import XCTest

final class CatalogCompositionTests: XCTestCase {
  private let canary = "APPPILOTKIT_DEMO_CATALOG_SECRET_CANARY_7f9c4b2e"

  func testCatalogMembershipSchemasAndDigestAreFrozen() throws {
    let composition = try makeDemoCatalogComposition(processGeneration: 41)

    XCTAssertEqual(
      composition.catalog.items,
      [
        SemanticCapabilityItem(id: "acceptance.catalog.increment", kind: .action, declarationRevision: 1),
        SemanticCapabilityItem(id: "acceptance.catalog.reset", kind: .action, declarationRevision: 1),
        SemanticCapabilityItem(id: "acceptance.catalog.state", kind: .resource, declarationRevision: 1),
      ]
    )
    XCTAssertEqual(composition.catalog.identity.id, "catalog_demo_catalog")
    XCTAssertEqual(composition.catalog.identity.generation, 41)

    let state = try composition.catalog.declaration(for: "acceptance.catalog.state")
    XCTAssertEqual(state.valueSchema?.id, "schema_demo_catalog_state_v1")
    XCTAssertEqual(
      state.valueSchema?.digest,
      "sha256:b3ffcc96d1da03c447d243ba196f9af01a38447bbb03d5139aed9923c7300804"
    )
    let increment = try composition.catalog.declaration(for: "acceptance.catalog.increment")
    XCTAssertEqual(increment.inputSchema?.id, "schema_demo_catalog_increment_input_v1")
    XCTAssertEqual(
      increment.inputSchema?.digest,
      "sha256:bd75453e5e97b497e4ee54db7cd82f4e654d04da370d8106e645e0bb9769c2fe"
    )
    let reset = try composition.catalog.declaration(for: "acceptance.catalog.reset")
    XCTAssertEqual(reset.inputSchema?.id, "schema_demo_catalog_reset_input_v1")
    XCTAssertEqual(
      reset.inputSchema?.digest,
      "sha256:079b413f2ca72310290f3b07da82503b0dd74c408358570ead26ffeea94efd7c"
    )

    XCTAssertEqual(composition.catalog.items.count, 3)
  }

  func testColdProcessStateAndCatalogGenerationRestore() async throws {
    let first = try makeDemoCatalogComposition(processGeneration: 41)
    let restarted = try makeDemoCatalogComposition(processGeneration: 42)
    XCTAssertEqual(first.catalog.items, restarted.catalog.items)
    XCTAssertEqual(first.catalog.identity.generation, 41)
    XCTAssertEqual(restarted.catalog.identity.generation, 42)

    let declaration = try first.catalog.declaration(for: "acceptance.catalog.state")
    let value = try await first.catalog.queryResource(
      SemanticResourceQuery(
        capability: declaration.id,
        declarationRevision: declaration.declarationRevision,
        valueSchema: try XCTUnwrap(declaration.valueSchema)
      ),
      maximumOutputBytes: 4 * 1024
    )
    XCTAssertEqual(
      value.value,
      .object([
        "scenario": .string("demo.catalog"),
        "seed": .string("catalog-v1"),
        "platform": .string("ios"),
        "count": .integer(0),
        "available": .bool(true),
      ])
    )
  }

  func testCodecFailuresAreFailClosedAndSanitized() async throws {
    let composition = try makeDemoCatalogComposition(processGeneration: 11)
    let increment = try XCTUnwrap(
      composition.catalog.declaration(for: "acceptance.catalog.increment").inputSchema
    )
    let session = SemanticProtocolSessionContext(id: "session_catalog_test", generation: 11)
    let request = SemanticActionInvocation(
      capability: "acceptance.catalog.increment",
      declarationRevision: 1,
      inputSchema: increment,
      input: .object(["amount": .integer(1)])
    )

    for _ in 0..<3 {
      try await composition.actionCoordinator.invoke(
        request,
        authorizationGrant: nil,
        session: session,
        sessionIsActive: { true }
      )
    }
    do {
      _ = try await queryState(composition, maximumOutputBytes: 4 * 1024)
      XCTFail("Expected unclassified disclosure to fail")
    } catch {
      XCTAssertEqual(error as? SemanticCatalogError, .disclosureDenied)
      XCTAssertFalse(String(describing: error).contains(canary))
    }

    try await composition.actionCoordinator.invoke(
      request,
      authorizationGrant: nil,
      session: session,
      sessionIsActive: { true }
    )
    try await composition.actionCoordinator.invoke(
      request,
      authorizationGrant: nil,
      session: session,
      sessionIsActive: { true }
    )
    do {
      _ = try await queryState(composition, maximumOutputBytes: 4 * 1024)
      XCTFail("Expected oversized disclosure to fail")
    } catch {
      XCTAssertEqual(error as? SemanticCatalogError, .resourceExhausted)
      XCTAssertFalse(String(describing: error).contains(canary))
    }
  }

  func testDestructiveResetRequiresGrantAndCurrentPolicyRejectsIt() async throws {
    let composition = try makeDemoCatalogComposition(processGeneration: 17)
    let inputSchema = try XCTUnwrap(
      composition.catalog.declaration(for: "acceptance.catalog.reset").inputSchema
    )
    let request = SemanticActionInvocation(
      capability: "acceptance.catalog.reset",
      declarationRevision: 1,
      inputSchema: inputSchema,
      input: .object(["confirm": .string("reset-catalog-v1")])
    )
    let session = SemanticProtocolSessionContext(id: "session_catalog_reset", generation: 17)

    for grant in [String?(nil), "grant-catalog-test"] {
      do {
        try await composition.actionCoordinator.invoke(
          request,
          authorizationGrant: grant,
          session: session,
          sessionIsActive: { true }
        )
        XCTFail("Expected destructive policy denial")
      } catch {
        XCTAssertEqual(error as? TargetActionCoordinatorError, .policyDenied)
      }
    }
  }

  private func queryState(
    _ composition: TargetRuntimeComposition,
    maximumOutputBytes: Int
  ) async throws -> DetachedSemanticValue {
    let declaration = try composition.catalog.declaration(for: "acceptance.catalog.state")
    return try await composition.catalog.queryResource(
      SemanticResourceQuery(
        capability: declaration.id,
        declarationRevision: declaration.declarationRevision,
        valueSchema: try XCTUnwrap(declaration.valueSchema)
      ),
      maximumOutputBytes: maximumOutputBytes
    )
  }
}
