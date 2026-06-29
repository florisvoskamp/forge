use anyhow::{Context, Result};

use crate::*;

pub(crate) async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Run {
            prompt,
            mock,
            mode,
            tui,
            resume,
            model,
        } => run(prompt.join(" "), mock, mode, tui, resume, model).await,
        Command::Chat {
            mock,
            mode,
            r#continue,
            resume,
            plain,
            inline,
            fullscreen,
            model,
        } => {
            let store = open_store()?;
            let resume_mode = resolve_resume_mode(r#continue, resume, &store, plain)?;
            // Full-screen unless `--inline`; `--fullscreen` / `--inline` override the config default.
            let fullscreen = if inline {
                false
            } else if fullscreen {
                true
            } else {
                forge_config::load()
                    .map(|c| c.tui.fullscreen)
                    .unwrap_or(true)
            };
            chat(mock, mode, resume_mode, plain, fullscreen, model).await
        }
        Command::Sessions => sessions(),
        Command::Replay { ids, json, rerun } => {
            if rerun {
                replay_rerun_cmd(&ids).await
            } else {
                replay_cmd(&ids, json)
            }
        }
        Command::Assay { sub } => assay_cmd(sub).await,
        Command::Bench { sub } => match sub {
            BenchCmd::Swe {
                dataset,
                out,
                limit,
                model,
                workdir,
                agent,
                timeout_secs,
                attempts,
            } => {
                bench::run_swe(
                    dataset,
                    out,
                    limit,
                    model,
                    workdir,
                    agent,
                    timeout_secs,
                    attempts,
                )
                .await
            }
            BenchCmd::Passk { reports } => bench::passk(&reports),
            BenchCmd::Report { metrics, evals } => bench::report(&metrics, &evals),
        },
        Command::Commands => commands_cmd(),
        Command::Models { probe, all, clear } => models(probe, all, clear).await,
        Command::Mesh { prompt, json } => mesh_explain(prompt.join(" "), json).await,
        Command::Benchmarks { refresh } => benchmarks_cmd(refresh).await,
        Command::Local { sub } => local_cmd(sub).await,
        Command::Doctor => {
            let fails = doctor::run().await?;
            if fails > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Update { check } => tokio::task::spawn_blocking(move || update::run(check))
            .await
            .context("update task")?,
        Command::Auth {
            provider,
            remove,
            list,
            replace,
        } => auth(&provider, remove, list, replace),
        Command::Setup | Command::Init => setup(),
        Command::Mcp { cmd } => mcp_cmd(cmd).await,
        Command::Memory { cmd, global } => memory_cmd(cmd, global),
        Command::McpServe => mcp_serve::run().await,
        Command::Lattice { op } => lattice_cmd(op).await,
        Command::Import { source } => import_cmd(source),
        Command::Git { cmd } => git_cmd(cmd),
        Command::Nl { query, mode } => nl_cmd(query.join(" "), mode).await,
        Command::Skill { sub } => skill_cmd(sub).await,
        Command::Migrate { cmd } => migrate_cmd(cmd).await,
        Command::Plugin { cmd } => plugin_cmd(cmd),
        Command::SelfMcp { action } => self_mcp_cmd(action),
    }
}
