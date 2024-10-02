//! Contains the helper functions over here for the plan generation

use std::{collections::HashMap, sync::Arc};

use super::types::Result;
use axum::response::{sse, Sse};
use futures::StreamExt;
use llm_client::clients::types::LLMClientCompletionResponse;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    agent::types::{AgentAnswerStreamEvent, ConversationMessage},
    agentic::{
        symbol::events::{
            input::SymbolEventRequestId, message_event::SymbolEventMessageProperties,
        },
        tool::lsp::file_diagnostics::DiagnosticMap,
        tool::plan::{
            plan::Plan,
            service::{PlanService, PlanServiceError},
        },
    },
    application::config::configuration::Configuration,
    user_context::types::UserContext,
};

async fn append_to_plan(
    plan_id: uuid::Uuid,
    plan_storage_path: String,
    plan_service: PlanService,
    query: String,
    user_context: UserContext,
    message_properties: SymbolEventMessageProperties,
    agent_sender: UnboundedSender<anyhow::Result<ConversationMessage>>,
) {
    let plan = plan_service.load_plan(&plan_storage_path).await;
    if let Err(_) = plan {
        let final_answer = "failed to load plan from storage".to_owned();
        let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
            plan_id,
            AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                final_answer.to_owned(),
                Some(final_answer.to_owned()),
                "Custom".to_owned(),
            )),
        )));
        return;
    }
    let plan = plan.expect("plan to be present");
    let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
        plan_id,
        AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
            "Generating new steps from the context".to_owned(),
            Some("Generating new steps from the context".to_owned()),
            "Custom".to_owned(),
        )),
    )));
    if let Ok(plan) = plan_service
        .append_steps(plan, query, user_context, message_properties)
        .await
    {
        let plan_debug_view = plan.to_debug_message();
        let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
            plan_id,
            AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                plan_debug_view.to_owned(),
                Some(plan_debug_view),
                "Custom".to_owned(),
            )),
        )));
        let _ = plan_service.save_plan(&plan, &plan_storage_path).await;
    } else {
        let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
            plan_id,
            AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                "Failed to add new steps to the plan".to_owned(),
                Some("Failed to add new steps to the plan".to_owned()),
                "Custom".to_owned(),
            )),
        )));
        // errored to update the plan
    }
}

/// Executes the plan until a checkpoint
pub async fn execute_plan_until(
    // the checkpoint until which we want to execute the plan
    execute_until: usize,
    plan_id: uuid::Uuid,
    plan_storage_path: String,
    plan_service: PlanService,
    message_properties: SymbolEventMessageProperties,
    agent_sender: UnboundedSender<anyhow::Result<ConversationMessage>>,
) {
    // loads the plan from a storage location
    let plan = plan_service.load_plan(&plan_storage_path).await;
    if let Err(_) = plan {
        let final_answer = "failed to load plan from stroage".to_owned();
        let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
            plan_id,
            AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                final_answer.to_owned(),
                Some(final_answer.to_owned()),
                "Custom".to_owned(),
            )),
        )));
        return;
    }
    let mut plan = plan.expect("plan to be present");
    for (idx, plan_step) in plan
        .steps()
        .to_vec()
        .iter()
        .enumerate()
        .filter_map(|(idx, step)| {
            if idx <= execute_until {
                Some((idx, step))
            } else {
                None
            }
        })
    {
        if plan.checkpoint().is_some() && idx <= plan.checkpoint().unwrap_or_default() {
            let executing_step = format!(
                "Already executed step:{}, checkpoint is at: {}",
                idx,
                plan.checkpoint().unwrap_or_default()
            );
            let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
                plan_id,
                AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                    executing_step.to_owned(),
                    Some(executing_step.to_owned()),
                    "Custom".to_owned(),
                )),
            )));
            continue;
        }
        // starting executing each step over here
        let checkpoint = plan.checkpoint().unwrap_or_default();
        let context = plan_service.prepare_context(plan.steps(), checkpoint).await;
        let execution_result = plan_service
            .execute_step(plan_step, context, message_properties.clone())
            .await;
        if let Err(_) = execution_result {
            let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
                plan_id,
                AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                    format!("Errored out while executing step: {}", idx).to_owned(),
                    Some(format!("Errored out while executing step: {}", idx).to_owned()),
                    "Custom".to_owned(),
                )),
            )));
            return;
        }
        let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
            plan_id,
            AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                format!("Finished executing until: {}", idx).to_owned(),
                Some(format!("Finished executing until: {}", idx).to_owned()),
                "Custom".to_owned(),
            )),
        )));
        let _ = plan.increment_checkpoint();
        // save the updated checkpoint in the storage layer
        let _ = plan_service.save_plan(&plan, &plan_storage_path).await;
    }
}

/// Create the plan using the context present over here
pub async fn create_plan(
    user_query: String,
    user_context: UserContext,
    editor_url: String,
    plan_id: uuid::Uuid,
    plan_storage_path: String,
    plan_service: PlanService,
    is_deep_reasoning: bool,
    // we can send events using this
    agent_sender: UnboundedSender<anyhow::Result<ConversationMessage>>,
) -> Result<Plan, PlanServiceError> {
    let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
        plan_id.clone(),
        AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
            "Generating plan".to_owned(),
            Some("Generating plan".to_owned()),
            "Custom".to_owned(),
        )),
    )));
    let cancellation_token = tokio_util::sync::CancellationToken::new();
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let plan_id_str = plan_id.to_string();
    let message_properties = SymbolEventMessageProperties::new(
        SymbolEventRequestId::new(plan_id_str.to_owned(), plan_id_str.to_owned()),
        sender,
        editor_url,
        cancellation_token,
    );

    let plan = plan_service
        .create_plan(
            plan_id_str,
            user_query,
            user_context,
            is_deep_reasoning,
            plan_storage_path.to_owned(),
            message_properties,
        )
        .await;

    match plan.as_ref() {
        Ok(plan) => {
            // send over a response that we are done generating the plan
            let final_answer = format!(
                r#"finished generating plan at [location]({})
plan_information:
{}"#,
                &plan_storage_path,
                plan.to_debug_message(),
            );
            let _ = plan_service.save_plan(&plan, &plan_storage_path).await;
            let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
                plan_id,
                AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                    final_answer.to_owned(),
                    Some(final_answer.to_owned()),
                    "Custom".to_owned(),
                )),
            )));
        }
        Err(e) => {
            eprintln!("Failed to generate plan: {:?}", e);
            let final_answer = "Failed to generate plan".to_owned();
            let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
                plan_id,
                AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                    final_answer.to_owned(),
                    Some(final_answer.to_owned()),
                    "Custom".to_owned(),
                )),
            )));
        }
    }
    // drop the sender over here
    drop(agent_sender);
    // return the plan at the end of the creation loop
    plan
}

/// Converts diagnostics messages with snippet into PlanStep
pub async fn generate_steps_from_diagnostics(
    plan_id: uuid::Uuid,
    plan_storage_path: String,
    plan_service: PlanService,
    message_properties: SymbolEventMessageProperties,
    agent_sender: UnboundedSender<anyhow::Result<ConversationMessage>>,
    is_deep_reasoning: bool,
) {
    let plan = plan_service.load_plan(&plan_storage_path).await;
    if let Err(_) = plan {
        let final_answer = "failed to load plan from stroage".to_owned();
        let _ = agent_sender.send(Ok(ConversationMessage::answer_update(
            plan_id,
            AgentAnswerStreamEvent::LLMAnswer(LLMClientCompletionResponse::new(
                final_answer.to_owned(),
                Some(final_answer.to_owned()),
                "Custom".to_owned(),
            )),
        )));
        return;
    };
    let mut plan = plan.expect("plan to be present");

    if let None = plan.checkpoint() {
        println!("webserver::plan::generate_steps_from_diagnostics::no_checkpoint");

        // ui event should be here
        return;
    }
    let checkpoint = plan.checkpoint().expect("checkpoint to be present");

    // all files edited up to checkpoint
    let edited_files = plan_service.get_edited_files(&plan, checkpoint);

    println!(
        "webserver::plan::generate_steps_from_diagnostics::edited_files: {}",
        edited_files.join("\n")
    );

    // get all diagnostics present on these files
    let file_lsp_diagnostics = plan_service
        .tool_box()
        .get_lsp_diagnostics_for_files(edited_files, message_properties.clone())
        .await
        .unwrap_or(vec![]); // empty vec is acceptable

    let diagnostics_grouped_by_file: DiagnosticMap =
        file_lsp_diagnostics
            .into_iter()
            .fold(HashMap::new(), |mut acc, error| {
                acc.entry(error.fs_file_path().to_owned())
                    .or_insert_with(Vec::new)
                    .push(error);
                acc
            });

    dbg!(&diagnostics_grouped_by_file);

    let _root_request_id = message_properties.root_request_id().to_owned();
    let _editor_url = message_properties.editor_url();
    let user_query = "Fix these LSP errors.";

    let _response = plan_service
        .tool_box()
        .generate_steps_with_diagnostics(
            user_query,
            message_properties.clone(),
            diagnostics_grouped_by_file,
            is_deep_reasoning,
        )
        .await;

    // let response = plan_service.tool_box().

    // now we fix lsp errors, per file.
    // possibly, with a round of GtR's
}

/// handler akin to handle_execute_plan_until. Main purpose is to spawn generate_steps_from_diagnostics
pub async fn handle_diagnostics_to_steps(
    plan_id: uuid::Uuid,
    plan_storage_path: String,
    editor_url: String,
    plan_service: PlanService,
    is_deep_reasoning: bool,
) -> Result<
    Sse<std::pin::Pin<Box<dyn tokio_stream::Stream<Item = anyhow::Result<sse::Event>> + Send>>>,
> {
    let cancellation_token = tokio_util::sync::CancellationToken::new();
    let (ui_sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let plan_id_str = plan_id.to_string();
    let message_properties = SymbolEventMessageProperties::new(
        SymbolEventRequestId::new(plan_id_str.to_owned(), plan_id_str.to_owned()),
        ui_sender,
        editor_url,
        cancellation_token,
    );

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

    // this is the main thing to change.
    let _ = tokio::spawn(async move {
        generate_steps_from_diagnostics(
            plan_id,
            plan_storage_path,
            plan_service,
            message_properties,
            sender,
            is_deep_reasoning,
        )
        .await;
    });

    let conversation_message_stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(receiver);
    // TODO(skcd): Re-introduce this again when we have a better way to manage
    // server side events on the client side
    let init_stream = futures::stream::once(async move {
        Ok(sse::Event::default()
            .json_data(json!({
                "session_id": plan_id.to_owned(),
            }))
            // This should never happen, so we force an unwrap.
            .expect("failed to serialize initialization object"))
    });

    // // We know the stream is unwind safe as it doesn't use synchronization primitives like locks.
    let answer_stream = conversation_message_stream.map(
        |conversation_message: anyhow::Result<ConversationMessage>| {
            if let Err(e) = &conversation_message {
                tracing::error!("error in conversation message stream: {}", e);
            }
            sse::Event::default()
                .json_data(conversation_message.expect("should not fail deserialization"))
                .map_err(anyhow::Error::new)
        },
    );

    // TODO(skcd): Re-introduce this again when we have a better way to manage
    // server side events on the client side
    let done_stream = futures::stream::once(async move {
        Ok(sse::Event::default()
            .json_data(json!(
                {"done": "[CODESTORY_DONE]".to_owned(),
                "session_id": plan_id.to_owned(),
            }))
            .expect("failed to send done object"))
    });

    let stream = init_stream.chain(answer_stream).chain(done_stream);

    Ok(Sse::new(Box::pin(stream)))
}

pub async fn handle_execute_plan_until(
    execute_until: usize,
    plan_id: uuid::Uuid,
    plan_storage_path: String,
    editor_url: String,
    plan_service: PlanService,
) -> Result<
    Sse<std::pin::Pin<Box<dyn tokio_stream::Stream<Item = anyhow::Result<sse::Event>> + Send>>>,
> {
    let cancellation_token = tokio_util::sync::CancellationToken::new();
    let (ui_sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let plan_id_str = plan_id.to_string();
    let message_properties = SymbolEventMessageProperties::new(
        SymbolEventRequestId::new(plan_id_str.to_owned(), plan_id_str.to_owned()),
        ui_sender,
        editor_url,
        cancellation_token,
    );

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let _ = tokio::spawn(async move {
        execute_plan_until(
            execute_until,
            plan_id,
            plan_storage_path,
            plan_service,
            message_properties,
            sender,
        )
        .await;
    });
    let conversation_message_stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(receiver);
    // TODO(skcd): Re-introduce this again when we have a better way to manage
    // server side events on the client side
    let init_stream = futures::stream::once(async move {
        Ok(sse::Event::default()
            .json_data(json!({
                "session_id": plan_id.to_owned(),
            }))
            // This should never happen, so we force an unwrap.
            .expect("failed to serialize initialization object"))
    });

    // // We know the stream is unwind safe as it doesn't use synchronization primitives like locks.
    let answer_stream = conversation_message_stream.map(
        |conversation_message: anyhow::Result<ConversationMessage>| {
            if let Err(e) = &conversation_message {
                tracing::error!("error in conversation message stream: {}", e);
            }
            sse::Event::default()
                .json_data(conversation_message.expect("should not fail deserialization"))
                .map_err(anyhow::Error::new)
        },
    );

    // TODO(skcd): Re-introduce this again when we have a better way to manage
    // server side events on the client side
    let done_stream = futures::stream::once(async move {
        Ok(sse::Event::default()
            .json_data(json!(
                {"done": "[CODESTORY_DONE]".to_owned(),
                "session_id": plan_id.to_owned(),
            }))
            .expect("failed to send done object"))
    });

    let stream = init_stream.chain(answer_stream).chain(done_stream);

    Ok(Sse::new(Box::pin(stream)))
}

pub async fn handle_append_plan(
    user_query: String,
    user_context: UserContext,
    editor_url: String,
    plan_id: uuid::Uuid,
    plan_storage_path: String,
    plan_service: PlanService,
) -> Result<
    Sse<std::pin::Pin<Box<dyn tokio_stream::Stream<Item = anyhow::Result<sse::Event>> + Send>>>,
> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let cancellation_token = tokio_util::sync::CancellationToken::new();
    let (ui_sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let plan_id_str = plan_id.to_string();
    let message_properties = SymbolEventMessageProperties::new(
        SymbolEventRequestId::new(plan_id_str.to_owned(), plan_id_str.to_owned()),
        ui_sender,
        editor_url,
        cancellation_token,
    );
    // we let the plan creation happen in the background
    let _ = tokio::spawn(async move {
        append_to_plan(
            plan_id,
            plan_storage_path,
            plan_service,
            user_query,
            user_context,
            message_properties,
            sender,
        )
        .await;
    });
    let conversation_message_stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(receiver);
    // TODO(skcd): Re-introduce this again when we have a better way to manage
    // server side events on the client side
    let init_stream = futures::stream::once(async move {
        Ok(sse::Event::default()
            .json_data(json!({
                "session_id": plan_id.to_owned(),
            }))
            // This should never happen, so we force an unwrap.
            .expect("failed to serialize initialization object"))
    });

    // // We know the stream is unwind safe as it doesn't use synchronization primitives like locks.
    let answer_stream = conversation_message_stream.map(
        |conversation_message: anyhow::Result<ConversationMessage>| {
            if let Err(e) = &conversation_message {
                tracing::error!("error in conversation message stream: {}", e);
            }
            sse::Event::default()
                .json_data(conversation_message.expect("should not fail deserialization"))
                .map_err(anyhow::Error::new)
        },
    );

    // TODO(skcd): Re-introduce this again when we have a better way to manage
    // server side events on the client side
    let done_stream = futures::stream::once(async move {
        Ok(sse::Event::default()
            .json_data(json!(
                {"done": "[CODESTORY_DONE]".to_owned(),
                "session_id": plan_id.to_owned(),
            }))
            .expect("failed to send done object"))
    });

    let stream = init_stream.chain(answer_stream).chain(done_stream);

    Ok(Sse::new(Box::pin(stream)))
}

pub async fn handle_create_plan(
    user_query: String,
    user_context: UserContext,
    editor_url: String,
    plan_id: uuid::Uuid,
    plan_storage_path: String,
    plan_service: PlanService,
    is_deep_reasoning: bool,
) -> Result<
    Sse<std::pin::Pin<Box<dyn tokio_stream::Stream<Item = anyhow::Result<sse::Event>> + Send>>>,
> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    // we let the plan creation happen in the background
    let _ = tokio::spawn(async move {
        let _ = create_plan(
            user_query,
            user_context,
            editor_url,
            plan_id,
            plan_storage_path,
            plan_service,
            is_deep_reasoning,
            sender,
        )
        .await;
    });
    let conversation_message_stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(receiver);
    // TODO(skcd): Re-introduce this again when we have a better way to manage
    // server side events on the client side
    let init_stream = futures::stream::once(async move {
        Ok(sse::Event::default()
            .json_data(json!({
                "session_id": plan_id.to_owned(),
            }))
            // This should never happen, so we force an unwrap.
            .expect("failed to serialize initialization object"))
    });

    // // We know the stream is unwind safe as it doesn't use synchronization primitives like locks.
    let answer_stream = conversation_message_stream.map(
        |conversation_message: anyhow::Result<ConversationMessage>| {
            if let Err(e) = &conversation_message {
                tracing::error!("error in conversation message stream: {}", e);
            }
            sse::Event::default()
                .json_data(conversation_message.expect("should not fail deserialization"))
                .map_err(anyhow::Error::new)
        },
    );

    // TODO(skcd): Re-introduce this again when we have a better way to manage
    // server side events on the client side
    let done_stream = futures::stream::once(async move {
        Ok(sse::Event::default()
            .json_data(json!(
                {"done": "[CODESTORY_DONE]".to_owned(),
                "session_id": plan_id.to_owned(),
            }))
            .expect("failed to send done object"))
    });

    let stream = init_stream.chain(answer_stream).chain(done_stream);

    Ok(Sse::new(Box::pin(stream)))
}

pub async fn check_plan_storage_path(config: Arc<Configuration>, plan_id: String) -> String {
    let mut plan_path = config.index_dir.clone();
    plan_path = plan_path.join("plans");
    // check if the plan_storage_path_exists
    if tokio::fs::metadata(&plan_path).await.is_err() {
        tokio::fs::create_dir(&plan_path)
            .await
            .expect("directory creation to not fail");
    }
    plan_path = plan_path.join(plan_id);
    plan_path
        .to_str()
        .expect("path conversion to work on all platforms")
        .to_owned()
}
