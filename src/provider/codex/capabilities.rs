use super::*;
use std::time::Duration;

#[derive(Clone)]
pub(super) struct ControlProbe {
    pub live: bool,
    pub queue: bool,
    pub diagnostic: Value,
}

pub(super) async fn probe(binary: &str) -> ControlProbe {
    let directory =
        std::env::temp_dir().join(format!("todex-codex-schema-{}", uuid::Uuid::new_v4()));
    let result = async {
        tokio::fs::create_dir_all(&directory).await?;
        let mut spec = CommandSpec::new(binary, &directory);
        spec.args = vec!["app-server".into(),"generate-json-schema".into(),"--experimental".into(),"--out".into(),directory.display().to_string()];
        let output = super::super::process::run_bounded_command(&spec,4096,Duration::from_secs(5)).await?;
        if !output.success { return Err(AppError::Unsupported("Installed Codex cannot export its experimental protocol schema".to_owned())); }
        let path = directory.join("ClientRequest.json");
        if tokio::fs::metadata(&path).await?.len() > 16 * 1024 * 1024 { return Err(AppError::Unsupported("Codex schema exceeded the inspection size limit".to_owned())); }
        let schema: Value = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        let (live,queue) = parse(&schema);
        Ok::<_,AppError>(ControlProbe {live,queue,diagnostic:json!({"status":"verified","source":"installed-schema","experimental":true,"liveControls":live,"nativeQueue":queue})})
    }.await;
    let _ = tokio::fs::remove_dir_all(&directory).await;
    result.unwrap_or_else(|_|ControlProbe {live:false,queue:false,diagnostic:json!({"status":"unavailable","source":"installed-schema","reason":"The installed CLI schema could not be verified; experimental controls are disabled. Restart the backend after updating the CLI to probe again."})})
}

fn parse(schema: &Value) -> (bool, bool) {
    let supports = |method: &str, fields: &[&str]| {
        schema
            .get("oneOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|request| {
                let methods = request
                    .pointer("/properties/method/enum")
                    .and_then(Value::as_array);
                if !methods.is_some_and(|methods| {
                    methods.iter().any(|value| value.as_str() == Some(method))
                }) {
                    return false;
                }
                let Some(params) = request.pointer("/properties/params") else {
                    return false;
                };
                let params = if let Some(reference) = params.get("$ref").and_then(Value::as_str) {
                    let Some(pointer) = reference.strip_prefix('#') else {
                        return false;
                    };
                    let Some(definition) = schema.pointer(pointer) else {
                        return false;
                    };
                    definition
                } else {
                    params
                };
                fields.iter().all(|field| {
                    params
                        .get("properties")
                        .and_then(|properties| properties.get(*field))
                        .is_some()
                })
            })
    };
    (
        supports("turn/steer", &["threadId", "expectedTurnId", "input"])
            && supports(
                "turn/settings/update",
                &["threadId", "turnId", "model", "effort"],
            ),
        supports(
            "thread/queue/add",
            &["threadId", "input", "clientUserMessageId"],
        ) && supports("thread/queue/delete", &["threadId", "queuedSubmissionId"])
            && supports("thread/queue/list", &["threadId", "cursor"])
            && supports("thread/queue/start", &["threadId", "queuedSubmissionId"]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_gate_requires_all_methods_and_wire_fields() {
        let pairs: &[(&str, &[&str])] = &[
            ("turn/steer", &["threadId", "expectedTurnId", "input"]),
            (
                "turn/settings/update",
                &["threadId", "turnId", "model", "effort"],
            ),
            (
                "thread/queue/add",
                &["threadId", "input", "clientUserMessageId"],
            ),
            ("thread/queue/delete", &["threadId", "queuedSubmissionId"]),
            ("thread/queue/list", &["threadId", "cursor"]),
            ("thread/queue/start", &["threadId", "queuedSubmissionId"]),
        ];
        let mut definitions = serde_json::Map::new();
        let entries: Vec<Value> = pairs.iter().enumerate().map(|(i,(method,fields))| {
            let properties: serde_json::Map<String,Value> = fields.iter().map(|field|((*field).to_owned(),json!({"type":"string"}))).collect();
            definitions.insert(format!("Params{i}"),json!({"properties":properties}));
            json!({"properties":{"method":{"enum":[method]},"params":{"$ref":format!("#/definitions/Params{i}")}}})
        }).collect();
        let mut schema = json!({"definitions":definitions,"oneOf":entries});
        assert_eq!(parse(&schema), (true, true));
        schema["definitions"]["Params0"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("expectedTurnId");
        assert_eq!(parse(&schema), (false, true));
        schema["oneOf"].as_array_mut().unwrap().pop();
        assert_eq!(parse(&schema), (false, false));
        assert_eq!(parse(&json!({})), (false, false));
    }
}
