# Cross-platform image-evidence mechanisms

- Status: research recommendation, not an accepted implementation or protocol decision
- Date: 2026-08-08
- Issue: [#25 — Research cross-platform screenshot and image-evidence mechanisms](https://github.com/patrick-fu/AppPilotKit/issues/25)
- Scope: iOS and Android image-evidence acquisition on Simulator, Emulator, and physical devices; no production screenshot or Artifact code
- Host inspected: macOS 26.6, Xcode 26.2, `devicectl` 506.6, `simctl` 506.6, `adb` 36.0.2

## Recommendation

Use a **split acquisition model** with one primary in-process SDK path per
platform and tightly scoped display-level host fallbacks. The SDK owns
the trusted app-surface capture because only it can guarantee window
correlation, point/pixel-accurate crop geometry, fail-closed sensitive-content
handling, and Target Ephemeral Data. The host CLI owns Artifact persistence and
derived crop/annotation generation, preserving the accepted Artifact descriptor.

### Acquisition ownership

| Target kind | Primary capture | Host fallback | Host handling |
| --- | --- | --- | --- |
| iOS Simulator | SDK `drawHierarchy` in-process | `simctl io <device> screenshot` | persist/derive only SDK-masked bytes; discard fallback bytes after diagnostic use |
| iOS physical device | SDK `drawHierarchy` in-process | **none** (`devicectl` has no screenshot command in the inspected Xcode) | persist/derive only SDK-masked bytes |
| Android Emulator | SDK `PixelCopy.request(Window,…)` in-process | `adb exec-out screencap -p` | persist/derive only SDK-masked bytes; discard fallback bytes after diagnostic use |
| Android physical device | SDK `PixelCopy.request(Window,…)` in-process | `adb exec-out screencap -p` | persist/derive only SDK-masked bytes; discard fallback bytes after diagnostic use |

The in-process SDK path is authoritative for all four Target kinds because it
correlates the captured bitmap to a specific `UIWindow` / Android `Window` and
to the coordinate space already established by the snapshot contract (iOS
points with display scale; Android physical pixels, scale 1). [ADR 0002 §
Node representation](../adr/0002-ui-snapshot-and-inspection.md); [CONTEXT.md
§ Image Evidence](../../CONTEXT.md)

Host fallbacks (`simctl io screenshot`, `adb exec-out screencap -p`) capture a
full display framebuffer with no app-window correlation and no app-controlled
sensitive masking. `simctl` is Simulator-only; ADB may reach either an Emulator
or an explicitly connected and authorized physical Android device. They require
the already-authorized developer transport defined by the transport topology;
they add no listener or credential path. They are useful for SDK-unavailable
diagnostics but must not be the primary evidence path for an opted-in App.
Because they lack the SDK Screenshot Mask and the display-to-window transform,
their bytes are never Image Evidence, never persisted as Artifacts, and never
used for node crops or annotations. A future decision could admit a fallback
only after it defines and verifies both protections.

### Decision boundary

This research recommends the acquisition-ownership model, the coordinate-space
and crop-geometry contract, the fail-closed sensitive-content rules, and the
Artifact metadata mapping. It explicitly **excludes** production screenshot
code, Artifact persistence implementation, protocol method definitions,
annotation rendering, and SDK integration — those are future implementation
slices that follow this recommendation.

## Evidence labels

- **Documented** — stated by an owning Apple/Google public API, official tool
  help, or AOSP source.
- **Observed** — completed probe on the inspected host/device.
- **Inference / recommendation** — derived here from documented facts and
  repository contracts.

## Mechanism comparison

### iOS mechanisms

| Dimension | SDK `drawHierarchy` + `UIGraphicsImageRenderer` | Host `simctl io screenshot` | Host `devicectl` |
| --- | --- | --- | --- |
| Acquisition ownership | in-process SDK, app surface | host CLI, display framebuffer | **not available** |
| OS availability | iOS 15–26 (drawHierarchy since iOS 7) | Xcode `simctl`, Simulator only | physical device, no screenshot subcommand |
| Orientation/scale | honors window `screen.scale`; renderer format `.scale` controls output | full display at native pixel density | n/a |
| Color | `preferredRange` / `prefersExtendedRange` for wide gamut | display framebuffer | n/a |
| Window correlation | targets one specific `UIWindow` or subtree [ADR 0004] | full display; no window selection | n/a |
| Secure content | exact `isSecureTextEntry` pixels in this API are **not documented**; app Screenshot Mask is mandatory before Host handoff | full framebuffer; **no app masking** | n/a |
| Crop geometry | `UIGraphicsImageRenderer` size = point rect × scale; crop = renderer region or Host-side Bitmap extract | full screen only; crop on Host | n/a |
| Cancellation | synchronous UI capture with no native cancellation; a future contract must check before capture and before Host handoff | process termination | n/a |
| Failure | `drawHierarchy == false` means an incomplete snapshot; discard it rather than publish evidence | nonzero exit, no PNG | n/a |

### Android mechanisms

| Dimension | SDK `PixelCopy.request(Window,…)` | SDK `View.drawToBitmap` | Host `adb exec-out screencap -p` | System `MediaProjection` |
| --- | --- | --- | --- | --- |
| Acquisition ownership | in-process SDK, app window | in-process SDK, single View | host CLI, display framebuffer | system, full screen, user consent |
| OS availability | API 24+ (core); `Window` overload API 26+, `Surface`/`SurfaceView` overloads API 24+ [Documented] | AndroidX extension; underlying `View.draw` is longstanding | ADB/device-image dependent | API 21+ [Documented] |
| Orientation/scale | physical pixels, scale 1 [ADR 0002] | View coordinates → Bitmap | physical pixels | physical pixels |
| Color | caller-supplied `Bitmap`; `ARGB_8888` or `RGBA_F16` wide gamut | caller-supplied `Bitmap` | framebuffer native format | VirtualDisplay Surface |
| Window correlation | targets one `Window` or `Surface` [Documented] | targets one `View`; **misses SurfaceView/TextureView/GL** | full display; no window selection | full display incl. system UI |
| Secure content | `FLAG_SECURE` prevents screenshots; exact `PixelCopy` result remains a probe | not a window-capture security control; misses Surface content | protected content must not appear; exact pixels are device/compositor dependent | protected content must not appear; exact pixels are device/compositor dependent |
| Crop geometry | `Rect srcRect` targets a `Window` source region; AppPilotKit's physical-pixel mapping is a contract inference to probe | View bounds → Bitmap.createBitmap | full screen; crop via `Bitmap.createBitmap` on Host | VirtualDisplay region |
| Cancellation | async callback; no native cancel API; a future contract may abandon the result and discard later bytes | sync call | process termination | VirtualDisplay.stop |
| Failure | `ERROR_UNKNOWN/1`, `ERROR_TIMEOUT/2`, `ERROR_SOURCE_INVALID/4`, `ERROR_DESTINATION_INVALID`, `ERROR_SOURCE_NO_DATA` [Documented] | none (silent blank for uncaptured surfaces) | nonzero exit | consent denied → no capture |

## Verified claims

### iOS

| Claim | Evidence | Source |
| --- | --- | --- |
| `drawHierarchy(in:afterScreenUpdates:)` "Renders a snapshot of the complete view hierarchy as visible onscreen into the current context." Available iOS 7.0+. Returns `Bool`: true if snapshot is complete, false if any view is missing image data. | Documented | [Apple: drawHierarchy](https://developer.apple.com/documentation/uikit/uiview/drawhierarchy(in:afterscreenupdates:)) |
| `UIGraphicsImageRenderer` is "A graphics renderer for creating Core Graphics-backed images." | Documented | [Apple: UIGraphicsImageRenderer](https://developer.apple.com/documentation/uikit/uigraphicsimagerenderer) |
| `UIGraphicsImageRendererFormat` is "A set of drawing attributes that represents the configuration of an image renderer context." | Documented | [Apple: UIGraphicsImageRendererFormat](https://developer.apple.com/documentation/uikit/uigraphicsimagerendererformat) |
| Renderer format `.scale` = "The display scale determines the number of pixels per point." iOS 10.0+. Default equals the main screen scale. | Documented | [Apple: scale](https://developer.apple.com/documentation/uikit/uigraphicsimagerendererformat/scale) |
| Renderer format `.opaque` = "A Boolean value that indicates whether the underlying Core Graphics context has an alpha channel." | Documented | [Apple: opaque](https://developer.apple.com/documentation/uikit/uigraphicsimagerendererformat/opaque) |
| Renderer format `.preferredRange` = "The preferred color range of the image renderer context." Available iOS 12.0+; "affects the pixel format." | Documented | [Apple: preferredRange](https://developer.apple.com/documentation/uikit/uigraphicsimagerendererformat/preferredrange) |
| Renderer format `.prefersExtendedRange` = "A Boolean value that specifies whether the bitmap context uses extended color." iOS 10.0+. | Documented | [Apple: prefersExtendedRange](https://developer.apple.com/documentation/uikit/uigraphicsimagerendererformat/prefersextendedrange) |
| Renderer format `.supportsHighDynamicRange` — iOS 17.0+. HDR support; not available on iOS 15–16. | Documented | [Apple: supportsHighDynamicRange](https://developer.apple.com/documentation/uikit/uigraphicsimagerendererformat/supportshighdynamicrange) |
| `UIScreen.scale` = "The natural scale factor associated with the screen." | Documented | [Apple: UIScreen.scale](https://developer.apple.com/documentation/uikit/uiscreen/scale) |
| `UIScreen.nativeScale` and `UIScreen.nativeBounds` exist for raw display coordinates. `UIScreen.bounds` is "measured in points" and changes with orientation; `UIScreen.nativeBounds` is "measured in pixels" and fixed in portrait-up. | Documented | [Apple: UIScreen](https://developer.apple.com/documentation/uikit/uiscreen) |
| `UIWindow.screen` = "The screen to display the window on." "A window is always displayed on only one screen." `UIWindowScene.screen` iOS 13.0+. | Documented | [Apple: UIWindow.screen](https://developer.apple.com/documentation/uikit/uiwindow/screen); [Apple: UIWindowScene.screen](https://developer.apple.com/documentation/uikit/uiwindowscene/screen) |
| `UIScreen.isCaptured` = "A Boolean value that indicates whether the system is actively cloning the screen to another destination." | Documented | [Apple: isCaptured](https://developer.apple.com/documentation/uikit/uiscreen/iscaptured) |
| `UIScreen.capturedDidChangeNotification` = "A notification that posts when the capture status of the screen changes." | Documented | [Apple: capturedDidChangeNotification](https://developer.apple.com/documentation/uikit/uiscreen/captureddidchangenotification) |
| `simctl io screenshot` "Saves a screenshot as a PNG" to a file or stdout; `--type` supports png/tiff/bmp/gif/jpeg; `--display` selects internal/external. | Observed | `xcrun simctl io --help` on `simctl` 506.6 |
| `devicectl` device subcommands are: copy, info, install, notification, orientation, process, reboot, sysdiagnose, uninstall — no screenshot or screen-capture command. | Observed | `xcrun devicectl device --help` on `devicectl` 506.6 |
| `devicectl` states "JSON output to a user-provided file on disk is the ONLY supported interface for scripts/programs to consume command output." | Documented | `xcrun devicectl device --help` on `devicectl` 506.6 |
| `CALayer.render(in:)` "Renders the layer and its sublayers into the specified context." Available iOS 2.0+. Documented macOS 10.5 limitations (AVPlayerLayer, 3D transforms) are macOS-only, not iOS. | Documented | [Apple: CALayer.render](https://developer.apple.com/documentation/quartzcore/calayer/render(in:)) |
| `isSecureTextEntry` = "A Boolean value that indicates whether a text object disables copying, and in some cases, prevents recording/broadcasting and also hides the text." Available on all iOS versions (introducedAt not specified). | Documented | [Apple: UITextInputTraits.isSecureTextEntry](https://developer.apple.com/documentation/uikit/UITextInputTraits/isSecureTextEntry) |
| `UITraitCollection.displayScale` = "The display scale of the trait collection." Available iOS 8.0+. This is the trait-level display scale; `UIScreen.scale` is the screen-level natural scale factor. | Documented | [Apple: UITraitCollection.displayScale](https://developer.apple.com/documentation/uikit/uitraitcollection/displayscale) |
| `isSecureTextEntry` may visually hide text, but Apple does not document exact `drawHierarchy` pixels. | Inference | Derived from `isSecureTextEntry` ("hides the text") + UIKit rendering model. **Needs probe** to confirm exact rendering across iOS 15–26; it is not a Screenshot Mask. |

### Android

| Claim | Evidence | Source |
| --- | --- | --- |
| `PixelCopy` provides methods to copy pixels from a `Surface`, `SurfaceView`, or `Window` into a `Bitmap`. `request(Window, Bitmap, OnPixelCopyFinishedListener, Handler)` and `request(Window, Rect, Bitmap, …)` overloads exist. | Documented | [Google: PixelCopy](https://developer.android.com/reference/android/view/PixelCopy) |
| `PixelCopy` result constants: `SUCCESS` = 0 ("The pixel copy request succeeded"); `ERROR_UNKNOWN` = 1; `ERROR_TIMEOUT` = 2 ("A timeout occurred while trying to acquire a buffer from the source to copy from."); `ERROR_SOURCE_NO_DATA` = 3; `ERROR_SOURCE_INVALID` = 4 ("It is not possible to copy from the source. This can happen if the source is hardware-protected or destroyed."); `ERROR_DESTINATION_INVALID` = 5. | Documented | [Google: PixelCopy constants](https://developer.android.com/reference/android/view/PixelCopy) |
| `PixelCopy` added in API level 24; the `PixelCopy.Request` builder overloads added in API level 26 and 34. | Documented | [Google: PixelCopy](https://developer.android.com/reference/android/view/PixelCopy) (data-version-added attributes) |
| `WindowManager.LayoutParams.FLAG_SECURE` = "Window flag: treat the content of the window as secure, preventing it from appearing in screenshots or from being viewed on non-secure displays." | Documented | [Google: FLAG_SECURE](https://developer.android.com/reference/android/view/WindowManager.LayoutParams#FLAG_SECURE) |
| `View.drawToBitmap(Bitmap)` is an AndroidX Core Kotlin extension (not a framework `View` method): "Return a Bitmap representation of this View." "This does not take into account any transformations such as scale or translation." Uses software rendering pipeline. | Documented | [Google: AndroidX drawToBitmap](https://developer.android.com/reference/kotlin/androidx/core/view/package-summary#drawToBitmap(android.view.View,android.graphics.Bitmap.Config)) |
| `View.drawToBitmap` does not capture content rendered into `SurfaceView`, `TextureView`, or GL surfaces because those render to a separate hardware buffer. `SurfaceView` "punches a hole in its window"; `TextureView` "does not create a separate window" but "When rendered in software, TextureView will draw nothing." | Documented+Inference | [Google: SurfaceView](https://developer.android.com/reference/android/view/SurfaceView); [Google: TextureView](https://developer.android.com/reference/android/view/TextureView) |
| Android drawing cache is deprecated API 28: "For screenshots of the UI for feedback reports or unit testing the `PixelCopy` API is recommended." | Documented | [Google: View drawing cache](https://developer.android.com/reference/android/view/View#DRAWING_CACHE_QUALITY_AUTO) |
| `MediaProjection` = "A screen capture session can be started through `MediaProjectionManager.createScreenCaptureIntent`. This grants the ability to capture screen contents, but not system audio." API 21+. "Your app must request user consent before each media projection session." Android 14+ app sharing "excludes the status bar, navigation bar, notifications, and other system UI elements." | Documented | [Google: MediaProjection](https://developer.android.com/reference/android/media/projection/MediaProjection); [Google: Media Projection guide](https://developer.android.com/media/grow/media-projection) |
| `screencap` usage: `screencap [-ahp] [-d display-id] [FILENAME]`; `-p` outputs PNG; if no FILENAME, results printed to stdout; captures a specific display via `-d`. | Documented | [AOSP: screencap.cpp](https://android.googlesource.com/platform/frameworks/base/+/refs/heads/main/cmds/screencap/screencap.cpp) (usage function) |
| ADB `framebuffer:` service "is used to send snapshots of the framebuffer to a client." | Documented | [AOSP: ADB services — framebuffer](https://android.googlesource.com/platform/packages/modules/adb/+/HEAD/docs/dev/services.md) |
| `FLAG_SECURE` windows appear blank in screenshots: "When the screenshot is taken the resulting image is blank." Applies to `screencap`, `MediaProjection`, and system screenshots. | Documented+Inference | [Google: FLAG_SECURE](https://developer.android.com/reference/android/view/WindowManager.LayoutParams#FLAG_SECURE); [Google: Android Security — Activities](https://developer.android.com/security/fraud-prevention/activities) |
| `PixelCopy` on a `FLAG_SECURE` window returns `ERROR_SOURCE_INVALID` ("hardware-protected") rather than a blank bitmap. | Inference | `ERROR_SOURCE_INVALID` doc says "This can happen if the source is hardware-protected." **Needs probe.** |

## Coordinate-space and crop-geometry contract

The crop-geometry model must align with ADR 0002's coordinate spaces and
applies only to an SDK-captured full-window original, never a display-level host
fallback. Every future screenshot result must bind its source ID and capture
window to a `source-to-bitmap` mapping recorded at capture time: pixel size,
orientation, source origin, insets, scale, clipping, and the explicit rounding
rule. The Host must reject a node crop when that mapping is absent or invalid.

- **iOS**: Each source declares screen-space points with a display scale (the
  source window's `UIScreen.scale`). A node `frame` is in points. A crop rect
  first maps into the captured window's local coordinate space. For an
  axis-aligned capture, `localPointRect = nodeFrame - sourceWindowScreenOrigin`
  and `cropPixelRect = localPointRect × scale`, followed by the future
  contract's explicit rounding and clipping rule. A transformed or rotated
  window uses the recorded source-to-bitmap mapping instead.
  The SDK produces the original full-window bitmap at `scale` pixels per point
  via `UIGraphicsImageRenderer`; crops are derived on the Host only after it
  applies that mapping to the snapshot node's frame. [ADR 0002 § Node
  representation](../adr/0002-ui-snapshot-and-inspection.md); [ADR 0004 §
  Geometry and index fields](../adr/0004-uikit-view-snapshot-provider.md)
- **Android**: Each source declares physical pixels with scale 1. A node
  `frame` is in physical pixels. A crop rect may map directly to a same-sized
  `PixelCopy.request(Window, …)` bitmap only after the capture-time source
  origin, insets, orientation, and clipping have been verified; the Android
  API docs do not establish that equivalence. Until probe 7 does so for a target
  configuration, the SDK must report the source-to-bitmap transform and the
  Host must apply it (or reject the crop). [ADR 0002 § Node
  representation](../adr/0002-ui-snapshot-and-inspection.md)

Crops and annotations are always **derived** images generated on the Host from
the original. Annotation never mutates the original. Every original and derived
image is a distinct sensitive Artifact. [CONTEXT.md § Image Evidence](../../CONTEXT.md)

## Sensitive-content handling

The repository's disclosure model requires fail-closed handling. Concretely:

1. The SDK capture must apply the app-declared **Screenshot Mask** (secret
   regions obscured) before returning the bitmap to the Host. No unmasked
   original Artifact is retained. [CONTEXT.md § Screenshot Mask](../../CONTEXT.md)
2. On iOS, `isSecureTextEntry` is not a documented screenshot-redaction
   guarantee. The app-declared mask remains required for text and non-text
   secret regions alike.
3. On Android, `FLAG_SECURE` prevents protected content from appearing in
   screenshots, but the exact `PixelCopy` result is not documented. The SDK
   must additionally apply app-declared masks for content that is sensitive but
   not `FLAG_SECURE`.
4. If any redaction is incomplete or a field is unclassified, the entire
   capture must fail before Artifact creation — no best-effort disclosure.
   [CONTEXT.md § Fail-Closed Disclosure](../../CONTEXT.md)
5. Captured bitmaps are **Target Ephemeral Data**: retained only in Target
   memory, never written to device storage, destroyed when the session or
   process scope expires. [CONTEXT.md § Target Ephemeral Data](../../CONTEXT.md)

## File-backed Artifact metadata

The accepted CLI Artifact descriptor is preserved unchanged. Each SDK-masked
image (original or derived) maps to it as follows; unmasked display fallback
bytes never receive an Artifact descriptor:

| Artifact field | Image-evidence value |
| --- | --- |
| `id` | `artifact_image-<scope>` or `artifact_crop-<scope>-<ref>` |
| `kind` | `image_evidence_original` or `image_evidence_crop` / `image_evidence_annotated` |
| `path` | absolute Host-local path in the Artifact Workspace |
| `media_type` | `image/png` (lossless; preferred) or `image/jpeg` |
| `size_bytes` | file byte count |
| `digest` | `{ algorithm: "sha-256", value: <64-hex> }` |
| `sensitive` | `true` (all Image Evidence is sensitive) |

[Artifact schema](../../cli/contracts/v1/schema/artifact.schema.json);
[ADR 0006 § Recovery and safety — Artifacts](../adr/0006-agent-facing-cli-contract.md);
[CONTEXT.md § Sensitive Artifact](../../CONTEXT.md)

The spike `write_artifact` already computes `sha256`, `bytes`, atomic
no-clobber publish, and directory sync. Production Artifact persistence should
reuse that pattern (stream-to-temp, hash, persist-no-clobber) but that code
itself is excluded from this research's scope. [spike: artifact.rs](../../cli/spikes/rust-foundation/src/artifact.rs)

## Cancellation and failure

| Scenario | iOS SDK behavior | Android SDK behavior |
| --- | --- | --- |
| Cancellation before capture completes | no native cancellation; future contract checks before capture and before Host handoff, otherwise discards the bitmap | no native cancellation; future contract may stop awaiting the callback and must discard a late result |
| Capture failure | `drawHierarchy == false` or renderer failure → no evidence bytes | documented `ERROR_SOURCE_INVALID/4`, `ERROR_SOURCE_NO_DATA`, `ERROR_UNKNOWN/1`, or `ERROR_TIMEOUT/2` → no evidence bytes |
| Artifact persistence failure | host's accepted Artifact path reports its existing conflict/I/O outcome | same |
| Oversized capture | future screenshot contract needs an explicit in-memory limit before Host handoff | same |

The accepted Artifact conflict behavior remains unchanged: an existing
destination maps to `artifact.alreadyExists` (exit 7). Screenshot-specific
protocol errors and cancellation semantics remain future contract work; this
research does not assign them. [ADR 0006 § Output and channel
rules](../adr/0006-agent-facing-cli-contract.md)

## Secure and obscured content

`UIScreen.isCaptured` and `UIScreen.capturedDidChangeNotification` let the SDK
detect that the system is mirroring or recording the screen. [Apple:
isCaptured](https://developer.apple.com/documentation/uikit/uiscreen/iscaptured)

This is diagnostic evidence, not a redaction signal. A future SDK may report
`isCaptured` as non-sensitive metadata or choose to refuse capture by explicit
policy, but it must not infer that `false` means no screenshot occurred or that
the screenshot is safe.

On Android, there is no direct `isCaptured` equivalent. `FLAG_SECURE` behavior
for `PixelCopy` remains a probe candidate; the SDK must never treat the flag as
a substitute for its own Screenshot Mask.

## Minimal throwaway probes

The following probes are needed to remove residual mechanism risk. Each is a
throwaway snippet, not production code.

1. **iOS `drawHierarchy` + secure text on iOS 15 vs iOS 26** — confirm that
   `isSecureTextEntry` fields render blank in a `drawHierarchy` capture across
   the full support range. Rationale: this is Inference, not Documented, and the
   iOS 15 baseline matters.
2. **iOS `drawHierarchy` wide-gamut color fidelity** — capture a Display P3
   gradient with `preferredRange = .extended` and `.standard`, verify the bitmap
   color space differs. Rationale: confirm `preferredRange` controls output color.
3. **iOS crop point→pixel mapping** — capture a known view at `scale = 3.0`,
   crop the view's point-frame region, verify the pixel dimensions equal
   `pointWidth × 3` × `pointHeight × 3`, including a rotated window and known
   insets plus a non-zero window origin where the configuration permits it.
   Rationale: validates the source-to-bitmap mapping rather than assuming a
   screen-space frame has a zero bitmap origin.
4. **Android `PixelCopy` on `SurfaceView`** — capture a `SurfaceView` rendering
   video/GL via `PixelCopy.request(SurfaceView, …)` on a physical device and
   Emulator. Confirm the GL content appears (unlike `drawToBitmap`). Rationale:
   confirms PixelCopy is the correct Surface-capture mechanism.
5. **Android `PixelCopy` + `FLAG_SECURE`** — set `FLAG_SECURE` on a test window,
   call `PixelCopy.request(Window, …)`, and observe whether the result is
   `SUCCESS` with blank pixels or `ERROR_SOURCE_INVALID/4`. Rationale: the exact
   behavior is inferred, not documented; it determines whether `FLAG_SECURE` is
   sufficient masking or requires additional SDK masks.
6. **Android `screencap` on Emulator vs physical device** — run
   `adb exec-out screencap -p` on both, verify identical PNG structure and
   pixel dimensions, then discard the outputs. Rationale: confirms diagnostic
   command availability only; it does not validate Image Evidence or cropping.
7. **Android `PixelCopy` Rect and source-origin geometry** — capture known
   colored markers at the source root and content edges in portrait and
   landscape, including status/navigation insets and a non-fullscreen window
   where available. Verify the `Rect` bitmap dimensions, marker positions, and
   any source-window offset against the snapshot node frames. Rationale:
   validates (or supplies) the scale-1 source-to-bitmap mapping; it must not be
   inferred solely from matching dimensions.

Probes 1–3 and 4–7 each require one throwaway test app and can run in one
session per platform. No production code is produced.

## Remaining risks

- **iOS physical device has no host screenshot fallback.** If the SDK is not
  running or the app surface is not capturable, there is no `devicectl`
  screenshot path. The SDK in-process capture is the only option. This is
  confirmed by `devicectl device --help` having no screenshot subcommand.
  [Observed]
- **`CALayer.render(in:)` vs `drawHierarchy` for GPU content.** `drawHierarchy`
  is documented as capturing "the complete view hierarchy as visible onscreen";
  `CALayer.render` documented limitations (AVPlayerLayer, 3D transforms) apply
  to macOS 10.5 only. The recommendation uses `drawHierarchy` exclusively;
  probe 1 confirms secure-text rendering, not GPU-layer limitations. [Inference]
- **`displayScale` is on `UITraitCollection`, not `UIScreen`.** `UIScreen.scale`
  is the screen-level natural scale factor used by ADR 0004. `UITraitCollection.displayScale`
  (iOS 8.0+) provides the trait-level display scale and may be useful for
  renderer format selection. Both are Documented. [Apple: UITraitCollection.displayScale](https://developer.apple.com/documentation/uikit/uitraitcollection/displayscale)
- **Android `FLAG_SECURE` exact rendering in PixelCopy.** Whether it produces
  blank, transparent, black, or `ERROR_SOURCE_INVALID` determines whether
  `FLAG_SECURE` alone is sufficient masking or whether the SDK must enforce
  Screenshot Masks for all sensitive content. Probe 5 resolves this. [Inference]
- **Android `PixelCopy.Request` builder (API 26+) vs legacy overloads (API
  24).** The `PixelCopy.Request` builder API (API 26/34) offers more control
  (color space, scaling). The Window/Surface overloads (API 24) cover the MVP
  range (API 26+). The recommendation uses the legacy overloads for simplicity;
  the builder API is a future option. [Documented]
- **Android source-to-bitmap origin.** ADR 0002 fixes source geometry in
  physical pixels, but the PixelCopy API does not document that a Window bitmap
  has the same origin after system bars, non-fullscreen windows, rotation, or
  folding. Until probe 7 records a valid transform for each configuration, a
  Host crop must be rejected rather than offset heuristically. [Inference]
