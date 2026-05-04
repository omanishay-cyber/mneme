//! `mneme` — the user-facing command-line tool.
//!
//! The binary is intentionally thin. Every subcommand maps 1:1 to a
//! handler in `crate::commands::<name>::run`. Errors bubble up as
//! [`CliError`], which has a stable [`CliError::exit_code`] that hooks
//! and shells can branch on.
//!
//! ```text
//! mneme install [--platform=<name>] [--dry-run] [--scope=...]
//! mneme uninstall [--platform=<name>] [--scope=...]
//! mneme register-mcp [--platform=<name>]
//! mneme unregister-mcp [--platform=<name>]
//! mneme rollback [--platform=<name>]
//! mneme models <op>            # status | install | install-onnx-runtime
//! mneme build [project_path]
//! mneme update [project_path]
//! mneme self-update [--force] [--check-only]
//! mneme status
//! mneme view [--web]
//! mneme audit [--scope=theme|security|all]
//! mneme recall <query> [--type=...] [--limit=N]
//! mneme blast <target> [--depth=N]
//! mneme graphify
//! mneme godnodes [--n=N]
//! mneme drift [--severity=...]
//! mneme history <query> [--since=...]
//! mneme graph-diff <from> <to> [--format=json|table|markdown] [--files=...] [--node-type=...]
//! mneme export --format=graphml|obsidian|cypher|svg|jsonld -o <path> [--kinds=...] [--files=...] [--max-nodes=N]
//! mneme snap
//! mneme doctor
//! mneme rebuild
//! mneme step <op> [arg]
//! mneme federated <op>
//! mneme why <target>
//! mneme inject  --prompt=... --session-id=... --cwd=...
//! mneme session-prime --project=... --session-id=...
//! mneme pre-tool   --tool=... --params=... --session-id=...
//! mneme post-tool  --tool=... --result-file=... --session-id=...
//! mneme turn-end   --session-id=... [--pre-compact|--subagent]
//! mneme session-end --session-id=...
//! mneme daemon <op>            # start | stop | status | service-run
//! mneme cache <op>             # du | clear
//! mneme abort
//! mneme mcp <op>               # stdio
//! ```
//!
//! Bug DOC-5 (2026-05-01): list extended from the original 22 to the
//! actual 33 subcommands (added: register-mcp, unregister-mcp, rollback,
//! models, federated, why, cache, abort, mcp). The previous list was
//! the v0.1 surface and never updated as the CLI grew.

use clap::{Parser, Subcommand};
use mneme_cli::commands;
use mneme_cli::error::{CliError, CliResult};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing::error;

/// Top-level CLI args.
#[derive(Debug, Parser)]
#[command(
    name = "mneme",
    version,
    about = "Mneme — the AI superbrain. Persistent per-project memory + 14-view graph + drift detector + 30+ MCP tools.",
    long_about = None,
    propagate_version = true,
)]
struct Cli {
    /// Lower the log threshold (-v=info, -vv=debug, -vvv=trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Force JSON-formatted log output (otherwise pretty).
    #[arg(long, global = true, env = "MNEME_LOG_JSON")]
    log_json: bool,

    /// Override the supervisor IPC socket path. Useful for tests.
    #[arg(long, global = true, env = "MNEME_SOCKET")]
    socket: Option<PathBuf>,

    /// The subcommand to run.
    #[command(subcommand)]
    cmd: Command,
}

/// Every mneme subcommand.
#[derive(Debug, Subcommand)]
enum Command {
    /// Install mneme into one or more AI platforms.
    Install(commands::install::InstallArgs),
    /// Reverse of `install`.
    Uninstall(commands::uninstall::UninstallArgs),
    /// Register ONLY the MCP server entry with the target platform —
    /// no hooks, no CLAUDE.md manifest. Preferred by the one-line
    /// installer and anyone who just wants the MCP tools without
    /// touching their settings.json or instructions file.
    #[command(name = "register-mcp")]
    RegisterMcp(commands::register_mcp::RegisterMcpArgs),
    /// Remove ONLY the MCP server entry. Inverse of `register-mcp`.
    #[command(name = "unregister-mcp")]
    UnregisterMcp(commands::register_mcp::RegisterMcpArgs),
    /// Reverse a previous install using its receipt. Restores every
    /// file mneme touched to its pre-install state (with sha256 drift
    /// detection so hand-edits aren't clobbered). Receipts live at
    /// `~/.mneme/install-receipts/`.
    Rollback(commands::rollback::RollbackArgs),
    /// Manage local models (embeddings, optional LLM).
    Models(commands::models::ModelsArgs),
    /// Initial full project ingest.
    Build(commands::build::BuildArgs),
    /// Incremental update.
    Update(commands::update::UpdateArgs),
    /// Replace the installed `mneme` binary set with the latest
    /// GitHub release. Distinct from `mneme update`, which is the
    /// project-incremental re-index command. Naming follows the
    /// convention of `rustup self update`, `gh self-update`, and
    /// `cargo install --self`: "update the binary itself" vs
    /// "update the project index".
    #[command(name = "self-update")]
    SelfUpdate(commands::self_update::SelfUpdateArgs),
    /// Print graph stats / drift findings count / last build time.
    Status(commands::status::StatusArgs),
    /// Open the vision app (Tauri or browser).
    View(commands::view::ViewArgs),
    /// Run all configured scanners.
    Audit(commands::audit::AuditArgs),
    /// Semantic recall against history / decisions / concepts / files.
    Recall(commands::recall::RecallArgs),
    /// Blast radius for a file or function.
    Blast(commands::blast::BlastArgs),
    /// Multimodal extraction pass (PDF, image, audio, video, .ipynb...).
    Graphify(commands::graphify::GraphifyArgs),
    /// Top-N most-connected concepts.
    Godnodes(commands::godnodes::GodNodesArgs),
    /// Show current drift findings.
    Drift(commands::drift::DriftArgs),
    /// Search the conversation history.
    History(commands::history::HistoryArgs),
    /// Show what changed between two graph snapshots: nodes added /
    /// removed / modified / renamed, plus optional edge-level changes.
    /// Snapshot identifiers accept labels, `HEAD`, `HEAD~N`, and
    /// explicit `.db` paths.
    #[command(name = "graph-diff")]
    GraphDiff(commands::graph_diff::GraphDiffArgs),
    /// Export the project's code-graph to a portable, tool-friendly
    /// format: GraphML (Yed/Cytoscape/Gephi), Obsidian wiki-link
    /// markdown, Neo4j Cypher script, static SVG, or schema.org
    /// JSON-LD. Filters mirror the rest of the CLI: `--kinds`,
    /// `--files`, `--max-nodes`.
    Export(commands::export::ExportArgs),
    /// Take a manual snapshot of the active shard.
    Snap(commands::snap::SnapArgs),
    /// Self-test the running daemon and shards.
    Doctor(commands::doctor::DoctorArgs),
    /// Drop everything and re-parse from scratch.
    Rebuild(commands::rebuild::RebuildArgs),
    /// Step Ledger operations.
    Step(commands::step::StepArgs),
    /// Moat 4: federated pattern matching (opt-in, privacy-first).
    Federated(commands::federated::FederatedArgs),
    /// Why-Chain: decision trace from ledger + git + concept graph.
    Why(commands::why::WhyArgs),
    /// Hook entry: UserPromptSubmit (emits JSON additional_context).
    Inject(commands::inject::InjectArgs),
    /// Hook entry: SessionStart.
    #[command(name = "session-prime")]
    SessionPrime(commands::session_prime::SessionPrimeArgs),
    /// Hook entry: PreToolUse.
    #[command(name = "pre-tool")]
    PreTool(commands::pre_tool::PreToolArgs),
    /// Hook entry: PostToolUse.
    #[command(name = "post-tool")]
    PostTool(commands::post_tool::PostToolArgs),
    /// Hook entry: Stop (between turns).
    #[command(name = "turn-end")]
    TurnEnd(commands::turn_end::TurnEndArgs),
    /// Hook entry: SessionEnd.
    #[command(name = "session-end")]
    SessionEnd(commands::session_end::SessionEndArgs),
    /// Daemon control: start | stop | restart | status | logs.
    Daemon(commands::daemon::DaemonArgs),
    /// Cache management: du | prune | gc | drop. (NEW-058)
    Cache(commands::cache::CacheArgs),
    /// Gracefully cancel an in-progress `mneme build`. Reads the
    /// per-project `.lock` stamp, sends SIGTERM/CTRL_C, waits up to
    /// `--timeout-secs` for graceful exit, then escalates to
    /// SIGKILL/TerminateProcess. Always runs `wal_checkpoint(TRUNCATE)`
    /// on every shard DB and removes the stale `.lock` afterwards so
    /// the next build starts from a clean slate.
    Abort(commands::abort::AbortArgs),
    /// Launch the Bun MCP server (used by Claude Code / Codex / etc.
    /// to talk to mneme via stdio). `mneme mcp stdio`.
    Mcp {
        /// Transport mode. Currently only `stdio` is supported.
        #[arg(default_value = "stdio")]
        transport: String,
    },
}

/// A1-031 (2026-05-04): force the Windows console to UTF-8 at startup
/// so user-facing strings containing characters like `>=`, `->`, `*`,
/// or `[ok]` (and any future Unicode glyph) render correctly instead
/// of mojibake (`>=` -> `ΓëÑ`, etc.). Equivalent to `chcp 65001`. Best-
/// effort: a non-Windows host or a host where SetConsoleOutputCP fails
/// (rare; legacy console host without UTF-8 support) silently falls
/// through. The fix is one Win32 call at startup; no per-string sweep.
#[cfg(windows)]
fn ensure_utf8_console() {
    extern "system" {
        fn SetConsoleOutputCP(wCodePageID: u32) -> i32;
        fn SetConsoleCP(wCodePageID: u32) -> i32;
    }
    const CP_UTF8: u32 = 65001;
    // Safety: SetConsoleOutputCP / SetConsoleCP are Win32 calls that take
    // a UINT and return BOOL; no pointer arithmetic, no aliasing.
    unsafe {
        let _ = SetConsoleOutputCP(CP_UTF8);
        let _ = SetConsoleCP(CP_UTF8);
    }
}

#[cfg(not(windows))]
fn ensure_utf8_console() {
    // POSIX consoles are UTF-8 by default; nothing to do.
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    ensure_utf8_console();

    let cli = Cli::parse();

    init_tracing(cli.verbose, cli.log_json);

    let result = dispatch(cli).await;

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Hook outputs are JSON; print the error there too so the host
            // can display something. For interactive commands we use the
            // tracing logger.
            error!(error = %err, exit_code = err.exit_code(), "mneme command failed");
            eprintln!("error: {err}");
            ExitCode::from(err.exit_code() as u8)
        }
    }
}

async fn dispatch(cli: Cli) -> CliResult<()> {
    let socket_override = cli.socket.clone();
    match cli.cmd {
        Command::Install(args) => commands::install::run(args).await,
        Command::Uninstall(args) => commands::uninstall::run(args).await,
        Command::RegisterMcp(args) => commands::register_mcp::register(args).await,
        Command::UnregisterMcp(args) => commands::register_mcp::unregister(args).await,
        Command::Rollback(args) => commands::rollback::run(args).await,
        Command::Models(args) => commands::models::run(args).await,
        Command::Build(args) => commands::build::run(args, socket_override).await,
        Command::Update(args) => commands::update::run(args, socket_override).await,
        Command::SelfUpdate(args) => commands::self_update::run(args).await,
        Command::Status(args) => commands::status::run(args, socket_override).await,
        Command::View(args) => commands::view::run(args).await,
        Command::Audit(args) => commands::audit::run(args, socket_override).await,
        Command::Recall(args) => commands::recall::run(args, socket_override).await,
        Command::Blast(args) => commands::blast::run(args, socket_override).await,
        Command::Graphify(args) => commands::graphify::run(args, socket_override).await,
        Command::Godnodes(args) => commands::godnodes::run(args, socket_override).await,
        Command::Drift(args) => commands::drift::run(args, socket_override).await,
        Command::History(args) => commands::history::run(args).await,
        Command::GraphDiff(args) => commands::graph_diff::run(args).await,
        Command::Export(args) => commands::export::run(args).await,
        Command::Snap(args) => commands::snap::run(args, socket_override).await,
        Command::Doctor(args) => commands::doctor::run(args, socket_override).await,
        Command::Rebuild(args) => commands::rebuild::run(args, socket_override).await,
        Command::Step(args) => commands::step::run(args, socket_override).await,
        Command::Federated(args) => commands::federated::run(args).await,
        Command::Why(args) => commands::why::cmd_why(args, socket_override).await,
        Command::Inject(args) => commands::inject::run(args, socket_override).await,
        Command::SessionPrime(args) => commands::session_prime::run(args, socket_override).await,
        Command::PreTool(args) => commands::pre_tool::run(args, socket_override).await,
        Command::PostTool(args) => commands::post_tool::run(args, socket_override).await,
        Command::TurnEnd(args) => commands::turn_end::run(args, socket_override).await,
        Command::SessionEnd(args) => commands::session_end::run(args, socket_override).await,
        Command::Daemon(args) => commands::daemon::run(args, socket_override).await,
        Command::Cache(args) => commands::cache::run(args).await,
        Command::Abort(args) => commands::abort::run(args).await,
        Command::Mcp { transport } => launch_mcp(transport).await,
    }
}

/// Exec into the Bun MCP server. Searches for `mcp/index.ts` at:
///   1. $MNEME_MCP_PATH (env var)
///   2. <PathManager::default_root()>/mcp/src/index.ts (production install,
///      honors MNEME_HOME)
///   3. <PathManager::default_root()>/mcp/index.ts (legacy production layout)
///   4. ./mcp/index.ts (development, relative to cwd)
async fn launch_mcp(transport: String) -> CliResult<()> {
    if transport != "stdio" {
        return Err(CliError::Other(format!(
            "only stdio transport supported, got {transport:?}"
        )));
    }
    let mneme_root = common::paths::PathManager::default_root()
        .root()
        .to_path_buf();
    let candidates: Vec<PathBuf> = [
        std::env::var("MNEME_MCP_PATH").ok().map(PathBuf::from),
        Some(mneme_root.join("mcp").join("src").join("index.ts")),
        Some(mneme_root.join("mcp").join("index.ts")),
        Some(PathBuf::from("mcp/src/index.ts")),
        Some(PathBuf::from("mcp/index.ts")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mcp_path = candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .ok_or_else(|| {
            CliError::Other(
                "mcp/index.ts not found — set MNEME_MCP_PATH or install the MCP server".into(),
            )
        })?;
    let bun = which_bun();
    let mut cmd = std::process::Command::new(&bun);
    cmd.arg(mcp_path);
    // Bug M11 (D-window class): suppress console-window allocation
    // when this Bun child is spawned from a windowless context. Today
    // `mneme.exe` is started by Claude Code (windowless) so the bun
    // child inherits its no-console state — but any future code path
    // that auto-spawns `mneme mcp stdio` from a windowless parent
    // would leak a console without this flag. See
    // `windows_launch_mcp_flags`.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_launch_mcp_flags());
    }
    let status = cmd
        .status()
        .map_err(|e| CliError::Other(format!("failed to spawn bun: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::Other(format!(
            "bun exited with {}",
            status.code().unwrap_or(-1)
        )))
    }
}

fn which_bun() -> String {
    // Prefer explicit env, then common per-OS install locations, then
    // bare "bun" on PATH.
    if let Ok(p) = std::env::var("MNEME_BUN") {
        return p;
    }
    #[cfg(windows)]
    {
        // 1. WinGet shim — official `winget install Oven-sh.Bun`.
        if let Ok(la) = std::env::var("LOCALAPPDATA") {
            let candidate = std::path::Path::new(&la).join(r"Microsoft\WinGet\Links\bun.exe");
            if candidate.exists() {
                return candidate.to_string_lossy().into();
            }
        }
        // 2. Official PowerShell installer drops to `%USERPROFILE%\.bun\bin\bun.exe`.
        if let Some(home) = dirs::home_dir() {
            let candidate = home.join(".bun").join("bin").join("bun.exe");
            if candidate.exists() {
                return candidate.to_string_lossy().into();
            }
        }
    }
    #[cfg(not(windows))]
    {
        // Official `curl -fsSL https://bun.sh/install | bash` drops to
        // `$HOME/.bun/bin/bun` on both Linux and macOS. No `.exe` suffix.
        if let Some(home) = dirs::home_dir() {
            let candidate = home.join(".bun").join("bin").join("bun");
            if candidate.exists() {
                return candidate.to_string_lossy().into();
            }
        }
        // Homebrew (macOS + Linuxbrew) installs to /opt/homebrew/bin or
        // /usr/local/bin; both are usually on PATH, so the bare "bun"
        // fallback picks them up. Explicit checks avoid relying on a
        // shell-inherited PATH that Rust's `std::process` may not see.
        for prefix in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            let candidate = std::path::Path::new(prefix).join("bun");
            if candidate.exists() {
                return candidate.to_string_lossy().into();
            }
        }
    }
    "bun".into()
}

/// Bug M11 (D-window class): canonical Windows process-creation
/// flags for the Bun MCP child spawned by `launch_mcp`. Sets
/// `CREATE_NO_WINDOW` (`0x08000000`) so no console window is
/// allocated when `mneme mcp stdio` is invoked from a windowless
/// parent. The constant is exposed unconditionally so pure-Rust unit
/// tests can pin the contract on every host platform — the
/// `cmd.creation_flags(...)` call site is `#[cfg(windows)]` only.
pub(crate) fn windows_launch_mcp_flags() -> u32 {
    /// CREATE_NO_WINDOW from `windows-sys`: suppresses console window
    /// allocation for the child process. Canonical Win32 doc:
    /// <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    CREATE_NO_WINDOW
}

fn init_tracing(verbose: u8, json: bool) {
    use tracing_subscriber::filter::EnvFilter;
    use tracing_subscriber::fmt;

    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let env_filter = EnvFilter::try_from_env("MNEME_LOG").unwrap_or_else(|_| EnvFilter::new(level));

    if json {
        let _ = fmt()
            .with_env_filter(env_filter)
            .json()
            .with_writer(std::io::stderr)
            .try_init();
    } else {
        let _ = fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .try_init();
    }
}

// Make the binary still buildable as `cargo build -p mneme-cli` even
// when the workspace's `common` crate isn't yet present.
#[allow(unused_imports)]
use mneme_cli as _;

#[cfg(test)]
mod tests {
    /// Bug M11 (D-window class): the Bun MCP child spawned from
    /// `launch_mcp` must include the Windows `CREATE_NO_WINDOW` flag
    /// (`0x08000000`). Today `mneme.exe` is started by Claude Code
    /// (windowless) so the bun child inherits its no-console state,
    /// but any future code path that auto-spawns `mneme mcp stdio`
    /// from a windowless context (e.g. a per-session MCP) will leak
    /// a console without this flag. The fix exposes a pure-Rust
    /// `windows_launch_mcp_flags()` helper that returns the canonical
    /// flag bitfield; this test pins the contract so future edits
    /// cannot silently regress it.
    #[test]
    fn windows_launch_mcp_flags() {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let flags = super::windows_launch_mcp_flags();
        assert_eq!(
            flags & CREATE_NO_WINDOW,
            CREATE_NO_WINDOW,
            "launch_mcp Bun spawn must set CREATE_NO_WINDOW (0x08000000); got {flags:#010x}"
        );
    }
}
