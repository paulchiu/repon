# Every displayed value carries per-cell provenance

Progressive loading across hundreds of Repos means values arrive from different sources on different clocks, which is exactly the defect class behind nearly every UI complaint about a predecessor tool: the screen contradicted itself. Every displayed value therefore carries a provenance state, Unknown, Loading, Fresh(at), Stale(at) or Failed, and rendering is a total function of that state. The type system makes the bad states unrepresentable rather than relying on discipline.

## Consequences

- An absent value never renders as zero. An ahead/behind count that has not been computed shows as unknown, not 0.
- Every widget must handle all five states; there is no "just show the number" path.
- A slow Repo times out to Unknown rather than holding the table, and a newer refresh supersedes an older one rather than queueing.
- This is the load-bearing architectural decision; the core's data model (see [0005](0005-rendering-agnostic-core.md)) is shaped around it.
