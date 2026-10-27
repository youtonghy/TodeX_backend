//! `/v2/providers/pi/subagent-templates*` — the global custom agent types of
//! the `@tintinweb/pi-subagents` extension (see
//! [`crate::pi_agent_templates`]). Every write needs a signed device: a
//! template decides the prompt, tools and model a subagent runs with.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent_providers::AgentDirs;
use crate::app_state::AppState;
use crate::error::AppError;
use crate::pi_agent_templates::{self, TemplateDocument, TemplateInput, TemplateList};

use super::agent_providers::require_signed_device;
use super::v2::require_auth;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v2/providers/pi/subagent-templates",
            get(list).post(create),
        )
        .route(
            "/v2/providers/pi/subagent-templates/built-ins/{name}",
            put(set_built_in),
        )
        .route(
            "/v2/providers/pi/subagent-templates/{file}",
            get(read).put(update).delete(delete),
        )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteRequest {
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    front_matter: Option<Map<String, Value>>,
    #[serde(default)]
    body: Option<String>,
}

impl WriteRequest {
    fn parse(body: Value) -> Result<Self, AppError> {
        serde_json::from_value(body)
            .map_err(|error| AppError::InvalidRequest(format!("invalid template request: {error}")))
    }

    fn input(&mut self) -> Result<TemplateInput, AppError> {
        match (
            self.content.take(),
            self.front_matter.take(),
            self.body.take(),
        ) {
            (Some(content), None, None) => Ok(TemplateInput::Content { content }),
            (None, Some(front_matter), Some(body)) => {
                Ok(TemplateInput::Form { front_matter, body })
            }
            _ => Err(AppError::InvalidRequest(
                "send either content, or frontMatter and body".to_owned(),
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BuiltInRequest {
    enabled: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Deleted {
    file: String,
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<TemplateList>, AppError> {
    require_auth(&state, &headers)?;
    blocking(|| pi_agent_templates::list(&AgentDirs::detect())).await
}

async fn read(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Result<Json<TemplateDocument>, AppError> {
    require_auth(&state, &headers)?;
    blocking(move || pi_agent_templates::read(&AgentDirs::detect(), &file)).await
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<TemplateDocument>, AppError> {
    require_signed_device(&state, &headers, "creating Pi subagent templates")?;
    let mut request = WriteRequest::parse(body)?;
    if request.revision.is_some() {
        return Err(AppError::InvalidRequest(
            "a new template has no revision".to_owned(),
        ));
    }
    let file = request
        .file
        .take()
        .ok_or_else(|| AppError::InvalidRequest("file is required".to_owned()))?;
    let input = request.input()?;
    blocking(move || pi_agent_templates::create(&AgentDirs::detect(), &file, input)).await
}

async fn update(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<TemplateDocument>, AppError> {
    require_signed_device(&state, &headers, "editing Pi subagent templates")?;
    let mut request = WriteRequest::parse(body)?;
    if request.file.is_some() {
        return Err(AppError::InvalidRequest(
            "a template cannot be renamed".to_owned(),
        ));
    }
    let revision = request
        .revision
        .take()
        .ok_or_else(|| AppError::InvalidRequest("revision is required".to_owned()))?;
    let input = request.input()?;
    blocking(move || pi_agent_templates::update(&AgentDirs::detect(), &file, &revision, input))
        .await
}

async fn delete(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Deleted>, AppError> {
    require_signed_device(&state, &headers, "deleting Pi subagent templates")?;
    blocking(move || {
        pi_agent_templates::delete(&AgentDirs::detect(), &file)?;
        Ok(Deleted { file })
    })
    .await
}

async fn set_built_in(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<TemplateList>, AppError> {
    require_signed_device(&state, &headers, "switching Pi built-in subagents")?;
    let request: BuiltInRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid built-in request: {error}")))?;
    blocking(move || pi_agent_templates::set_built_in(&AgentDirs::detect(), &name, request.enabled))
        .await
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<Json<T>, AppError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
        .map(Json)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requests_carry_either_the_text_or_the_form() {
        let mut text = WriteRequest::parse(json!({"file": "a", "content": "x"})).unwrap();
        assert!(matches!(text.input(), Ok(TemplateInput::Content { content }) if content == "x"));
        let mut form = WriteRequest::parse(
            json!({"revision": "r", "frontMatter": {"color": "red"}, "body": ""}),
        )
        .unwrap();
        assert!(matches!(form.input(), Ok(TemplateInput::Form { .. })));
        for body in [
            json!({"content": "x", "body": "y"}),
            json!({"frontMatter": {}}),
            json!({}),
        ] {
            let mut request = WriteRequest::parse(body.clone()).unwrap();
            assert!(
                matches!(request.input(), Err(AppError::InvalidRequest(_))),
                "{body}"
            );
        }
        assert!(WriteRequest::parse(json!({"content": "x", "path": "/etc/passwd"})).is_err());
    }
}
