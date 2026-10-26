//! `generate_image`: make an image with any OpenAI-compatible images API and
//! save it to the workspace, ready for share_file.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use openslacktag_core::error::{Error, Result};
use openslacktag_core::tool::{PermissionRequest, Tool, ToolContext, ToolOutcome, ToolSource};
use serde_json::{Value, json};

const SIZES: [&str; 3] = ["1024x1024", "1536x1024", "1024x1536"];

#[derive(Debug, Clone)]
pub struct ImageConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

#[derive(Debug)]
pub struct GenerateImage {
    config: ImageConfig,
    http: reqwest::Client,
}

impl GenerateImage {
    pub fn new(config: ImageConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .build()
                .unwrap_or_default(),
        }
    }

    async fn generate(&self, prompt: &str, size: &str) -> anyhow::Result<Vec<u8>> {
        let mut request = self
            .http
            .post(format!(
                "{}/images/generations",
                self.config.base_url.trim_end_matches('/')
            ))
            .json(&json!({"model": self.config.model, "prompt": prompt, "size": size, "n": 1}));
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        let status = response.status();
        let body: Value = response.json().await?;
        if !status.is_success() {
            anyhow::bail!(
                "the images API returned {status}: {}",
                body["error"]["message"].as_str().unwrap_or("no details")
            );
        }
        let image = &body["data"][0];
        if let Some(b64) = image["b64_json"].as_str() {
            return Ok(base64::engine::general_purpose::STANDARD.decode(b64)?);
        }
        if let Some(url) = image["url"].as_str() {
            return Ok(self
                .http
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?
                .to_vec());
        }
        anyhow::bail!("the images API returned no image")
    }
}

#[async_trait]
impl Tool for GenerateImage {
    fn id(&self) -> &str {
        "generate_image"
    }

    fn description(&self) -> &str {
        "Generate an image from a description and save it in your workspace as a PNG. Share it with share_file. \
         Describe the subject, style, composition and any text exactly."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string"},
                "size": {"type": "string", "enum": SIZES, "description": "Square, landscape or portrait (default square)"},
                "name": {"type": "string", "description": "Optional file name, without extension"}
            },
            "required": ["prompt"],
            "additionalProperties": false
        })
    }

    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    fn permission(&self, args: &Value) -> PermissionRequest {
        PermissionRequest::new("generate_image", args["prompt"].as_str().unwrap_or("*"))
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let prompt = args["prompt"].as_str().unwrap_or_default().trim();
        if prompt.is_empty() {
            return Err(Error::InvalidInput("prompt is empty".into()));
        }
        let size = args["size"].as_str().unwrap_or(SIZES[0]);
        let bytes = self
            .generate(prompt, size)
            .await
            .map_err(|e| Error::Upstream {
                status: None,
                message: format!("{e:#}"),
            })?;
        let stem = args["name"]
            .as_str()
            .map(crate::safe_file_name)
            .unwrap_or_else(|| {
                format!("image-{}", &uuid::Uuid::new_v4().simple().to_string()[..6])
            });
        let relative = format!("images/{stem}.png");
        let path = crate::files::resolve(&ctx.root, &relative)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, &bytes)?;
        let mut outcome = ToolOutcome::text(format!(
            "Saved {relative} ({} bytes). Share it with share_file.",
            bytes.len()
        ));
        // Shown to the model too, so it can check the result before sharing.
        outcome.images.push(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        ));
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use axum::routing::post;
    use axum::{Json, Router};
    use openslacktag_core::SessionId;
    use openslacktag_mock::MockGate;
    use std::sync::Arc;

    #[tokio::test]
    async fn generates_and_saves_an_image() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/images/generations",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["model"], "img-model");
                assert_eq!(body["size"], "1536x1024");
                let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNGfake");
                Json(json!({"data": [{"b64_json": png}]}))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext {
            root: dir.path().to_path_buf(),
            session: SessionId::new(),
            call_id: "c".into(),
            permissions: Arc::new(MockGate::allow_all()),
        };
        let tool = GenerateImage::new(ImageConfig {
            base_url: base,
            api_key: Some("k".into()),
            model: "img-model".into(),
        });
        let out = tool
            .execute(
                json!({"prompt": "a fox", "size": "1536x1024", "name": "fox"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.output.contains("images/fox.png"));
        assert_eq!(
            std::fs::read(dir.path().join("images/fox.png")).unwrap(),
            b"\x89PNGfake"
        );
        assert!(out.images[0].starts_with("data:image/png;base64,"));
    }
}
