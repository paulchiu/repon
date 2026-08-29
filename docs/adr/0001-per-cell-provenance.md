# Every displayed value carries per-cell provenance

Progressive loading across hundreds of Repos means values arrive from different sources on different clocks, which is exactly the defect class behind nearly every UI complaint about a predecessor tool: the screen contradicted itself. Every displayed value therefore carries a provenance state, Unknown, Loading, Fresh(at), Stale(at) or Failed, and rendering is a total function of that state. The type system makes the bad states unrepresentable rather than relying on discipline.

## Consequences

- An absent value never renders as zero. An ahead/behind count that has not been computed shows as unknown, not 0.
- Every widget must handle all five states; there is no "just show the number" path.
- A slow Repo times out to Unknown rather than holding the table, and a newer refresh supersedes an older one rather than queueing.
- This is the load-bearing architectural decision; the core's data model (see [0005](0005-rendering-agnostic-core.md)) is shaped around it.

## Amended by 0013

Two clarifications, both from measuring the refresh model. Unknown carries a reason, so that timed out, no upstream, no default branch and no remote are all Unknown and all render `?` while the detail pane says which; a sixth state was rejected rather than the distinction. And a slow Repo no longer times out on its own clock: the deadline belongs to the generation, which is cancelled at thirty seconds, and every cell still Loading in it becomes Unknown at that moment. Supersession is per entity rather than global, so a refresh over the Selection cannot strand the rows it never spoke for. See [0013](0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md).

## Amended by 0015

Loading is no longer one of the five. A cell is four settled answers, Unknown with its reason, Known with its value and timestamp, Failed, and NotApplicable, plus an orthogonal in-flight flag. Two reasons: 0013 already stopped treating Loading as a peer when it made in-flight a row property outranking the least-settled-state summary, and a flat five-arm enum cannot hold a cell's previous value while a re-probe is in flight, which [layout-and-provenance.md](../spec/layout-and-provenance.md) requires. NotApplicable becomes a settled answer rather than an absent value, since an `Option` around the cell would reintroduce the bare default path this ADR exists to close. The five states a reader sees on screen are unchanged. The type also gains a requirement this ADR never stated: a cell must be `Clone`, because the consumer reads a cloned snapshot every frame. See [0015](0015-the-core-owns-the-table.md).
