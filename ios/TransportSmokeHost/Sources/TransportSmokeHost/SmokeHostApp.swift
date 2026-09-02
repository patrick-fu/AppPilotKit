#if APPPILOTKIT_INTERNAL && canImport(UIKit)
import AppPilotKit
@_spi(AppPilotKitTargetTransportInternal) import AppPilotKitTargetTransportInternal
import UIKit

@main
@MainActor
final class SmokeHostAppDelegate: UIResponder, UIApplicationDelegate {
  private var transport: AppPilotKitTargetTransport?
  var window: UIWindow?

  func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    let window = UIWindow(frame: UIScreen.main.bounds)
    window.rootViewController = UIViewController()
    window.makeKeyAndVisible()
    self.window = window

    Task { [weak self] in
      do {
        self?.transport = try await AppPilotKitTargetTransport.startFromEnvironment(
          compositionFactory: makeSmokeComposition
        )
      } catch {
        // A normal manual launch deliberately has no descriptor and therefore
        // no listener. The Broker-owned launch is the only bootstrap path.
      }
    }
    return true
  }

  func applicationWillTerminate(_ application: UIApplication) {
    let active = transport
    transport = nil
    Task { await active?.stop() }
  }
}

func makeSmokeComposition(generation: UInt64) throws -> TargetRuntimeComposition {
  let schema = try SemanticSchema(
    id: "schema_smoke_ready_v1",
    revision: 1,
    document: .object([
      "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
      "$id": .string("app://smoke/ready@1"),
      "type": .string("object"),
      "properties": .object([
        "ready": .object([
          "type": .string("boolean"),
          "const": .bool(true),
        ]),
      ]),
      "required": .array([.string("ready")]),
      "additionalProperties": .bool(false),
    ])
  )
  let output = SemanticOutputCodec<Bool>(schema: schema) { ready in
    .object(["ready": .publicValue(.bool(ready))])
  }
  let builder = SemanticCatalogBuilder()
  try builder.registerResource(
    id: "smoke.ready",
    declarationRevision: 1,
    output: output,
    handler: { true }
  )
  let catalog = try builder.freeze(
    identity: SemanticCatalogIdentity(id: "catalog_smokehost0001", generation: generation)
  )
  let coordinator = TargetActionCoordinator(
    catalog: catalog,
    targetID: "target_smokehost",
    evidence: SmokeEvidence(),
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
      discover: { _, declaration in declaration.id == "smoke.ready" },
      discloseSchema: { _, declaration in declaration.id == "smoke.ready" },
      discloseResource: { _, declaration in declaration.id == "smoke.ready" },
      discloseAction: { _, _ in false }
    ),
    actionCoordinator: coordinator,
    processGeneration: generation
  )
}

private struct SmokeEvidence: ActionEvidencePort {
  func captureBefore(context: TargetActionContext) async throws {}
  func observeStability(context: TargetActionContext) async throws {}
  func captureAfter(context: TargetActionContext) async throws {}
}
#elseif !APPPILOTKIT_INTERNAL
#error("TransportSmokeHost is Debug/Internal-only and has no Release target.")
#else
@main
struct UnsupportedHostPlatform {
  static func main() {}
}
#endif
