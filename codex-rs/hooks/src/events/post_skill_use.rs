use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;

use super::common;
use crate::engine::CommandShell;
use crate::engine::ConfiguredHandler;
use crate::engine::command_runner::CommandRunResult;
use crate::engine::dispatcher;
use crate::engine::output_parser;
use crate::schema::NullableString;
use crate::schema::PostSkillUseCommandInput;

#[derive(Debug, Clone)]
pub struct PostSkillUseRequest {
    pub session_id: ThreadId,
    pub turn_id: String,
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub skill_name: String,
    pub skill_path: PathBuf,
    pub skill_scope: String,
    pub invocation_type: String,
}

#[derive(Debug)]
pub struct PostSkillUseOutcome {
    pub hook_events: Vec<HookCompletedEvent>,
    pub additional_contexts: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PostSkillUseHandlerData {
    additional_contexts_for_model: Vec<String>,
}

pub(crate) fn preview(
    handlers: &[ConfiguredHandler],
    request: &PostSkillUseRequest,
) -> Vec<HookRunSummary> {
    dispatcher::select_handlers(
        handlers,
        HookEventName::PostSkillUse,
        Some(&request.skill_name),
    )
    .into_iter()
    .map(|handler| dispatcher::running_summary(&handler))
    .collect()
}

pub(crate) async fn run(
    handlers: &[ConfiguredHandler],
    shell: &CommandShell,
    request: PostSkillUseRequest,
) -> PostSkillUseOutcome {
    let matched = dispatcher::select_handlers(
        handlers,
        HookEventName::PostSkillUse,
        Some(&request.skill_name),
    );
    if matched.is_empty() {
        return PostSkillUseOutcome {
            hook_events: Vec::new(),
            additional_contexts: Vec::new(),
        };
    }

    let input_json = match serde_json::to_string(&PostSkillUseCommandInput {
        session_id: request.session_id.to_string(),
        turn_id: request.turn_id.clone(),
        transcript_path: NullableString::from_path(request.transcript_path.clone()),
        cwd: request.cwd.display().to_string(),
        hook_event_name: "PostSkillUse".to_string(),
        model: request.model.clone(),
        permission_mode: request.permission_mode.clone(),
        skill_name: request.skill_name.clone(),
        skill_path: request.skill_path.display().to_string(),
        skill_scope: request.skill_scope.clone(),
        invocation_type: request.invocation_type.clone(),
    }) {
        Ok(input_json) => input_json,
        Err(error) => {
            return serialization_failure_outcome(common::serialization_failure_hook_events(
                matched,
                Some(request.turn_id),
                format!("failed to serialize post skill use hook input: {error}"),
            ));
        }
    };

    let results = dispatcher::execute_handlers(
        shell,
        matched,
        input_json,
        request.cwd.as_path(),
        Some(request.turn_id),
        parse_completed,
    )
    .await;

    let additional_contexts = common::flatten_additional_contexts(
        results
            .iter()
            .map(|result| result.data.additional_contexts_for_model.as_slice()),
    );

    PostSkillUseOutcome {
        hook_events: results.into_iter().map(|result| result.completed).collect(),
        additional_contexts,
    }
}

fn parse_completed(
    handler: &ConfiguredHandler,
    run_result: CommandRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<PostSkillUseHandlerData> {
    let mut entries = Vec::new();
    let mut status = HookRunStatus::Completed;
    let mut additional_contexts_for_model = Vec::new();

    match run_result.error.as_deref() {
        Some(error) => {
            status = HookRunStatus::Failed;
            entries.push(HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: error.to_string(),
            });
        }
        None => match run_result.exit_code {
            Some(0) => {
                let trimmed_stdout = run_result.stdout.trim();
                if trimmed_stdout.is_empty() {
                } else if let Some(parsed) = output_parser::parse_post_skill_use(&run_result.stdout)
                {
                    if let Some(system_message) = parsed.universal.system_message {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: system_message,
                        });
                    }
                    if let Some(additional_context) = parsed.additional_context {
                        common::append_additional_context(
                            &mut entries,
                            &mut additional_contexts_for_model,
                            additional_context,
                        );
                    }
                } else if trimmed_stdout.starts_with('{') || trimmed_stdout.starts_with('[') {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "hook returned invalid post-skill-use JSON output".to_string(),
                    });
                } else {
                    common::append_additional_context(
                        &mut entries,
                        &mut additional_contexts_for_model,
                        trimmed_stdout.to_string(),
                    );
                }
            }
            Some(exit_code) => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: format!("hook exited with code {exit_code}"),
                });
            }
            None => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: "hook exited without a status code".to_string(),
                });
            }
        },
    }

    let completed = HookCompletedEvent {
        turn_id,
        run: dispatcher::completed_summary(handler, &run_result, status, entries),
    };

    dispatcher::ParsedHandler {
        completed,
        data: PostSkillUseHandlerData {
            additional_contexts_for_model,
        },
    }
}

fn serialization_failure_outcome(hook_events: Vec<HookCompletedEvent>) -> PostSkillUseOutcome {
    PostSkillUseOutcome {
        hook_events,
        additional_contexts: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use codex_protocol::protocol::HookEventName;
    use codex_protocol::protocol::HookOutputEntry;
    use codex_protocol::protocol::HookOutputEntryKind;
    use codex_protocol::protocol::HookRunStatus;
    use pretty_assertions::assert_eq;

    use super::PostSkillUseHandlerData;
    use super::parse_completed;
    use crate::engine::ConfiguredHandler;
    use crate::engine::command_runner::CommandRunResult;

    #[test]
    fn additional_context_is_recorded() {
        let parsed = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"PostSkillUse","additionalContext":"remember skill context"}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PostSkillUseHandlerData {
                additional_contexts_for_model: vec!["remember skill context".to_string()],
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Context,
                text: "remember skill context".to_string(),
            }]
        );
    }

    #[test]
    fn plain_text_context_is_recorded() {
        let parsed = parse_completed(
            &handler(),
            run_result(Some(0), "plain skill note", ""),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            PostSkillUseHandlerData {
                additional_contexts_for_model: vec!["plain skill note".to_string()],
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
    }

    fn handler() -> ConfiguredHandler {
        ConfiguredHandler {
            event_name: HookEventName::PostSkillUse,
            matcher: Some("^demo$".to_string()),
            command: "echo post skill".to_string(),
            timeout_sec: 5,
            status_message: Some("running post skill use hook".to_string()),
            source_path: PathBuf::from("/tmp/hooks.json"),
            display_order: 0,
        }
    }

    fn run_result(exit_code: Option<i32>, stdout: &str, stderr: &str) -> CommandRunResult {
        CommandRunResult {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit_code,
            error: None,
            started_at: 0,
            completed_at: 0,
            duration_ms: 0,
        }
    }
}
