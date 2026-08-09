//! wukong: the command-line and terminal face of the governor. Bare
//! `wukong` opens the TUI dashboard; the verbs are thin wrappers over
//! the daemon socket, so scripts and prompts get the same governor the
//! dashboard drives.

mod adopt;
mod client;
mod init;
mod launchd;
mod pkg_cli;
mod settings_cli;
mod tui;

use clap::{Parser, Subcommand};
use wukong_core::events::Resolution;
use wukong_core::ipc::{Request, Response};

#[derive(Parser)]
#[command(
    name = "wukong",
    version,
    about = "Your Mac's governor: dotfiles and packages, watched and remembered",
    long_about = "wukong watches the parts of your Mac you care about and remembers \
them, so you never have to.\n\n\
Tracked dotfiles commit automatically to a private mirror repository the \
moment they stop changing — every commit passes a mandatory secret gate \
first, and anything suspicious is held in a review inbox instead of \
reaching git. Homebrew installs are recorded in a manifest that syncs \
with the same repository; installs made behind wukong's back are offered \
for adoption. Running `wukong` with no command opens the dashboard.\n\n\
State lives under XDG paths: config in ~/.config/wukong, the mirror \
repository and database in ~/.local/share/wukong, the daemon socket and \
log in ~/.local/state/wukong. The daemon runs as a launchd agent and is \
managed with `wukong daemon`.",
    after_help = "Run 'wukong <command> --help' for details on any command.",
    after_long_help = "EXAMPLES:\n  \
wukong init                      set this machine up\n  \
wukong adopt-dotfiles            find and track the usual dotfiles\n  \
wukong install jq                brew install, remembered\n  \
wukong                           open the dashboard\n  \
wukong status                    one-screen health summary"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Set up wukong on this machine
    #[command(
        long_about = "Set up wukong on this machine: detect the machine name, write the \
starter config, create the mirror store repository (cloning your remote \
if one exists — the new-machine bootstrap), install the launchd agent, \
and start the daemon. Idempotent: run it again any time to repair a \
half-configured machine."
    )]
    #[command(after_long_help = "EXAMPLES:\n  \
wukong init                      fresh machine, prompts for the remote\n  \
wukong restore                   then bring a cloned store's files live")]
    Init,

    /// Install a package and remember it in the manifest
    #[command(
        long_about = "Run the provider's own installer (its output streams through \
untouched) and record the package in the manifest, which commits and \
syncs like every dotfile. Homebrew formulae by default; --via selects \
any supported provider. If the daemon is down the install still \
works — the package is offered for adoption when it returns. App \
Store apps have no install verb here: install them in the App Store \
and wukong offers them for adoption."
    )]
    #[command(after_long_help = "EXAMPLES:\n  \
wukong install jq                             a brew formula\n  \
wukong install --cask raycast                 a GUI app\n  \
wukong install --via npm typescript           an npm global\n  \
wukong install --via go github.com/x/tool     a go binary (module path)\n  \
wukong install --via gem colorls              a user-installed gem\n  \
wukong install --no-track foo                 install without remembering")]
    Install {
        /// Package name, exactly as the provider knows it
        #[arg(value_name = "PACKAGE")]
        name: String,
        /// Which provider installs it (default: brew formula)
        #[arg(long, value_enum, conflicts_with = "cask")]
        via: Option<pkg_cli::ViaArg>,
        /// Shorthand for --via cask
        #[arg(long)]
        cask: bool,
        /// Install without recording it ("don't track this one")
        #[arg(long)]
        no_track: bool,
    },

    /// Uninstall a package and drop it from the manifest
    #[command(
        long_about = "Run the provider's own uninstaller and remove the package from the \
manifest. Any pending inbox offer for the package resolves itself. \
Providers without an uninstall command (go binaries, App Store apps) \
say so and leave the file to you."
    )]
    Rm {
        /// Package name, exactly as the provider knows it
        #[arg(value_name = "PACKAGE")]
        name: String,
        /// Which provider uninstalls it (default: brew formula)
        #[arg(long, value_enum, conflicts_with = "cask")]
        via: Option<pkg_cli::ViaArg>,
        /// Shorthand for --via cask
        #[arg(long)]
        cask: bool,
    },

    /// Package manifest: list, sync, adopt, ignore
    #[command(subcommand_value_name = "ACTION")]
    Pkg {
        #[command(subcommand)]
        action: PkgAction,
    },

    /// macOS settings: observe, record, apply
    #[command(subcommand_value_name = "ACTION")]
    #[command(
        long_about = "Govern macOS defaults the way files and packages are governed. The \
daemon watches a curated corpus of settings; when one changes, the \
inbox offers to record the new value. Recorded values live in a \
manifest that commits and syncs like everything else, and `settings \
sync` applies them on a new machine — including the Dock/Finder \
restarts each setting needs. Reads come straight from the preference \
plists; writes always go through `defaults`, keeping cfprefsd \
coherent."
    )]
    Settings {
        #[command(subcommand)]
        action: SettingsAction,
    },

    /// Start tracking files — their changes commit automatically
    #[command(
        long_about = "Start tracking one or more files. Each file is mirrored and committed \
immediately, then every future change commits on its own once the file \
stops changing. Every commit passes the secret gate: a detected \
credential holds the file in the inbox instead of reaching git. Files \
whose NAME is credential-bearing (.env, private keys, .netrc…) are \
refused outright."
    )]
    #[command(after_long_help = "EXAMPLES:\n  \
wukong track ~/.zshrc\n  \
wukong track ~/.zshrc ~/.gitconfig ~/.config/starship.toml")]
    Track {
        /// One or more files to govern
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
        /// Store as age ciphertext only — the sealed lane. Also the
        /// only way to track credential-named files (.env, .netrc…)
        #[arg(long)]
        sealed: bool,
    },

    /// Find this machine's well-known dotfiles and track them all
    #[command(
        long_about = "Scan a curated list of well-known single-file configs — shell startup \
files, git config, editor and terminal configs, tool settings — track \
everything that exists and isn't already tracked, after one \
confirmation. Each file still passes the secret gate individually: a \
token in your real .zshrc quarantines that one file without blocking \
the rest. Credential files are never on the candidate list."
    )]
    AdoptDotfiles {
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },

    /// Stop offering anything under a path (the noise valve)
    #[command(
        long_about = "Stop offering anything under a path, now and permanently: applied \
immediately, persisted to config, and every open inbox offer under the \
path is resolved away. Use it when an app churns its config and floods \
the inbox — or press 'x' on the offer in the dashboard, which excludes \
the offending file's directory. Tracked files are never affected: \
tracking always outranks excluding."
    )]
    #[command(after_long_help = "EXAMPLES:\n  \
wukong exclude ~/.config/noisyapp")]
    Exclude {
        /// Directory (or file) to stop watching
        #[arg(value_name = "PATH")]
        path: String,
    },

    /// Show what changed: live file vs the stored copy
    #[command(
        long_about = "Unified diff between the live file and its stored mirror copy — what \
would be committed at the next settle. Output is raw (this is you, \
reading your own file at your own terminal); empty means live and \
store agree."
    )]
    Diff {
        /// A tracked file
        #[arg(value_name = "PATH")]
        path: String,
    },

    /// Show the store's commit history for a tracked file
    Log {
        /// A tracked file
        #[arg(value_name = "PATH")]
        path: String,
        /// How many commits to show
        #[arg(short = 'n', long, default_value_t = 20, value_name = "COUNT")]
        limit: usize,
    },

    /// Move a tracked file to the sealed lane (ciphertext-only store)
    #[command(
        long_about = "Convert a tracked file to the sealed lane: from now on the store \
holds only age ciphertext, so the remote never sees its plaintext. The \
live file is untouched. The first seal on a machine creates the key \
pair: the private identity goes to the macOS Keychain (or the \
configured identity file), the public recipient into the store so \
every clone can encrypt. Move the identity to other machines with \
`wukong seal-key export` / `import`."
    )]
    Seal {
        /// A tracked file
        #[arg(value_name = "PATH")]
        path: String,
    },

    /// Return a sealed file to the plaintext lane, back through the gate
    #[command(
        long_about = "Return a sealed file to the plaintext lane. The content goes back \
through the secret gate — anything it finds is HELD in the inbox for \
review, exactly as if the file were newly tracked."
    )]
    Unseal {
        /// A sealed file
        #[arg(value_name = "PATH")]
        path: String,
    },

    /// Manage the seal identity (export, import, status)
    #[command(subcommand_value_name = "ACTION")]
    SealKey {
        #[command(subcommand)]
        action: SealKeyAction,
    },

    /// Stop tracking a file
    #[command(
        long_about = "Stop tracking a file: it is removed from the mirror (as a commit, so \
history is preserved) and its changes are no longer watched. The live \
file is untouched."
    )]
    Untrack {
        /// The tracked file to release
        #[arg(value_name = "PATH")]
        path: String,
    },

    /// One-screen health summary
    #[command(
        long_about = "One screen answering the questions that matter: which machine, which \
remote, how many files tracked, how many inbox items wait, how many \
commits are unpushed, and how long ago the last push landed."
    )]
    Status {
        /// Machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },

    /// List tracked files
    #[command(
        long_about = "List every tracked file. A leading '!' marks a file that is in the \
store but missing on disk (deleted, or not yet restored)."
    )]
    Files {
        /// Machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },

    /// Show the review inbox
    #[command(
        long_about = "List open inbox items: quarantined secrets, sentinel files offered \
for tracking, and packages offered for adoption or removal. Resolve \
them with `wukong resolve` or from the dashboard."
    )]
    Inbox {
        /// Machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },

    /// Resolve an inbox item
    #[command(
        long_about = "Resolve one inbox item by id. What each resolution means depends on \
the item:\n\n\
Quarantined secret — approve commits the secret and remembers the \
decision (this exact token never asks again; a rotated token does). \
redact commits with the secret masked in the stored copy, forever; the \
live file is never touched. ignore sets the item aside until the file \
changes again.\n\n\
Sentinel offer — approve starts tracking the file. ignore sets it \
aside; it may return when the file next changes.\n\n\
Package offer — approve adopts it into the manifest. ignore is the \
PERMANENT opt-out: the package is never offered again."
    )]
    #[command(after_long_help = "EXAMPLES:\n  \
wukong inbox                     find the item id\n  \
wukong resolve 3 approve\n  \
wukong resolve 3 redact          commit, but mask the secret")]
    Resolve {
        /// Item id, as shown by `wukong inbox`
        #[arg(value_name = "ID")]
        id: i64,
        #[arg(value_enum, value_name = "RESOLUTION")]
        resolution: ResolutionArg,
    },

    /// Push the store to its remote now
    #[command(
        long_about = "Push the mirror repository to its remote now, rather than waiting for \
the push timer. Reports the push's real outcome; a second push while \
one is running joins it and shares its result."
    )]
    Push,

    /// Copy stored files back to their live locations
    #[command(
        long_about = "Copy stored files back to their live locations and track them — the \
new-machine bootstrap after `wukong init` cloned an existing store. \
Restored files come back owner-only (0600). Existing live files that \
differ are skipped unless --force is given."
    )]
    #[command(after_long_help = "EXAMPLES:\n  \
wukong restore                   everything in the store\n  \
wukong restore ~/.zshrc          one file\n  \
wukong restore --force           overwrite files that differ")]
    Restore {
        /// One file to restore (default: everything in the store)
        #[arg(value_name = "PATH")]
        path: Option<String>,
        /// Overwrite live files that differ from the stored copy
        #[arg(long)]
        force: bool,
    },

    /// Manage the background daemon
    #[command(subcommand_value_name = "ACTION")]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Remove wukong from this machine
    #[command(
        long_about = "Stop the daemon and remove the launchd agent. Your data — the config, \
the database, and the LOCAL store repository — is kept unless --purge \
is given; the remote store is never touched either way. Binaries are \
left in place (wukong cannot safely delete itself); the command prints \
where they are."
    )]
    #[command(after_long_help = "EXAMPLES:\n  \
wukong uninstall                 stop the daemon, keep all data\n  \
wukong uninstall --purge         remove local data too (confirms first)")]
    Uninstall {
        /// Also delete config, database, and the local store repository
        #[arg(long)]
        purge: bool,
        /// Skip the confirmation prompt (with --purge)
        #[arg(long)]
        yes: bool,
    },

    /// Check the health of the whole setup
    #[command(
        long_about = "Check the whole setup: config parses, store exists, remote configured \
and REACHABLE, daemon running, launchd agent installed — plus the \
tracked/inbox/unpushed counts and the last push age."
    )]
    Doctor,

    /// Generate man pages into a directory
    #[command(hide = true)]
    GenMan {
        #[arg(value_name = "DIR")]
        dir: std::path::PathBuf,
    },

    /// Generate a shell completion script on stdout
    #[command(hide = true)]
    GenCompletions {
        #[arg(value_enum, value_name = "SHELL")]
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
pub enum SealKeyAction {
    /// Print the private identity for transfer to another machine
    #[command(
        long_about = "Print the age identity to stdout — the ONE secret that unlocks \
every sealed file. Move it to another machine through a channel you \
trust (password manager, AirDrop), then `wukong seal-key import` \
there. Anyone holding this line can read your sealed files."
    )]
    Export,
    /// Read an identity from stdin and store it on this machine
    Import,
    /// Is the identity present, and does the recipient match?
    Status,
}

#[derive(Subcommand)]
pub enum SettingsAction {
    /// Every governed setting: recorded value, live value, drift
    #[command(
        long_about = "Every governed setting — the curated corpus plus anything recorded \
by hand. '·' marks observed-only settings (no recorded value), '!' \
marks drift (recorded value differs from this machine)."
    )]
    List {
        /// Machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },
    /// Show only the drift: recorded values this machine doesn't match
    Diff,
    /// Apply every recorded value this machine doesn't match
    #[command(
        long_about = "Apply every recorded setting this machine has drifted from, via \
`defaults write`, then restart the affected processes (Dock, Finder, \
SystemUIServer…) once each. Shows the plan and confirms first."
    )]
    Sync {
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Find out what a settings change actually changed, and record it
    #[command(
        long_about = "The discovery tool for settings outside the corpus: snapshot every \
preference key, change the setting anywhere (System Settings, an app's \
preferences, `defaults write`), diff, and record what you choose. App \
furniture — window positions, timestamps, session state — is filtered \
from the signal; --all shows it anyway. Recorded keys become governed: \
watched for change, synced through the store, applied by `settings \
sync`. Scope: top-level scalar keys in ~/Library/Preferences (ByHost \
and sandboxed-container domains are not yet captured)."
    )]
    #[command(after_long_help = "EXAMPLES:\n  \
wukong settings capture          interactive: snapshot, wait, pick\n  \
wukong settings capture --start  scripting: phase 1\n  \
wukong settings capture --diff --json   scripting: phase 2")]
    Capture {
        /// Only snapshot (phase 1 of the scripted flow)
        #[arg(long, conflicts_with = "diff")]
        start: bool,
        /// Only diff against the snapshot (phase 2)
        #[arg(long)]
        diff: bool,
        /// Include filtered noise in the output
        #[arg(long)]
        all: bool,
        /// Machine-readable JSON on stdout (with --diff)
        #[arg(long)]
        json: bool,
    },

    /// Record a setting's current value as this machine's desired value
    #[command(
        long_about = "Record the CURRENT live value of a setting into the manifest — the \
explicit path for settings outside the curated corpus. Set the value \
first (System Settings or `defaults write`), then record it."
    )]
    #[command(after_long_help = "EXAMPLES:\n  \
wukong settings record com.apple.dock autohide\n  \
wukong settings record NSGlobalDomain KeyRepeat")]
    Record {
        /// Preference domain (`NSGlobalDomain` for the global one)
        #[arg(value_name = "DOMAIN")]
        domain: String,
        #[arg(value_name = "KEY")]
        key: String,
    },
    /// Never offer this setting again
    Ignore {
        #[arg(value_name = "DOMAIN")]
        domain: String,
        #[arg(value_name = "KEY")]
        key: String,
    },
    /// Allow a previously ignored setting to be offered again
    Unignore {
        #[arg(value_name = "DOMAIN")]
        domain: String,
        #[arg(value_name = "KEY")]
        key: String,
    },
}

#[derive(Subcommand)]
pub enum PkgAction {
    /// Manifest entries and whether each is actually installed
    #[command(
        long_about = "Every manifest entry with its live state. A leading '!' marks a \
package that is in the manifest but not installed — `wukong pkg sync` \
installs those."
    )]
    List {
        /// Machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },

    /// Install everything in the manifest that's missing
    #[command(
        long_about = "Install every missing manifest package via its own provider, grouped \
and shown as the exact commands before anything runs. App Store apps \
install through `mas` when their id was captured at adoption; the rest \
of the apps come back as a checklist."
    )]
    Sync {
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
        /// Show the plan without executing anything
        #[arg(long)]
        dry_run: bool,
    },

    /// Bulk-adopt everything installed on request, across providers
    #[command(
        long_about = "Day-one onboarding: put everything installed on request — brew \
formulae and casks (dependencies never count), npm/pnpm/bun globals, \
cargo and go binaries, gems, pipx/uv tools, dotnet tools, pub globals — \
straight into the manifest, in one commit. Apps (App Store included) \
stay offer-driven: a used Mac's /Applications is too noisy to adopt \
wholesale."
    )]
    AdoptInstalled,

    /// Where every provider is observed, and why (or why not)
    #[command(
        long_about = "One row per provider: the root wukong watches, how it was found \
(fixed path, probed once at startup, or a [packages.roots] override), \
whether it is active, and how many packages it currently sees. The \
answer to \"why isn't X being offered?\"."
    )]
    Providers {
        /// Machine-readable JSON on stdout
        #[arg(long)]
        json: bool,
    },

    /// Never offer this package again
    #[command(
        long_about = "The permanent per-package opt-out: recorded in the manifest's ignore \
list (so it syncs), honored across reinstalls. Undo with \
`wukong pkg unignore`."
    )]
    Ignore {
        /// Package or app name
        #[arg(value_name = "NAME")]
        name: String,
        /// Which provider it belongs to
        #[arg(long, value_enum, default_value = "formula")]
        via: PkgProviderArg,
    },

    /// Allow a previously ignored package to be offered again
    Unignore {
        /// Package or app name
        #[arg(value_name = "NAME")]
        name: String,
        /// Which provider it belongs to
        #[arg(long, value_enum, default_value = "formula")]
        via: PkgProviderArg,
    },
}

/// Every provider including App — ignore/unignore applies to apps too.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum PkgProviderArg {
    Formula,
    Cask,
    App,
    Mas,
    Npm,
    Pnpm,
    Bun,
    Cargo,
    Go,
    Gem,
    Pipx,
    Uv,
    Dotnet,
    Pub,
}

impl From<PkgProviderArg> for wukong_core::pkg::Provider {
    fn from(arg: PkgProviderArg) -> Self {
        use wukong_core::pkg::Provider as P;
        match arg {
            PkgProviderArg::Formula => P::Formula,
            PkgProviderArg::Cask => P::Cask,
            PkgProviderArg::App => P::App,
            PkgProviderArg::Mas => P::Mas,
            PkgProviderArg::Npm => P::Npm,
            PkgProviderArg::Pnpm => P::Pnpm,
            PkgProviderArg::Bun => P::Bun,
            PkgProviderArg::Cargo => P::Cargo,
            PkgProviderArg::Go => P::Go,
            PkgProviderArg::Gem => P::Gem,
            PkgProviderArg::Pipx => P::Pipx,
            PkgProviderArg::Uv => P::Uv,
            PkgProviderArg::Dotnet => P::Dotnet,
            PkgProviderArg::Pub => P::Pub,
        }
    }
}

#[derive(Subcommand, Clone, Copy)]
pub enum DaemonAction {
    /// Start the daemon (installing the launchd agent if needed)
    Start,
    /// Stop the daemon and unload the launchd agent
    Stop,
    /// Restart the daemon (required after editing config.toml)
    Restart,
    /// Is the daemon running? (exit code 0 = yes, 1 = no)
    Status,
}

/// The resolution verbs, with their meaning spelled out where clap
/// shows possible values. Converted to the core type at the boundary.
#[derive(Clone, Copy, clap::ValueEnum)]
enum ResolutionArg {
    /// Accept: commit the secret / track the file / adopt the package
    Approve,
    /// Commit with the secret masked in the stored copy, forever
    Redact,
    /// Set aside (for package offers: permanently)
    Ignore,
    /// Quarantines only: the whole file moves to the ciphertext lane
    Seal,
}

impl From<ResolutionArg> for Resolution {
    fn from(arg: ResolutionArg) -> Self {
        match arg {
            ResolutionArg::Approve => Resolution::Approve,
            ResolutionArg::Redact => Resolution::Redact,
            ResolutionArg::Ignore => Resolution::Ignore,
            ResolutionArg::Seal => Resolution::Seal,
        }
    }
}

/// `--via`/`--cask` to a provider: cask sugar wins over the default.
fn resolve_via(via: Option<pkg_cli::ViaArg>, cask: bool) -> wukong_core::pkg::Provider {
    if cask {
        wukong_core::pkg::Provider::Cask
    } else {
        via.map_or(wukong_core::pkg::Provider::Formula, Into::into)
    }
}

/// seal-key runs CLI-side: the identity store and the store repo are
/// both reachable without the daemon.
fn seal_key(action: &SealKeyAction) -> anyhow::Result<()> {
    use wukong_core::{paths, seal};
    let config = match wukong_core::Config::load() {
        Ok(Some(config)) => config,
        _ => wukong_core::Config::default(),
    };
    let store = seal::IdentityStore::from_config(config.seal.identity_file.as_deref());
    let recipient_path = paths::store_dir().join(seal::RECIPIENT_REL);
    match action {
        SealKeyAction::Export => match store.load()? {
            Some(identity) => {
                eprintln!("# The one secret that unlocks every sealed file. Guard it.");
                println!("{identity}");
                Ok(())
            }
            None => anyhow::bail!("no seal identity on this machine"),
        },
        SealKeyAction::Import => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let identity = line.trim();
            if !identity.starts_with("AGE-SECRET-KEY-1") {
                anyhow::bail!("that does not look like an age identity");
            }
            // Prove it decrypts what this store encrypts, when a
            // recipient exists to check against.
            if let Ok(recipient) = std::fs::read_to_string(&recipient_path) {
                let probe = seal::encrypt(recipient.trim(), b"probe")?;
                if seal::decrypt(identity, &probe).is_err() {
                    anyhow::bail!("identity does not match this store's recipient — wrong key?");
                }
            }
            store.save(identity)?;
            println!("seal identity imported");
            Ok(())
        }
        SealKeyAction::Status => {
            let has_identity = store.load()?.is_some();
            let recipient = std::fs::read_to_string(&recipient_path).ok();
            println!(
                "identity   {}",
                if has_identity { "present" } else { "missing" }
            );
            match &recipient {
                Some(r) => println!("recipient  {}", r.trim()),
                None => println!("recipient  (none — nothing sealed yet)"),
            }
            if let (true, Some(r)) = (has_identity, &recipient) {
                let identity = store.load()?.expect("checked");
                let probe = seal::encrypt(r.trim(), b"probe")?;
                let matches = seal::decrypt(&identity, &probe).is_ok();
                println!(
                    "match      {}",
                    if matches {
                        "identity unlocks this store"
                    } else {
                        "MISMATCH — this identity cannot decrypt this store"
                    }
                );
            }
            Ok(())
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => tui::run(),
        Some(Command::Init) => init::run(),
        Some(Command::Install {
            name,
            via,
            cask,
            no_track,
        }) => pkg_cli::install(&name, resolve_via(via, cask), no_track),
        Some(Command::Rm { name, via, cask }) => pkg_cli::rm(&name, resolve_via(via, cask)),
        Some(Command::Pkg { action }) => match action {
            PkgAction::List { json } => pkg_cli::list(json),
            PkgAction::Sync { yes, dry_run } => pkg_cli::sync(yes, dry_run),
            PkgAction::AdoptInstalled => pkg_cli::adopt_installed(),
            PkgAction::Providers { json } => pkg_cli::providers(json),
            PkgAction::Ignore { name, via } => pkg_cli::ignore(&name, via.into(), false),
            PkgAction::Unignore { name, via } => pkg_cli::ignore(&name, via.into(), true),
        },
        Some(Command::Settings { action }) => match action {
            SettingsAction::List { json } => settings_cli::list(json),
            SettingsAction::Diff => settings_cli::diff(),
            SettingsAction::Sync { yes } => settings_cli::sync(yes),
            SettingsAction::Capture {
                start,
                diff,
                all,
                json,
            } => {
                let phase = if start {
                    settings_cli::CapturePhase::Start
                } else if diff {
                    settings_cli::CapturePhase::Diff
                } else {
                    settings_cli::CapturePhase::Interactive
                };
                settings_cli::capture(&phase, all, json)
            }
            SettingsAction::Record { domain, key } => settings_cli::record(&domain, &key),
            SettingsAction::Ignore { domain, key } => settings_cli::ignore(&domain, &key, false),
            SettingsAction::Unignore { domain, key } => settings_cli::ignore(&domain, &key, true),
        },
        Some(Command::Track { paths, sealed }) => {
            for path in paths {
                say(Request::Track { path, sealed })?;
            }
            Ok(())
        }
        Some(Command::Seal { path }) => say(Request::Seal { path }),
        Some(Command::Unseal { path }) => say(Request::Unseal { path }),
        Some(Command::SealKey { action }) => seal_key(&action),
        Some(Command::AdoptDotfiles { yes }) => adopt::run(yes),
        Some(Command::Exclude { path }) => say(Request::Exclude { path }),
        Some(Command::Diff { path }) => say(Request::Diff { path }),
        Some(Command::Log { path, limit }) => say(Request::FileLog { path, limit }),
        Some(Command::Untrack { path }) => say(Request::Untrack { path }),
        Some(Command::Status { json }) => status(json),
        Some(Command::Files { json }) => files(json),
        Some(Command::Inbox { json }) => inbox(json),
        Some(Command::Resolve { id, resolution }) => say(Request::InboxResolve {
            id,
            resolution: resolution.into(),
        }),
        Some(Command::Push) => say(Request::PushNow),
        Some(Command::Restore { path, force }) => say(Request::Restore { path, force }),
        Some(Command::Daemon { action }) => launchd::run(action),
        Some(Command::Uninstall { purge, yes }) => launchd::uninstall(purge, yes),
        Some(Command::Doctor) => {
            doctor();
            Ok(())
        }
        Some(Command::GenMan { dir }) => gen_man(&dir),
        Some(Command::GenCompletions { shell }) => {
            use clap::CommandFactory as _;
            clap_complete::generate(shell, &mut Cli::command(), "wukong", &mut std::io::stdout());
            Ok(())
        }
    }
}

/// Render roff man pages: wukong.1 plus one page per visible
/// subcommand (wukong-track.1, wukong-pkg-sync.1, …).
fn gen_man(dir: &std::path::Path) -> anyhow::Result<()> {
    use clap::CommandFactory as _;
    std::fs::create_dir_all(dir)?;
    let mut root = Cli::command();
    root.build();
    render_man(&root, "wukong", dir)?;
    println!("man pages written to {}", dir.display());
    Ok(())
}

fn render_man(cmd: &clap::Command, name: &str, dir: &std::path::Path) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    clap_mangen::Man::new(cmd.clone()).render(&mut buf)?;
    std::fs::write(dir.join(format!("{name}.1")), buf)?;
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() || sub.get_name() == "help" {
            continue;
        }
        render_man(sub, &format!("{name}-{}", sub.get_name()), dir)?;
    }
    Ok(())
}

/// Fire a request whose success is just a message.
pub fn say(req: Request) -> anyhow::Result<()> {
    match client::call(req)? {
        Response::Ok { message } => {
            println!("{message}");
            Ok(())
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        other => {
            println!("{other:?}");
            Ok(())
        }
    }
}

fn status(json: bool) -> anyhow::Result<()> {
    let Response::Status(s) = client::call(Request::Status)? else {
        anyhow::bail!("unexpected response");
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }
    println!("machine   {}", s.machine);
    println!(
        "remote    {}",
        if s.remote.is_empty() {
            "(local only)"
        } else {
            &s.remote
        }
    );
    println!("tracked   {} files", s.tracked);
    println!(
        "inbox     {} item{}",
        s.inbox,
        if s.inbox == 1 { "" } else { "s" }
    );
    println!("unpushed  {} commit(s)", s.unpushed);
    println!("last push {}", age_of(s.last_push.as_deref()));
    println!("uptime    {}", human_secs(s.uptime_secs));
    Ok(())
}

fn files(json: bool) -> anyhow::Result<()> {
    let Response::Tracked { files } = client::call(Request::TrackedList)? else {
        anyhow::bail!("unexpected response");
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&files)?);
        return Ok(());
    }
    if files.is_empty() {
        println!("nothing tracked yet — `wukong track ~/.zshrc`");
    }
    for f in files {
        let mark = if f.exists { " " } else { "!" };
        println!("{mark} {}", f.display);
    }
    Ok(())
}

fn inbox(json: bool) -> anyhow::Result<()> {
    let Response::Inbox { items } = client::call(Request::InboxList)? else {
        anyhow::bail!("unexpected response");
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("inbox is clear");
        return Ok(());
    }
    for item in items {
        println!("#{}  {}  {}", item.id, item.subject, item.detail);
    }
    println!("\nresolve with `wukong resolve <id> approve|redact|ignore`");
    Ok(())
}

fn doctor() {
    use wukong_core::{Config, paths};
    let check = |ok: bool, label: &str| println!("{} {label}", if ok { "✓" } else { "✗" });

    let config = match Config::load() {
        Ok(Some(config)) => {
            check(true, "config parses");
            config
        }
        Ok(None) => {
            check(false, "config missing — run `wukong init`");
            Config::default()
        }
        Err(e) => {
            check(false, &format!("config broken: {e}"));
            Config::default()
        }
    };
    check(!config.machine.is_empty(), "initialized (machine name set)");
    check(
        paths::store_dir().join(".git").exists(),
        "store repo exists",
    );
    check(!config.remote.is_empty(), "remote configured");
    if !config.remote.is_empty() {
        check(remote_reachable(&config.remote), "remote reachable");
    }
    check(client::connected(), "daemon running");
    check(launchd::agent_path().exists(), "launchd agent installed");
    if client::connected()
        && let Ok(Response::Status(s)) = client::call(Request::Status)
    {
        println!(
            "\n{} tracked · {} inbox · {} unpushed · last push {}",
            s.tracked,
            s.inbox,
            s.unpushed,
            age_of(s.last_push.as_deref())
        );
    }
}

/// "2h ago" from an RFC3339 timestamp — the remote-machine question
/// is "is it still syncing", and a raw timestamp doesn't answer it.
fn age_of(ts: Option<&str>) -> String {
    let Some(ts) = ts else {
        return "(never)".to_string();
    };
    let Ok(then) = ts.parse::<jiff::Timestamp>() else {
        return ts.to_string();
    };
    let secs = jiff::Timestamp::now().duration_since(then).as_secs();
    match u64::try_from(secs) {
        Ok(secs) => format!("{} ago", human_secs(secs)),
        Err(_) => ts.to_string(),
    }
}

/// Prompt-free, fast-failing reachability probe.
fn remote_reachable(remote: &str) -> bool {
    std::process::Command::new("git")
        .args(["ls-remote", "--heads", remote])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=10",
        )
        .output()
        .is_ok_and(|o| o.status.success())
}

fn human_secs(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    }
}
