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
          compositionFactory: makeFoundationComposition
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
