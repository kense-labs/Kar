use clap::{Args, Subcommand};
use kar::{
    Error, Result,
    chat::{
        ChatCursor, ChatHistory, ChatNotification, ChatSession, ChatStore, ChatTurn,
        CheckpointPoint, Item, QueuedInput, TurnStatus,
    },
};
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};
use tokio::io::AsyncBufReadExt;
use uuid::Uuid;

#[derive(Args)]
pub(super) struct Chat {
    /// Directory containing saved chats. Defaults to the configured chat store.
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    #[arg(long, global = true)]
    pub(super) json: bool,
    #[command(subcommand)]
    action: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Create a durable chat, optionally sending its first prompt immediately.
    New {
        #[command(flatten)]
        settings: Box<super::NativeSettings>,
        runtime: String,
        #[command(flatten)]
        message: Message,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long)]
        bin: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Continue the latest or selected chat in its saved workspace.
    Resume {
        #[arg(long)]
        chat: Option<Uuid>,
        /// Pass a native argument for this launch. Repeat for separate arguments.
        #[arg(long = "launch-arg", value_name = "ARG", allow_hyphen_values = true)]
        launch_args: Vec<String>,
        #[command(flatten)]
        message: Message,
    },
    /// Append a text input to the saved native chat, including while the controller is offline.
    Enqueue {
        text: String,
        #[arg(long)]
        chat: Option<Uuid>,
        #[arg(long)]
        request_id: Option<Uuid>,
    },
    /// Show the queue in admission order with its delivery receipts.
    Queue {
        #[arg(long)]
        chat: Option<Uuid>,
    },
    /// Withdraw a queued input before native dispatch starts.
    Withdraw {
        request_id: Uuid,
        #[arg(long)]
        chat: Option<Uuid>,
    },
    /// Follow durable chat events as JSON lines without attaching a second agent.
    Watch {
        #[arg(long)]
        chat: Option<Uuid>,
        /// Exclusive cursor printed on a previous event.
        #[arg(long)]
        after: Option<ChatCursor>,
    },
    /// Serve a read-only /snapshot and /events SSE feed for one saved chat.
    Serve {
        #[arg(long)]
        chat: Option<Uuid>,
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: std::net::SocketAddr,
    },
    /// List the last observed native tasks without launching a provider.
    Tasks {
        #[arg(long)]
        chat: Option<Uuid>,
    },
    /// Compact an idle chat and wait for the native result.
    Compact {
        #[arg(long)]
        chat: Option<Uuid>,
    },
    /// Read the last reported token counts for each native open.
    Usage {
        #[arg(long)]
        chat: Option<Uuid>,
    },
    /// Read, set or clear the native goal for a goal-enabled chat.
    Goal {
        #[arg(long, global = true)]
        chat: Option<Uuid>,
        #[command(subcommand)]
        action: GoalAction,
    },
    /// Ask once from a disposable native copy of the chat.
    Ask {
        question: String,
        #[arg(long)]
        chat: Option<Uuid>,
    },
    /// Show the turn timeline without starting any agent.
    History {
        #[arg(long)]
        chat: Option<Uuid>,
    },
    /// List saved chats, most recently used first.
    List,
    /// Restore a turn's workspace and continue from its native model context in a new chat.
    Fork {
        turn: Uuid,
        /// Restore the selected turn into a separate Kar-managed linked checkout.
        #[arg(long)]
        worktree: Option<PathBuf>,
        #[arg(long)]
        chat: Option<Uuid>,
        #[command(flatten)]
        message: Message,
    },
    /// List the files added, modified or deleted during one turn.
    Changes {
        turn: Uuid,
        #[arg(long)]
        chat: Option<Uuid>,
    },
    /// Restore the workspace after a turn (or before it with --before).
    Rollback {
        turn: Uuid,
        #[arg(long)]
        chat: Option<Uuid>,
        #[arg(long)]
        before: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Adopt an earlier `kar run --record` JSONL file from the original workspace.
    Import {
        runtime: String,
        recording: PathBuf,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        bin: Option<PathBuf>,
    },
}
#[derive(Subcommand)]
enum GoalAction {
    Read,
    Set {
        objective: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        token_budget: Option<u64>,
    },
    Clear,
}
#[derive(Args)]
struct Message {
    /// Omit for interactive input. Each submitted line is a turn.
    prompt: Option<String>,
    /// Reusing this key retrieves the original turn.
    #[arg(long, requires = "prompt")]
    request_id: Option<Uuid>,
}
fn selected(store: &ChatStore, id: Option<Uuid>) -> Result<Uuid> {
    id.map_or_else(|| store.latest(), Ok)
}
fn display_input(input: &QueuedInput) {
    println!("{}  {:?}  {}", input.id, input.status, input.text);
}
fn display(turn: &ChatTurn) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "[{} · {:?}]", turn.id, turn.status)?;
    if let Some(from) = turn.steered_from {
        writeln!(out, "Follow-up from {from}")?;
    }
    if let Some(receipt) = turn.native_input_receipt {
        writeln!(out, "Native input {receipt:?}")?;
    }
    for timeline in &turn.items {
        let owner = if timeline.source.path.is_empty() {
            String::new()
        } else {
            format!(" [{}]", timeline.source.path.join("/"))
        };
        match &timeline.item {
            Item::Message {
                role,
                text,
                provider_id,
                ..
            } => {
                let steer = provider_id
                    .as_deref()
                    .and_then(|id| Uuid::parse_str(id).ok())
                    .and_then(|id| turn.steered_inputs.get(&id));
                match steer {
                    Some(status) => writeln!(out, "{role:?}{owner} [{status:?}]> {text}")?,
                    None => writeln!(out, "{role:?}{owner}> {text}")?,
                }
            }
            Item::Tool { id, activity } => writeln!(
                out,
                "Tool{owner}> {} ({id})",
                activity.name.as_deref().unwrap_or("tool")
            )?,
            Item::Retry { status } => {
                write!(out, "Retry{owner}")?;
                if let Some(n) = status.attempt {
                    write!(out, " attempt {n}")?;
                }
                if let Some(ms) = status.delay_ms {
                    write!(out, " after {ms} ms")?;
                }
                if let Some(reason) = &status.reason {
                    write!(out, " ({reason})")?;
                }
                writeln!(out)?;
            }
            Item::Compaction { report } => writeln!(out, "Compaction{owner}> {}", report.state)?,
            Item::Diff { diff, .. } => writeln!(out, "Diff{owner}>\n{diff}")?,
        }
    }
    if let Some(cp) = &turn.checkpoint {
        if cp.after_tree.is_none() {
            writeln!(out, "Workspace: final checkpoint unavailable")?;
        } else {
            for f in &cp.files {
                writeln!(out, "{:?}  {}", f.kind, f.path.display())?;
            }
        }
    }
    Ok(())
}
fn display_notice(notice: &ChatNotification) -> Result<()> {
    if let Some(input) = notice.joined_input {
        println!("Native notification joined input {input}");
    } else {
        println!("Native notification");
    }
    display(&notice.activity)
}
fn display_history(history: &ChatHistory) -> Result<()> {
    let mut notices = history.notifications.iter().peekable();
    for turn in &history.turns {
        while let Some(notice) = notices.next_if(|n| n.activity.started_ms < turn.started_ms) {
            display_notice(notice)?;
        }
        display(turn)?;
    }
    for notice in notices {
        display_notice(notice)?;
    }
    Ok(())
}
async fn send(session: &ChatSession, key: Option<Uuid>, text: String, json: bool) -> Result<()> {
    let id = key.unwrap_or_else(Uuid::new_v4);
    eprintln!("request {id}");
    let turn = tokio::select! {
        result=session.send(id,text)=>result?,
        signal=tokio::signal::ctrl_c()=>{signal?;let _=tokio::time::timeout(Duration::from_secs(2),session.abort()).await;return Err(Error::Closed("interrupted by user".into()));}
    };
    if json {
        super::json_line(&turn, std::io::stdout())?;
    } else {
        display(&turn)?;
    }
    if turn.status != TurnStatus::Completed {
        return Err(Error::Refused(format!("turn status: {:?}", turn.status)));
    }
    Ok(())
}
async fn talk(session: &ChatSession, message: Message, json: bool) -> Result<()> {
    let info = session.info();
    eprintln!(
        "chat {} · {} · {} · native {}",
        info.id,
        info.workspace.display(),
        info.runtime,
        info.native_id
    );
    session.drain_queue().await?;
    if let Some(prompt) = message.prompt {
        return send(session, message.request_id, prompt, json).await;
    }
    let history = session.history()?;
    if json {
        super::json_line(&history, std::io::stdout())?;
    } else {
        display_history(&history)?;
    }
    let interactive = std::io::stdin().is_terminal();
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        if interactive {
            eprint!("you> ");
            std::io::stderr().flush()?;
        }
        let line = tokio::select! {
            line=lines.next_line()=>line?,
            signal=tokio::signal::ctrl_c()=>{signal?;break;}
        };
        let Some(text) = line else { break };
        if text.trim().is_empty() {
            continue;
        }
        send(session, None, text, json).await?;
    }
    Ok(())
}
impl Chat {
    pub(super) async fn run(self) -> Result<()> {
        let store = match self.store {
            Some(p) => ChatStore::open(p)?,
            None => ChatStore::local()?,
        };
        let (session, message) = match self.action {
            Action::Compact { chat } => {
                let session = store.resume(selected(&store, chat)?).await?;
                let result = match session.start_compaction().await {
                    Ok(handle) => Ok(handle.wait(None).await),
                    Err(e) => Err(e),
                };
                let closed = session.close().await;
                let report = result?;
                closed?;
                if self.json {
                    super::json_line(&report, std::io::stdout())?;
                } else {
                    println!("{:?}", report.status);
                }
                return match report.status {
                    kar::CompactionStatus::Completed { .. } => Ok(()),
                    _ => Err(Error::Refused("manual compaction did not complete".into())),
                };
            }
            Action::Usage { chat } => {
                let history = store.history(selected(&store, chat)?)?;
                if self.json {
                    super::json_line(&history.usage, std::io::stdout())?;
                } else {
                    for sample in history.usage {
                        if let Some(total) = sample.usage.total {
                            println!(
                                "{}  input={}  output={}",
                                sample.cursor.stream_id, total.input, total.output
                            );
                        }
                        for (child, usage) in sample.children {
                            if let Some(total) = usage.total {
                                println!(
                                    "  {child}  input={}  output={}",
                                    total.input, total.output
                                );
                            }
                        }
                    }
                }
                return Ok(());
            }
            Action::Tasks { chat } => {
                let history = store.history(selected(&store, chat)?)?;
                if self.json {
                    super::json_line(&history.tasks, std::io::stdout())?;
                } else {
                    for task in history.tasks {
                        println!(
                            "{}  {:?}  {}",
                            task.id,
                            task.status,
                            task.details.description.as_deref().unwrap_or_default()
                        );
                    }
                }
                return Ok(());
            }
            Action::Goal { chat, action } => {
                let id = selected(&store, chat)?;
                let session = store.resume(id).await?;
                let result = match action {
                    GoalAction::Read => session.read_goal().await,
                    GoalAction::Set {
                        objective,
                        status,
                        token_budget,
                    } => session
                        .set_goal(kar::GoalSetOptions {
                            objective,
                            status,
                            token_budget,
                        })
                        .await
                        .map(Some),
                    GoalAction::Clear => session.clear_goal().await.map(|()| None),
                };
                let closed = session.close().await;
                let goal = result?;
                closed?;
                if self.json {
                    super::json_line(&goal, std::io::stdout())?;
                } else if let Some(goal) = goal {
                    println!("{}  {}", goal.status, goal.objective);
                } else {
                    println!("No goal");
                }
                return Ok(());
            }
            Action::Ask { chat, question } => {
                let answer = store
                    .side_question(selected(&store, chat)?, &question)
                    .await?;
                if self.json {
                    super::json_line(&answer, std::io::stdout())?;
                } else {
                    println!("{answer}");
                }
                return Ok(());
            }
            Action::Watch { chat, after } => {
                let mut feed = store.follow(selected(&store, chat)?, after)?;
                loop {
                    tokio::select! {
                        result = feed.recv() => super::json_line(&result?,std::io::stdout())?,
                        signal = tokio::signal::ctrl_c() => { signal?; return Ok(()); }
                    }
                }
            }
            Action::Serve { chat, listen } => {
                let id = selected(&store, chat)?;
                store.follow(id, None)?;
                let listener = tokio::net::TcpListener::bind(listen).await?;
                let addr = listener.local_addr()?;
                if self.json {
                    super::json_line(
                        &serde_json::json!({"chat":id,"url":format!("http://{addr}")}),
                        std::io::stdout(),
                    )?;
                } else {
                    eprintln!("Chat {id} listening at http://{addr}");
                }
                tokio::select! {
                    result = axum::serve(listener, kar::chat::event_router(store, id)).into_future() => result?,
                    signal = tokio::signal::ctrl_c() => signal?,
                }
                return Ok(());
            }
            Action::Enqueue {
                chat,
                request_id,
                text,
            } => {
                let receipt = store.enqueue(
                    selected(&store, chat)?,
                    request_id.unwrap_or_else(Uuid::new_v4),
                    text,
                )?;
                if self.json {
                    super::json_line(&receipt, std::io::stdout())?;
                } else {
                    display_input(&receipt);
                }
                return Ok(());
            }
            Action::Queue { chat } => {
                let inputs = store.queued_inputs(selected(&store, chat)?)?;
                if self.json {
                    super::json_line(&inputs, std::io::stdout())?;
                } else {
                    for input in inputs {
                        display_input(&input);
                    }
                }
                return Ok(());
            }
            Action::Withdraw { chat, request_id } => {
                let receipt = store.withdraw(selected(&store, chat)?, request_id)?;
                if self.json {
                    super::json_line(&receipt, std::io::stdout())?;
                } else {
                    display_input(&receipt);
                }
                return Ok(());
            }
            Action::Fork {
                turn,
                chat,
                worktree,
                message,
            } => {
                let source = selected(&store, chat)?;
                let branch = match worktree {
                    Some(path) => store.fork_at_in_worktree(source, turn, path).await?,
                    None => store.fork_at(source, turn).await?,
                };
                (branch, message)
            }
            Action::Changes { turn, chat } => {
                let history = store.history(selected(&store, chat)?)?;
                let cp = history
                    .turns
                    .iter()
                    .find(|t| t.id == turn)
                    .and_then(|t| t.checkpoint.as_ref())
                    .filter(|cp| cp.after_tree.is_some())
                    .ok_or_else(|| {
                        Error::Refused("no completed workspace checkpoint for this turn".into())
                    })?;
                if self.json {
                    super::json_line(&cp.files, std::io::stdout())?;
                } else if cp.files.is_empty() {
                    println!("No file changes");
                } else {
                    for f in &cp.files {
                        println!("{:?}  {}", f.kind, f.path.display());
                    }
                }
                return Ok(());
            }
            Action::Rollback {
                turn,
                chat,
                before,
                dry_run,
            } => {
                let report = store.rollback(
                    selected(&store, chat)?,
                    CheckpointPoint { turn, before },
                    dry_run,
                )?;
                if self.json {
                    super::json_line(&report, std::io::stdout())?;
                } else {
                    for f in &report.files {
                        println!("{:?}  {}", f.kind, f.path.display());
                    }
                    for f in &report.conflicts {
                        eprintln!("Conflict  {}", f.path.display());
                    }
                    if report.applied {
                        println!("Restored workspace");
                    }
                }
                if !report.conflicts.is_empty() {
                    return Err(Error::Refused(
                        "local changes conflict with workspace rollback".into(),
                    ));
                }
                return Ok(());
            }
            Action::Import {
                runtime,
                recording,
                cwd,
                bin,
            } => {
                let info = store.import_recording(&runtime, &recording, &cwd, bin.as_deref())?;
                if self.json {
                    super::json_line(&info, std::io::stdout())?;
                } else {
                    println!("{}", info.id);
                }
                return Ok(());
            }
            Action::List => {
                let chats = store.list()?;
                if self.json {
                    super::json_line(&chats, std::io::stdout())?;
                } else {
                    for c in chats {
                        println!(
                            "{}  {}  {}  {}",
                            c.id,
                            c.runtime,
                            c.workspace.display(),
                            c.native_id
                        );
                    }
                }
                return Ok(());
            }
            Action::History { chat } => {
                let history = store.history(selected(&store, chat)?)?;
                if self.json {
                    super::json_line(&history, std::io::stdout())?;
                } else {
                    println!(
                        "{}  {}  {}",
                        history.chat.id,
                        history.chat.runtime,
                        history.chat.workspace.display()
                    );
                    display_history(&history)?;
                }
                return Ok(());
            }
            Action::New {
                settings,
                runtime,
                message,
                cwd,
                bin,
                model,
            } => {
                let mut opts = settings.options(cwd);
                opts.model = model;
                (store.create(&runtime, bin.as_deref(), opts).await?, message)
            }
            Action::Resume {
                chat,
                message,
                launch_args,
            } => (
                store
                    .resume_with(selected(&store, chat)?, |o| o.launch_args = launch_args)
                    .await?,
                message,
            ),
        };
        let result = talk(&session, message, self.json).await;
        let closed = session.close().await;
        result.and(closed)
    }
}
