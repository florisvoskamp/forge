//! A deterministic provider for offline tests, the walking skeleton, and the TUI driver harness.
//!
//! Default behaviour: on the first turn it asks to read `Cargo.toml` via a `read_file` tool call;
//! once it sees the tool result in the transcript, it returns a final text answer. This exercises
//! the full agent loop (route -> model -> tool -> model -> done) with no network.
//!
//! Intent-aware behaviour (so the full-screen panels can be tested offline — `scripts/tui-drive.sh`).
//! A planning prompt (the `/plan` injected prompt names `present_plan`, or any prompt containing
//! `mock:plan`) makes the mock call `present_plan` with sample steps, so the plan card renders and
//! the approval flow runs, then it stops awaiting approval. A prompt asking to track tasks
//! (`update_tasks` / `task list` / `mock:tasks`) makes the mock call `update_tasks` with a sample
//! list, so the sticky task panel renders. These reproduce, with no API or CLI bridge, the exact
//! paths behind "no task list / no plan card".

use async_trait::async_trait;
use forge_types::{new_id, Message, Role, ToolCall, Usage};
use serde_json::json;

use crate::{EventSink, ModelResponse, Provider, ProviderError, StreamEvent, ToolSpec};

#[derive(Debug, Default)]
pub struct MockProvider;

/// Emit `text` to the sink word by word, simulating streaming.
fn stream_words(text: &str, on_event: &mut EventSink<'_>) {
    for word in text.split_inclusive(' ') {
        on_event(StreamEvent::Text(word.to_string()));
    }
}

fn resp(content: &str, tool_calls: Vec<ToolCall>, input: u64, output: u64) -> ModelResponse {
    ModelResponse {
        content: content.to_string(),
        tool_calls,
        usage: Usage {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: 0,
            cost_usd: 0.0,
        },
        quotas: Vec::new(),
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _model: &str,
        messages: &[Message],
        _tools: &[ToolSpec],
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let last_user = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let lu = last_user.to_lowercase();
        // Tool results already in the transcript this turn, so a terminal tool isn't re-emitted.
        let tool_results: String = messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let plan_proposed = tool_results.contains("Plan presented to the user for approval");
        let tasks_set = tool_results.contains("task list updated");
        let used_read = tool_results.contains("workspace") || tool_results.contains("read_file");

        let wants_plan = lu.contains("present_plan")
            || lu.contains("mock:plan")
            || lu.contains("step-by-step plan")
            || lu.contains("ordered plan");
        let wants_tasks = lu.contains("update_tasks")
            || lu.contains("mock:tasks")
            || lu.contains("task list")
            || lu.contains("track tasks");

        // Planning turn → render the plan card, then stop for approval.
        if wants_plan {
            if plan_proposed {
                let content = "The plan is on screen for your review.";
                stream_words(content, on_event);
                return Ok(resp(content, vec![], 42, 18));
            }
            let content = "Here's a plan for the refactor.";
            stream_words(content, on_event);
            return Ok(resp(
                content,
                vec![ToolCall {
                    id: new_id(),
                    name: "present_plan".to_string(),
                    args: json!({
                        "title": "Split main.rs into modules",
                        "steps": [
                            {"title": "Extract clap structs to cli/args.rs", "detail": "Move Command/BenchCmd/… verbatim"},
                            {"title": "Extract dispatch to cli/dispatch.rs", "detail": "Top-level match on Command"},
                            {"title": "One file per subcommand handler", "detail": "cli/commands/<name>.rs"},
                            {"title": "Move every clap struct and enum out of main.rs into cli/args.rs without changing any field, variant, attribute, or doc comment", "detail": "Keep the parsed CLI surface byte-for-byte identical across the whole refactor so no command, flag, or help string shifts position."},
                        ],
                        "notes": "Pure mechanical move; cargo build after each step.",
                    }),
                }],
                30,
                12,
            ));
        }

        // Task-tracking turn → seed the sticky task panel, then finish.
        if wants_tasks {
            if tasks_set {
                let content = "Tasks are tracked in the panel above.";
                stream_words(content, on_event);
                return Ok(resp(content, vec![], 42, 18));
            }
            let content = "Setting up the task list.";
            stream_words(content, on_event);
            return Ok(resp(
                content,
                vec![ToolCall {
                    id: new_id(),
                    name: "update_tasks".to_string(),
                    args: json!({
                        "tasks": [
                            {"title": "Scan the codebase", "status": "in_progress"},
                            {"title": "Apply the change", "status": "pending"},
                            {"title": "Verify the build", "status": "pending"},
                        ],
                    }),
                }],
                30,
                12,
            ));
        }

        // Default: a read_file round-trip then a final answer. The token counts here are load-bearing
        // — cost/budget tests in forge-core depend on this exact usage (30/12 then 42/18).
        if used_read {
            let content = "Done — I read the project manifest and the workspace looks healthy.";
            stream_words(content, on_event);
            Ok(resp(content, vec![], 42, 18))
        } else {
            let content = "Let me inspect the project manifest.";
            stream_words(content, on_event);
            Ok(resp(
                content,
                vec![ToolCall {
                    id: new_id(),
                    name: "read_file".to_string(),
                    args: json!({ "path": "Cargo.toml" }),
                }],
                30,
                12,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_turn_requests_a_tool() {
        let p = MockProvider;
        let res = p
            .complete(
                "mock",
                &[Message::user("check the project")],
                &[],
                &mut |_| {},
            )
            .await
            .unwrap();
        assert!(res.wants_tools());
        assert_eq!(res.tool_calls[0].name, "read_file");
    }

    #[tokio::test]
    async fn after_tool_result_it_finishes() {
        let p = MockProvider;
        let msgs = vec![
            Message::user("check the project"),
            Message::assistant("Let me inspect the project manifest."),
            Message::new(Role::Tool, "[workspace] ..."),
        ];
        let res = p.complete("mock", &msgs, &[], &mut |_| {}).await.unwrap();
        assert!(!res.wants_tools());
        assert!(!res.content.is_empty());
    }

    #[tokio::test]
    async fn a_planning_prompt_calls_present_plan_then_stops() {
        let p = MockProvider;
        // The /plan injected prompt names `present_plan`.
        let plan_prompt = "produce a step-by-step plan; call the `present_plan` tool";
        let res = p
            .complete("mock", &[Message::user(plan_prompt)], &[], &mut |_| {})
            .await
            .unwrap();
        assert_eq!(res.tool_calls[0].name, "present_plan");
        assert!(res.tool_calls[0].args.get("steps").is_some());
        // Once the plan-proposed result is in the transcript, it must NOT re-propose (no loop).
        let after = vec![
            Message::user(plan_prompt),
            Message::assistant("Here's a plan for the refactor."),
            Message::new(
                Role::Tool,
                "Plan presented to the user for approval. STOP now",
            ),
        ];
        let res2 = p.complete("mock", &after, &[], &mut |_| {}).await.unwrap();
        assert!(!res2.wants_tools(), "stops after the plan is on screen");
    }

    #[tokio::test]
    async fn a_task_tracking_prompt_calls_update_tasks() {
        let p = MockProvider;
        let res = p
            .complete(
                "mock",
                &[Message::user("refactor this with a task list")],
                &[],
                &mut |_| {},
            )
            .await
            .unwrap();
        assert_eq!(res.tool_calls[0].name, "update_tasks");
        assert!(res.tool_calls[0].args.get("tasks").is_some());
    }

    #[tokio::test]
    async fn streams_text_to_the_sink() {
        let p = MockProvider;
        let mut streamed = String::new();
        let res = p
            .complete("mock", &[Message::user("check it")], &[], &mut |ev| {
                if let StreamEvent::Text(t) = ev {
                    streamed.push_str(&t)
                }
            })
            .await
            .unwrap();
        assert_eq!(
            streamed, res.content,
            "streamed text deltas reconstruct the full content"
        );
    }
}
