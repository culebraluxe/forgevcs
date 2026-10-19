//! Argument parsing for the `forgevcs` human CLI.
//!
//! The surface is deliberately small and dependency-free: a global prefix (`--base-url`,
//! `--config-dir`, `--token-file`) and one command tree. Unknown options are errors rather than
//! silently ignored, so a typo cannot change what a command talked to.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};

/// Options that apply to every command and must precede it on the command line.
#[derive(Debug, Default)]
pub struct GlobalOptions {
    /// Override the service base URL (`FORGEVCS_BASE_URL` also works).
    pub base_url: Option<String>,
    /// Override the config directory (`FORGEVCS_CONFIG_DIR` also works).
    pub config_dir: Option<PathBuf>,
    /// Override the token file path (`FORGEVCS_TOKEN_FILE` also works).
    pub token_file: Option<PathBuf>,
}

/// One parsed invocation.
#[derive(Debug)]
pub enum Parsed {
    /// Run a command.
    Run(GlobalOptions, Command),
    /// Print this text and exit successfully (`--help`).
    Help(String),
}

/// The human operations the CLI wraps. Each maps to exactly one façade route.
#[derive(Debug)]
pub enum Command {
    /// `auth status` — show the configured target, stored identity and token expiry.
    AuthStatus,
    /// `auth mint` — exchange a human secret for a bearer token and store it (0600).
    AuthMint {
        login: String,
        secret_file: Option<PathBuf>,
        show_token: bool,
    },
    /// `auth logout` — remove the locally stored token and session.
    AuthLogout,
    /// `health` — probe `/health`, `/ready` and `/version`.
    Health,
    /// `init <repo>` — create/initialize a repository.
    Init { repo: String },
    /// `refs <repo>` — list refs.
    Refs { repo: String },
    /// `ref create <repo> --name <ref> --target <changeset>` — provision a ref.
    RefCreate {
        repo: String,
        name: String,
        target: String,
        reason: Option<String>,
    },
    /// `log <repo>` — read a commit page.
    Log {
        repo: String,
        ref_name: String,
        limit: u32,
        path: Option<String>,
        first_parent_only: bool,
        page_token: Option<String>,
    },
    /// `pull <repo>` — read history and object bytes.
    Pull {
        repo: String,
        ref_name: String,
        limit: u32,
        path: Option<String>,
        objects: Vec<String>,
    },
    /// `push <repo> --changeset <file>` — upload a changeset body.
    Push {
        repo: String,
        changeset: PathBuf,
        ref_name: Option<String>,
        mode: Option<String>,
    },
    /// `merge <repo> --a … --b … --change-id …` — create a two-parent merge changeset.
    Merge {
        repo: String,
        a: String,
        b: String,
        change_id: String,
        author: Option<String>,
        message: Option<String>,
    },
    /// `rebase <repo> --changeset … --onto …` — replay a changeset.
    Rebase {
        repo: String,
        changeset: String,
        onto: String,
    },
    /// `merge-land <repo> --head …` — merge a head into the ref tip and land the result.
    MergeLand {
        repo: String,
        head: String,
        ref_name: String,
        author: Option<String>,
        message: Option<String>,
        change_id: Option<String>,
    },
    /// `workspace create <repo>`.
    WorkspaceCreate {
        repo: String,
        ref_name: Option<String>,
        changeset: Option<String>,
        sparse: Vec<String>,
    },
    /// `workspace status <repo> <workspace-id>`.
    WorkspaceStatus { repo: String, id: String },
    /// `workspace destroy <repo> <workspace-id>`.
    WorkspaceDestroy { repo: String, id: String },
}

/// The complete usage text.
pub fn usage() -> String {
    r#"forgevcs — human CLI for the ForgeVCS REST façade

USAGE:
    forgevcs [--base-url URL] [--config-dir PATH] [--token-file PATH] <command> [options]

GLOBAL OPTIONS (before the command):
    --base-url URL     service base URL (default: $FORGEVCS_BASE_URL or config.toml or
                       http://127.0.0.1:3000)
    --config-dir PATH  configuration/token directory (default: $FORGEVCS_CONFIG_DIR or
                       $XDG_CONFIG_HOME/forgevcs or ~/.config/forgevcs)
    --token-file PATH  bearer token file (default: <config-dir>/token)
    -h, --help         print this text

COMMANDS:
    auth status
        Show the target, stored principal/session and token presence.

    auth mint --login <login> [--secret-file PATH] [--show-token]
        Exchange a configured human secret for a bearer token. The secret is read from stdin
        (or --secret-file, or $FORGEVCS_HUMAN_SECRET) and is never printed; the token is stored
        in <config-dir>/token with mode 0600.

    auth logout
        Delete the locally stored token and session.

    health
        Probe /health, /ready and /version.

    init <repo>
        POST /repos/{repo}/init — create or bind a repository.

    refs <repo>
        GET /repos/{repo}/refs.

    ref create <repo> --name <ref> --target <changeset> [--reason TEXT]
        POST /repos/{repo}/refs — provision a ref at an existing changeset. Needed before the
        first `push --mode update` to a new lane ref (there is no implicit ref creation).

    log <repo> [--ref main] [--limit 100] [--path P] [--first-parent-only] [--page-token HEX]
        GET /repos/{repo}/commits.

    pull <repo> [--ref main] [--limit 100] [--path P] [--objects ID[,ID…]]
        POST /repos/{repo}/pull — history plus the requested object bytes.

    push <repo> --changeset <file> [--ref main] [--mode land|update|none]
        POST /repos/{repo}/push. The file is the façade PushBody JSON:
        {"objects":[{"id":"b1…","data":"<base64>"}],
         "changeset":{"parents":[],"delta":[{"path":"f","id":"b1…"}],
                      "meta":{"author":"A <a@b>","message":"m"},
                      "change_id":"<32 hex>"},
         "ref":"main"}
        --ref/--mode override the file's values.

    merge <repo> --a <id> --b <id> --change-id <32 hex> [--author "A <a@b>"] [--message M]
        POST /repos/{repo}/merge.

    rebase <repo> --changeset <id> --onto <id>
        POST /repos/{repo}/rebase.

    merge-land <repo> --head <id> [--ref main] [--author "A <a@b>"] [--message M]
        POST /repos/{repo}/merge-land.

    workspace create <repo> [--ref main | --changeset <id>] [--sparse a,b]
    workspace status <repo> <workspace-id>
    workspace destroy <repo> <workspace-id>

Every command prints the service's JSON response on stdout and exits non-zero on an HTTP error
(the façade error envelope's `code`/`message` are included). No Git wire protocol; no deploy.
"#
    .to_owned()
}

/// Parse the process arguments (without the program name).
pub fn parse(args: Vec<String>) -> Result<Parsed> {
    let mut global = GlobalOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" | "help" => return Ok(Parsed::Help(usage())),
            "--base-url" => global.base_url = Some(take_value(&args, &mut i, "--base-url")?),
            "--config-dir" => {
                global.config_dir = Some(PathBuf::from(take_value(&args, &mut i, "--config-dir")?));
            }
            "--token-file" => {
                global.token_file = Some(PathBuf::from(take_value(&args, &mut i, "--token-file")?));
            }
            raw if raw.starts_with('-') => bail!("unknown global option {raw:?}\n\n{}", usage()),
            _ => break,
        }
        i += 1;
    }
    if i >= args.len() {
        return Ok(Parsed::Help(usage()));
    }
    let command = parse_command(&args[i], &args[i + 1..])?;
    Ok(Parsed::Run(global, command))
}

fn take_value(args: &[String], i: &mut usize, name: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
}

fn parse_command(command: &str, rest: &[String]) -> Result<Command> {
    match command {
        "auth" => parse_auth(rest),
        "health" => {
            let opts = Options::parse(rest, &[], &[])?;
            opts.no_positional("health")?;
            Ok(Command::Health)
        }
        "init" => {
            let opts = Options::parse(rest, &[], &[])?;
            Ok(Command::Init {
                repo: opts.one_positional("init <repo>")?,
            })
        }
        "refs" => {
            let opts = Options::parse(rest, &[], &[])?;
            Ok(Command::Refs {
                repo: opts.one_positional("refs <repo>")?,
            })
        }
        "ref" => {
            let Some(sub) = rest.first() else {
                bail!("ref needs a subcommand: create");
            };
            if sub != "create" {
                bail!("unknown ref subcommand {sub:?} (expected create)");
            }
            let opts = Options::parse(&rest[1..], &["--name", "--target", "--reason"], &[])?;
            Ok(Command::RefCreate {
                repo: opts.one_positional("ref create <repo>")?,
                name: required(&opts, "--name", "ref create needs --name <ref>")?,
                target: required(&opts, "--target", "ref create needs --target <changeset>")?,
                reason: opts.value("--reason"),
            })
        }
        "log" => {
            let opts = Options::parse(
                rest,
                &["--ref", "--limit", "--path", "--page-token"],
                &["--first-parent-only"],
            )?;
            let repo = opts.one_positional("log <repo>")?;
            Ok(Command::Log {
                repo,
                ref_name: opts.value("--ref").unwrap_or_else(|| "main".to_owned()),
                limit: opts.parse_value("--limit", 100)?,
                path: opts.value("--path"),
                first_parent_only: opts.has("--first-parent-only"),
                page_token: opts.value("--page-token"),
            })
        }
        "pull" => {
            let opts = Options::parse(rest, &["--ref", "--limit", "--path", "--objects"], &[])?;
            let repo = opts.one_positional("pull <repo>")?;
            Ok(Command::Pull {
                repo,
                ref_name: opts.value("--ref").unwrap_or_else(|| "main".to_owned()),
                limit: opts.parse_value("--limit", 100)?,
                path: opts.value("--path"),
                objects: opts
                    .value("--objects")
                    .map(|raw| {
                        raw.split(',')
                            .map(str::trim)
                            .filter(|id| !id.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        }
        "push" => {
            let opts = Options::parse(rest, &["--changeset", "--ref", "--mode"], &[])?;
            let repo = opts.one_positional("push <repo>")?;
            let changeset = opts
                .value("--changeset")
                .ok_or_else(|| anyhow::anyhow!("push needs --changeset <file>"))?;
            let mode = opts.value("--mode");
            if let Some(mode) = &mode
                && !matches!(mode.as_str(), "land" | "update" | "none")
            {
                bail!("--mode must be one of land, update, none (got {mode:?})");
            }
            Ok(Command::Push {
                repo,
                changeset: PathBuf::from(changeset),
                ref_name: opts.value("--ref"),
                mode,
            })
        }
        "merge" => {
            let opts = Options::parse(
                rest,
                &["--a", "--b", "--change-id", "--author", "--message"],
                &[],
            )?;
            let repo = opts.one_positional("merge <repo>")?;
            Ok(Command::Merge {
                repo,
                a: required(&opts, "--a", "merge needs --a <changeset>")?,
                b: required(&opts, "--b", "merge needs --b <changeset>")?,
                change_id: required(&opts, "--change-id", "merge needs --change-id <32 hex>")?,
                author: opts.value("--author"),
                message: opts.value("--message"),
            })
        }
        "rebase" => {
            let opts = Options::parse(rest, &["--changeset", "--onto"], &[])?;
            let repo = opts.one_positional("rebase <repo>")?;
            Ok(Command::Rebase {
                repo,
                changeset: required(&opts, "--changeset", "rebase needs --changeset <id>")?,
                onto: required(&opts, "--onto", "rebase needs --onto <id>")?,
            })
        }
        "merge-land" => {
            let opts = Options::parse(
                rest,
                &["--head", "--ref", "--author", "--message", "--change-id"],
                &[],
            )?;
            let repo = opts.one_positional("merge-land <repo>")?;
            Ok(Command::MergeLand {
                repo,
                head: required(&opts, "--head", "merge-land needs --head <id>")?,
                ref_name: opts.value("--ref").unwrap_or_else(|| "main".to_owned()),
                author: opts.value("--author"),
                message: opts.value("--message"),
                change_id: opts.value("--change-id"),
            })
        }
        "workspace" => parse_workspace(rest),
        other => bail!("unknown command {other:?}\n\n{}", usage()),
    }
}

fn parse_auth(rest: &[String]) -> Result<Command> {
    let Some(sub) = rest.first() else {
        bail!("auth needs a subcommand: status, mint, logout");
    };
    match sub.as_str() {
        "status" => {
            let opts = Options::parse(&rest[1..], &[], &[])?;
            opts.no_positional("auth status")?;
            Ok(Command::AuthStatus)
        }
        "mint" => {
            let opts =
                Options::parse(&rest[1..], &["--login", "--secret-file"], &["--show-token"])?;
            opts.no_positional("auth mint")?;
            Ok(Command::AuthMint {
                login: opts
                    .value("--login")
                    .ok_or_else(|| anyhow::anyhow!("auth mint needs --login <login>"))?,
                secret_file: opts.value("--secret-file").map(PathBuf::from),
                show_token: opts.has("--show-token"),
            })
        }
        "logout" => {
            let opts = Options::parse(&rest[1..], &[], &[])?;
            opts.no_positional("auth logout")?;
            Ok(Command::AuthLogout)
        }
        other => bail!("unknown auth subcommand {other:?} (expected status, mint, logout)"),
    }
}

fn parse_workspace(rest: &[String]) -> Result<Command> {
    let Some(sub) = rest.first() else {
        bail!("workspace needs a subcommand: create, status, destroy");
    };
    match sub.as_str() {
        "create" => {
            let opts = Options::parse(&rest[1..], &["--ref", "--changeset", "--sparse"], &[])?;
            let repo = opts.one_positional("workspace create <repo>")?;
            if opts.value("--ref").is_some() && opts.value("--changeset").is_some() {
                bail!("workspace create takes --ref or --changeset, not both");
            }
            Ok(Command::WorkspaceCreate {
                repo,
                ref_name: opts.value("--ref"),
                changeset: opts.value("--changeset"),
                sparse: opts
                    .value("--sparse")
                    .map(|raw| {
                        raw.split(',')
                            .map(str::trim)
                            .filter(|path| !path.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        }
        "status" => {
            let opts = Options::parse(&rest[1..], &[], &[])?;
            let (repo, id) = opts.two_positional("workspace status <repo> <workspace-id>")?;
            Ok(Command::WorkspaceStatus { repo, id })
        }
        "destroy" => {
            let opts = Options::parse(&rest[1..], &[], &[])?;
            let (repo, id) = opts.two_positional("workspace destroy <repo> <workspace-id>")?;
            Ok(Command::WorkspaceDestroy { repo, id })
        }
        other => bail!("unknown workspace subcommand {other:?} (expected create, status, destroy)"),
    }
}

fn required(opts: &Options, name: &str, message: &str) -> Result<String> {
    opts.value(name).ok_or_else(|| anyhow::anyhow!("{message}"))
}

/// Parsed positional arguments plus `--flag value` / `--switch` options for one command.
#[derive(Debug, Default)]
struct Options {
    positional: Vec<String>,
    values: BTreeMap<String, String>,
    switches: Vec<String>,
}

impl Options {
    fn parse(raw: &[String], value_flags: &[&str], switch_flags: &[&str]) -> Result<Self> {
        let mut opts = Self::default();
        let mut i = 0;
        while i < raw.len() {
            let arg = &raw[i];
            if value_flags.contains(&arg.as_str()) {
                i += 1;
                let value = raw
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))?;
                if opts.values.insert(arg.clone(), value.clone()).is_some() {
                    bail!("{arg} was given more than once");
                }
            } else if switch_flags.contains(&arg.as_str()) {
                if opts.switches.contains(arg) {
                    bail!("{arg} was given more than once");
                }
                opts.switches.push(arg.clone());
            } else if arg.starts_with('-') {
                bail!("unknown option {arg:?}");
            } else {
                opts.positional.push(arg.clone());
            }
            i += 1;
        }
        Ok(opts)
    }

    fn value(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    fn parse_value<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        match self.values.get(name) {
            Some(raw) => raw
                .parse()
                .map_err(|error| anyhow::anyhow!("{name}: {error}")),
            None => Ok(default),
        }
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }

    fn one_positional(&self, usage: &str) -> Result<String> {
        match self.positional.as_slice() {
            [one] => Ok(one.clone()),
            _ => bail!("usage: {usage}"),
        }
    }

    fn two_positional(&self, usage: &str) -> Result<(String, String)> {
        match self.positional.as_slice() {
            [one, two] => Ok((one.clone(), two.clone())),
            _ => bail!("usage: {usage}"),
        }
    }

    fn no_positional(&self, usage: &str) -> Result<()> {
        if self.positional.is_empty() {
            Ok(())
        } else {
            bail!("usage: {usage}")
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn parse_args(args: &[&str]) -> Parsed {
        parse(args.iter().map(|arg| (*arg).to_owned()).collect()).expect("arguments parse")
    }

    fn command(args: &[&str]) -> Command {
        match parse_args(args) {
            Parsed::Run(_, command) => command,
            Parsed::Help(_) => panic!("expected a command"),
        }
    }

    #[test]
    fn global_options_precede_the_command() {
        match parse_args(&[
            "--base-url",
            "https://example.test",
            "--config-dir",
            "/tmp/cfg",
            "refs",
            "repo",
        ]) {
            Parsed::Run(global, Command::Refs { repo }) => {
                assert_eq!(global.base_url.as_deref(), Some("https://example.test"));
                assert_eq!(
                    global.config_dir.as_deref(),
                    Some(std::path::Path::new("/tmp/cfg"))
                );
                assert_eq!(repo, "repo");
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn help_is_available_with_or_without_a_command() {
        assert!(matches!(parse_args(&["--help"]), Parsed::Help(_)));
        assert!(matches!(parse(vec![],), Ok(Parsed::Help(_))));
        assert!(matches!(parse_args(&["help"]), Parsed::Help(_)));
    }

    #[test]
    fn every_command_parses() {
        assert!(matches!(command(&["health"]), Command::Health));
        assert!(matches!(command(&["auth", "status"]), Command::AuthStatus));
        assert!(matches!(
            command(&["auth", "mint", "--login", "culebraluxe"]),
            Command::AuthMint { .. }
        ));
        assert!(matches!(
            command(&["init", "repo"]),
            Command::Init { repo } if repo == "repo"
        ));
        assert!(matches!(command(&["refs", "repo"]), Command::Refs { repo } if repo == "repo"));
        assert!(matches!(
            command(&[
                "ref", "create", "repo", "--name", "lane/a", "--target", "c1aa"
            ]),
            Command::RefCreate { name, target, .. } if name == "lane/a" && target == "c1aa"
        ));
        assert!(matches!(
            command(&["log", "repo", "--ref", "main", "--limit", "5", "--first-parent-only"]),
            Command::Log {
                ref_name,
                limit: 5,
                first_parent_only: true,
                ..
            } if ref_name == "main"
        ));
        assert!(matches!(
            command(&["pull", "repo", "--objects", "b1aa,b1bb"]),
            Command::Pull { objects, .. } if objects == vec!["b1aa".to_owned(), "b1bb".to_owned()]
        ));
        assert!(matches!(
            command(&["push", "repo", "--changeset", "body.json", "--mode", "land"]),
            Command::Push { mode: Some(mode), .. } if mode == "land"
        ));
        assert!(matches!(
            command(&["merge", "repo", "--a", "a", "--b", "b", "--change-id", "cd"]),
            Command::Merge { .. }
        ));
        assert!(matches!(
            command(&["rebase", "repo", "--changeset", "c", "--onto", "o"]),
            Command::Rebase { .. }
        ));
        assert!(matches!(
            command(&["merge-land", "repo", "--head", "h"]),
            Command::MergeLand { ref_name, .. } if ref_name == "main"
        ));
        assert!(matches!(
            command(&["workspace", "create", "repo", "--sparse", "a,b"]),
            Command::WorkspaceCreate { sparse, .. } if sparse.len() == 2
        ));
        assert!(matches!(
            command(&["workspace", "status", "repo", "id"]),
            Command::WorkspaceStatus { .. }
        ));
        assert!(matches!(
            command(&["workspace", "destroy", "repo", "id"]),
            Command::WorkspaceDestroy { .. }
        ));
    }

    #[test]
    fn unknown_options_and_missing_values_are_errors() {
        for args in [
            vec!["init", "repo", "--nope"],
            vec!["push", "repo"],
            vec!["merge", "repo", "--a", "a"],
            vec![
                "workspace",
                "create",
                "repo",
                "--ref",
                "main",
                "--changeset",
                "c",
            ],
            vec!["--base-url"],
        ] {
            assert!(
                parse(args.iter().map(|arg| (*arg).to_owned()).collect()).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn invalid_push_mode_is_rejected_client_side() {
        assert!(
            parse(
                ["push", "repo", "--changeset", "f", "--mode", "force"]
                    .iter()
                    .map(|arg| (*arg).to_owned())
                    .collect()
            )
            .is_err()
        );
    }
}
