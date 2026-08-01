# ADR 0002: UI snapshot and inspection

- Status: Accepted
- Date: 2026-08-01
- Issue: [#4](https://github.com/patrick-fu/AppPilotKit/issues/4)

## Context

Agents need a small interface for seeing a running app, locating a node, and
revealing only the relevant part of a large hierarchy. The providers behind
that seam are materially different: UIKit and Android View expose native view
trees, Compose exposes semantics, and SwiftUI often requires a semantics or
accessibility projection.

A single normalized tree would erase diagnostic information. Returning every
native property and every node by default would instead overflow agent context
and make common queries expensive.

## Decision

### Protocol minor and capabilities

UI inspection is introduced in protocol v1.1 with two capabilities and two
methods:

- `ui.snapshot` captures a provider snapshot and returns its first bounded
  page;
- `ui.inspect` queries or expands an existing snapshot.

The corresponding capabilities are `ui.snapshot` and `ui.inspect`. A client
must negotiate protocol minor 1 and the capabilities before invoking them.

### Snapshot seam

A snapshot is immutable and identified by a session-scoped ID plus a monotonic
generation. Each captured source records its platform, provider, representation
kind, native schema, root reference, and whether coverage is complete or
partial.

One snapshot may contain multiple source trees. Sources can overlap visually;
the protocol does not deduplicate UIKit hosting SwiftUI, Android View hosting
Compose, or accessibility projections. The source identity remains available
on every node. A session targets one app process, so all sources in one
snapshot declare the same platform.

An initial snapshot request filters adapters with `providers`, whose values are
provider names such as `uikit.views` or `compose.semantics`. Each captured tree
then receives a snapshot-local `source.id`, such as `uikit.main`; selectors use
those returned IDs in `sourceIds`. Provider names and source IDs are not
interchangeable.

Providers retain snapshots until session end or bounded eviction. An evicted
snapshot fails with `ui.snapshotExpired`; an unknown reference in a retained
snapshot fails with `ui.referenceNotFound`. Both require a changed request or a
fresh snapshot, so `retryable` is false.

### Stable references

Node references are opaque and stable only inside one snapshot. They must not
be compared across snapshots or parsed by callers. Every request repeats the
snapshot ID and generation, so stale or cross-snapshot references fail instead
of targeting a coincidentally similar node.

Cross-snapshot identity is explicitly deferred. A future implementation may
offer matching hints, but they cannot redefine v1.1 node-reference semantics.

### Node representation

Results use a flat, depth-first list with `parentRef`, `childIndex`, `depth`,
and `childCount`. Flat adjacency records paginate and deduplicate more safely
than recursive JSON while preserving native order.

Each source has exactly one depth-zero node. Every non-root node carries both
`parentRef` and its zero-based native `childIndex`; selected siblings are
emitted in increasing `childIndex` order even when compact selection skips
intermediate siblings.

`childIndex` and `childCount` describe the captured native source, not only the
current selection or page. A source also declares a screen coordinate space:
iOS uses points and its display scale converts points to screenshot pixels;
Android uses physical pixels with scale 1. Geometry queries name exactly one
source and use that source's coordinate space.

Each node has two distinct payload layers:

- `index` is a small optional projection for deterministic search: identifier,
  redacted text, class name, type name, provider-native trait strings, frame,
  visibility, and interactivity;
- `native` is an optional provider-owned object returned only when native detail
  is requested.

The index is not a cross-platform role model. Missing and unknown values stay
absent, a node without an index simply cannot match index-field predicates,
provider trait names are not rewritten, and native payload schemas are
versioned by each provider.

### Compact selection versus complete traversal

`ui.snapshot` defaults to `selection: agent`. For each requested provider, the
server selects:

1. the root;
2. nodes the provider marks visible or interactive;
3. the ancestors required to preserve paths to those nodes.

Nodes are emitted in source order and provider-native depth-first order. The
response reports total and selected node counts plus the applied criteria.
Intentional compact selection is not truncation: `selectedNodes < totalNodes`
may coexist with `truncated: false`.

Every snapshot page is ancestor-closed: a returned non-root node's parent and
source root also appear earlier in that page. Repeated path nodes count toward
`returnedItems`, while cursor progress advances the not-yet-revealed selection.

`selection: full` explicitly selects every captured node. It is still bounded
by negotiated item/byte limits and paginated with opaque cursors. Partial
provider coverage is disclosed on the source; “full” never claims more than
the provider can observe.

### Targeted inspection

`ui.inspect` accepts one target:

- explicit node references; or
- an AND-composed query over source, identifier, text, class/type, traits,
  visibility, interactivity, and geometry. A query can be scoped beneath one
  `withinRef`.

It optionally expands a bounded number of direct ancestor levels and descendant
levels. `siblings: N` includes up to N preceding and N following siblings of
each direct match. Expansion is unioned and deduplicated; `matchedRefs`
distinguishes direct matches from context nodes in the flat result.

Inspection pages remain source-contiguous and follow provider-native
depth-first order. A source has at most one returned depth-zero node; a page may
start below that root when the requested ancestor depth is zero. If a returned
parent is present, it precedes its descendants in depth-first order.

`withinRef` defines an inclusive subtree scope: the referenced node and all its
descendants are eligible to match the other predicates. It is not a predicate
by itself. A query with only `withinRef` is invalid; callers request a subtree
by targeting that reference and setting `descendantsDepth` explicitly.

String matching supports exact, prefix, suffix, and contains operations with
explicit case sensitivity. Arbitrary regular expressions are excluded from
v1.1 to avoid inconsistent engines and unbounded provider work.

Case-sensitive matching compares the JSON Unicode scalar sequence without
normalization. A case-insensitive predicate value must be ASCII. Providers fold
ASCII `A` through `Z` to `a` through `z` in both the predicate and candidate;
all other candidate scalars remain unchanged before applying the selected
operation. Thus `log in` can match the mixed candidate `Log in 登录` with
`contains`, while it cannot match it with `exact`. This keeps Swift and Kotlin
implementations deterministic; callers use case-sensitive matching for
non-ASCII predicates in v1.1.

### Pagination and privacy

Every result uses the v1.0 disclosure metadata. `returnedItems` equals the node
array length, and the serialized JSON-RPC envelope must fit the applied byte
limit. A first request carries selection or inspection parameters. A
continuation request carries only the returned snapshot identity and opaque
cursor; sending selection, target, traversal, detail, provider, or limit fields
with a cursor fails with `invalidParams`. Cursors restore and bind the session,
method, snapshot, canonical target, traversal, detail level, provider selection,
limits, and provider snapshot. Eviction fails with `ui.snapshotExpired`.

Ancestor paths may repeat on later pages to keep each page usable. Because the
snapshot is immutable, every repeated node reference has structurally identical
JSON node content across pages; JSON object member order is irrelevant.

A success response must also correlate with its initial request: snapshot
selection and requested providers match the result, applied limits do not exceed
requested hints, inspection uses the requested snapshot, and explicit target
references equal `matchedRefs`. A mismatched success is a protocol violation,
not a partial match.

Providers redact sensitive text and native fields before serialization. The
protocol cannot request an unredacted bypass.

## Consequences

- Agents learn two methods for capture, lookup, and bounded expansion.
- UIKit, SwiftUI, Android View, Compose, and accessibility adapters keep their
  native diagnostic identity.
- Compact results are deterministic and explicit about intentional omission,
  provider limitations, and truncation.
- Recursive hierarchy rendering, screenshot correlation, and actions remain
  separate modules layered on stable snapshot references.

## Rejected alternatives

- **One cross-platform node type with normalized roles:** loses provider truth
  needed to diagnose platform-specific behavior.
- **Recursive node JSON:** makes pagination, deduplication, and bounded context
  substantially harder.
- **A separate method for query, subtree, ancestors, and siblings:** creates a
  shallow interface with repeated selection and pagination rules.
- **Cross-snapshot stable references:** cannot be guaranteed through ordinary
  UI mutation without provider-specific matching heuristics.
- **Regex selectors:** add engine differences and denial-of-service risk before
  a demonstrated need.
