#if canImport(UIKit)
  import AppPilotKit
  import AppPilotKitUIKit
  import UIKit
  import XCTest

  @MainActor
  final class UIKitSnapshotProviderTests: XCTestCase {
    func testMissingWindowsFailsAndRuntimeMapsProviderFailure() async throws {
      let provider = UIKitSnapshotProvider(windows: { [] })

      do {
        _ = try await provider.capture()
        XCTFail("Expected capture without windows to fail")
      } catch {
        XCTAssertEqual(error as? UIKitSnapshotProviderError, .noWindows)
      }

      let runtime = try UISnapshotRuntime(providers: [provider])
      do {
        _ = try await runtime.capture(
          in: UISnapshotScope(sessionID: "session-uikit", processGeneration: 1)
        )
        XCTFail("Expected runtime capture without windows to fail")
      } catch let error as UISnapshotRuntimeError {
        XCTAssertEqual(error.kind, .internalError)
      }
    }

    func testRuntimeStoresDetachedCaptureWithUniqueOpaqueReferences() async throws {
      let firstWindow = UIWindow(frame: CGRect(x: 0, y: 0, width: 320, height: 640))
      let capturedView = UIView(frame: CGRect(x: 10, y: 20, width: 80, height: 30))
      firstWindow.addSubview(capturedView)
      let secondWindow = UIWindow(frame: CGRect(x: 0, y: 0, width: 320, height: 640))
      secondWindow.addSubview(UILabel(frame: CGRect(x: 1, y: 2, width: 3, height: 4)))
      let provider = UIKitSnapshotProvider(windows: { [firstWindow, secondWindow] })
      let localCapture = try await provider.capture()
      let localIDs = Set(localCapture.sources.flatMap { $0.nodes.map(\.id) })
      let runtime = try UISnapshotRuntime(providers: [provider])
      let scope = UISnapshotScope(sessionID: "session-uikit", processGeneration: 1)

      let stored = try await runtime.capture(in: scope)
      capturedView.frame = CGRect(x: 200, y: 300, width: 1, height: 1)
      capturedView.removeFromSuperview()
      let resolved = try await runtime.resolve(stored.identity, in: scope)

      XCTAssertEqual(resolved, stored)
      XCTAssertEqual(stored.sources.count, 2)
      XCTAssertEqual(Set(stored.nodes.map(\.reference)).count, stored.nodes.count)
      XCTAssertTrue(stored.nodes.allSatisfy { $0.reference.hasPrefix("node_") })
      let storedJSON = try XCTUnwrap(
        String(data: JSONEncoder().encode(stored), encoding: .utf8)
      )
      XCTAssertTrue(localIDs.allSatisfy { !storedJSON.contains($0) })
      XCTAssertEqual(
        stored.nodes[1].index?.frame,
        UIRect(x: 10, y: 20, width: 80, height: 30)
      )
    }

    func testDisclosureModesOmitTextAndOnlyAllowBoundedIdentifiers() async throws {
      let secret = "known-secret-value"
      let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 320, height: 640))
      let label = UILabel(frame: CGRect(x: 0, y: 0, width: 100, height: 20))
      label.accessibilityIdentifier = secret
      label.accessibilityLabel = secret
      label.accessibilityValue = secret
      label.accessibilityHint = secret
      label.text = secret
      label.attributedText = NSAttributedString(string: secret)
      let textField = UITextField(frame: CGRect(x: 0, y: 30, width: 100, height: 20))
      textField.accessibilityIdentifier = secret
      textField.accessibilityLabel = secret
      textField.accessibilityValue = secret
      textField.accessibilityHint = secret
      textField.text = secret
      textField.attributedText = NSAttributedString(string: secret)
      let button = UIButton(frame: CGRect(x: 0, y: 60, width: 100, height: 20))
      button.accessibilityIdentifier = secret
      button.accessibilityLabel = secret
      button.accessibilityValue = secret
      button.accessibilityHint = secret
      button.setTitle(secret, for: .normal)
      button.setAttributedTitle(NSAttributedString(string: secret), for: .normal)
      window.addSubview(label)
      window.addSubview(textField)
      window.addSubview(button)

      let structural = UIKitSnapshotProvider(windows: { [window] })
      let identifiers = UIKitSnapshotProvider(
        disclosure: .identifiers,
        windows: { [window] }
      )

      let structuralCapture = try await structural.capture()
      let structuralNode = structuralCapture.sources[0].nodes[1]
      let identifierNode = try await identifiers.capture().sources[0].nodes[1]

      XCTAssertNil(structuralNode.index?.identifier)
      XCTAssertNil(structuralNode.index?.text)
      XCTAssertEqual(identifierNode.index?.identifier, secret)
      XCTAssertNil(identifierNode.index?.text)
      XCTAssertFalse(identifierNode.native?.values.contains(.string(secret)) ?? true)
      let structuralJSON = try XCTUnwrap(
        String(data: JSONEncoder().encode(structuralCapture), encoding: .utf8)
      )
      XCTAssertFalse(structuralJSON.contains(secret))

      label.accessibilityIdentifier = String(repeating: "x", count: 513)
      let oversizedNode = try await identifiers.capture().sources[0].nodes[1]
      XCTAssertNil(oversizedNode.index?.identifier)
    }

    func testCaptureClassifiesEffectiveVisibilityAndInteraction() async throws {
      let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 320, height: 640))
      window.isHidden = false

      let hiddenContainer = UIView(frame: CGRect(x: 0, y: 0, width: 100, height: 100))
      hiddenContainer.isHidden = true
      hiddenContainer.addSubview(
        UIButton(frame: CGRect(x: 0, y: 0, width: 40, height: 40))
      )
      let visibleButton = UIButton(frame: CGRect(x: 10, y: 110, width: 40, height: 40))
      let gestureView = UIView(frame: CGRect(x: 10, y: 160, width: 40, height: 40))
      gestureView.addGestureRecognizer(UITapGestureRecognizer())
      let disabledButton = UIButton(frame: CGRect(x: 10, y: 210, width: 40, height: 40))
      disabledButton.isEnabled = false
      let transparentContainer = UIView(frame: CGRect(x: 10, y: 260, width: 40, height: 40))
      transparentContainer.alpha = 0.01
      let offscreenView = UIView(frame: CGRect(x: 400, y: 0, width: 40, height: 40))
      window.addSubview(hiddenContainer)
      window.addSubview(visibleButton)
      window.addSubview(gestureView)
      window.addSubview(disabledButton)
      window.addSubview(transparentContainer)
      window.addSubview(offscreenView)
      let provider = UIKitSnapshotProvider(windows: { [window] })

      let nodes = try await provider.capture().sources[0].nodes

      XCTAssertEqual(
        nodes.map(\.index?.visible),
        [true, false, false, true, true, true, false, false]
      )
      XCTAssertEqual(
        nodes.map(\.index?.interactive),
        [false, false, false, true, true, false, false, false]
      )
    }

    func testInteractionRequiresEnabledAncestorChain() async throws {
      let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 320, height: 640))
      window.isHidden = false
      let disabledContainer = UIView(frame: CGRect(x: 0, y: 0, width: 100, height: 100))
      disabledContainer.isUserInteractionEnabled = false
      disabledContainer.addSubview(
        UIButton(frame: CGRect(x: 0, y: 0, width: 40, height: 40))
      )
      window.addSubview(disabledContainer)
      let provider = UIKitSnapshotProvider(windows: { [window] })

      let capturedButton = try await provider.capture().sources[0].nodes[2]

      XCTAssertEqual(capturedButton.index?.visible, true)
      XCTAssertEqual(capturedButton.index?.interactive, false)
    }

    func testCaptureEmitsScreenPointGeometryAndWhitelistedNativeFields() async throws {
      let window = UIWindow(frame: CGRect(x: 30, y: 40, width: 320, height: 640))
      window.isHidden = false
      window.windowLevel = .alert
      let view = UIView(frame: CGRect(x: 10, y: 20, width: 200, height: 100))
      view.alpha = 0.75
      view.isOpaque = true
      view.clipsToBounds = true
      view.isUserInteractionEnabled = false
      view.tag = 42
      let child = UIView(frame: CGRect(x: 5, y: 6, width: 80, height: 30))
      view.addSubview(child)
      window.addSubview(view)
      let provider = UIKitSnapshotProvider(windows: { [window] })

      let capture = try await provider.capture()

      let root = capture.sources[0].nodes[0]
      let capturedView = capture.sources[0].nodes[1]
      let capturedChild = capture.sources[0].nodes[2]
      XCTAssertEqual(
        capturedView.index?.frame,
        UIRect(x: 40, y: 60, width: 200, height: 100)
      )
      XCTAssertEqual(
        capturedChild.index?.frame,
        UIRect(x: 45, y: 66, width: 80, height: 30)
      )
      XCTAssertEqual(
        capturedView.native,
        [
          "alpha": .number(0.75),
          "clipsToBounds": .bool(true),
          "hidden": .bool(false),
          "opaque": .bool(true),
          "tag": .integer(42),
          "userInteractionEnabled": .bool(false),
        ]
      )
      XCTAssertEqual(root.native?["keyWindow"], .bool(false))
      XCTAssertEqual(root.native?["windowLevel"], .number(UIWindow.Level.alert.rawValue))
      XCTAssertEqual(
        root.native.map { Set($0.keys) },
        [
          "alpha", "clipsToBounds", "hidden", "keyWindow", "opaque", "tag",
          "userInteractionEnabled", "windowLevel",
        ])
    }

    func testCaptureRejectsNonFiniteNativeNumbers() async throws {
      let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 320, height: 640))
      window.addSubview(
        NonFiniteAlphaView(frame: CGRect(x: 0, y: 0, width: 40, height: 40))
      )
      let provider = UIKitSnapshotProvider(windows: { [window] })

      do {
        _ = try await provider.capture()
        XCTFail("Expected capture with a non-finite native value to fail")
      } catch {
        XCTAssertEqual(error as? UIKitSnapshotProviderError, .invalidNativeValue)
      }
    }

    func testCapturePreservesWindowOrderAndDeduplicatesIdentity() async throws {
      let first = UIWindow(frame: CGRect(x: 0, y: 0, width: 100, height: 100))
      let second = UIWindow(frame: CGRect(x: 0, y: 0, width: 200, height: 200))
      first.addSubview(UIView())
      second.addSubview(UIButton())
      let provider = UIKitSnapshotProvider(windows: { [first, first, second] })

      let capture = try await provider.capture()

      XCTAssertEqual(capture.sources.map(\.id), ["uikit.window.0", "uikit.window.1"])
      XCTAssertEqual(
        capture.sources.map { $0.nodes[1].index?.className },
        ["UIView", "UIButton"]
      )
    }

    func testCapturePreservesOneWindowViewTreeAsNativeDepthFirstSource() async throws {
      let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 320, height: 640))
      window.isHidden = false
      let container = UIView(frame: CGRect(x: 10, y: 20, width: 200, height: 100))
      let button = UIButton(frame: CGRect(x: 5, y: 6, width: 80, height: 30))
      let label = UILabel(frame: CGRect(x: 100, y: 10, width: 60, height: 20))
      container.addSubview(button)
      window.addSubview(container)
      window.addSubview(label)
      let provider = UIKitSnapshotProvider(windows: { [window] })

      let capture = try await provider.capture()

      XCTAssertEqual(
        provider.descriptor,
        UIProviderDescriptor(name: "uikit.views", platform: .iOS)
      )
      let source = try XCTUnwrap(capture.sources.first)
      XCTAssertEqual(capture.sources.count, 1)
      XCTAssertEqual(source.id, "uikit.window.0")
      XCTAssertEqual(source.provider, "uikit.views")
      XCTAssertEqual(source.platform, .iOS)
      XCTAssertEqual(source.representation, .native)
      XCTAssertEqual(source.nativeSchema, "uikit.view@1")
      XCTAssertEqual(source.coordinateSpace.unit, .point)
      XCTAssertEqual(source.coordinateSpace.scale, window.screen.scale)
      XCTAssertEqual(source.coverage, .complete)
      XCTAssertNil(source.limitations)
      XCTAssertEqual(
        source.nodes.compactMap(\.index?.className),
        ["UIWindow", "UIView", "UIButton", "UILabel"]
      )

      let root = source.nodes[0]
      let capturedContainer = source.nodes[1]
      let capturedButton = source.nodes[2]
      let capturedLabel = source.nodes[3]
      XCTAssertNil(root.parentID)
      XCTAssertNil(root.childIndex)
      XCTAssertEqual(root.depth, 0)
      XCTAssertEqual(root.childCount, 2)
      XCTAssertEqual(capturedContainer.parentID, root.id)
      XCTAssertEqual(capturedContainer.childIndex, 0)
      XCTAssertEqual(capturedContainer.depth, 1)
      XCTAssertEqual(capturedContainer.childCount, 1)
      XCTAssertEqual(capturedButton.parentID, capturedContainer.id)
      XCTAssertEqual(capturedButton.childIndex, 0)
      XCTAssertEqual(capturedButton.depth, 2)
      XCTAssertEqual(capturedButton.childCount, 0)
      XCTAssertEqual(capturedLabel.parentID, root.id)
      XCTAssertEqual(capturedLabel.childIndex, 1)
      XCTAssertEqual(capturedLabel.depth, 1)
      XCTAssertEqual(capturedLabel.childCount, 0)
    }
  }

  @MainActor
  private final class NonFiniteAlphaView: UIView {
    override var alpha: CGFloat {
      get { .nan }
      set {}
    }
  }
#endif
