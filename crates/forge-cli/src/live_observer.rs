use forge_tui::PresenterEvent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LiveEvent {
    AssistantText(String),
    AssistantDelta(String),
    Reasoning(String),
    AssistantDone,
    Warning(String),
    ToolStart {
        name: String,
        args: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        summary: String,
    },
    Routing {
        tier: String,
        model: String,
        rationale: String,
    },
    Cost {
        session_total_usd: f64,
        session_in: u64,
        session_out: u64,
        context_tokens: u64,
        context_limit: Option<u32>,
    },
    SubagentStart {
        id: String,
        agent: String,
        task: String,
        model: Option<String>,
        phase: Option<String>,
    },
    SubagentProgress {
        id: String,
        snippet: String,
    },
    SubagentResult {
        id: String,
        agent: String,
        ok: bool,
        summary: String,
        cost_usd: f64,
    },
}

pub fn to_live_event(event: &PresenterEvent) -> Option<LiveEvent> {
    match event {
        PresenterEvent::AssistantText(t) => Some(LiveEvent::AssistantText(t.clone())),
        PresenterEvent::AssistantDelta(d) => Some(LiveEvent::AssistantDelta(d.clone())),
        PresenterEvent::Reasoning(r) => Some(LiveEvent::Reasoning(r.clone())),
        PresenterEvent::AssistantDone => Some(LiveEvent::AssistantDone),
        PresenterEvent::Warning(w) => Some(LiveEvent::Warning(w.clone())),
        PresenterEvent::ToolStart { name, args } => Some(LiveEvent::ToolStart {
            name: name.clone(),
            args: args.clone(),
        }),
        PresenterEvent::ToolResult { name, ok, summary } => Some(LiveEvent::ToolResult {
            name: name.clone(),
            ok: *ok,
            summary: summary.clone(),
        }),
        PresenterEvent::Routing {
            tier,
            model,
            rationale,
        } => Some(LiveEvent::Routing {
            tier: tier.clone(),
            model: model.clone(),
            rationale: rationale.clone(),
        }),
        PresenterEvent::Cost {
            session_total_usd,
            session_in,
            session_out,
            context_tokens,
            context_limit,
        } => Some(LiveEvent::Cost {
            session_total_usd: *session_total_usd,
            session_in: *session_in,
            session_out: *session_out,
            context_tokens: *context_tokens,
            context_limit: *context_limit,
        }),
        PresenterEvent::SubagentStart {
            id,
            agent,
            task,
            model,
            phase,
        } => Some(LiveEvent::SubagentStart {
            id: id.clone(),
            agent: agent.clone(),
            task: task.clone(),
            model: model.clone(),
            phase: phase.clone(),
        }),
        PresenterEvent::SubagentProgress { id, snippet } => Some(LiveEvent::SubagentProgress {
            id: id.clone(),
            snippet: snippet.clone(),
        }),
        PresenterEvent::SubagentResult {
            id,
            agent,
            ok,
            summary,
            cost_usd,
        } => Some(LiveEvent::SubagentResult {
            id: id.clone(),
            agent: agent.clone(),
            ok: *ok,
            summary: summary.clone(),
            cost_usd: *cost_usd,
        }),
        _ => None,
    }
}

pub fn live_event_to_presenter(event: LiveEvent) -> Option<PresenterEvent> {
    match event {
        LiveEvent::AssistantText(t) => Some(PresenterEvent::AssistantText(t)),
        LiveEvent::AssistantDelta(d) => Some(PresenterEvent::AssistantDelta(d)),
        LiveEvent::Reasoning(r) => Some(PresenterEvent::Reasoning(r)),
        LiveEvent::AssistantDone => Some(PresenterEvent::AssistantDone),
        LiveEvent::Warning(w) => Some(PresenterEvent::Warning(w)),
        LiveEvent::ToolStart { name, args } => Some(PresenterEvent::ToolStart { name, args }),
        LiveEvent::ToolResult { name, ok, summary } => {
            Some(PresenterEvent::ToolResult { name, ok, summary })
        }
        LiveEvent::Routing {
            tier,
            model,
            rationale,
        } => Some(PresenterEvent::Routing {
            tier,
            model,
            rationale,
        }),
        LiveEvent::Cost {
            session_total_usd,
            session_in,
            session_out,
            context_tokens,
            context_limit,
        } => Some(PresenterEvent::Cost {
            session_total_usd,
            session_in,
            session_out,
            context_tokens,
            context_limit,
        }),
        LiveEvent::SubagentStart {
            id,
            agent,
            task,
            model,
            phase,
        } => Some(PresenterEvent::SubagentStart {
            id,
            agent,
            task,
            model,
            phase,
        }),
        LiveEvent::SubagentProgress { id, snippet } => {
            Some(PresenterEvent::SubagentProgress { id, snippet })
        }
        LiveEvent::SubagentResult {
            id,
            agent,
            ok,
            summary,
            cost_usd,
        } => Some(PresenterEvent::SubagentResult {
            id,
            agent,
            ok,
            summary,
            cost_usd,
        }),
    }
}
