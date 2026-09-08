#if !APPPILOTKIT_INTERNAL
#error("AcceptanceHost is Debug/Internal-only and has no Release target.")
#endif

import AppPilotKit
@_spi(AppPilotKitTargetTransportInternal) import AppPilotKitTargetTransportInternal
import Foundation

enum AcceptanceHostIdentity {
  static let bundleIdentifier = "dev.apppilotkit.acceptancehost.ios"
  static let foundationScenario = "demo.foundation"
  static let foundationSeed = "foundation-v1"
  static let foundationResource = "acceptance.foundation.state"
}

private struct FoundationScenarioValue: Sendable {
  let scenario = AcceptanceHostIdentity.foundationScenario
  let seed = AcceptanceHostIdentity.foundationSeed
  let platform = "ios"
}

func makeFoundationComposition(processGeneration: UInt64) throws -> TargetRuntimeComposition {
  let valueSchema = try SemanticSchema(
    id: "schema_demo_foundation_state_v1",
    revision: 1,
    document: .object([
      "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
      "$id": .string("app://acceptance.foundation.state/value@1"),
      "type": .string("object"),
      "required": .array([
        .string("scenario"),
        .string("seed"),
        .string("platform"),
      ]),
      "properties": .object([
        "scenario": .object([
          "type": .string("string"),
          "const": .string(AcceptanceHostIdentity.foundationScenario),
        ]),
        "seed": .object([
          "type": .string("string"),
          "const": .string(AcceptanceHostIdentity.foundationSeed),
        ]),
        "platform": .object([
          "type": .string("string"),
        ]),
      ]),
      "additionalProperties": .bool(false),
    ])
  )
  let output = SemanticOutputCodec<FoundationScenarioValue>(schema: valueSchema) { state in
    .object([
      "scenario": .publicValue(.string(state.scenario)),
      "seed": .publicValue(.string(state.seed)),
      "platform": .publicValue(.string(state.platform)),
    ])
  }
  let catalogBuilder = SemanticCatalogBuilder()
  try catalogBuilder.registerResource(
    id: AcceptanceHostIdentity.foundationResource,
    declarationRevision: 1,
    output: output,
    handler: { FoundationScenarioValue() }
  )
  let catalog = try catalogBuilder.freeze(
    identity: SemanticCatalogIdentity(
      id: "catalog_acceptance_foundation",
      generation: processGeneration
    )
  )
  let actionCoordinator = TargetActionCoordinator(
    catalog: catalog,
    targetID: "target_acceptance_foundation",
    evidence: FoundationAcceptanceEvidence(),
    policy: TargetActionPolicy(
      resolve: { _, _ in nil },
      validateDestructive: { _ in false },
      consumeDestructive: { _ in false }
    )
  )
  return try TargetRuntimeComposition(
    catalog: catalog,
    limits: SemanticProtocolLimits(
      maximumRequestBytes: 4 * 1024,
      maximumResponseBytes: 16 * 1024,
      maximumPageItems: 16
    ),
    policy: SemanticProtocolPolicy(
      discover: { _, declaration in declaration.id == AcceptanceHostIdentity.foundationResource },
      discloseSchema: { _, declaration in declaration.id == AcceptanceHostIdentity.foundationResource },
      discloseResource: { _, declaration in declaration.id == AcceptanceHostIdentity.foundationResource },
      discloseAction: { _, _ in false }
    ),
    actionCoordinator: actionCoordinator,
    processGeneration: processGeneration
  )
}

// MARK: - Demo catalog composition

private enum DemoCatalogMode: Sendable {
  case safe
  case unclassified
  case sensitive
  case oversized
}

private func demoCatalogSecretCanary() -> String {
  ["APPPILOTKIT_DEMO_CATALOG_", "SECRET_CANARY_7f9c4b2e"].joined()
}

private struct DemoCatalogStateValue: Sendable {
  let count: Int64
  let mode: DemoCatalogMode

  /// `available` is deliberately a value in the public state, while the
  /// Resource itself remains queryable so the fail-closed fixtures can be
  /// observed at counts 3–5.
  var available: Bool { count.isMultiple(of: 2) }
}

private actor DemoCatalogState {
  private var count: Int64 = 0

  func snapshot() -> DemoCatalogStateValue {
    let mode: DemoCatalogMode
    switch count {
    case 3: mode = .unclassified
    case 4...: mode = .oversized
    default: mode = .safe
    }
    return DemoCatalogStateValue(count: count, mode: mode)
  }

  func increment() { count += 1 }
  func reset() { count = 0 }
  func isAvailable() -> Bool { count != 1 }
}

private struct DemoCatalogIncrementInput: Sendable {
  let amount: Int64
}

private struct DemoCatalogResetInput: Sendable {
  let confirm: String
}

private struct DemoCatalogEvidenceState: Sendable {
  let before: Int
  let stable: Int
  let after: Int
}

private actor DemoCatalogEvidenceRecorder {
  private var value = DemoCatalogEvidenceState(before: 0, stable: 0, after: 0)

  func recordBefore() { value = DemoCatalogEvidenceState(before: value.before + 1, stable: value.stable, after: value.after) }
  func recordStability() { value = DemoCatalogEvidenceState(before: value.before, stable: value.stable + 1, after: value.after) }
  func recordAfter() { value = DemoCatalogEvidenceState(before: value.before, stable: value.stable, after: value.after + 1) }
}

private struct DemoCatalogEvidence: ActionEvidencePort {
  let recorder: DemoCatalogEvidenceRecorder

  func captureBefore(context: TargetActionContext) async throws {
    await recorder.recordBefore()
  }

  func observeStability(context: TargetActionContext) async throws {
    await recorder.recordStability()
  }

  func captureAfter(context: TargetActionContext) async throws {
    await recorder.recordAfter()
  }
}

private enum DemoCatalogInputError: Error {
  case invalid
}

func makeDemoCatalogComposition(processGeneration: UInt64) throws -> TargetRuntimeComposition {
  let state = DemoCatalogState()
  let stateSchema = try SemanticSchema(
    id: "schema_demo_catalog_state_v1",
    revision: 1,
    document: .object([
      "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
      "$id": .string("app://acceptance.catalog.state/value@1"),
      "type": .string("object"),
      "required": .array([
        .string("scenario"), .string("seed"), .string("platform"),
        .string("count"), .string("available"),
      ]),
      "properties": .object([
        "scenario": .object(["type": .string("string"), "const": .string("demo.catalog")]),
        "seed": .object(["type": .string("string"), "const": .string("catalog-v1")]),
        "platform": .object(["type": .string("string")]),
        "count": .object(["type": .string("integer"), "minimum": .integer(0)]),
        "available": .object(["type": .string("boolean")]),
      ]),
      "additionalProperties": .bool(false),
    ])
  )
  let incrementSchema = try SemanticSchema(
    id: "schema_demo_catalog_increment_input_v1",
    revision: 1,
    document: .object([
      "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
      "$id": .string("app://acceptance.catalog.increment/input@1"),
      "type": .string("object"),
      "required": .array([.string("amount")]),
      "properties": .object([
        "amount": .object(["type": .string("integer"), "const": .integer(1)]),
      ]),
      "additionalProperties": .bool(false),
    ])
  )
  let resetSchema = try SemanticSchema(
    id: "schema_demo_catalog_reset_input_v1",
    revision: 1,
    document: .object([
      "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
      "$id": .string("app://acceptance.catalog.reset/input@1"),
      "type": .string("object"),
      "required": .array([.string("confirm")]),
      "properties": .object([
        "confirm": .object(["type": .string("string"), "const": .string("reset-catalog-v1")]),
      ]),
      "additionalProperties": .bool(false),
    ])
  )

  let output = SemanticOutputCodec<DemoCatalogStateValue>(schema: stateSchema) { value in
    let fields: [String: SemanticDisclosureValue] = [
      "scenario": .publicValue(.string("demo.catalog")),
      "seed": .publicValue(.string("catalog-v1")),
      "platform": .publicValue(.string("ios")),
      "count": .publicValue(.integer(value.count)),
      "available": .publicValue(.bool(value.available)),
    ]
    switch value.mode {
    case .safe:
      return .object(fields)
    case .unclassified:
      var unsafe = fields
      unsafe["secret"] = .unclassified(
        .string(demoCatalogSecretCanary())
      )
      return .object(unsafe)
    case .sensitive:
      var unsafe = fields
      unsafe["secret"] = .sensitive(
        .string(demoCatalogSecretCanary())
      )
      return .object(unsafe)
    case .oversized:
      var oversized = fields
      oversized["platform"] = .publicValue(.string(String(repeating: "x", count: 5_000)))
      return .object(oversized)
    }
  }

  let incrementInput = SemanticInputCodec(schema: incrementSchema) { raw in
    guard case .object(let object) = raw,
      case .integer(let amount)? = object["amount"], amount == 1
    else { throw DemoCatalogInputError.invalid }
    return DemoCatalogIncrementInput(amount: amount)
  }
  let resetInput = SemanticInputCodec(schema: resetSchema) { raw in
    guard case .object(let object) = raw,
      case .string(let confirm)? = object["confirm"], confirm == "reset-catalog-v1"
    else { throw DemoCatalogInputError.invalid }
    return DemoCatalogResetInput(confirm: confirm)
  }

  let builder = SemanticCatalogBuilder()
  try builder.registerResource(
    id: "acceptance.catalog.state",
    declarationRevision: 1,
    output: output,
    availability: { await state.isAvailable() },
    handler: { await state.snapshot() }
  )
  try builder.registerAction(
    id: "acceptance.catalog.increment",
    declarationRevision: 1,
    input: incrementInput,
    policy: SemanticActionPolicy(authorization: .none, retrySafety: .noAutomaticRetry),
    availability: { true },
    handler: { _ in await state.increment() }
  )
  try builder.registerAction(
    id: "acceptance.catalog.reset",
    declarationRevision: 1,
    input: resetInput,
    policy: SemanticActionPolicy(
      authorization: .destructiveAuthorization,
      retrySafety: .retryWithProofOnly
    ),
    availability: { true },
    handler: { _ in await state.reset() }
  )
  let catalog = try builder.freeze(
    identity: SemanticCatalogIdentity(id: "catalog_demo_catalog", generation: processGeneration)
  )
  let evidence = DemoCatalogEvidence(recorder: DemoCatalogEvidenceRecorder())
  let actionCoordinator = TargetActionCoordinator(
    catalog: catalog,
    targetID: "target_acceptance_catalog",
    evidence: evidence,
    policy: TargetActionPolicy(
      resolve: { _, subject in
        SemanticActionPolicy(
          authorization: subject.declaredAuthorization,
          retrySafety: subject.retrySafety
        )
      },
      validateDestructive: { _ in false },
      consumeDestructive: { _ in false }
    )
  )
  return try TargetRuntimeComposition(
    catalog: catalog,
    limits: SemanticProtocolLimits(
      maximumRequestBytes: 4 * 1024,
      maximumResponseBytes: 4 * 1024,
      maximumPageItems: 16
    ),
    policy: SemanticProtocolPolicy(
      discover: { _, _ in true },
      discloseSchema: { _, _ in true },
      discloseResource: { _, _ in true },
      discloseAction: { _, _ in true }
    ),
    actionCoordinator: actionCoordinator,
    processGeneration: processGeneration
  )
}

// Stable aliases used by native acceptance tests and older harness revisions.
func makeCatalogComposition(processGeneration: UInt64) throws -> TargetRuntimeComposition {
  try makeDemoCatalogComposition(processGeneration: processGeneration)
}

private struct FoundationAcceptanceEvidence: ActionEvidencePort {
  func captureBefore(context: TargetActionContext) async throws {}
  func observeStability(context: TargetActionContext) async throws {}
  func captureAfter(context: TargetActionContext) async throws {}
}

enum AcceptanceHostTerminationAction: Equatable {
  case none
  case awaitStartingTransport
  case stopActiveTransport
}

final class AcceptanceHostTransportLifecycle {
  private enum State: Equatable {
    case idle
    case starting
    case active
    case stopping
  }

  private var state = State.idle
  private var deferredStart = false

  func requestStart() -> Bool {
    switch state {
    case .idle:
      state = .starting
      return true
    case .stopping:
      deferredStart = true
      return false
    case .starting, .active:
      return false
    }
  }

  func didStart() -> Bool {
    guard state == .starting else { return false }
    state = .active
    return true
  }

  func startFailed() -> Bool {
    switch state {
    case .starting:
      state = .idle
      return false
    case .stopping:
      state = .idle
      let restart = deferredStart
      deferredStart = false
      return restart
    case .idle, .active:
      return false
    }
  }

  func beginTermination() -> AcceptanceHostTerminationAction {
    switch state {
    case .active:
      state = .stopping
      return .stopActiveTransport
    case .starting:
      state = .stopping
      return .awaitStartingTransport
    case .idle, .stopping:
      return .none
    }
  }

  func didStop() -> Bool {
    guard state == .stopping else { return false }
    state = .idle
    let restart = deferredStart
    deferredStart = false
    return restart
  }
}

#if canImport(UIKit)
import UIKit

@main
@MainActor
final class AcceptanceHostAppDelegate: UIResponder, UIApplicationDelegate {
  private var transport: AppPilotKitTargetTransport?
  private let lifecycle = AcceptanceHostTransportLifecycle()
  var window: UIWindow?

  func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    let window = UIWindow(frame: UIScreen.main.bounds)
    window.rootViewController = UIViewController()
    window.makeKeyAndVisible()
    self.window = window
    return true
  }

  func applicationDidBecomeActive(_ application: UIApplication) {
    startTransportIfRequested()
  }

  func applicationWillTerminate(_ application: UIApplication) {
    switch lifecycle.beginTermination() {
    case .none, .awaitStartingTransport:
      break
    case .stopActiveTransport:
      let activeTransport = transport
      transport = nil
      Task { [weak self] in
        await activeTransport?.stop()
        self?.restartAfterStoppingIfRequested()
      }
    }
  }

  private func startTransportIfRequested() {
    guard lifecycle.requestStart() else { return }
    Task { [weak self] in
      do {
        let started = try await AppPilotKitTargetTransport.startFromEnvironment(
          compositionFactory: makeDemoCatalogComposition
        )
        guard let self, self.lifecycle.didStart() else {
          await started.stop()
          self?.restartAfterStoppingIfRequested()
          return
        }
        self.transport = started
      } catch {
        // A normal launch has no Broker descriptor and intentionally exposes no listener.
        self?.restartAfterStartingFailureIfRequested()
      }
    }
  }

  private func restartAfterStoppingIfRequested() {
    if lifecycle.didStop() {
      startTransportIfRequested()
    }
  }

  private func restartAfterStartingFailureIfRequested() {
    if lifecycle.startFailed() {
      startTransportIfRequested()
    }
  }
}
#else
@main
struct UnsupportedAcceptanceHostPlatform {
  static func main() {}
}
#endif
