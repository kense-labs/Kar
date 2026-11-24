use clap::{Args, Subcommand};
use kar::{
    Result,
    workbench_git::{BranchQuery, Repository},
};
use std::{ffi::OsString, path::PathBuf, time::Duration};
use uuid::Uuid;

#[derive(Args)]
pub struct Git {
    /// Checkout for repository operations.
    #[arg(long, default_value = ".", global = true)]
    repo: PathBuf,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Initialize Git in an existing directory, preserving its files.
    Init,
    /// List branches in this checkout, with optional filtering and paging.
    Refs(BranchQuery),
    CreateBranch {
        branch: String,
        #[arg(long)]
        switch: bool,
    },
    SwitchBranch {
        branch: String,
    },
    Clone {
        source: OsString,
        destination: PathBuf,
    },
    Status,
    /// Print one JSON snapshot immediately and another when status changes.
    Watch {
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(1..))]
        interval_ms: u64,
    },
    /// Create a new branch and checkout; reuse the request ID to retry.
    WorktreeAdd {
        id: Uuid,
        path: PathBuf,
        branch: String,
        #[arg(long, default_value = "HEAD")]
        start: String,
    },
    /// Remove a clean managed checkout while retaining its branch.
    WorktreeRemove {
        id: Uuid,
    },
    Stage {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    Commit {
        #[arg(short, long)]
        message: String,
    },
    Push,
    Pull,
}

impl Git {
    pub fn run(self) -> Result<()> {
        if let Action::Clone {
            source,
            destination,
        } = self.command
        {
            let repo = Repository::clone_into(&source, &destination)?;
            return super::json_line(&repo.status()?, std::io::stdout());
        }
        if let Action::Refs(query) = &self.command {
            return super::json_line(
                &Repository::branches_in(&self.repo, query)?,
                std::io::stdout(),
            );
        }
        let repo = if matches!(self.command, Action::Init) {
            Repository::init(&self.repo)?
        } else {
            Repository::open(&self.repo)?
        };
        match self.command {
            Action::Clone { .. } | Action::Refs(_) => unreachable!(),
            Action::Init | Action::Status => super::json_line(&repo.status()?, std::io::stdout()),
            Action::CreateBranch { branch, switch } => {
                repo.create_branch(&branch, switch)?;
                super::json_line(&repo.status()?, std::io::stdout())
            }
            Action::SwitchBranch { branch } => {
                repo.switch_branch(&branch)?;
                super::json_line(&repo.status()?, std::io::stdout())
            }
            Action::Watch { interval_ms } => {
                repo.watch(Duration::from_millis(interval_ms), |status| {
                    super::json_line(status, std::io::stdout())?;
                    Ok(true)
                })
            }
            Action::WorktreeAdd {
                id,
                path,
                branch,
                start,
            } => super::json_line(
                &repo.create_worktree(id, &path, &branch, &start)?,
                std::io::stdout(),
            ),
            Action::WorktreeRemove { id } => repo.remove_worktree(id),
            Action::Stage { paths } => repo.stage(&paths),
            Action::Commit { message } => {
                super::json_line(&repo.commit(&message)?, std::io::stdout())
            }
            Action::Push => repo.push(),
            Action::Pull => super::json_line(&repo.pull()?, std::io::stdout()),
        }
    }
}
