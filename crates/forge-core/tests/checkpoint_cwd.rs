//! Production-faithful check of code-checkpoint restore: the *default* checkpoint root
//! (`.forge/checkpoints`, relative) + a relative file path, run from a real working directory —
//! exactly how `forge chat` is wired. Reproduces the "file changes didn't revert" report.

use std::sync::Arc;

use forge_config::{Config, OneOrMany, PriceOverride};
use forge_core::Session;
use forge_mesh::HeuristicRouter;
use forge_provider::{EventSink, ModelResponse, Provider, ProviderError, ToolSpec};
use forge_store::Store;
use forge_tui::HeadlessPresenter;
use forge_types::{new_id, Message, PermissionMode, Role, ToolCall, Usage};

struct WriteOnceProvider {
    path: String,
    content: String,
}

#[async_trait::async_trait]
impl Provider for WriteOnceProvider {
    async fn complete(
        &self,
        _model: &str,
        messages: &[Message],
        _tools: &[ToolSpec],
        _on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let usage = Usage::default();
        if messages.iter().any(|m| m.role == Role::Tool) {
            return Ok(ModelResponse {
                content: "done".into(),
                tool_calls: vec![],
                usage,
                quotas: Vec::new(),
            });
        }
        Ok(ModelResponse {
            content: "writing".into(),
            tool_calls: vec![ToolCall {
                id: new_id(),
                name: "write_file".into(),
                args: serde_json::json!({ "path": self.path, "content": self.content }),
            }],
            usage,
            quotas: Vec::new(),
        })
    }
}

#[tokio::test]
async fn undo_reverts_files_with_the_default_relative_checkpoint_root() {
    // Run from a fresh temp cwd so relative `.forge/checkpoints` + a relative file path behave
    // exactly as in production (this test owns the process cwd; it's the only one here).
    let dir = std::env::temp_dir().join(format!("forge-cwd-{}", forge_types::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_current_dir(&dir).unwrap();

    std::fs::write("notes.txt", "ORIGINAL").unwrap();

    // Keyless priced complex model so the write turn runs deterministically; Bypass allows it.
    let mut config = Config {
        permission_mode: PermissionMode::Bypass,
        ..Config::default()
    };
    config
        .mesh
        .models
        .insert("trivial".into(), OneOrMany::One("ollama::w".into()));
    config.mesh.pricing.insert(
        "ollama::w".into(),
        PriceOverride {
            input_per_1k: 0.0,
            output_per_1k: 0.0,
        },
    );

    let mut session = Session::start(
        Arc::new(Store::open_in_memory().unwrap()),
        Arc::new(WriteOnceProvider {
            path: "notes.txt".into(), // RELATIVE — as a model commonly passes
            content: "MODEL EDIT".into(),
        }),
        Arc::new(HeuristicRouter::new(config.clone())),
        forge_tools::ToolRegistry::with_core_tools(),
        Box::new(HeadlessPresenter::new(false)),
        config,
        ".",
    )
    .unwrap();
    // NOTE: deliberately NOT calling set_checkpoint_root — exercising the production default.

    session.run_turn("edit my notes").await.unwrap();
    assert_eq!(std::fs::read_to_string("notes.txt").unwrap(), "MODEL EDIT");

    let seq = session.checkpoints().unwrap().last().unwrap().seq;
    let report = session.rewind_to(seq).unwrap().restore;

    assert!(
        !report.restored.is_empty(),
        "the write was snapshotted + restored: {report:?}"
    );
    assert_eq!(
        std::fs::read_to_string("notes.txt").unwrap(),
        "ORIGINAL",
        "undo reverts the file with the default relative checkpoint root"
    );

    std::env::set_current_dir(std::env::temp_dir()).ok();
    std::fs::remove_dir_all(&dir).ok();
}
