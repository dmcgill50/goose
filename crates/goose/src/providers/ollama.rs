use super::base::{Provider, ProviderUsage, Usage};
use super::configs::{ModelConfig, OllamaProviderConfig, ProviderModelConfig};
use super::utils::{
    get_model, messages_to_openai_spec, openai_response_to_message, tools_to_openai_spec,
    ImageFormat,
};
use crate::message::Message;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use mcp_core::tool::Tool;
use reqwest::Client;
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::time::Duration;

pub const OLLAMA_HOST: &str = "http://localhost:11434";
pub const OLLAMA_MODEL: &str = "qwen2.5";

pub struct OllamaProvider {
    client: Client,
    config: OllamaProviderConfig,
}

impl OllamaProvider {
    pub fn new(config: OllamaProviderConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(600)) // 10 minutes timeout
            .build()?;

        Ok(Self { client, config })
    }

    fn get_usage(data: &Value) -> Result<Usage> {
        let usage = data
            .get("usage")
            .ok_or_else(|| anyhow!("No usage data in response"))?;

        let input_tokens = usage
            .get("prompt_tokens")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);

        let output_tokens = usage
            .get("completion_tokens")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);

        let total_tokens = usage
            .get("total_tokens")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .or_else(|| match (input_tokens, output_tokens) {
                (Some(input), Some(output)) => Some(input + output),
                _ => None,
            });

        Ok(Usage::new(input_tokens, output_tokens, total_tokens))
    }

    async fn post(&self, payload: Value) -> Result<Value> {
        let url = format!(
            "{}/v1/chat/completions",
            self.config.host.trim_end_matches('/')
        );

        let response = self.client.post(&url).json(&payload).send().await?;

        match response.status() {
            StatusCode::OK => Ok(response.json().await?),
            status if status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() >= 500 => {
                Err(anyhow!("Server error: {}", status))
            }
            _ => Err(anyhow!(
                "Request failed: {}\nPayload: {}",
                response.status(),
                payload
            )),
        }
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, ProviderUsage)> {
        let system_message = json!({
            "role": "system",
            "content": system
        });

        let messages_spec = messages_to_openai_spec(messages, &ImageFormat::OpenAi);
        let tools_spec = tools_to_openai_spec(tools)?;

        let mut messages_array = vec![system_message];
        messages_array.extend(messages_spec);

        let mut payload = json!({
            "model": self.config.model.model_name,
            "messages": messages_array
        });

        if !tools_spec.is_empty() {
            payload
                .as_object_mut()
                .unwrap()
                .insert("tools".to_string(), json!(tools_spec));
        }
        if let Some(temp) = self.config.model.temperature {
            payload
                .as_object_mut()
                .unwrap()
                .insert("temperature".to_string(), json!(temp));
        }
        if let Some(tokens) = self.config.model.max_tokens {
            payload
                .as_object_mut()
                .unwrap()
                .insert("max_tokens".to_string(), json!(tokens));
        }

        let response = self.post(payload).await?;

        // Parse response
        let message = openai_response_to_message(response.clone())?;
        let usage = Self::get_usage(&response)?;
        let model = get_model(&response);
        let cost = None;

        Ok((message, ProviderUsage::new(model, usage, cost)))
    }

    fn get_model_config(&self) -> &ModelConfig {
        self.config.model_config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageContent;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn _setup_mock_server(response_body: Value) -> (MockServer, OllamaProvider) {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&mock_server)
            .await;

        // Create the OllamaProvider with the mock server's URL as the host
        let config = OllamaProviderConfig {
            host: mock_server.uri(),
            model: ModelConfig::new(OLLAMA_MODEL.to_string()),
        };

        let provider = OllamaProvider::new(config).unwrap();
        (mock_server, provider)
    }

    #[tokio::test]
    async fn test_complete_basic() -> Result<()> {
        // Mock response for normal completion
        let response_body = json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I assist you today?",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 15,
                "total_tokens": 27
            }
        });

        let (_, provider) = _setup_mock_server(response_body).await;

        // Prepare input messages
        let messages = vec![Message::user().with_text("Hello?")];

        // Call the complete method
        let (message, usage) = provider
            .complete("You are a helpful assistant.", &messages, &[])
            .await?;

        // Assert the response
        if let MessageContent::Text(text) = &message.content[0] {
            assert_eq!(text.text, "Hello! How can I assist you today?");
        } else {
            panic!("Expected Text content");
        }
        assert_eq!(usage.usage.input_tokens, Some(12));
        assert_eq!(usage.usage.output_tokens, Some(15));
        assert_eq!(usage.usage.total_tokens, Some(27));

        Ok(())
    }

    #[tokio::test]
    async fn test_complete_tool_request() -> Result<()> {
        // Mock response for tool calling
        let response_body = json!({
            "id": "chatcmpl-tool",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_h5d3s25w",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"filename\":\"test.txt\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 63,
                "completion_tokens": 70,
                "total_tokens": 133
            }
        });

        let (_, provider) = _setup_mock_server(response_body).await;

        // Input messages
        let messages = vec![Message::user().with_text("Can you read the test.txt file?")];

        // Define the tool
        let tool = Tool::new(
            "read_file",
            "Read the content of a file",
            json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "The name of the file to read"
                    }
                },
                "required": ["filename"]
            }),
        );

        // Call the complete method
        let (message, usage) = provider
            .complete("You are a helpful assistant.", &messages, &[tool])
            .await?;

        // Assert the response
        if let MessageContent::ToolRequest(tool_request) = &message.content[0] {
            let tool_call = tool_request.tool_call.as_ref().unwrap();
            assert_eq!(tool_call.name, "read_file");
            assert_eq!(tool_call.arguments, json!({"filename": "test.txt"}));
        } else {
            panic!("Expected ToolCall content");
        }

        assert_eq!(usage.usage.input_tokens, Some(63));
        assert_eq!(usage.usage.output_tokens, Some(70));
        assert_eq!(usage.usage.total_tokens, Some(133));

        Ok(())
    }

    #[tokio::test]
    async fn test_server_error() -> Result<()> {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let config = OllamaProviderConfig {
            host: mock_server.uri(),
            model: ModelConfig::new(OLLAMA_MODEL.to_string()),
        };

        let provider = OllamaProvider::new(config)?;
        let messages = vec![Message::user().with_text("Hello?")];
        let result = provider
            .complete("You are a helpful assistant.", &messages, &[])
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Server error: 500"));

        Ok(())
    }
}
