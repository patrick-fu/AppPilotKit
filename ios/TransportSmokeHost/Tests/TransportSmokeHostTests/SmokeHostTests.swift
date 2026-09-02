#if APPPILOTKIT_INTERNAL && canImport(UIKit)
@_spi(AppPilotKitTargetTransportInternal) import AppPilotKitTargetTransportInternal
@testable import TransportSmokeHost
import XCTest

final class SmokeHostTests: XCTestCase {
  func testCatalogContainsOnlyTheFixedReadOnlySmokeResource() throws {
    let composition = try makeSmokeComposition(generation: 7)
    let declaration = try composition.catalog.declaration(for: "smoke.ready")

    XCTAssertEqual(declaration.id, "smoke.ready")
    XCTAssertEqual(declaration.kind, .resource)
    XCTAssertEqual(declaration.declarationRevision, 1)
    XCTAssertEqual(declaration.valueSchema?.id, "schema_smoke_ready_v1")
    XCTAssertThrowsError(try composition.catalog.declaration(for: "smoke.reset"))
  }
}
#endif
