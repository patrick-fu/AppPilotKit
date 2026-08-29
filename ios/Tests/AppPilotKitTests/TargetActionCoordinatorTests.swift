@testable import AppPilotKit
import XCTest

final class TargetActionCoordinatorTests: XCTestCase {
  func testDestructiveActionRequiresExactGrantAndRecordsEvidence() async throws {
    let calls = Counter()
    let evidence = EvidenceFixture()
    let authorization = AuthorizationFixture()
    let fixture = try makeFixture(
      authorization: .destructiveAuthorization,
      calls: calls,
      evidence: evidence,
      validate: { request in await authorization.validate(request) },
      consume: { request in await authorization.consume(request) }
    )

    do {
      try await fixture.coordinator.invoke(
        fixture.request,
        authorizationGrant: nil,
        session: fixture.session,
        sessionIsActive: { true }
      )
      XCTFail("missing destructive grant must fail")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .policyDenied)
    }
    var callCount = await calls.value
    XCTAssertEqual(callCount, 0)

    try await fixture.coordinator.invoke(
      fixture.request,
      authorizationGrant: "grant",
      session: fixture.session,
      sessionIsActive: { true }
    )
    callCount = await calls.value
    XCTAssertEqual(callCount, 1)
    let evidenceCalls = await evidence.calls
    XCTAssertEqual(evidenceCalls, ["before", "stability", "after"])
    let received = await authorization.received
    XCTAssertEqual(received?.binding.targetID, "target_fixture")
    XCTAssertEqual(received?.binding.processGeneration, fixture.session.generation)
    XCTAssertEqual(received?.binding.capability, "account.delete")
    XCTAssertEqual(received?.grant, "grant")

    do {
      try await fixture.coordinator.invoke(
        fixture.request,
        authorizationGrant: "grant",
        session: fixture.session,
        sessionIsActive: { true }
      )
      XCTFail("a destructive grant must be consumed at most once")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .policyDenied)
    }
    callCount = await calls.value
    XCTAssertEqual(callCount, 1)
  }

  func testPolicyAndEvidenceFailuresStayBeforeDispatch() async throws {
    let calls = Counter()
    let denied = try makeFixture(
      authorization: .none,
      calls: calls,
      resolve: { _, _ in nil }
    )
    do {
      try await denied.coordinator.invoke(
        denied.request,
        authorizationGrant: nil,
        session: denied.session,
        sessionIsActive: { true }
      )
      XCTFail("missing effective policy must fail closed")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .policyDenied)
    }
    var callCount = await calls.value
    XCTAssertEqual(callCount, 0)

    let beforeFailure = try makeFixture(
      authorization: .none,
      calls: calls,
      evidence: EvidenceFixture(failBefore: true)
    )
    do {
      try await beforeFailure.coordinator.invoke(
        beforeFailure.request,
        authorizationGrant: nil,
        session: beforeFailure.session,
        sessionIsActive: { true }
      )
      XCTFail("before evidence must be required")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .preDispatchFailed)
    }
    callCount = await calls.value
    XCTAssertEqual(callCount, 0)
  }

  func testPostDispatchFailuresAreNeverRetried() async throws {
    let calls = Counter()
    let handlerFailure = try makeFixture(
      authorization: .none,
      calls: calls,
      handlerFails: true
    )
    do {
      try await handlerFailure.coordinator.invoke(
        handlerFailure.request,
        authorizationGrant: nil,
        session: handlerFailure.session,
        sessionIsActive: { true }
      )
      XCTFail("a thrown handler has an ambiguous outcome")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .outcomeUnknown)
    }
    var callCount = await calls.value
    XCTAssertEqual(callCount, 1)

    let active = ActiveSwitch()
    let invalidated = try makeFixture(
      authorization: .none,
      calls: calls,
      handler: {
        await calls.increment()
        await active.invalidate()
      }
    )
    do {
      try await invalidated.coordinator.invoke(
        invalidated.request,
        authorizationGrant: nil,
        session: invalidated.session,
        sessionIsActive: { await active.value }
      )
      XCTFail("session loss after handoff must be ambiguous")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .outcomeUnknown)
    }
    callCount = await calls.value
    XCTAssertEqual(callCount, 2)

    let incompleteEvidence = EvidenceFixture(failAfter: true)
    let incomplete = try makeFixture(
      authorization: .none,
      calls: calls,
      evidence: incompleteEvidence
    )
    do {
      try await incomplete.coordinator.invoke(
        incomplete.request,
        authorizationGrant: nil,
        session: incomplete.session,
        sessionIsActive: { true }
      )
      XCTFail("post-dispatch evidence failure must be ambiguous")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .outcomeUnknown)
    }
    callCount = await calls.value
    XCTAssertEqual(callCount, 3)
  }

  func testOrdinaryAndSemanticMutationsConflictInBothDirectionsWithoutQueueing() async throws {
    let ordinaryGate = Gate()
    let first = try makeFixture(authorization: .none)
    let ordinary = Task {
      try await first.coordinator.invokeOrdinary(
        subject: ActionPolicySubject(
          id: "ordinary.fixture",
          declaredAuthorization: .none,
          retrySafety: .noAutomaticRetry
        ),
        authorizationGrant: nil,
        context: TargetActionContext(
          targetID: "target_fixture",
          processGeneration: first.session.generation,
          sessionID: first.session.id
        ),
        sessionIsActive: { true },
        body: {
        await ordinaryGate.enterAndWait()
        }
      )
    }
    await ordinaryGate.waitUntilEntered()
    do {
      try await first.coordinator.invoke(
        first.request,
        authorizationGrant: nil,
        session: first.session,
        sessionIsActive: { true }
      )
      XCTFail("semantic mutation must not queue behind an ordinary mutation")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .conflict)
    }
    await ordinaryGate.release()
    _ = try await ordinary.value

    let semanticGate = Gate()
    let second = try makeFixture(
      authorization: .none,
      handler: { await semanticGate.enterAndWait() }
    )
    let semantic = Task {
      try await second.coordinator.invoke(
        second.request,
        authorizationGrant: nil,
        session: second.session,
        sessionIsActive: { true }
      )
    }
    await semanticGate.waitUntilEntered()
    do {
      _ = try await second.coordinator.invokeOrdinary(
        subject: ActionPolicySubject(
          id: "ordinary.fixture",
          declaredAuthorization: .none,
          retrySafety: .noAutomaticRetry
        ),
        authorizationGrant: nil,
        context: TargetActionContext(
          targetID: "target_fixture",
          processGeneration: second.session.generation,
          sessionID: second.session.id
        ),
        sessionIsActive: { true },
        body: { true }
      )
      XCTFail("ordinary mutation must not queue behind a semantic mutation")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .conflict)
    }
    await semanticGate.release()
    try await semantic.value
  }

  func testInvalidInputDoesNotTouchPolicyGrantWriterEvidenceOrHandler() async throws {
    let calls = Counter()
    let policyCalls = Counter()
    let grants = AuthorizationFixture()
    let evidence = EvidenceFixture()
    let fixture = try makeFixture(
      authorization: .destructiveAuthorization,
      calls: calls,
      evidence: evidence,
      resolve: { _, subject in
        await policyCalls.increment()
        return SemanticActionPolicy(
          authorization: subject.declaredAuthorization,
          retrySafety: subject.retrySafety
        )
      },
      validate: { request in await grants.validate(request) },
      consume: { request in await grants.consume(request) }
    )
    var invalid = fixture.request
    invalid = SemanticActionInvocation(
      capability: fixture.request.capability,
      declarationRevision: fixture.request.declarationRevision,
      inputSchema: fixture.request.inputSchema,
      input: .object(["undeclared": .string("nope")])
    )
    do {
      try await fixture.coordinator.invoke(
        invalid,
        authorizationGrant: "grant",
        session: fixture.session,
        sessionIsActive: { true }
      )
      XCTFail("invalid input must fail before side effects")
    } catch {
      XCTAssertEqual(error as? SemanticCatalogError, .invalidInput)
    }
    let observed1 = await calls.value
    XCTAssertEqual(observed1, 0)
    let observed2 = await policyCalls.value
    XCTAssertEqual(observed2, 0)
    let observed3 = await grants.validateCount
    XCTAssertEqual(observed3, 0)
    let observed4 = await grants.consumeCount
    XCTAssertEqual(observed4, 0)
    let observed5 = await evidence.calls
    XCTAssertEqual(observed5, [])
  }

  func testGrantValidateSucceedsButBusyWriterDoesNotConsume() async throws {
    let holdSchema = try SemanticSchema(
      id: "schema_actionhold01",
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://fixture/hold@1"),
        "type": .string("object"),
        "properties": .object([:]),
        "additionalProperties": .bool(false),
      ])
    )
    let grantSchema = try SemanticSchema(
      id: "schema_actiongrant01",
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://fixture/granted@1"),
        "type": .string("object"),
        "properties": .object([:]),
        "additionalProperties": .bool(false),
      ])
    )
    let hold = Gate()
    let grantedCalls = Counter()
    let builder = SemanticCatalogBuilder()
    try builder.registerAction(
      id: "writer.hold",
      declarationRevision: 1,
      input: SemanticInputCodec(schema: holdSchema) { $0 },
      policy: SemanticActionPolicy(authorization: .none, retrySafety: .noAutomaticRetry)
    ) { _ in await hold.enterAndWait() }
    try builder.registerAction(
      id: "writer.granted",
      declarationRevision: 1,
      input: SemanticInputCodec(schema: grantSchema) { $0 },
      policy: SemanticActionPolicy(
        authorization: .destructiveAuthorization,
        retrySafety: .noAutomaticRetry
      )
    ) { _ in await grantedCalls.increment() }
    let catalog = try builder.freeze(
      identity: SemanticCatalogIdentity(id: "catalog_fixture0002", generation: 7)
    )
    let grants = AuthorizationFixture()
    let coordinator = TargetActionCoordinator(
      catalog: catalog,
      targetID: "target_fixture",
      evidence: EvidenceFixture(),
      policy: TargetActionPolicy(
        resolve: { _, subject in
          SemanticActionPolicy(
            authorization: subject.declaredAuthorization,
            retrySafety: subject.retrySafety
          )
        },
        validateDestructive: { request in await grants.validate(request) },
        consumeDestructive: { request in await grants.consume(request) }
      )
    )
    let session = SemanticProtocolSessionContext(id: "session_fixture0001", generation: 7)
    let occupying = Task {
      try await coordinator.invoke(
        SemanticActionInvocation(
          capability: "writer.hold",
          declarationRevision: 1,
          inputSchema: holdSchema.handle,
          input: .object([:])
        ),
        authorizationGrant: nil,
        session: session,
        sessionIsActive: { true }
      )
    }
    await hold.waitUntilEntered()
    let grantedRequest = SemanticActionInvocation(
      capability: "writer.granted",
      declarationRevision: 1,
      inputSchema: grantSchema.handle,
      input: .object([:])
    )
    do {
      try await coordinator.invoke(
        grantedRequest,
        authorizationGrant: "grant",
        session: session,
        sessionIsActive: { true }
      )
      XCTFail("busy writer must conflict")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .conflict)
    }
    let observed6 = await grants.validateCount
    XCTAssertEqual(observed6, 1)
    let observed7 = await grants.consumeCount
    XCTAssertEqual(observed7, 0)
    let observed8 = await grantedCalls.value
    XCTAssertEqual(observed8, 0)
    await hold.release()
    try await occupying.value
    try await coordinator.invoke(
      grantedRequest,
      authorizationGrant: "grant",
      session: session,
      sessionIsActive: { true }
    )
    let observed9 = await grants.consumeCount
    XCTAssertEqual(observed9, 1)
    let observed10 = await grantedCalls.value
    XCTAssertEqual(observed10, 1)
  }

  func testChangedBindingRejectsConsumeWithoutDispatch() async throws {
    let calls = Counter()
    let grants = AuthorizationFixture()
    let fixture = try makeFixture(
      authorization: .destructiveAuthorization,
      calls: calls,
      validate: { request in await grants.validate(request) },
      consume: { request in await grants.consume(request) }
    )
    await grants.requireExactBindingOnConsume(false)
    do {
      try await fixture.coordinator.invoke(
        fixture.request,
        authorizationGrant: "grant",
        session: fixture.session,
        sessionIsActive: { true }
      )
      XCTFail("mutated binding must fail closed")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .policyDenied)
    }
    let observed11 = await grants.validateCount
    XCTAssertEqual(observed11, 1)
    let observed12 = await grants.consumeCount
    XCTAssertEqual(observed12, 1)
    let observed13 = await grants.consumed
    XCTAssertEqual(observed13, false)
    let observed14 = await calls.value
    XCTAssertEqual(observed14, 0)
  }

  func testFailureAfterConsumeBeforeDispatchIsKnownAndRejectsReplay() async throws {
    let calls = Counter()
    let grants = AuthorizationFixture()
    let fixture = try makeFixture(
      authorization: .destructiveAuthorization,
      calls: calls,
      evidence: EvidenceFixture(failBefore: true),
      validate: { request in await grants.validate(request) },
      consume: { request in await grants.consume(request) }
    )
    do {
      try await fixture.coordinator.invoke(
        fixture.request,
        authorizationGrant: "grant",
        session: fixture.session,
        sessionIsActive: { true }
      )
      XCTFail("evidence-before failure must stay pre-handoff")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .preDispatchFailed)
    }
    let observed15 = await calls.value
    XCTAssertEqual(observed15, 0)
    let observed16 = await grants.consumed
    XCTAssertEqual(observed16, true)
    do {
      try await fixture.coordinator.invoke(
        fixture.request,
        authorizationGrant: "grant",
        session: fixture.session,
        sessionIsActive: { true }
      )
      XCTFail("consumed grant must not replay")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .policyDenied)
    }
    let observed17 = await calls.value
    XCTAssertEqual(observed17, 0)
  }

  func testSameTargetCoordinatorsShareAWriter() async throws {
    let firstGate = Gate()
    let first = try makeFixture(
      authorization: .none,
      targetID: "target_shared",
      handler: { await firstGate.enterAndWait() }
    )
    let secondCalls = Counter()
    let second = try makeFixture(
      authorization: .none,
      targetID: "target_shared",
      calls: secondCalls
    )
    let occupying = Task {
      try await first.coordinator.invoke(
        first.request,
        authorizationGrant: nil,
        session: first.session,
        sessionIsActive: { true }
      )
    }
    await firstGate.waitUntilEntered()
    do {
      try await second.coordinator.invoke(
        second.request,
        authorizationGrant: nil,
        session: second.session,
        sessionIsActive: { true }
      )
      XCTFail("the same Target must not admit a second writer")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .conflict)
    }
    let observed = await secondCalls.value
    XCTAssertEqual(observed, 0)
    await firstGate.release()
    try await occupying.value
  }

  func testConsumeThrowIsKnownPreDispatchAndLeavesGrantUnconsumed() async throws {
    let calls = Counter()
    let grants = AuthorizationFixture()
    let throwOnConsume = Flag(true)
    let fixture = try makeFixture(
      authorization: .destructiveAuthorization,
      calls: calls,
      validate: { request in await grants.validate(request) },
      consume: { request in
        if await throwOnConsume.value { throw FixtureError.failed }
        return await grants.consume(request)
      }
    )
    do {
      try await fixture.coordinator.invoke(
        fixture.request,
        authorizationGrant: "grant",
        session: fixture.session,
        sessionIsActive: { true }
      )
      XCTFail("consume infrastructure throw must fail closed")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .preDispatchFailed)
    }
    let observedCalls = await calls.value
    XCTAssertEqual(observedCalls, 0)
    let consumed = await grants.consumed
    XCTAssertEqual(consumed, false)

    await throwOnConsume.set(false)
    try await fixture.coordinator.invoke(
      fixture.request,
      authorizationGrant: "grant",
      session: fixture.session,
      sessionIsActive: { true }
    )
    let completed = await calls.value
    XCTAssertEqual(completed, 1)
    let consumedAfter = await grants.consumed
    XCTAssertEqual(consumedAfter, true)
  }

  func testCancellationAfterWriterAcquireDoesNotConsume() async throws {
    let calls = Counter()
    let grants = AuthorizationFixture()
    let checks = Counter()
    let afterWriter = Gate()
    let fixture = try makeFixture(
      authorization: .destructiveAuthorization,
      calls: calls,
      validate: { request in await grants.validate(request) },
      consume: { request in await grants.consume(request) }
    )
    let invokeTask = Task {
      try await fixture.coordinator.invoke(
        fixture.request,
        authorizationGrant: "grant",
        session: fixture.session,
        sessionIsActive: {
          let count = await checks.increment()
          if count == 2 {
            await afterWriter.enterAndWait()
          }
          return true
        }
      )
    }
    await afterWriter.waitUntilEntered()
    invokeTask.cancel()
    await afterWriter.release()
    do {
      try await invokeTask.value
      XCTFail("cancelled invoke must fail before consume")
    } catch {
      XCTAssertEqual(error as? TargetActionCoordinatorError, .preDispatchFailed)
    }
    let observedCalls = await calls.value
    XCTAssertEqual(observedCalls, 0)
    let validateCount = await grants.validateCount
    XCTAssertEqual(validateCount, 1)
    let consumeCount = await grants.consumeCount
    XCTAssertEqual(consumeCount, 0)
  }

  func testDifferentTargetsDoNotShareAWriter() async throws {
    let firstGate = Gate()
    let first = try makeFixture(
      authorization: .none,
      targetID: "target_a",
      handler: { await firstGate.enterAndWait() }
    )
    let secondCalls = Counter()
    let second = try makeFixture(
      authorization: .none,
      targetID: "target_b",
      calls: secondCalls
    )
    let occupying = Task {
      try await first.coordinator.invoke(
        first.request,
        authorizationGrant: nil,
        session: first.session,
        sessionIsActive: { true }
      )
    }
    await firstGate.waitUntilEntered()
    try await second.coordinator.invoke(
      second.request,
      authorizationGrant: nil,
      session: second.session,
      sessionIsActive: { true }
    )
    let observed18 = await secondCalls.value
    XCTAssertEqual(observed18, 1)
    await firstGate.release()
    try await occupying.value
  }

  private func makeFixture(
    authorization: SemanticActionAuthorization,
    targetID: String = "target_fixture",
    calls: Counter = Counter(),
    evidence: EvidenceFixture = EvidenceFixture(),
    handlerFails: Bool = false,
    resolve: @escaping @Sendable (
      TargetActionContext,
      ActionPolicySubject
    ) async throws -> SemanticActionPolicy? = { _, subject in
      SemanticActionPolicy(
        authorization: subject.declaredAuthorization,
        retrySafety: subject.retrySafety
      )
    },
    validate: @escaping @Sendable (DestructiveGrantCheck) async throws -> Bool = { $0.grant == "grant" },
    consume: @escaping @Sendable (DestructiveGrantCheck) async throws -> Bool = { $0.grant == "grant" },
    handler: (@Sendable () async throws -> Void)? = nil
  ) throws -> Fixture {
    let schema = try SemanticSchema(
      id: "schema_action0001",
      revision: 1,
      document: .object([
        "$schema": .string("https://json-schema.org/draft/2020-12/schema"),
        "$id": .string("app://fixture/action@1"),
        "type": .string("object"),
        "properties": .object([:]),
        "additionalProperties": .bool(false),
      ])
    )
    let builder = SemanticCatalogBuilder()
    try builder.registerAction(
      id: "account.delete",
      declarationRevision: 1,
      input: SemanticInputCodec(schema: schema) { $0 },
      policy: SemanticActionPolicy(
        authorization: authorization,
        retrySafety: .noAutomaticRetry
      ),
      handler: { _ in
        if let handler {
          try await handler()
        } else {
          await calls.increment()
          if handlerFails { throw FixtureError.failed }
        }
      }
    )
    let catalog = try builder.freeze(
      identity: SemanticCatalogIdentity(id: "catalog_fixture0001", generation: 7)
    )
    let coordinator = TargetActionCoordinator(
      catalog: catalog,
      targetID: targetID,
      evidence: evidence,
      policy: TargetActionPolicy(
        resolve: resolve,
        validateDestructive: validate,
        consumeDestructive: consume
      )
    )
    let request = SemanticActionInvocation(
      capability: "account.delete",
      declarationRevision: 1,
      inputSchema: schema.handle,
      input: .object([:])
    )
    return Fixture(
      coordinator: coordinator,
      request: request,
      session: SemanticProtocolSessionContext(id: "session_fixture0001", generation: 7)
    )
  }
}

private struct Fixture {
  let coordinator: TargetActionCoordinator
  let request: SemanticActionInvocation
  let session: SemanticProtocolSessionContext
}

private actor Flag {
  private(set) var value: Bool
  init(_ value: Bool) { self.value = value }
  func set(_ value: Bool) { self.value = value }
}

private actor Counter {
  private(set) var value = 0
  @discardableResult
  func increment() -> Int {
    value += 1
    return value
  }
}

private actor AuthorizationFixture {
  private(set) var received: DestructiveGrantCheck?
  private(set) var validateCount = 0
  private(set) var consumeCount = 0
  private(set) var consumed = false
  private var recordedBinding: CanonicalActionBinding?
  private var mismatchOnConsume = false

  func validate(_ request: DestructiveGrantCheck) -> Bool {
    received = request
    validateCount += 1
    guard request.grant == "grant", !consumed else { return false }
    recordedBinding = request.binding
    return true
  }

  func consume(_ request: DestructiveGrantCheck) -> Bool {
    received = request
    consumeCount += 1
    guard request.grant == "grant", !consumed else { return false }
    if mismatchOnConsume {
      return false
    }
    guard recordedBinding == request.binding else { return false }
    consumed = true
    return true
  }

  func requireExactBindingOnConsume(_ matches: Bool) {
    mismatchOnConsume = !matches
  }
}

private actor ActiveSwitch {
  private(set) var value = true
  func invalidate() { value = false }
}

private actor EvidenceFixture: ActionEvidencePort {
  private let failBefore: Bool
  private let failAfter: Bool
  private(set) var calls: [String] = []

  init(failBefore: Bool = false, failAfter: Bool = false) {
    self.failBefore = failBefore
    self.failAfter = failAfter
  }

  func captureBefore(context: TargetActionContext) throws {
    calls.append("before")
    if failBefore { throw FixtureError.failed }
  }

  func observeStability(context: TargetActionContext) throws {
    calls.append("stability")
  }

  func captureAfter(context: TargetActionContext) throws {
    calls.append("after")
    if failAfter { throw FixtureError.failed }
  }
}

private actor Gate {
  private var entered = false
  private var entryWaiter: CheckedContinuation<Void, Never>?
  private var releaseWaiter: CheckedContinuation<Void, Never>?

  func enterAndWait() async {
    entered = true
    entryWaiter?.resume()
    entryWaiter = nil
    await withCheckedContinuation { releaseWaiter = $0 }
  }

  func waitUntilEntered() async {
    guard !entered else { return }
    await withCheckedContinuation { entryWaiter = $0 }
  }

  func release() {
    releaseWaiter?.resume()
    releaseWaiter = nil
  }
}

private enum FixtureError: Error {
  case failed
}
