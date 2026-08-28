# No git-state cache; session state persists by name

Computed git state is never persisted to disk, while session state (the Selection, the active Filter, the active Set) is. The distinction: session state is the user's input, so it can only be absent, never stale; git state is the world, which goes stale and lies, and a stale cache would undermine the provenance guarantees of [0001](0001-per-cell-provenance.md).

## Consequences

- Selection restores by name, never by index; unknown names are dropped silently.
- A corrupt state file behaves exactly like no file, and an explicit flag always beats stored state.
- A restored Filter announces itself with its match count, so a silently narrowed view cannot masquerade as the whole set.
- Every launch recomputes git state from scratch, so first-frame performance must come from progressive loading, not a cache.
