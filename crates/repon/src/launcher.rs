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

/// `argv[0]` as the program, the rest as its arguments; an empty `argv` runs an empty
/// program name, which fails to spawn rather than panicking. Shared by a Launcher's own,
/// non-`shell` argv and [`crate::editor::edit`]'s editor-chain argv, the handoff machinery's
/// other caller.
pub(crate) fn command_from_argv(argv: &[String]) -> Command {
    let mut command = Command::new(argv.first().cloned().unwrap_or_default());
    command.args(argv.iter().skip(1));
    command
}

/// The `Command` a Launcher runs, before [`run`] hands it the terminal. Visible to the crate
/// so an `App`-level test can drive a real handoff where `Tui::new` cannot be constructed.
pub(crate) fn build_command(launcher: &Launcher, entity: &EntityState) -> Command {
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
        command_from_argv(&argv)
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

    // Criterion 4: the editor chain's rungs, including the rung this brief calls out by
    // name: an earlier rung set to an empty string is not the same as unset, and must still
    // fall through rather than running a literal empty program.
    #[test]
    fn editor_chain_treats_an_empty_editor_value_as_unset_and_falls_through_to_the_vi_fallback() {
        let editor_blank_visual_unset = Source::EditorChain.resolve_argv(|name| match name {
            "EDITOR" => Some(String::new()),
            _ => None,
        });
        assert_eq!(editor_blank_visual_unset, vec!["vi".to_string()]);
    }

    // Criterion 4's exclusion, the substance of the criterion per this ticket's brief:
    // `GIT_EDITOR` and `core.editor` must appear nowhere in either crate's production
    // source, since tooling commonly exports `GIT_EDITOR=true` to stop editors opening.
    #[test]
    fn git_editor_and_core_editor_appear_nowhere_in_either_crates_production_source() {
        for needle in ["GIT_EDITOR", "core.editor"] {
            let offending = crate::test_support::production_lines_containing(needle);
            assert!(
                offending.is_empty(),
                "found `{needle}`; the editor chain deliberately excludes git's own editor \
                 variable and config key (docs/spec/config.md's \"Launchers\"), at: {offending:?}"
            );
        }
    }

    /// A bare `EntityState` at `path`, otherwise unprobed: enough for [`build_command`] to
    /// resolve a working directory and an environment contract from.
    fn entity_at(path: &std::path::Path) -> EntityState {
        EntityState::new(
            repon_core::EntityKey::new(std::sync::Arc::from(path)),
            std::sync::Arc::from(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("entity"),
            ),
            std::sync::Arc::from(path),
            repon_core::Kind::Repo,
        )
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    // Criterion 1: shell metacharacters embedded in a single configured argv element must
    // reach the child as one literal argument rather than being split or interpreted, which
    // is what "executed without a shell" actually buys: `shell = false` here even though the
    // string itself looks like shell syntax.
    #[test]
    fn shell_defaulting_off_never_splits_or_interprets_argv_that_looks_like_shell_syntax() {
        let dir = tempfile::tempdir().expect("temp dir");
        let entity = entity_at(dir.path());
        let launcher = Launcher {
            name: "test".to_string(),
            source: Source::Args(vec!["echo".to_string(), "a && b; c | d`e`".to_string()]),
            shell: false,
            env: BTreeMap::new(),
        };

        let command = build_command(&launcher, &entity);

        assert_eq!(command.get_program(), std::ffi::OsStr::new("echo"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("a && b; c | d`e`")],
            "the whole hostile string must arrive as one argument, never split or interpreted"
        );
    }

    // Criterion 2: the zeroth-argument detail, precise and easy to get subtly wrong. Asserts
    // the actual argv the child receives via `Command`'s own introspection, not what was
    // passed to the builder.
    #[test]
    fn shell_mode_wraps_the_configured_command_in_the_users_shell_with_repon_as_its_zeroth_argument()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let entity = entity_at(dir.path());
        let launcher = Launcher {
            name: "log".to_string(),
            source: Source::Args(vec!["git log --oneline -20 | less".to_string()]),
            shell: true,
            env: BTreeMap::new(),
        };

        let command = build_command(&launcher, &entity);

        let expected_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        assert_eq!(command.get_program(), std::ffi::OsStr::new(&expected_shell));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new("git log --oneline -20 | less"),
                std::ffi::OsStr::new("repon"),
            ],
            "POSIX sh -c fills $0 from the first argument after the command string; `repon` \
             must be that literal trailing argument, not the launched program's own name"
        );
    }

    // Criterion 3: "a merged environment over the guaranteed set". A declared `env` override
    // must win over the environment contract's own guaranteed pair for the same name, not
    // merely sit alongside it.
    #[test]
    fn a_declared_env_override_wins_over_the_guaranteed_environment_contract_pair() {
        let dir = tempfile::tempdir().expect("temp dir");
        let entity = entity_at(dir.path());
        let mut env = BTreeMap::new();
        env.insert("REPON_REPO_NAME".to_string(), "overridden".to_string());
        let launcher = Launcher {
            name: "test".to_string(),
            source: Source::Args(vec!["true".to_string()]),
            shell: false,
            env,
        };

        let command = build_command(&launcher, &entity);

        let envs: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("REPON_REPO_NAME"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new("overridden")),
            "a declared env override must win over the guaranteed REPON_REPO_NAME pair"
        );
    }

    // Criterion 1: repo context (a branch name, a repo path) reaches the child only through
    // the environment, never through argv, and a value containing shell metacharacters
    // (including a literal newline, which a path can carry even though a branch name cannot)
    // cannot break out of its word because nothing here ever hands it to a shell. A real,
    // disposable git repository, per this project's own testing convention: the "real
    // interface" is a real repo on disk, not a git-backend trait.
    #[test]
    fn a_hostile_branch_and_path_reach_the_child_only_as_literal_environment_values() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().canonicalize().expect("canonicalize temp dir");
        // No spaces (git's ref-format rejects them); the newline lives in the path instead,
        // since a branch name cannot carry one.
        let repo_name = "repo;$(touch__pwn_a)`touch__pwn_b`\n-tail";
        let repo_path = root.join(repo_name);
        std::fs::create_dir_all(&repo_path).expect("create a hostilely-named repo directory");
        run_git(&repo_path, &["init", "-q", "."]);
        run_git(
            &repo_path,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "first",
            ],
        );
        let hostile_branch = "feature/$(touch__pwn_c);`touch__pwn_d`";
        run_git(&repo_path, &["checkout", "-q", "-b", hostile_branch]);

        let core = repon_core::Core::start(repon_core::CoreSpec {
            set: repon_core::SetSpec {
                name: "test".to_string(),
                roots: vec![root.clone()],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: std::time::Duration::from_secs(3600),
            status_stale_after: std::time::Duration::from_secs(3600),
            generation_deadline: std::time::Duration::from_secs(3600),
            show_submodules: false,
        });
        let key = core.snapshot().entities[0].key.clone();
        let entity = core.probe_now(&key);

        let launcher = Launcher {
            name: "probe".to_string(),
            source: Source::Args(vec!["printenv".to_string(), "REPON_BRANCH".to_string()]),
            shell: false,
            env: BTreeMap::new(),
        };

        // The argv itself never carries repo context: only the launcher's own literal,
        // configured argv.
        let command = build_command(&launcher, &entity);
        assert_eq!(command.get_program(), std::ffi::OsStr::new("printenv"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("REPON_BRANCH")]
        );
        assert_eq!(command.get_current_dir(), Some(repo_path.as_path()));
        let envs: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("REPON_BRANCH"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new(hostile_branch)),
            "REPON_BRANCH must carry the hostile value byte-for-byte, as an environment value"
        );

        // End to end: a real child sees the value as one opaque string, never executed. The
        // hostile value's own `$(...)` and backtick would, if ever handed to a shell, create
        // marker files in the child's own working directory; their absence is the proof
        // nothing here ever did that.
        let mut executable = build_command(&launcher, &entity);
        let output = executable.output().expect("run printenv");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim_end(),
            hostile_branch,
            "the child must see the hostile branch name exactly, with nothing stripped or split"
        );
        for marker in [
            "touch__pwn_a",
            "touch__pwn_b",
            "touch__pwn_c",
            "touch__pwn_d",
        ] {
            assert!(
                !repo_path.join(marker).exists(),
                "found `{marker}`, which only exists if something executed the hostile value \
                 rather than treating it as an opaque string"
            );
        }
    }

    // Criterion 1's second claim: "no template substitution into argv exists anywhere". The
    // schema itself cannot express one (`launcher_config_carries_no_working_directory_field...`
    // and this module's own exhaustive `Source` match are the structural half); this is the
    // textual half, an absence scan for the concrete shape a template mechanism would take:
    // a `str::replace` call standing in for a placeholder substitution. A bare `"{repo}"`-style
    // needle is not used here, since Rust's own `format!`/`tracing` interpolation syntax
    // shares that shape for unrelated reasons and would make this scan noisy rather than
    // precise.
    #[test]
    fn no_placeholder_substitution_mechanism_exists_anywhere_in_either_crate() {
        for needle in [
            "replace(\"{",
            "replace(\"$REPON",
            "args_template",
            "argv_template",
        ] {
            let offending = crate::test_support::production_lines_containing(needle);
            assert!(
                offending.is_empty(),
                "found `{needle}`; repo context reaches a Launcher only through the \
                 environment, with no template substitution into argv anywhere, at: {offending:?}"
            );
        }
    }
}
