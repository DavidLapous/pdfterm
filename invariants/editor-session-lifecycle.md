# Editor session and terminal lifecycle

Updated: 2026-09-21 PDT.
Status: required contract; deviations are tracked in `review.md` and `next_steps.md`.
This file defines behavior, not a claim that every acceptance test already passes.

## Ownership and scope

The viewer owns rendering and navigation. The bundled Lua adapter owns transient
editor intent, project jobs and its socket listener. The terminal backend owns
exact surface handles. The client SSH bridge owns only the windows and master it
created. Do not introduce another durable session manager or duplicate transport
implementation in a downstream editor configuration.

## Identity and transport

A navigation intent identifies one source location, PDF/revision, session and any
terminal handle acquired for that intent. Builds and resolution cannot retarget
its terminal by observing later global focus. Superseded callbacks cannot replace
current identity. Unknown or stale identity fails explicitly before terminal effects.

Socket communication does not require window control. Valid existing viewer
attachment must work without terminal capture/launch. Plain SSH does not authorize
local terminal automation merely because terminal environment variables exist.
The explicit client bridge is a distinct opt-in route. Inverse focus stays opt-in.
Automatic editor sessions use distinct private endpoints; explicit session names
are intentional pairing, not permission to steal a live socket.

## Lifetime and bounds

The viewer limit bounds live owned viewers, not historical launch count. External
window exits must be reconciled. Failure to query liveness is an error, not proof
of absence. Close is authorized only for owned handles and must not close the
source terminal or an independent/reused viewer. Define already-gone handling
without accepting arbitrary unknown IDs as owned. Normal editor exit releases its
readers; the SSH shell/bridge may outlive successive editors. Shell/bridge shutdown
releases its remaining owned resources. SIGKILL/client crash guarantees must not
be overstated.

Finite helper deadlines/output bounds include owned descendants and pipe lifetime.
Authentication/bootstrap and the shared SSH master have explicit separate lifetimes;
finite helper cleanup must neither orphan children nor destroy a healthy master or
an unrelated process group. Cleanup failures are visible.

## Errors and evidence

No silent fallback to another viewer, session, terminal, or SSH mode; no retry that
silently retargets user intent. Bounds, parsing, quoting and current-user socket
ownership remain enforced during repairs. Optional text refinement may report
line-only navigation; it must not fabricate a precise source position.

Tests assert this contract against actual source. Mock windows and headless tests
are useful but do not certify native window automation, authentication or graphics.
Record exact tested revisions and separate skipped/unavailable coverage from passes.
