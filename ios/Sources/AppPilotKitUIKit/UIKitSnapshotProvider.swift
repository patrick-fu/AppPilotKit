#if canImport(UIKit)
  import AppPilotKit
  import UIKit

  public enum UIKitSnapshotDisclosure: Sendable {
    case structural
    case identifiers
  }

  public enum UIKitSnapshotProviderError: Error, Equatable, Sendable {
    case invalidGeometry
    case invalidNativeValue
    case noWindows
  }

  @MainActor
  public final class UIKitSnapshotProvider: UISnapshotProvider {
    private struct PendingView {
      let view: UIView
      let parentID: String?
      let childIndex: Int?
      let depth: Int
    }

    nonisolated public let descriptor = UIProviderDescriptor(
      name: "uikit.views",
      platform: .iOS
    )

    private let disclosure: UIKitSnapshotDisclosure
    private let windowSupplier: @MainActor () -> [UIWindow]

    public convenience init(disclosure: UIKitSnapshotDisclosure = .structural) {
      self.init(disclosure: disclosure, windows: Self.applicationWindows)
    }

    public init(
      disclosure: UIKitSnapshotDisclosure = .structural,
      windows: @escaping @MainActor () -> [UIWindow]
    ) {
      self.disclosure = disclosure
      self.windowSupplier = windows
    }

    public func capture() async throws -> RedactedProviderCapture {
      var seenWindows = Set<ObjectIdentifier>()
      let windows = windowSupplier().filter { window in
        seenWindows.insert(ObjectIdentifier(window)).inserted
      }
      guard !windows.isEmpty else {
        throw UIKitSnapshotProviderError.noWindows
      }
      var sources: [RedactedSourceCapture] = []
      for (offset, window) in windows.enumerated() {
        sources.append(try capture(window: window, sourceOffset: offset))
      }
      return RedactedProviderCapture(sources: sources)
    }

    private static func applicationWindows() -> [UIWindow] {
      UIApplication.shared.connectedScenes
        .compactMap { $0 as? UIWindowScene }
        .sorted {
          $0.session.persistentIdentifier < $1.session.persistentIdentifier
        }
        .flatMap(\.windows)
    }

    private func capture(
      window: UIWindow,
      sourceOffset: Int
    ) throws -> RedactedSourceCapture {
      var nodes: [RedactedNodeCapture] = []
      var pending = [
        PendingView(view: window, parentID: nil, childIndex: nil, depth: 0)
      ]

      while let current = pending.popLast() {
        let localID = "view.\(nodes.count)"
        let visible = isEffectivelyVisible(current.view, in: window)
        nodes.append(
          RedactedNodeCapture(
            id: localID,
            parentID: current.parentID,
            childIndex: current.childIndex,
            depth: current.depth,
            childCount: current.view.subviews.count,
            index: RedactedNodeIndex(
              identifier: disclosedIdentifier(of: current.view),
              className: NSStringFromClass(type(of: current.view)),
              frame: try screenFrame(of: current.view, in: window),
              visible: visible,
              interactive: isInteractive(current.view, in: window, visible: visible)
            ),
            native: try nativeFields(of: current.view)
          )
        )
        for (childIndex, child) in current.view.subviews.enumerated().reversed() {
          pending.append(
            PendingView(
              view: child,
              parentID: localID,
              childIndex: childIndex,
              depth: current.depth + 1
            )
          )
        }
      }

      return RedactedSourceCapture(
        id: "uikit.window.\(sourceOffset)",
        provider: descriptor.name,
        platform: descriptor.platform,
        representation: .native,
        nativeSchema: "uikit.view@1",
        coordinateSpace: UICoordinateSpace(unit: .point, scale: window.screen.scale),
        coverage: .complete,
        nodes: nodes
      )
    }

    private func screenFrame(of view: UIView, in window: UIWindow) throws -> UIRect {
      let windowFrame = view.convert(view.bounds, to: window)
      let screenFrame = window.convert(windowFrame, to: nil)
      let values = [
        screenFrame.origin.x,
        screenFrame.origin.y,
        screenFrame.size.width,
        screenFrame.size.height,
      ]
      guard values.allSatisfy(\.isFinite),
        screenFrame.size.width >= 0,
        screenFrame.size.height >= 0
      else {
        throw UIKitSnapshotProviderError.invalidGeometry
      }
      return UIRect(
        x: screenFrame.origin.x,
        y: screenFrame.origin.y,
        width: screenFrame.size.width,
        height: screenFrame.size.height
      )
    }

    private func nativeFields(of view: UIView) throws -> [String: JSONValue] {
      let alpha = Double(view.alpha)
      guard alpha.isFinite else {
        throw UIKitSnapshotProviderError.invalidNativeValue
      }
      var fields: [String: JSONValue] = [
        "alpha": .number(alpha),
        "clipsToBounds": .bool(view.clipsToBounds),
        "hidden": .bool(view.isHidden),
        "opaque": .bool(view.isOpaque),
        "tag": .integer(Int64(view.tag)),
        "userInteractionEnabled": .bool(view.isUserInteractionEnabled),
      ]
      if let window = view as? UIWindow {
        let windowLevel = Double(window.windowLevel.rawValue)
        guard windowLevel.isFinite else {
          throw UIKitSnapshotProviderError.invalidNativeValue
        }
        fields["keyWindow"] = .bool(window.isKeyWindow)
        fields["windowLevel"] = .number(windowLevel)
      }
      return fields
    }

    private func disclosedIdentifier(of view: UIView) -> String? {
      guard disclosure == .identifiers,
        let identifier = view.accessibilityIdentifier,
        !identifier.isEmpty,
        identifier.unicodeScalars.count <= 512
      else {
        return nil
      }
      return identifier
    }

    private func isEffectivelyVisible(_ view: UIView, in window: UIWindow) -> Bool {
      guard view === window || view.window === window, !view.bounds.isEmpty else {
        return false
      }
      var cumulativeAlpha: CGFloat = 1
      var current: UIView? = view
      var reachedWindow = false
      while let candidate = current {
        guard !candidate.isHidden else {
          return false
        }
        cumulativeAlpha *= candidate.alpha
        guard cumulativeAlpha > 0.01 else {
          return false
        }
        if candidate === window {
          reachedWindow = true
          break
        }
        current = candidate.superview
      }
      guard reachedWindow else {
        return false
      }
      let frameInWindow = view.convert(view.bounds, to: window)
      return !frameInWindow.isNull && frameInWindow.intersects(window.bounds)
    }

    private func isInteractive(_ view: UIView, in window: UIWindow, visible: Bool) -> Bool {
      guard visible, !(view is UIWindow) else {
        return false
      }
      var current: UIView? = view
      while let candidate = current {
        guard candidate.isUserInteractionEnabled else {
          return false
        }
        if candidate === window {
          break
        }
        current = candidate.superview
      }
      if let control = view as? UIControl, control.isEnabled {
        return true
      }
      return view.gestureRecognizers?.contains(where: \.isEnabled) == true
    }
  }
#endif
