# Launchers and Actions get separate palettes on separate keys

`!` opens the Launcher palette (behaving like `:` in TUI apps, showing options rather than jumping straight to a shell); Actions get a separate key and palette. The split exists as a safety boundary. A Launcher acts on one Repo and hands over the terminal; an Action acts on N Repos unattended and can do damage.

## Consequences

- The two palettes may look identical, but they must never be one keystroke or one fuzzy-match slip apart, so "open a shell here" cannot become "run this across 99 repos".
- To a future reader this will look like duplication worth removing. Merging the palettes reopens the failure mode above.
