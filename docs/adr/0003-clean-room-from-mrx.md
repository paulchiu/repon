# Repon is a full clean-room rebuild

Repon's predecessor is mrx, a multi-repo tool covering similar ground. Its upstream carries no licence file, so all rights are reserved, and the tool is entangled with work configuration. mrx therefore informs the problem statement and the recorded requirements only (captured in [the mrx research](<../research/2026-08-28 mrx history and requirements (clean room).md>)); every solution is re-derived here.

The practical rule is that mrx's source is not consulted while building Repon. mrx is credited under Influences in the README, which is what independent derivation looks like on the record: the influence is documented, and the code is not shared.

## The upstream may be named

This decision originally went further and held that the README would say nothing about the upstream repository at all, because [releasing.md](../spec/releasing.md) ships that file to crates.io, where it cannot be taken back. That part is reversed here: the README links `https://github.com/benfriebe/mrx`. The upstream is a public repository, and naming a public repository makes no licence claim, so the link credits the work without touching anything the clean room protects. Public visibility is still not a licence: the upstream carries no licence file, all rights remain reserved, and its source is still not consulted.

The permanence of the crates.io copy is unchanged, and it is why the reversal is this narrow. What the credit may carry is what mrx is and where it lives. What it may not carry is anything that is not the upstream author's to publish: no quote or paraphrase from a private conversation, no local path, no account of mrx's internals or history, and no framing of mrx by a relationship to anyone rather than by what it is. That bound is not the README's alone. It governs everything this repository publishes, which is why [the mrx research](<../research/2026-08-28 mrx history and requirements (clean room).md>) cites its private working notes by date and restates every finding in its own words rather than quoting them.
