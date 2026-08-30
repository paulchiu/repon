//! Launchers: configured handoff targets a single Entity's row can hand the terminal to
//! (lazygit, tuicr, an editor, a shell), per
//! [config.md](../../../../docs/spec/config.md#launchers) and
//! [ADR 0007](../../../../docs/adr/0007-launchers-are-argv-vectors.md). [`resolve`] turns
//! the four shipped defaults plus a document's declared `[[launcher]]` entries into the list
//! the palette shows; [`run`] is one of the two callers of
//! [`Tui::suspend_for_child`](crate::tui::Tui::suspend_for_child), the shared terminal-handoff
//! machinery [`crate::editor`] is the other.

use std::collections::BTreeMap;
use std::process::{Command, ExitStatus};

use color_eyre::eyre::Result;
use repon_core::EntityState;

use crate::config::document::{Document, LauncherConfig};
use crate::tui::Tui;

/// One Launcher ready for the palette: its name, how its argv is produced, and its own
/// config-declared shell opt-in and environment overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher {
    pub name: String,
    pub source: Source,
    pub shell: bool,
    pub env: BTreeMap<String, String>,
}

/// How a Launcher's argv is produced. `args` and `from_env` are the two forms
/// [config.md](../../../../docs/spec/config.md#launchers) lets a `[[launcher]]` entry
/// declare; `EditorChain` and `ShellFallback` exist only for the two shipped defaults whose
/// fallback chain a single `from_env` name cannot express, and are never reachable from a
/// declared entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A literal argv vector.
    Args(Vec<String>),
    /// Reads one named environment variable, splits it with shell-words. Unset, or set
    /// empty, resolves to an empty argv: no config-declared fallback exists for this form.
    FromEnv(String),
    /// The shipped `editor` default: the first of `VISUAL`, then `EDITOR`, that is set and
    /// non-empty, split with shell-words; if neither is, the literal single-word `vi`.
    EditorChain,
    /// The shipped `shell` default: `SHELL` if set and non-empty, split with shell-words;
    /// otherwise the literal single-word `/bin/sh`.
    ShellFallback,
}

impl Source {
    /// Resolves to argv at invocation time. `lookup` stands in for `std::env::var` so a test
    /// never has to mutate the real process environment, which parallel tests would race.
    pub fn resolve_argv(&self, lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
        match self {
            Source::Args(args) => args.clone(),
            Source::FromEnv(name) => env_argv(&lookup, name).unwrap_or_default(),
            Source::EditorChain => chain_argv(&lookup, &["VISUAL", "EDITOR"], "vi"),
            Source::ShellFallback => chain_argv(&lookup, &["SHELL"], "/bin/sh"),
        }
    }
}

/// `name`'s value split with shell-words, or `None` if unset, empty (after trimming), or
/// unparsable (an unbalanced quote), so every failure mode here reads as "nothing to run"
/// rather than a panic.
fn env_argv(lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Option<Vec<String>> {
    let value = lookup(name)?;
    if value.trim().is_empty() {
        return None;
    }
    shell_words::split(&value).ok()
}

/// The first of `vars`, in order, that is set and non-empty, split with shell-words; the
/// literal single-word `fallback` if none is.
fn chain_argv(
    lookup: &impl Fn(&str) -> Option<String>,
    vars: &[&str],
    fallback: &str,
) -> Vec<String> {
    for var in vars {
        if let Some(argv) = env_argv(lookup, var) {
            return argv;
        }
    }
    vec![fallback.to_string()]
}

/// The four Launchers Repon ships, in [config.md](../../../../docs/spec/config.md#launchers)'s
/// own declared order, before a document's `[[launcher]]` entries replace, drop or extend them.
fn shipped_defaults() -> Vec<Launcher> {
    vec![
        Launcher {
            name: "lazygit".to_string(),
            source: Source::Args(vec!["lazygit".to_string()]),
            shell: false,
            env: BTreeMap::new(),
        },
        Launcher {
            name: "tuicr".to_string(),
            source: Source::Args(vec!["tuicr".to_string()]),
            shell: false,
            env: BTreeMap::new(),
        },
        Launcher {
            name: "editor".to_string(),
            source: Source::EditorChain,
            shell: false,
            env: BTreeMap::new(),
        },
        Launcher {
            name: "shell".to_string(),
            source: Source::ShellFallback,
            shell: false,
            env: BTreeMap::new(),
        },
    ]
}

impl Launcher {
    fn from_config(config: &LauncherConfig) -> Self {
        let source = match (&config.args, &config.from_env) {
            (Some(args), _) => Source::Args(args.clone()),
            (None, Some(name)) => Source::FromEnv(name.clone()),
            (None, None) => Source::Args(Vec::new()),
        };
        Self {
            name: config.name.get_ref().clone(),
            source,
            shell: config.shell,
            env: config.env.clone(),
        }
    }
}

/// Merges the four shipped defaults with `document`'s declared `[[launcher]]` entries, in
/// file order:
/// [config.md](../../../../docs/spec/config.md#launchers): "Declaring a `[[launcher]]` with a
/// shipped name replaces it in place; `disabled = true` drops it." An entry whose name is not
/// one of the four shipped defaults is appended, unless it is itself `disabled`, in which
/// case declaring it does nothing.
pub fn resolve(document: &Document) -> Vec<Launcher> {
    let mut result = shipped_defaults();
    for declared in &document.launchers {
        let name = declared.name.get_ref().as_str();
        match result.iter().position(|launcher| launcher.name == name) {
            Some(position) if declared.disabled => {
                result.remove(position);
            }
            Some(position) => {
                result[position] = Launcher::from_config(declared);
            }
            None if !declared.disabled => {
                result.push(Launcher::from_config(declared));
            }
            None => {}
        }
    }
    result
}

/// Runs `launcher` against `entity`'s own working directory, suspending Repon's terminal for
/// the handoff and reclaiming it once the child exits. `cwd` is not a config field
/// ([config.md](../../../../docs/spec/config.md#launchers)): every Launcher starts in the
/// entity's own working directory. `launcher.env` is merged over the environment contract's
/// guaranteed pairs, so a declared override always wins.
pub fn run(tui: &mut Tui, launcher: &Launcher, entity: &EntityState) -> Result<ExitStatus> {
    let mut command = build_command(launcher, entity);
    tui.suspend_for_child(&mut command)
}

fn build_command(launcher: &Launcher, entity: &EntityState) -> Command {
    let argv = launcher
        .source
        .resolve_argv(|name| std::env::var(name).ok());

    let mut command = if launcher.shell {
        // ADR 0007's visible opt-in: `$SHELL -c <string>` with a literal `repon` as `$0`, so
        // POSIX `sh -c` does not silently eat the first real argument.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut command = Command::new(shell);
        command.arg("-c").arg(argv.join(" ")).arg("repon");
        command
    } else {
        let mut command = Command::new(argv.first().cloned().unwrap_or_default());
        command.args(argv.iter().skip(1));
        command
    };

    command.current_dir(entity.key.path());
    for (name, value) in repon_core::environment(entity, None) {
        match value {
            Some(value) => {
                command.env(name, value);
            }
            None => {
                command.env_remove(name);
            }
        }
    }
    for (name, value) in &launcher.env {
        command.env(name, value);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launcher_config(
        name: &str,
        args: Option<Vec<&str>>,
        from_env: Option<&str>,
        disabled: bool,
    ) -> LauncherConfig {
        LauncherConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            args: args.map(|args| args.into_iter().map(str::to_string).collect()),
            from_env: from_env.map(str::to_string),
            shell: false,
            env: BTreeMap::new(),
            disabled,
        }
    }

    /// `docs/spec/config.md`, read at test time so the shipped default names below are pinned
    /// against the design of record rather than copied by hand: renaming a shipped default
    /// here without renaming it in the spec (or vice versa) fails this test, per this
    /// ticket's brief on the "single source of truth" trap.
    fn spec_config_md() -> String {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec/config.md"))
            .expect("read docs/spec/config.md")
    }

    /// Parses "Four Launchers ship as defaults: lazygit, tuicr, an editor via `from_env`, and
    /// a shell." into `["lazygit", "tuicr", "editor", "shell"]`: split on the sentence's own
    /// commas, drop a leading "and "/"an "/"a " article, then cut at " via " to drop the
    /// backtick-quoted aside naming the mechanism rather than the name.
    fn spec_shipped_launcher_names(spec: &str) -> Vec<String> {
        const ANCHOR: &str = "Four Launchers ship as defaults:";
        let after = spec
            .split(ANCHOR)
            .nth(1)
            .expect("the shipped-defaults sentence is present");
        let sentence = after.split('.').next().expect("a sentence terminator");

        sentence
            .split(',')
            .map(str::trim)
            .filter(|phrase| !phrase.is_empty())
            .map(|phrase| phrase.strip_prefix("and ").unwrap_or(phrase))
            .map(|phrase| {
                phrase
                    .strip_prefix("an ")
                    .or_else(|| phrase.strip_prefix("a "))
                    .unwrap_or(phrase)
            })
            .map(|phrase| phrase.split(" via ").next().unwrap_or(phrase))
            .map(|phrase| phrase.trim().trim_matches('`').to_string())
            .collect()
    }

    // Criterion 1: the shipped names, read from the spec at test time, not copied by hand;
    // this also proves there are exactly four, in the spec's own order.
    #[test]
    fn shipped_launcher_names_match_the_spec_exactly_and_in_order() {
        let expected = spec_shipped_launcher_names(&spec_config_md());
        let actual: Vec<String> = shipped_defaults()
            .into_iter()
            .map(|launcher| launcher.name)
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn an_empty_document_resolves_to_exactly_the_four_shipped_defaults() {
        let resolved = resolve(&Document::default());
        assert_eq!(resolved, shipped_defaults());
    }

    // Criterion 1, replacement: a declared entry of a shipped name replaces it in place
    // rather than appending a duplicate.
    #[test]
    fn a_shipped_launcher_is_replaced_in_place_by_a_declared_entry_of_the_same_name() {
        let mut document = Document::default();
        document.launchers.push(launcher_config(
            "lazygit",
            Some(vec!["custom-lazygit", "--flag"]),
            None,
            false,
        ));

        let resolved = resolve(&document);

        assert_eq!(
            resolved.len(),
            4,
            "replacing a shipped name must not add a fifth entry"
        );
        let names: Vec<&str> = resolved
            .iter()
            .map(|launcher| launcher.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["lazygit", "tuicr", "editor", "shell"],
            "the replacement keeps the shipped position"
        );
        assert_eq!(
            resolved[0].source,
            Source::Args(vec!["custom-lazygit".to_string(), "--flag".to_string()])
        );
    }

    // Criterion 1, disabling: a separate behaviour from replacement, proven on its own so a
    // build that replaces-with-empty-args instead of actually dropping the entry still fails.
    #[test]
    fn disabling_a_shipped_launcher_drops_it_rather_than_replacing_it() {
        let mut document = Document::default();
        document
            .launchers
            .push(launcher_config("tuicr", None, None, true));

        let resolved = resolve(&document);

        assert_eq!(
            resolved.len(),
            3,
            "a disabled entry must be dropped, not kept as a fourth"
        );
        assert!(
            !resolved.iter().any(|launcher| launcher.name == "tuicr"),
            "tuicr must be gone entirely"
        );
        for name in ["lazygit", "editor", "shell"] {
            assert!(
                resolved.iter().any(|launcher| launcher.name == name),
                "disabling tuicr must not touch {name}"
            );
        }
    }

    #[test]
    fn a_declared_entry_with_a_new_name_is_appended_rather_than_replacing_anything() {
        let mut document = Document::default();
        document
            .launchers
            .push(launcher_config("scratch", Some(vec!["true"]), None, false));

        let resolved = resolve(&document);

        assert_eq!(resolved.len(), 5);
        assert_eq!(resolved.last().unwrap().name, "scratch");
    }

    #[test]
    fn declaring_a_disabled_entry_under_a_new_name_does_nothing() {
        let mut document = Document::default();
        document
            .launchers
            .push(launcher_config("scratch", None, None, true));

        let resolved = resolve(&document);

        assert_eq!(resolved, shipped_defaults());
    }

    // Source resolution.

    #[test]
    fn from_env_splits_a_multi_word_value_with_shell_words() {
        let source = Source::FromEnv("EDITOR".to_string());
        let argv = source.resolve_argv(|name| match name {
            "EDITOR" => Some("code --wait".to_string()),
            _ => None,
        });
        assert_eq!(argv, vec!["code".to_string(), "--wait".to_string()]);
    }

    #[test]
    fn from_env_resolves_to_an_empty_argv_when_unset_or_blank() {
        let source = Source::FromEnv("EDITOR".to_string());
        assert_eq!(source.resolve_argv(|_| None), Vec::<String>::new());
        assert_eq!(
            source.resolve_argv(|_| Some("   ".to_string())),
            Vec::<String>::new()
        );
    }

    #[test]
    fn editor_chain_prefers_visual_over_editor_over_the_vi_fallback() {
        let visual_and_editor_set = Source::EditorChain.resolve_argv(|name| match name {
            "VISUAL" => Some("code --wait".to_string()),
            "EDITOR" => Some("nano".to_string()),
            _ => None,
        });
        assert_eq!(
            visual_and_editor_set,
            vec!["code".to_string(), "--wait".to_string()]
        );

        let only_editor_set = Source::EditorChain.resolve_argv(|name| match name {
            "EDITOR" => Some("nano".to_string()),
            _ => None,
        });
        assert_eq!(only_editor_set, vec!["nano".to_string()]);

        let neither_set = Source::EditorChain.resolve_argv(|_| None);
        assert_eq!(neither_set, vec!["vi".to_string()]);

        // A set-but-empty VISUAL counts as unset, falling through to EDITOR.
        let visual_blank = Source::EditorChain.resolve_argv(|name| match name {
            "VISUAL" => Some(String::new()),
            "EDITOR" => Some("nano".to_string()),
            _ => None,
        });
        assert_eq!(visual_blank, vec!["nano".to_string()]);
    }

    #[test]
    fn shell_fallback_prefers_shell_over_the_bin_sh_fallback() {
        let shell_set = Source::ShellFallback.resolve_argv(|name| match name {
            "SHELL" => Some("/opt/homebrew/bin/zsh".to_string()),
            _ => None,
        });
        assert_eq!(shell_set, vec!["/opt/homebrew/bin/zsh".to_string()]);

        let unset = Source::ShellFallback.resolve_argv(|_| None);
        assert_eq!(unset, vec!["/bin/sh".to_string()]);
    }
}
