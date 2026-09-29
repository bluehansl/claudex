use anyhow::Context;
use clap::Parser;
use std::io::IsTerminal;

#[derive(Debug, Parser)]
pub(crate) struct PeerCommand {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// List live local peer sessions.
    List,
    /// Show held messages for one Claudex thread.
    Inbox {
        #[arg(long)]
        thread: String,
    },
    /// Approve a held message from a human terminal.
    Approve {
        #[arg(long)]
        thread: String,
        #[arg(long)]
        message: i64,
    },
    /// Reject a held message from a human terminal.
    Reject {
        #[arg(long)]
        thread: String,
        #[arg(long)]
        message: i64,
    },
}

pub(crate) async fn run(command: PeerCommand) -> anyhow::Result<()> {
    let approve = matches!(&command.action, Action::Approve { .. });
    match command.action {
        Action::List => {
            let root = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| std::path::PathBuf::from(home).join(".claude"))
                })
                .context("cannot resolve Claude home")?;
            for peer in codex_claude_peer::list_sessions(&root).await? {
                println!(
                    "{} [{}] {} {}",
                    peer.name,
                    peer.reference(),
                    peer.status,
                    peer.address()
                );
            }
        }
        Action::Inbox { thread } => {
            let id = codex_protocol::ThreadId::from_string(&thread)?;
            let root = codex_core::config::find_codex_home()?;
            let messages = codex_claude_peer::pending_approvals(
                &root.join("claude-peer").join(format!("{id}.sqlite")),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&messages)?);
        }
        Action::Approve { thread, message } | Action::Reject { thread, message } => {
            anyhow::ensure!(
                std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                "peer approval requires a human terminal"
            );
            let id = codex_protocol::ThreadId::from_string(&thread)?;
            let root = codex_core::config::find_codex_home()?;
            let applied = codex_claude_peer::decide_approval(
                &root.join("claude-peer").join(format!("{id}.sqlite")),
                message,
                approve,
            )
            .await?;
            anyhow::ensure!(applied, "held message was not found");
            println!(
                "{}",
                if approve {
                    "Message approved."
                } else {
                    "Message rejected."
                }
            );
        }
    }
    Ok(())
}
