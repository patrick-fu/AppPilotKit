import AppPilotKit
import XCTest

final class UISnapshotRuntimeTests: XCTestCase {
  @MainActor
  func testDuplicateProviderRegistrationFailsDeterministically() throws {
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: RedactedProviderCapture(sources: [])
    )

    XCTAssertThrowsError(try UISnapshotRuntime(providers: [provider, provider])) { error in
      XCTAssertEqual(
        error as? UISnapshotRuntimeError,
        .invalidParams("Duplicate provider name: fixture.views")
      )
    }
  }

  @MainActor
  func testRuntimeConstructionRejectsInvalidRegistrationAndLimits() throws {
    let invalidNameProvider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "Fixture Views", platform: .iOS),
      capture: RedactedProviderCapture(sources: [])
    )
    let androidProvider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .android),
      capture: RedactedProviderCapture(sources: [])
    )

    let constructions: [() throws -> UISnapshotRuntime] = [
      { try UISnapshotRuntime(providers: [invalidNameProvider]) },
      { try UISnapshotRuntime(providers: [androidProvider]) },
      {
        try UISnapshotRuntime(
          providers: [],
          limits: UISnapshotStoreLimits(
            maximumSnapshotCount: 0,
            maximumStoredBytes: 1024
          )
        )
      },
      {
        try UISnapshotRuntime(
          providers: [],
          limits: UISnapshotStoreLimits(
            maximumSnapshotCount: 1,
            maximumStoredBytes: 0
          )
        )
      },
    ]

    for construction in constructions {
      XCTAssertThrowsError(try construction()) { error in
        XCTAssertEqual((error as? UISnapshotRuntimeError)?.kind, .invalidParams)
      }
    }
  }

  @MainActor
  func testCaptureRejectsAnUnknownRequestedProvider() async throws {
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: RedactedProviderCapture(sources: [])
    )
    let runtime = try UISnapshotRuntime(providers: [provider])

    do {
      _ = try await runtime.capture(
        providers: ["z.missing", "a.missing"],
        in: UISnapshotScope(sessionID: "session-one", processGeneration: 1)
      )
      XCTFail("Expected an unknown provider failure")
    } catch {
      XCTAssertEqual(
        error as? UISnapshotRuntimeError,
        .invalidParams("Unknown provider: z.missing")
      )
    }
  }

  @MainActor
  func testCaptureRejectsInvalidScopeAndProviderSelection() async throws {
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: makeSingleNodeCapture(
        provider: "fixture.views",
        sourceID: "fixture.main",
        nodeID: "fixture-root"
      )
    )
    let invalidRequests: [(String, UISnapshotScope, [String]?)] = [
      (
        "empty session",
        UISnapshotScope(sessionID: "", processGeneration: 1),
        nil
      ),
      (
        "zero process generation",
        UISnapshotScope(sessionID: "session-one", processGeneration: 0),
        nil
      ),
      (
        "empty provider selection",
        UISnapshotScope(sessionID: "session-one", processGeneration: 1),
        []
      ),
      (
        "duplicate provider selection",
        UISnapshotScope(sessionID: "session-one", processGeneration: 1),
        ["fixture.views", "fixture.views"]
      ),
    ]

    for (name, scope, requestedProviders) in invalidRequests {
      let runtime = try UISnapshotRuntime(providers: [provider])
      do {
        _ = try await runtime.capture(providers: requestedProviders, in: scope)
        XCTFail("Expected invalid request to fail: \(name)")
      } catch let error as UISnapshotRuntimeError {
        XCTAssertEqual(error.kind, .invalidParams, name)
      }
    }
  }

  @MainActor
  func testProviderFailureLeavesNoPartialSnapshotAndMapsToInternalError() async throws {
    var secondProviderShouldFail = true
    var captureOrder: [String] = []
    let first = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "first.views", platform: .iOS)
    ) {
      captureOrder.append("first.views")
      return makeSingleNodeCapture(
        provider: "first.views",
        sourceID: "first.main",
        nodeID: "first-root"
      )
    }
    let second = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "second.views", platform: .iOS)
    ) {
      captureOrder.append("second.views")
      if secondProviderShouldFail {
        throw FixtureFailure.captureFailed
      }
      return makeSingleNodeCapture(
        provider: "second.views",
        sourceID: "second.main",
        nodeID: "second-root"
      )
    }
    let runtime = try UISnapshotRuntime(providers: [first, second])
    let scope = UISnapshotScope(sessionID: "session-one", processGeneration: 1)

    do {
      _ = try await runtime.capture(providers: ["second.views", "first.views"], in: scope)
      XCTFail("Expected provider capture to fail")
    } catch {
      XCTAssertEqual(
        error as? UISnapshotRuntimeError,
        .internalError("Provider second.views failed")
      )
    }

    secondProviderShouldFail = false
    captureOrder.removeAll()
    let captured = try await runtime.capture(
      providers: ["second.views", "first.views"],
      in: scope
    )

    XCTAssertEqual(captured.identity.generation, 1)
    XCTAssertEqual(captureOrder, ["first.views", "second.views"])
    XCTAssertEqual(captured.sources.map(\.provider), ["first.views", "second.views"])
  }

  @MainActor
  func testCaptureRejectsAnEmptyProviderCaptureBeforeCommit() async throws {
    var capture = RedactedProviderCapture(sources: [])
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS)
    ) {
      capture
    }
    let runtime = try UISnapshotRuntime(providers: [provider])
    let scope = UISnapshotScope(sessionID: "session-one", processGeneration: 1)

    do {
      _ = try await runtime.capture(in: scope)
      XCTFail("Expected an empty provider capture to fail")
    } catch let error as UISnapshotRuntimeError {
      XCTAssertEqual(error.kind, .internalError)
    }

    capture = makeSingleNodeCapture(
      provider: "fixture.views",
      sourceID: "fixture.main",
      nodeID: "fixture-root"
    )
    let valid = try await runtime.capture(in: scope)
    XCTAssertEqual(valid.identity.generation, 1)
  }

  @MainActor
  func testCaptureRejectsSourceMetadataThatViolatesTheProviderContract() async throws {
    let validSource = makeSingleNodeCapture(
      provider: "fixture.views",
      sourceID: "fixture.main",
      nodeID: "fixture-root"
    ).sources[0]
    let invalidCaptures: [(String, RedactedProviderCapture)] = [
      (
        "provider mismatch",
        RedactedProviderCapture(
          sources: [
            RedactedSourceCapture(
              id: "fixture.main",
              provider: "other.views",
              platform: .iOS,
              representation: .native,
              nativeSchema: "fixture.views@1",
              coordinateSpace: UICoordinateSpace(unit: .point, scale: 2),
              coverage: .complete,
              nodes: validSource.nodes
            )
          ]
        )
      ),
      (
        "non-iOS platform",
        RedactedProviderCapture(
          sources: [
            RedactedSourceCapture(
              id: "fixture.main",
              provider: "fixture.views",
              platform: .android,
              representation: .native,
              nativeSchema: "fixture.views@1",
              coordinateSpace: UICoordinateSpace(unit: .pixel, scale: 1),
              coverage: .complete,
              nodes: validSource.nodes
            )
          ]
        )
      ),
      (
        "pixel iOS coordinates",
        RedactedProviderCapture(
          sources: [
            RedactedSourceCapture(
              id: "fixture.main",
              provider: "fixture.views",
              platform: .iOS,
              representation: .native,
              nativeSchema: "fixture.views@1",
              coordinateSpace: UICoordinateSpace(unit: .pixel, scale: 2),
              coverage: .complete,
              nodes: validSource.nodes
            )
          ]
        )
      ),
      (
        "duplicate source identity",
        RedactedProviderCapture(sources: [validSource, validSource])
      ),
      (
        "partial coverage without limitations",
        RedactedProviderCapture(
          sources: [
            RedactedSourceCapture(
              id: "fixture.main",
              provider: "fixture.views",
              platform: .iOS,
              representation: .native,
              nativeSchema: "fixture.views@1",
              coordinateSpace: UICoordinateSpace(unit: .point, scale: 2),
              coverage: .partial,
              nodes: validSource.nodes
            )
          ]
        )
      ),
    ]

    for (name, capture) in invalidCaptures {
      let provider = FixtureProvider(
        descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
        capture: capture
      )
      let runtime = try UISnapshotRuntime(providers: [provider])
      do {
        _ = try await runtime.capture(
          in: UISnapshotScope(sessionID: "session-one", processGeneration: 1)
        )
        XCTFail("Expected invalid source metadata to fail: \(name)")
      } catch let error as UISnapshotRuntimeError {
        XCTAssertEqual(error.kind, .internalError, name)
      }
    }
  }

  @MainActor
  func testCaptureRejectsASourceWithoutNodes() async throws {
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: RedactedProviderCapture(
        sources: [
          RedactedSourceCapture(
            id: "fixture.main",
            provider: "fixture.views",
            platform: .iOS,
            representation: .native,
            nativeSchema: "fixture.views@1",
            coordinateSpace: UICoordinateSpace(unit: .point, scale: 2),
            coverage: .complete,
            nodes: []
          )
        ]
      )
    )
    let runtime = try UISnapshotRuntime(providers: [provider])

    do {
      _ = try await runtime.capture(
        in: UISnapshotScope(sessionID: "session-one", processGeneration: 1)
      )
      XCTFail("Expected an empty source to fail")
    } catch let error as UISnapshotRuntimeError {
      XCTAssertEqual(error.kind, .internalError)
    }
  }

  @MainActor
  func testCaptureRejectsInvalidProviderGraphs() async throws {
    let invalidGraphs: [(String, [RedactedNodeCapture])] = [
      (
        "missing root",
        [
          RedactedNodeCapture(
            id: "child",
            parentID: "missing",
            childIndex: 0,
            depth: 1,
            childCount: 0
          )
        ]
      ),
      (
        "extra root",
        [
          RedactedNodeCapture(id: "root-one", depth: 0, childCount: 0),
          RedactedNodeCapture(id: "root-two", depth: 0, childCount: 0),
        ]
      ),
      (
        "missing adjacency",
        [
          RedactedNodeCapture(id: "root", depth: 0, childCount: 1),
          RedactedNodeCapture(id: "child", depth: 1, childCount: 0),
        ]
      ),
      (
        "invalid parent depth",
        [
          RedactedNodeCapture(id: "root", depth: 0, childCount: 1),
          RedactedNodeCapture(
            id: "child",
            parentID: "root",
            childIndex: 0,
            depth: 2,
            childCount: 0
          ),
        ]
      ),
      (
        "non depth-first order",
        [
          RedactedNodeCapture(id: "root", depth: 0, childCount: 2),
          RedactedNodeCapture(
            id: "first",
            parentID: "root",
            childIndex: 0,
            depth: 1,
            childCount: 1
          ),
          RedactedNodeCapture(
            id: "second",
            parentID: "root",
            childIndex: 1,
            depth: 1,
            childCount: 0
          ),
          RedactedNodeCapture(
            id: "grandchild",
            parentID: "first",
            childIndex: 0,
            depth: 2,
            childCount: 0
          ),
        ]
      ),
      (
        "sibling order",
        [
          RedactedNodeCapture(id: "root", depth: 0, childCount: 2),
          RedactedNodeCapture(
            id: "second",
            parentID: "root",
            childIndex: 1,
            depth: 1,
            childCount: 0
          ),
          RedactedNodeCapture(
            id: "first",
            parentID: "root",
            childIndex: 0,
            depth: 1,
            childCount: 0
          ),
        ]
      ),
      (
        "child count mismatch",
        [
          RedactedNodeCapture(id: "root", depth: 0, childCount: 2),
          RedactedNodeCapture(
            id: "only-child",
            parentID: "root",
            childIndex: 0,
            depth: 1,
            childCount: 0
          ),
        ]
      ),
    ]

    for (name, nodes) in invalidGraphs {
      let provider = FixtureProvider(
        descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
        capture: makeCapture(
          provider: "fixture.views",
          sourceID: "fixture.main",
          nodes: nodes
        )
      )
      let runtime = try UISnapshotRuntime(providers: [provider])
      do {
        _ = try await runtime.capture(
          in: UISnapshotScope(sessionID: "session-one", processGeneration: 1)
        )
        XCTFail("Expected invalid graph to fail: \(name)")
      } catch let error as UISnapshotRuntimeError {
        XCTAssertEqual(error.kind, .internalError, name)
      }
    }
  }

  @MainActor
  func testProviderLocalIdentityIsNotExposedByValidationErrors() async throws {
    let secretLocalID = "known-secret-local-id"
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: makeCapture(
        provider: "fixture.views",
        sourceID: "fixture.main",
        nodes: [
          RedactedNodeCapture(id: secretLocalID, depth: 0, childCount: 1),
          RedactedNodeCapture(
            id: secretLocalID,
            parentID: secretLocalID,
            childIndex: 0,
            depth: 1,
            childCount: 0
          ),
        ]
      )
    )
    let runtime = try UISnapshotRuntime(providers: [provider])

    do {
      _ = try await runtime.capture(
        in: UISnapshotScope(sessionID: "session-one", processGeneration: 1)
      )
      XCTFail("Expected provider graph validation to fail")
    } catch let error as UISnapshotRuntimeError {
      XCTAssertEqual(error.kind, .internalError)
      XCTAssertFalse(String(describing: error).contains(secretLocalID))
    }
  }

  @MainActor
  func testCaptureRejectsValuesOutsideTheStoredContract() async throws {
    let invalidCaptures: [(String, RedactedProviderCapture)] = [
      (
        "invalid source ID",
        makeSingleNodeCapture(
          provider: "fixture.views",
          sourceID: "Fixture Main",
          nodeID: "root"
        )
      ),
      (
        "invalid native schema",
        RedactedProviderCapture(
          sources: [
            RedactedSourceCapture(
              id: "fixture.main",
              provider: "fixture.views",
              platform: .iOS,
              representation: .native,
              nativeSchema: "fixture",
              coordinateSpace: UICoordinateSpace(unit: .point, scale: 2),
              coverage: .complete,
              nodes: [
                RedactedNodeCapture(id: "root", depth: 0, childCount: 0)
              ]
            )
          ]
        )
      ),
      (
        "empty index",
        makeCapture(
          provider: "fixture.views",
          sourceID: "fixture.main",
          nodes: [
            RedactedNodeCapture(
              id: "root",
              depth: 0,
              childCount: 0,
              index: RedactedNodeIndex()
            )
          ]
        )
      ),
      (
        "invalid frame",
        makeCapture(
          provider: "fixture.views",
          sourceID: "fixture.main",
          nodes: [
            RedactedNodeCapture(
              id: "root",
              depth: 0,
              childCount: 0,
              index: RedactedNodeIndex(
                frame: UIRect(x: 0, y: 0, width: -1, height: 10)
              )
            )
          ]
        )
      ),
      (
        "non-finite native number",
        makeCapture(
          provider: "fixture.views",
          sourceID: "fixture.main",
          nodes: [
            RedactedNodeCapture(
              id: "root",
              depth: 0,
              childCount: 0,
              native: ["value": .number(.nan)]
            )
          ]
        )
      ),
      (
        "duplicate node identity",
        makeCapture(
          provider: "fixture.views",
          sourceID: "fixture.main",
          nodes: [
            RedactedNodeCapture(id: "root", depth: 0, childCount: 1),
            RedactedNodeCapture(
              id: "root",
              parentID: "root",
              childIndex: 0,
              depth: 1,
              childCount: 0
            ),
          ]
        )
      ),
    ]

    for (name, capture) in invalidCaptures {
      let provider = FixtureProvider(
        descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
        capture: capture
      )
      let runtime = try UISnapshotRuntime(providers: [provider])
      do {
        _ = try await runtime.capture(
          in: UISnapshotScope(sessionID: "session-one", processGeneration: 1)
        )
        XCTFail("Expected stored-contract validation failure: \(name)")
      } catch let error as UISnapshotRuntimeError {
        XCTAssertEqual(error.kind, .internalError, name)
      }
    }
  }

  func testJSONValuePreservesFullWidthNativeIntegers() throws {
    let value = JSONValue.object([
      "signed": .integer(Int64.max),
      "unsigned": .unsignedInteger(UInt64.max),
    ])
    let encoder = JSONEncoder()
    encoder.outputFormatting = .sortedKeys

    let data = try encoder.encode(value)
    let encoded = try XCTUnwrap(String(data: data, encoding: .utf8))
    let decoded = try JSONDecoder().decode(JSONValue.self, from: data)

    XCTAssertTrue(encoded.contains("9223372036854775807"))
    XCTAssertTrue(encoded.contains("18446744073709551615"))
    XCTAssertEqual(decoded, value)
  }

  @MainActor
  func testStoredSnapshotIsDetachedRedactedAndSupportsMultipleSources() async throws {
    let secret = "known-secret"
    let provider = RedactingFixtureProvider(
      rawText: "Account known-secret",
      secret: secret
    )
    let runtime = try UISnapshotRuntime(providers: [provider])
    let scope = UISnapshotScope(sessionID: "session-one", processGeneration: 1)

    let captured = try await runtime.capture(in: scope)
    provider.rawText = "Mutated known-secret"
    let resolved = try await runtime.resolve(captured.identity, in: scope)
    let encoded = try JSONEncoder().encode(resolved)
    let encodedText = try XCTUnwrap(String(data: encoded, encoding: .utf8))

    XCTAssertEqual(resolved, captured)
    XCTAssertEqual(resolved.sources.map(\.id), ["fixture.native", "fixture.accessibility"])
    XCTAssertEqual(Set(resolved.nodes.map(\.reference)).count, 2)
    XCTAssertTrue(encodedText.contains("Account [REDACTED]"))
    XCTAssertFalse(encodedText.contains(secret))
    XCTAssertFalse(encodedText.contains("Mutated"))
  }

  @MainActor
  func testCountLimitEvictsOldestGenerationAndResolveDoesNotRefreshFIFO() async throws {
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: makeSingleNodeCapture(
        provider: "fixture.views",
        sourceID: "fixture.main",
        nodeID: "fixture-root"
      )
    )
    let runtime = try UISnapshotRuntime(
      providers: [provider],
      limits: UISnapshotStoreLimits(
        maximumSnapshotCount: 2,
        maximumStoredBytes: 1024 * 1024
      )
    )
    let scope = UISnapshotScope(sessionID: "session-one", processGeneration: 1)

    let first = try await runtime.capture(in: scope)
    let second = try await runtime.capture(in: scope)
    _ = try await runtime.resolve(first.identity, in: scope)
    let third = try await runtime.capture(in: scope)

    do {
      _ = try await runtime.resolve(first.identity, in: scope)
      XCTFail("Expected the oldest snapshot to be evicted")
    } catch let error as UISnapshotRuntimeError {
      XCTAssertEqual(error, .snapshotExpired)
    }
    let resolvedSecond = try await runtime.resolve(second.identity, in: scope)
    let resolvedThird = try await runtime.resolve(third.identity, in: scope)
    XCTAssertEqual(resolvedSecond, second)
    XCTAssertEqual(resolvedThird, third)
  }

  @MainActor
  func testScopeInvalidationExpiresOnlyThatScopeWithoutResettingGeneration() async throws {
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: makeSingleNodeCapture(
        provider: "fixture.views",
        sourceID: "fixture.main",
        nodeID: "fixture-root"
      )
    )
    let runtime = try UISnapshotRuntime(providers: [provider])
    let firstScope = UISnapshotScope(sessionID: "session-one", processGeneration: 1)
    let secondScope = UISnapshotScope(sessionID: "session-two", processGeneration: 1)
    let first = try await runtime.capture(in: firstScope)
    let second = try await runtime.capture(in: secondScope)

    await runtime.invalidate(scope: firstScope)

    do {
      _ = try await runtime.resolve(first.identity, in: firstScope)
      XCTFail("Expected invalidated snapshot to expire")
    } catch let error as UISnapshotRuntimeError {
      XCTAssertEqual(error, .snapshotExpired)
    }
    let retained = try await runtime.resolve(second.identity, in: secondScope)
    XCTAssertEqual(retained, second)
    let recaptured = try await runtime.capture(in: firstScope)
    XCTAssertEqual(recaptured.identity.generation, 3)
  }

  @MainActor
  func testStoredBytesUsesCanonicalRecordAndByteLimitEvictsFIFO() async throws {
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: makeSingleNodeCapture(
        provider: "fixture.views",
        sourceID: "fixture.main",
        nodeID: "fixture-root"
      )
    )
    let scope = UISnapshotScope(sessionID: "session-one", processGeneration: 1)
    let calibrationRuntime = try UISnapshotRuntime(providers: [provider])
    let calibration = try await calibrationRuntime.capture(in: scope)
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    let expectedStoredBytes = try encoder.encode(
      ExpectedStoredSnapshotRecord(snapshot: calibration)
    ).count

    XCTAssertEqual(calibration.storedBytes, expectedStoredBytes)
    XCTAssertGreaterThan(calibration.storedBytes, 0)
    guard calibration.storedBytes > 0 else { return }

    let runtime = try UISnapshotRuntime(
      providers: [provider],
      limits: UISnapshotStoreLimits(
        maximumSnapshotCount: 10,
        maximumStoredBytes: calibration.storedBytes * 2 - 1
      )
    )
    let first = try await runtime.capture(in: scope)
    let second = try await runtime.capture(in: scope)

    do {
      _ = try await runtime.resolve(first.identity, in: scope)
      XCTFail("Expected byte pressure to evict the oldest snapshot")
    } catch let error as UISnapshotRuntimeError {
      XCTAssertEqual(error, .snapshotExpired)
    }
    let resolvedSecond = try await runtime.resolve(second.identity, in: scope)
    XCTAssertEqual(resolvedSecond, second)
  }

  @MainActor
  func testOversizedSnapshotDoesNotEvictOrPartiallyCommit() async throws {
    let smallCapture = makeSingleNodeCapture(
      provider: "fixture.views",
      sourceID: "fixture.main",
      nodeID: "fixture-root"
    )
    let calibrationProvider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: smallCapture
    )
    let scope = UISnapshotScope(sessionID: "session-one", processGeneration: 1)
    let calibrationRuntime = try UISnapshotRuntime(providers: [calibrationProvider])
    let byteCapacity = try await calibrationRuntime.capture(in: scope).storedBytes
    var currentCapture = smallCapture
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS)
    ) {
      currentCapture
    }
    let runtime = try UISnapshotRuntime(
      providers: [provider],
      limits: UISnapshotStoreLimits(
        maximumSnapshotCount: 10,
        maximumStoredBytes: byteCapacity
      )
    )
    let first = try await runtime.capture(in: scope)
    currentCapture = makeCapture(
      provider: "fixture.views",
      sourceID: "fixture.main",
      nodes: [
        RedactedNodeCapture(
          id: "fixture-root",
          depth: 0,
          childCount: 0,
          index: RedactedNodeIndex(text: String(repeating: "x", count: byteCapacity))
        )
      ]
    )

    do {
      _ = try await runtime.capture(in: scope)
      XCTFail("Expected oversized capture to fail")
    } catch let error as UISnapshotRuntimeError {
      XCTAssertEqual(error.kind, .resourceExhausted)
    }

    let stillRetained = try await runtime.resolve(first.identity, in: scope)
    XCTAssertEqual(stillRetained, first)
    currentCapture = smallCapture
    let second = try await runtime.capture(in: scope)
    XCTAssertEqual(second.identity.generation, 2)
  }

  @MainActor
  func testConcurrentCapturesCommitInRuntimeAcceptanceOrder() async throws {
    let provider = GateFixtureProvider()
    let runtime = try UISnapshotRuntime(providers: [provider])
    let scope = UISnapshotScope(sessionID: "session-one", processGeneration: 1)

    let firstTask = Task { try await runtime.capture(in: scope) }
    await provider.waitUntilCaptureCount(1)
    let secondTask = Task { try await runtime.capture(in: scope) }
    for _ in 0..<20 {
      await Task.yield()
    }
    let capturesBeforeFirstRelease = provider.captureCount

    provider.releaseNextCapture()
    await provider.waitUntilCaptureCount(2)
    provider.releaseNextCapture()
    let first = try await firstTask.value
    let second = try await secondTask.value

    XCTAssertEqual(capturesBeforeFirstRelease, 1)
    XCTAssertEqual(first.identity.generation, 1)
    XCTAssertEqual(second.identity.generation, 2)
  }

  @MainActor
  func testCancellationBeforeCommitDiscardsLateProviderResult() async throws {
    let provider = GateFixtureProvider()
    let runtime = try UISnapshotRuntime(providers: [provider])
    let scope = UISnapshotScope(sessionID: "session-one", processGeneration: 1)

    let cancelledTask = Task { try await runtime.capture(in: scope) }
    await provider.waitUntilCaptureCount(1)
    cancelledTask.cancel()
    provider.releaseNextCapture()

    do {
      _ = try await cancelledTask.value
      XCTFail("Expected capture cancellation")
    } catch is CancellationError {
      // Expected.
    }

    let nextTask = Task { try await runtime.capture(in: scope) }
    await provider.waitUntilCaptureCount(2)
    provider.releaseNextCapture()
    let captured = try await nextTask.value
    XCTAssertEqual(captured.identity.generation, 1)
  }

  @MainActor
  func testCaptureAssignsOpaqueReferencesAndCanBeResolved() async throws {
    let provider = FixtureProvider(
      descriptor: UIProviderDescriptor(name: "fixture.views", platform: .iOS),
      capture: RedactedProviderCapture(
        sources: [
          RedactedSourceCapture(
            id: "fixture.main",
            provider: "fixture.views",
            platform: .iOS,
            representation: .native,
            nativeSchema: "fixture.views@1",
            coordinateSpace: UICoordinateSpace(unit: .point, scale: 2),
            coverage: .complete,
            nodes: [
              RedactedNodeCapture(
                id: "provider-root",
                depth: 0,
                childCount: 1,
                index: RedactedNodeIndex(typeName: "Window")
              ),
              RedactedNodeCapture(
                id: "provider-button",
                parentID: "provider-root",
                childIndex: 0,
                depth: 1,
                childCount: 0,
                index: RedactedNodeIndex(
                  identifier: "login",
                  text: "Log in",
                  typeName: "Button",
                  visible: true,
                  interactive: true
                )
              ),
            ]
          )
        ]
      )
    )
    let runtime = try UISnapshotRuntime(
      providers: [provider],
      limits: UISnapshotStoreLimits(maximumSnapshotCount: 2, maximumStoredBytes: 64 * 1024)
    )
    let scope = UISnapshotScope(sessionID: "session-one", processGeneration: 7)

    let captured = try await runtime.capture(providers: ["fixture.views"], in: scope)

    XCTAssertEqual(captured.identity.generation, 1)
    XCTAssertTrue(captured.identity.id.hasPrefix("snapshot_"))
    XCTAssertEqual(captured.sources.map(\.id), ["fixture.main"])
    XCTAssertEqual(captured.nodes.count, 2)
    XCTAssertTrue(captured.nodes.allSatisfy { $0.reference.hasPrefix("node_") })
    XCTAssertFalse(captured.nodes.contains { $0.reference.contains("provider-") })
    XCTAssertEqual(captured.nodes[1].parentReference, captured.nodes[0].reference)

    let resolved = try await runtime.resolve(captured.identity, in: scope)
    XCTAssertEqual(resolved, captured)
  }
}

@MainActor
private final class FixtureProvider: UISnapshotProvider {
  nonisolated let descriptor: UIProviderDescriptor
  private let captureBody: () async throws -> RedactedProviderCapture

  init(descriptor: UIProviderDescriptor, capture: RedactedProviderCapture) {
    self.descriptor = descriptor
    self.captureBody = { capture }
  }

  init(
    descriptor: UIProviderDescriptor,
    capture: @escaping () async throws -> RedactedProviderCapture
  ) {
    self.descriptor = descriptor
    self.captureBody = capture
  }

  func capture() async throws -> RedactedProviderCapture {
    try await captureBody()
  }
}

@MainActor
private final class RedactingFixtureProvider: UISnapshotProvider {
  nonisolated let descriptor = UIProviderDescriptor(
    name: "fixture.views",
    platform: .iOS
  )
  var rawText: String
  private let secret: String

  init(rawText: String, secret: String) {
    self.rawText = rawText
    self.secret = secret
  }

  func capture() async throws -> RedactedProviderCapture {
    let redactedText = rawText.replacingOccurrences(of: secret, with: "[REDACTED]")
    return RedactedProviderCapture(
      sources: [
        RedactedSourceCapture(
          id: "fixture.native",
          provider: descriptor.name,
          platform: descriptor.platform,
          representation: .native,
          nativeSchema: "fixture.views@1",
          coordinateSpace: UICoordinateSpace(unit: .point, scale: 2),
          coverage: .complete,
          nodes: [
            RedactedNodeCapture(
              id: "provider-root",
              depth: 0,
              childCount: 0,
              index: RedactedNodeIndex(text: redactedText),
              native: ["rawLabel": .string(redactedText)]
            )
          ]
        ),
        RedactedSourceCapture(
          id: "fixture.accessibility",
          provider: descriptor.name,
          platform: descriptor.platform,
          representation: .accessibility,
          nativeSchema: "fixture.accessibility@1",
          coordinateSpace: UICoordinateSpace(unit: .point, scale: 2),
          coverage: .complete,
          nodes: [
            RedactedNodeCapture(
              id: "provider-root",
              depth: 0,
              childCount: 0,
              index: RedactedNodeIndex(typeName: "Application")
            )
          ]
        ),
      ]
    )
  }
}

@MainActor
private final class GateFixtureProvider: UISnapshotProvider {
  nonisolated let descriptor = UIProviderDescriptor(
    name: "fixture.views",
    platform: .iOS
  )
  private(set) var captureCount = 0
  private var continuations: [CheckedContinuation<Void, Never>] = []

  func capture() async throws -> RedactedProviderCapture {
    captureCount += 1
    await withCheckedContinuation { continuation in
      continuations.append(continuation)
    }
    return makeSingleNodeCapture(
      provider: descriptor.name,
      sourceID: "fixture.main",
      nodeID: "fixture-root"
    )
  }

  func waitUntilCaptureCount(_ expectedCount: Int) async {
    while captureCount < expectedCount {
      await Task.yield()
    }
  }

  func releaseNextCapture() {
    continuations.removeFirst().resume()
  }
}

private enum FixtureFailure: Error {
  case captureFailed
}

private struct ExpectedStoredSnapshotRecord: Encodable {
  let scope: UISnapshotScope
  let identity: UISnapshotIdentity
  let sources: [StoredUISource]
  let nodes: [StoredUINode]

  init(snapshot: StoredUISnapshot) {
    self.scope = snapshot.scope
    self.identity = snapshot.identity
    self.sources = snapshot.sources
    self.nodes = snapshot.nodes
  }
}

private func makeSingleNodeCapture(
  provider: String,
  sourceID: String,
  nodeID: String
) -> RedactedProviderCapture {
  makeCapture(
    provider: provider,
    sourceID: sourceID,
    nodes: [
      RedactedNodeCapture(id: nodeID, depth: 0, childCount: 0)
    ]
  )
}

private func makeCapture(
  provider: String,
  sourceID: String,
  nodes: [RedactedNodeCapture]
) -> RedactedProviderCapture {
  RedactedProviderCapture(
    sources: [
      RedactedSourceCapture(
        id: sourceID,
        provider: provider,
        platform: .iOS,
        representation: .native,
        nativeSchema: "fixture.views@1",
        coordinateSpace: UICoordinateSpace(unit: .point, scale: 2),
        coverage: .complete,
        nodes: nodes
      )
    ]
  )
}
