import Foundation

#if canImport(UIKit)
import UIKit

final class EligibilityMonitor: @unchecked Sendable {
  private let callback: @Sendable () -> Void
  private var tokens: [NSObjectProtocol] = []

  @MainActor
  static func install(callback: @escaping @Sendable () -> Void) -> EligibilityMonitor? {
    guard UIApplication.shared.applicationState == .active else { return nil }
    let monitor = EligibilityMonitor(callback: callback)
    guard UIApplication.shared.applicationState == .active else { return nil }
    return monitor
  }

  private init(callback: @escaping @Sendable () -> Void) {
    self.callback = callback
    let center = NotificationCenter.default
    tokens = [
      center.addObserver(
        forName: UIApplication.willResignActiveNotification,
        object: nil,
        queue: nil
      ) { [callback] _ in callback() },
      center.addObserver(
        forName: UIApplication.didEnterBackgroundNotification,
        object: nil,
        queue: nil
      ) { [callback] _ in callback() },
    ]
  }

  deinit {
    for token in tokens { NotificationCenter.default.removeObserver(token) }
  }
}
#else
final class EligibilityMonitor: @unchecked Sendable {
  @MainActor
  static func install(callback: @escaping @Sendable () -> Void) -> EligibilityMonitor? {
    EligibilityMonitor(callback: callback)
  }

  private init(callback: @escaping @Sendable () -> Void) {
    _ = callback
  }
}
#endif
