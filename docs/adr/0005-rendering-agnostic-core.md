# The core is a rendering-agnostic library

The core computes state (discovery, provenance, worktree classification, fan-out) and knows nothing about rendering; the TUI is one consumer of it. A predecessor grew two parallel presentation stacks kept consistent by hand, and they drifted despite a shared component layer built to prevent exactly that, so the defence here is structural: a library boundary, which shared components had already failed to provide. Unattended/non-TTY mode is not in v1, but it must be addable as a second consumer of the core, never a second stack.
