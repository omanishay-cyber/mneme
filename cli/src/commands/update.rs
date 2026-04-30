//! `mneme update [project_path]` — incremental update sweep.
//!
//! v0.3.1: direct-DB path. Delegates to the same inline parse pipeline
//! `mneme build` uses, with `--full=false` semantics — only files
//! modified since the last build get re-parsed. No supervisor round-
//! trip; works even when the daemon is down.

use clap::Args;
use std::path::PathBuf;

use crate::commands::build::{resolve_project, BuildArgs};
use crate::error::CliResult;

/// CLI args for `mneme update`.
#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Path to the project root. Defaults to CWD.
    pub project: Option<PathBuf>,

    /// Skip the pre-flight file-count confirmation. Equivalent to
    /// `mneme build --yes`. Update already targets an existing shard,
    /// so this is usually safe to set.
    #[arg(long, short = 'y', default_value_t = true)]
    pub yes: bool,
}

/// Entry point used by `main.rs`.
pub async fn run(args: UpdateArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    let project = resolve_project(args.project)?;
    tracing::info!(project = %project.display(), "incremental update (via build pipeline)");

    // Reuse the build pipeline — full=false means "only changed files"
    // (the pipeline's incremental parse path is keyed off mtime + sha).
    let build_args = BuildArgs {
        project: Some(project),
        full: false,
        limit: 0,
        dispatch: false,
        inline: true,
        yes: args.yes,
        // L4: incremental update is single-writer same as full build —
        // 0 = fail-fast if a competing build is in flight.
        lock_timeout_secs: 0,
        // B-017: incremental update reuses the full build pipeline
        // including the silent embed/graph passes. Default to
        // noisy (`quiet=false`) so the heartbeat fires.
        quiet: false,
    };
    crate::commands::build::run(build_args, socket_override).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Smoke clap harness — verify args parser without spinning up the
    /// full binary.
    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        args: UpdateArgs,
    }

    #[test]
    fn update_args_default_yes_true() {
        // Default behaviour is non-interactive; users get a confirmation
        // bypass because update targets an existing shard.
        let args = UpdateArgs {
            project: None,
            yes: true,
        };
        assert!(args.yes);
    }

    #[test]
    fn update_args_parse_with_no_flags_defaults_yes_true() {
        // Default value for --yes is true (per #[arg(default_value_t = true)]).
        let h = Harness::try_parse_from(["x"]).unwrap();
        assert!(h.args.project.is_none());
        assert!(h.args.yes, "default --yes must be true");
    }

    #[test]
    fn update_args_parse_with_short_yes_flag() {
        // `-y` is the short alias for `--yes`. Even though it defaults
        // true, accepting -y must be a no-op (confirmed true).
        let h = Harness::try_parse_from(["x", "-y"]).unwrap();
        assert!(h.args.yes);
    }

    #[test]
    fn update_args_parse_with_explicit_project_path() {
        // Positional project argument resolved to PathBuf.
        let h = Harness::try_parse_from(["x", "/some/path"]).unwrap();
        assert_eq!(
            h.args.project.as_ref().unwrap(),
            &PathBuf::from("/some/path")
        );
    }
}
