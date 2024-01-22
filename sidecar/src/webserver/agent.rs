use super::agent_stream::generate_agent_stream;
use super::model_selection::LLMClientConfig;
use super::types::json;
use anyhow::Context;
use std::collections::HashSet;

use axum::response::IntoResponse;
use axum::{extract::Query as axumQuery, Extension, Json};
/// We will invoke the agent to get the answer, we are moving to an agent based work
use serde::{Deserialize, Serialize};

use crate::agent::types::AgentAction;
use crate::agent::types::CodeSpan;
use crate::agent::types::ConversationMessage;
use crate::agent::types::{Agent, VariableInformation as AgentVariableInformation};
use crate::application::application::Application;
use crate::chunking::text_document::Position as DocumentPosition;
use crate::repo::types::RepoRef;
use crate::reporting::posthog::client::PosthogEvent;

use super::types::ApiResponse;
use super::types::Result;

fn default_thread_id() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SearchInformation {
    pub query: String,
    pub reporef: RepoRef,
    #[serde(default = "default_thread_id")]
    pub thread_id: uuid::Uuid,
    pub model_config: LLMClientConfig,
}

impl ApiResponse for SearchInformation {}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct SearchResponse {
    pub query: String,
    pub answer: String,
}

impl ApiResponse for SearchResponse {}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum SearchEvents {
    SearchEvent(),
}

pub async fn search_agent(
    axumQuery(SearchInformation {
        query,
        reporef,
        thread_id,
        model_config,
    }): axumQuery<SearchInformation>,
    Extension(app): Extension<Application>,
) -> Result<impl IntoResponse> {
    let reranker = app.reranker.clone();
    let chat_broker = app.chat_broker.clone();
    let llm_tokenizer = app.llm_tokenizer.clone();
    let session_id = uuid::Uuid::new_v4();
    let llm_broker = app.llm_broker.clone();
    let sql_db = app.sql.clone();
    let (sender, receiver) = tokio::sync::mpsc::channel(100);
    let action = AgentAction::Query(query.clone());
    let previous_conversation_message =
        ConversationMessage::load_from_db(sql_db.clone(), &reporef, thread_id)
            .await
            .expect("loading from db to never fail");
    let agent = Agent::prepare_for_search(
        app,
        reporef,
        session_id,
        &query,
        llm_broker,
        thread_id,
        sql_db,
        previous_conversation_message,
        sender,
        Default::default(),
        model_config,
        llm_tokenizer,
        chat_broker,
        reranker,
    );

    generate_agent_stream(agent, action, receiver).await
}

// Here we are going to provide a hybrid search index which combines both the
// lexical and the semantic search together
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct HybridSearchQuery {
    query: String,
    repo: RepoRef,
    model_config: LLMClientConfig,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct HybridSearchResponse {
    session_id: uuid::Uuid,
    query: String,
    code_spans: Vec<CodeSpan>,
}

impl ApiResponse for HybridSearchResponse {}

/// What's hybrid search? Hybrid search combines the best things about both semantic
/// and lexical search along with statistics from the git log to generate the
/// best code spans which are relevant
pub async fn hybrid_search(
    axumQuery(HybridSearchQuery {
        query,
        repo,
        model_config,
    }): axumQuery<HybridSearchQuery>,
    Extension(app): Extension<Application>,
) -> Result<impl IntoResponse> {
    // Here we want to do the following:
    // - do a semantic search (normalize it to a score between 0.5 -> 1)
    // - do a lexical search (normalize it to a score between 0.5 -> 1)
    // - get statistics from the git log (normalize it to a score between 0.5 -> 1)
    // hand-waving the numbers here for whatever works for now
    // - final score -> git_log_score * 4 + lexical_search * 2.5 + semantic_search_score
    // - combine the score as following
    let reranker = app.reranker.clone();
    let chat_broker = app.chat_broker.clone();
    let llm_broker = app.llm_broker.clone();
    let llm_tokenizer = app.llm_tokenizer.clone();
    let session_id = uuid::Uuid::new_v4();
    let conversation_id = uuid::Uuid::new_v4();
    let sql_db = app.sql.clone();
    let (sender, _) = tokio::sync::mpsc::channel(100);
    let mut agent = Agent::prepare_for_semantic_search(
        app,
        repo,
        session_id,
        &query,
        llm_broker,
        conversation_id,
        sql_db,
        vec![], // we don't have a previous conversation message here
        sender,
        Default::default(),
        model_config,
        llm_tokenizer,
        chat_broker,
        reranker,
    );
    let hybrid_search_results = agent.code_search_hybrid(&query).await.unwrap_or(vec![]);
    Ok(json(HybridSearchResponse {
        session_id: uuid::Uuid::new_v4(),
        query,
        code_spans: hybrid_search_results,
    }))
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ExplainRequest {
    query: String,
    relative_path: String,
    start_line: u64,
    end_line: u64,
    repo_ref: RepoRef,
    #[serde(default = "default_thread_id")]
    thread_id: uuid::Uuid,
    model_config: LLMClientConfig,
}

/// We are going to handle the explain function here, but its going to be very
/// bare-bones right now. We don't give the user the option to explore or do
/// more things with the agent yet, ideal explain feature will be when the user
/// gets to explore the repository or maybe that can be a different UX like the
/// crawler
pub async fn explain(
    axumQuery(ExplainRequest {
        query,
        relative_path,
        start_line,
        end_line,
        repo_ref,
        thread_id,
        model_config,
    }): axumQuery<ExplainRequest>,
    Extension(app): Extension<Application>,
) -> Result<impl IntoResponse> {
    let reranker = app.reranker.clone();
    let chat_broker = app.chat_broker.clone();
    let llm_broker = app.llm_broker.clone();
    let llm_tokenizer = app.llm_tokenizer.clone();
    let user_id = app.user_id.to_owned();
    let posthog_client = app.posthog_client.clone();
    let sql_db = app.sql.clone();
    let file_content = app
        .indexes
        .file
        .get_by_path(&relative_path, &repo_ref)
        .await
        .context("file retrieval failed")?
        .context("requested file not found")?
        .content;

    let mut previous_messages =
        ConversationMessage::load_from_db(app.sql.clone(), &repo_ref, thread_id)
            .await
            .expect("loading from db to never fail");

    let snippet = file_content
        .lines()
        .skip(start_line.try_into().expect("conversion_should_not_fail"))
        .take(
            (end_line - start_line)
                .try_into()
                .expect("conversion_should_not_fail"),
        )
        .collect::<Vec<_>>()
        .join("\n");

    let mut conversation_message = ConversationMessage::explain_message(
        thread_id,
        crate::agent::types::AgentState::Explain,
        query,
    );

    let code_span = CodeSpan {
        file_path: relative_path.to_owned(),
        alias: 0,
        start_line,
        end_line,
        data: snippet,
        score: Some(1.0),
    };
    conversation_message.add_code_spans(code_span.clone());
    conversation_message.add_path(relative_path);

    previous_messages.push(conversation_message);

    let action = AgentAction::Answer { paths: vec![0] };

    let (sender, receiver) = tokio::sync::mpsc::channel(100);

    let session_id = uuid::Uuid::new_v4();

    let sql = app.sql.clone();
    let editor_parsing = Default::default();

    let agent = Agent {
        application: app,
        reporef: repo_ref,
        session_id,
        conversation_messages: previous_messages,
        llm_broker,
        sql_db: sql,
        sender,
        user_context: None,
        project_labels: vec![],
        editor_parsing,
        model_config,
        llm_tokenizer,
        chat_broker,
        reranker,
    };

    generate_agent_stream(agent, action, receiver).await
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum VariableType {
    File,
    CodeSymbol,
    Selection,
}

impl Into<crate::agent::types::VariableType> for VariableType {
    fn into(self) -> crate::agent::types::VariableType {
        match self {
            VariableType::File => crate::agent::types::VariableType::File,
            VariableType::CodeSymbol => crate::agent::types::VariableType::CodeSymbol,
            VariableType::Selection => crate::agent::types::VariableType::Selection,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct VariableInformation {
    pub start_position: Position,
    pub end_position: Position,
    pub fs_file_path: String,
    pub name: String,
    #[serde(rename = "type")]
    pub variable_type: VariableType,
    pub content: String,
    pub language: String,
}

impl VariableInformation {
    pub fn to_agent_type(self) -> AgentVariableInformation {
        AgentVariableInformation {
            start_position: DocumentPosition::new(
                self.start_position.line,
                self.start_position.character,
                0,
            ),
            end_position: DocumentPosition::new(
                self.end_position.line,
                self.end_position.character,
                0,
            ),
            fs_file_path: self.fs_file_path,
            name: self.name,
            variable_type: self.variable_type.into(),
            content: self.content,
            language: self.language,
        }
    }

    pub fn is_selection(&self) -> bool {
        self.variable_type == VariableType::Selection
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FileContentValue {
    pub file_path: String,
    pub file_content: String,
    pub language: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct UserContext {
    pub variables: Vec<VariableInformation>,
    pub file_content_map: Vec<FileContentValue>,
}

impl UserContext {
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty() && self.file_content_map.is_empty()
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ActiveWindowData {
    pub file_path: String,
    pub file_content: String,
    pub language: String,
}

impl UserContext {
    fn merge_from_previous(mut self, previous: Option<&UserContext>) -> Self {
        // Here we try and merge the user contexts together, if we have something
        match previous {
            Some(previous_user_context) => {
                let previous_file_content = &previous_user_context.file_content_map;
                let previous_user_variables = &previous_user_context.variables;
                // We want to merge the variables together, but keep the unique
                // ones only
                // TODO(skcd): We should be filtering on variables here, but for
                // now we ball 🖲️
                self.variables
                    .extend(previous_user_variables.to_vec().into_iter());
                // We want to merge the file content map together, and only keep
                // the unique ones and the new file content map we are getting if
                // there are any repetitions
                let mut file_content_set: HashSet<String> = HashSet::new();
                self.file_content_map.iter().for_each(|file_content| {
                    file_content_set.insert(file_content.file_path.to_owned());
                });
                // Look at the previous ones and add those which are missing
                previous_file_content.into_iter().for_each(|file_content| {
                    if !file_content_set.contains(&file_content.file_path) {
                        self.file_content_map.push(file_content.clone());
                    }
                });
                self
            }
            None => self,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FollowupChatRequest {
    pub query: String,
    pub repo_ref: RepoRef,
    pub thread_id: uuid::Uuid,
    pub user_context: UserContext,
    pub project_labels: Vec<String>,
    pub active_window_data: Option<ActiveWindowData>,
    pub openai_key: Option<String>,
    pub model_config: LLMClientConfig,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepContextForView {
    pub repo_ref: RepoRef,
    pub precise_context: Vec<PreciseContext>,
    pub cursor_position: Option<CursorPosition>,
    pub current_view_port: Option<CurrentViewPort>,
    pub language: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionSnippet {
    pub context: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreciseContext {
    pub symbol: Symbol,
    pub hover_text: Vec<String>,
    pub definition_snippet: DefinitionSnippet,
    pub fs_file_path: String,
    pub relative_file_path: String,
    pub range: Range,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Symbol {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuzzy_name: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorPosition {
    pub start_position: Position,
    pub end_position: Position,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentViewPort {
    pub start_position: Position,
    pub end_position: Position,
    pub relative_path: String,
    pub fs_file_path: String,
    pub text_on_screen: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Position {
    pub line: usize,
    pub character: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    pub start_line: usize,
    pub start_character: usize,
    pub end_line: usize,
    pub end_character: usize,
}

pub async fn followup_chat(
    Extension(app): Extension<Application>,
    Json(FollowupChatRequest {
        query,
        repo_ref,
        thread_id,
        user_context,
        project_labels,
        active_window_data,
        openai_key,
        model_config,
    }): Json<FollowupChatRequest>,
) -> Result<impl IntoResponse> {
    let session_id = uuid::Uuid::new_v4();
    let user_id = app.user_id.to_owned();
    let mut event = PosthogEvent::new("model_config");
    let _ = event.insert_prop("config", model_config.logging_config());
    let _ = event.insert_prop("user_id", user_id);
    let _ = app.posthog_client.capture(event).await;
    // Here we do something special, if the user is asking a followup question
    // we just look at the previous conversation message the thread belonged
    // to and use that as context for grounding the agent response. In the future
    // we can obviously add more context using @ symbols etc
    let reranker = app.reranker.clone();
    let chat_broker = app.chat_broker.clone();
    let llm_broker = app.llm_broker.clone();
    let llm_tokenizer = app.llm_tokenizer.clone();
    let sql_db = app.sql.clone();
    let mut previous_messages =
        ConversationMessage::load_from_db(sql_db.clone(), &repo_ref, thread_id)
            .await
            .unwrap_or_default();
    let last_user_context = previous_messages
        .last()
        .map(|previous_message| previous_message.get_user_context());

    let user_context = user_context.merge_from_previous(last_user_context);

    let mut conversation_message = ConversationMessage::general_question(
        thread_id,
        crate::agent::types::AgentState::FollowupChat,
        query.to_owned(),
    );

    // Add the path for the active window to the conversation message as well
    if let Some(active_window_data) = &active_window_data {
        conversation_message.add_path(active_window_data.file_path.to_owned());
    }
    conversation_message.set_user_context(user_context.clone());
    conversation_message.set_active_window(active_window_data);

    // We add all the paths which we are going to get into the conversation message
    // so that we can use that for the next followup question
    user_context
        .file_content_map
        .iter()
        .for_each(|file_content_value| {
            conversation_message.add_path(file_content_value.file_path.to_owned());
        });

    // We also want to add the file path for the active window if it's not already there
    let file_path_len = conversation_message.get_paths().len();
    previous_messages.push(conversation_message);

    let (sender, receiver) = tokio::sync::mpsc::channel(100);

    // If this is a followup, right now we don't take in any additional context,
    // but only use the one from our previous conversation
    let action = AgentAction::Answer {
        paths: (0..file_path_len).collect(),
    };

    let agent = if let Some(openai_user_key) = openai_key {
        Agent::prepare_for_followup(
            app,
            repo_ref,
            session_id,
            llm_broker,
            sql_db,
            previous_messages,
            sender,
            user_context,
            project_labels,
            Default::default(),
            model_config,
            llm_tokenizer,
            chat_broker,
            reranker,
        )
    } else {
        Agent::prepare_for_followup(
            app,
            repo_ref,
            session_id,
            llm_broker,
            sql_db,
            previous_messages,
            sender,
            user_context,
            project_labels,
            Default::default(),
            model_config,
            llm_tokenizer,
            chat_broker,
            reranker,
        )
    };

    generate_agent_stream(agent, action, receiver).await
}

#[cfg(test)]
mod tests {
    use super::FollowupChatRequest;
    use serde_json;

    #[test]
    fn test_parsing() {
        let input_string = r#"
        {"repo_ref":"local//Users/skcd/scratch/website","query":"whats happenign here","thread_id":"7cb05252-1bb8-4d5e-a942-621ab5d5e114","deep_context":{"repoRef":"local//Users/skcd/scratch/website","preciseContext":[{"symbol":{"fuzzyName":"Author"},"fsFilePath":"/Users/skcd/scratch/website/interfaces/author.ts","relativeFilePath":"interfaces/author.ts","range":{"startLine":0,"startCharacter":0,"endLine":6,"endCharacter":1},"hoverText":["\n```typescript\n(alias) type Author = {\n    name: string;\n    picture: string;\n    twitter: string;\n    linkedin: string;\n    github: string;\n}\nimport Author\n```\n",""],"definitionSnippet":"type Author = {\n  name: string\n  picture: string\n  twitter: string\n  linkedin: string\n  github: string\n}"}],"cursorPosition":{"startPosition":{"line":16,"character":0},"endPosition":{"line":16,"character":0}},"currentViewPort":{"startPosition":{"line":0,"character":0},"endPosition":{"line":16,"character":0},"fsFilePath":"/Users/skcd/scratch/website/interfaces/post.ts","relativePath":"interfaces/post.ts","textOnScreen":"import type Author from './author'\n\ntype PostType = {\n  slug: string\n  title: string\n  date: string\n  coverImage: string\n  author: Author\n  excerpt: string\n  ogImage: {\n    url: string\n  }\n  content: string\n}\n\nexport default PostType\n"}}}
        "#;
        let parsed_response = serde_json::from_str::<FollowupChatRequest>(&input_string);
        assert!(parsed_response.is_ok());
    }
}
