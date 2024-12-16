use chrono::Utc;
use serde_json::{json, Value};
use tracing::{span, Id, Subscriber, Level, Metadata};
use tracing_subscriber::Layer;
use tracing::field::{Visit};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::time::Duration;
use uuid::Uuid;
use std::env;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use tracing_subscriber::registry::LookupSpan;

const DEFAULT_LANGFUSE_URL: &str = "http://localhost:3000";

#[derive(Debug, Serialize, Deserialize)]
struct IngestionResponse {
    successes: Vec<IngestionSuccess>,
    errors: Vec<IngestionError>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IngestionSuccess {
    id: String,
    status: i32,
}

#[derive(Debug, Serialize, Deserialize)]
struct IngestionError {
    id: String,
    status: i32,
    message: Option<String>,
    error: Option<Value>,
}

#[derive(Debug)]
struct JsonVisitor(serde_json::Map<String, Value>);

impl Visit for JsonVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            json!(format!("{:?}", value))
        );
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), json!(value));
    }
}

#[derive(Debug, Clone)]
pub struct LangfuseLayer {
    state: Arc<Mutex<LangfuseState>>,
    client: reqwest::Client,
    base_url: String,
}

#[derive(Debug)]
struct LangfuseState {
    public_key: String,
    secret_key: String,
    batch: Vec<Value>,
    current_trace_id: Option<String>,
    current_observation_id: Option<String>,
    active_spans: HashMap<u64, (String, String)>, // (langfuse_id, observation_id)
    span_hierarchy: HashMap<u64, u64>, // child span -> parent span
}

impl LangfuseLayer {
    pub fn new(public_key: String, secret_key: String) -> Self {
        let langfuse_url = env::var("LANGFUSE_URL")
            .unwrap_or_else(|_| DEFAULT_LANGFUSE_URL.to_string());

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        let layer = Self {
            state: Arc::new(Mutex::new(LangfuseState {
                public_key,
                secret_key,
                batch: Vec::new(),
                current_trace_id: None,
                current_observation_id: None,
                active_spans: HashMap::new(),
                span_hierarchy: HashMap::new(),
            })),
            client,
            base_url: langfuse_url,
        };

        let layer_clone = layer.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Err(e) = layer_clone.send_batch().await {
                    // tracing::error!("Failed to send batch to Langfuse: {}", e);
                    println!("Failed to send batch to Langfuse: {}", e);
                }
            }
        });

        layer
    }

    fn map_level(level: &Level) -> &'static str {
        match *level {
            Level::ERROR => "ERROR",
            Level::WARN => "WARNING",
            Level::INFO => "DEFAULT",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "DEBUG",
        }
    }

    async fn send_batch(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut state = self.state.lock().await;
        if state.batch.is_empty() {
            return Ok(());
        }

        let payload = json!({
            "batch": state.batch,
            "metadata": {
                "sdk": "rust-tracing",
                "sdk_version": env!("CARGO_PKG_VERSION")
            }
        });

        let url = format!("{}/api/public/ingestion", self.base_url);
        
        // tracing::debug!("Sending batch to Langfuse at {} with {} items", url, state.batch.len());
        // tracing::trace!("Payload: {}", serde_json::to_string_pretty(&payload).unwrap());

        let response = self.client
            .post(&url)
            .basic_auth(&state.public_key, Some(&state.secret_key))
            .json(&payload)
            .send()
            .await?;

        // tracing::debug!("Response status: {}", response.status());
        
        let response_text = response.text().await?;
        // tracing::trace!("Response body: {}", response_text);

        if let Ok(response_body) = serde_json::from_str::<IngestionResponse>(&response_text) {
            for error in &response_body.errors {
                // tracing::error!(
                //     "Langfuse ingestion error: {} - {} - {:?}",
                //     error.status,
                //     error.message.as_deref().unwrap_or("No message"),
                //     error.error
                // );
                println!("Langfuse ingestion error: {} - {} - {:?}",
                    error.status,
                    error.message.as_deref().unwrap_or("No message"),
                    error.error
                );
            }

            if !response_body.errors.is_empty() {
                return Err("Langfuse ingestion contained errors".into());
            }
        }

        tracing::debug!("Successfully sent batch to Langfuse");
        state.batch.clear();
        Ok(())
    }

    async fn create_trace_if_needed(&self) -> String {
        let mut state = self.state.lock().await;
        if state.current_trace_id.is_none() {
            let trace_id = Uuid::new_v4().to_string();
            let now = Utc::now();
            let trace_event = json!({
                "id": Uuid::new_v4().to_string(),
                "timestamp": now.to_rfc3339(),
                "type": "trace-create",
                "body": {
                    "id": trace_id,
                    "name": format!("{}", now.timestamp()),
                    "timestamp": now.to_rfc3339(),
                    "input": {},
                    "metadata": {},
                    "tags": [],
                    "public": false
                }
            });
            tracing::debug!("Creating new trace with ID: {}", trace_id);
            state.batch.push(trace_event);
            state.current_trace_id = Some(trace_id.clone());
            trace_id
        } else {
            state.current_trace_id.clone().unwrap()
        }
    }

    async fn set_current_observation_id(&self, observation_id: Option<String>) {
        let mut state = self.state.lock().await;
        state.current_observation_id = observation_id;
    }

    async fn is_active_span(&self, span_id: u64) -> Option<(String, String)> {
        let state = self.state.lock().await;
        state.active_spans.get(&span_id).cloned()
    }

    async fn add_active_span(&self, span_id: u64, langfuse_id: String, observation_id: String) {
        let mut state = self.state.lock().await;
        state.active_spans.insert(span_id, (langfuse_id.clone(), observation_id.clone()));
        // tracing::debug!("Added active span: {} ({}) with observation ID {} - Total active spans: {}", 
                //  span_id, langfuse_id, observation_id, state.active_spans.len());
    }

    async fn remove_active_span(&self, span_id: u64) -> Option<(String, String)> {
        let mut state = self.state.lock().await;
        let result = state.active_spans.remove(&span_id);
        // tracing::debug!("Removed active span: {} - Total active spans: {}", span_id, state.active_spans.len());
        result
    }

    async fn get_parent_observation_id(&self, span_id: u64) -> Option<String> {
        let state = self.state.lock().await;
        state.span_hierarchy.get(&span_id)
            .and_then(|parent_span_id| {
                state.active_spans.get(parent_span_id)
                    .map(|(_, observation_id)| observation_id.clone())
            })
    }

    async fn add_span_to_hierarchy(&self, child_span: u64, parent_span: u64) {
        let mut state = self.state.lock().await;
        state.span_hierarchy.insert(child_span, parent_span);
    }
}

impl<S> Layer<S> for LangfuseLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &span::Attributes<'_>,
        id: &span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let target = attrs.metadata().target();
        
        // Only process spans from goose
        // if !target.starts_with("agent_tracing") {
        //     // println!("Target is not goose::");
        //     println!("Target is {:?}", target);
        //     return;
        // }

        let mut visitor = JsonVisitor(serde_json::Map::new());
        attrs.record(&mut visitor);
        let metadata = visitor.0;

        let span_id = id.into_u64();
        let langfuse_id = Uuid::new_v4().to_string();
        let observation_id = Uuid::new_v4().to_string();
        let span_name = attrs.metadata().name().to_string();
        let start_time = Utc::now().to_rfc3339();
        let target = target.to_string();
        let langfuse_level = Self::map_level(attrs.metadata().level());
        
        // Get parent span ID if it exists
        let parent_span = ctx.lookup_current().and_then(|span| span.parent()).map(|s| s.id().into_u64());
        
        let layer = self.clone();
        tokio::spawn(async move {
            let parent_observation_id = if let Some(parent_id) = parent_span {
                layer.get_parent_observation_id(parent_id).await
            } else {
                None
            };

            if let Some(parent_id) = parent_span {
                layer.add_span_to_hierarchy(span_id, parent_id).await;
            }

            layer.add_active_span(span_id, langfuse_id.clone(), observation_id.clone()).await;
            let trace_id = layer.create_trace_if_needed().await;

            let observation_event = json!({
                "id": Uuid::new_v4().to_string(),
                "timestamp": start_time.clone(),
                "type": "observation-create",
                "body": {
                    "id": observation_id,
                    "traceId": trace_id,
                    "type": "SPAN",
                    "name": span_name,
                    "startTime": start_time,
                    "parentObservationId": parent_observation_id,
                    "metadata": metadata,
                    "level": langfuse_level
                }
            });

            let mut state = layer.state.lock().await;
            state.batch.push(observation_event);
        });
    }

    fn on_close(&self, id: Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let span_id = id.into_u64();
        let end_time = Utc::now().to_rfc3339();
        
        // tracing::debug!("Span closing: {}", span_id);
        
        let layer = self.clone();
        tokio::spawn(async move {
            if let Some((_langfuse_id, observation_id)) = layer.remove_active_span(span_id).await {
                let trace_id = layer.create_trace_if_needed().await;
                let update_event = json!({
                    "id": Uuid::new_v4().to_string(),
                    "timestamp": end_time.clone(),
                    "type": "observation-update",
                    "body": {
                        "id": observation_id,
                        "traceId": trace_id,
                        "endTime": end_time
                    }
                });

                let mut state = layer.state.lock().await;
                state.batch.push(update_event);
            }
        });
    }

    fn enabled(&self, metadata: &Metadata<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) -> bool {
        // println!("metadata is {:?}", metadata);
        metadata.target().starts_with("goose::")
    }

    fn on_record(&self, span: &Id, values: &span::Record<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let span_id = span.into_u64();
        let mut visitor = JsonVisitor(serde_json::Map::new());
        values.record(&mut visitor);
        let metadata = visitor.0;

        if !metadata.is_empty() {
            let layer = self.clone();
            tokio::spawn(async move {
                if let Some((_, observation_id)) = layer.is_active_span(span_id).await {
                    let trace_id = layer.create_trace_if_needed().await;
                    let update_event = json!({
                        "id": Uuid::new_v4().to_string(),
                        "timestamp": Utc::now().to_rfc3339(),
                        "type": "observation-update",
                        "body": {
                            "id": observation_id,
                            "traceId": trace_id,
                            "metadata": metadata
                        }
                    });

                    let mut state = layer.state.lock().await;
                    state.batch.push(update_event);
                }
            });
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = JsonVisitor(serde_json::Map::new());
        event.record(&mut visitor);
        let metadata = visitor.0;
        
        // Get the event name from metadata
        // let event_name = event.metadata().name().to_string();
        let event_name = metadata.get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("unnamed event")
        .to_string();
        // println!("metadata is {:?}", visitor.0.get("message"));
    
        if let Some(span) = ctx.lookup_current() {
            let span_id = span.id().into_u64();
            let layer = self.clone();
            tokio::spawn(async move {
                if let Some((_, observation_id)) = layer.is_active_span(span_id).await {
                    let trace_id = layer.create_trace_if_needed().await;
                    let event_id = Uuid::new_v4().to_string();
                    let event_event = json!({
                        "id": event_id,
                        "timestamp": Utc::now().to_rfc3339(),
                        "type": "event-create",
                        "body": {
                            "id": event_id,
                            "traceId": trace_id,
                            "parentObservationId": observation_id,
                            "name": event_name,  // Add event name here
                            "metadata": metadata
                        }
                    });
    
                    let mut state = layer.state.lock().await;
                    state.batch.push(event_event);
                }
            });
        } else {
            let layer = self.clone();
            tokio::spawn(async move {
                let trace_id = layer.create_trace_if_needed().await;
                let event_id = Uuid::new_v4().to_string();
                let event_event = json!({
                    "id": event_id,
                    "timestamp": Utc::now().to_rfc3339(),
                    "type": "event-create",
                    "body": {
                        "id": event_id,
                        "traceId": trace_id,
                        "name": event_name,  // Add event name here
                        "metadata": metadata
                    }
                });
    
                let mut state = layer.state.lock().await;
                state.batch.push(event_event);
            });
        }
    }

    fn on_enter(&self, span: &Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let span_id = span.into_u64();
        let layer = self.clone();
        
        tokio::spawn(async move {
            if let Some((_, observation_id)) = layer.is_active_span(span_id).await {
                layer.set_current_observation_id(Some(observation_id)).await;
            }
        });
    }

    fn on_exit(&self, _span: &Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let layer = self.clone();
        
        // When exiting a span, we should try to get the parent scope
        if let Some(span) = ctx.lookup_current() {
            if let Some(parent_id) = span.parent().map(|s| s.id().into_u64()) {
                tokio::spawn(async move {
                    if let Some((_, parent_observation_id)) = layer.is_active_span(parent_id).await {
                        layer.set_current_observation_id(Some(parent_observation_id)).await;
                    } else {
                        layer.set_current_observation_id(None).await;
                    }
                });
            } else {
                tokio::spawn(async move {
                    layer.set_current_observation_id(None).await;
                });
            }
        } else {
            tokio::spawn(async move {
                layer.set_current_observation_id(None).await;
            });
        }
    }
}

pub fn create_langfuse_layer() -> Option<LangfuseLayer> {
    let public_key = env::var("LANGFUSE_PUBLIC_KEY")
        .or_else(|_| env::var("LANGFUSE_INIT_PROJECT_PUBLIC_KEY"))
        .unwrap_or_else(|_| "publickey-local".to_string());
    
    let secret_key = env::var("LANGFUSE_SECRET_KEY")
        .or_else(|_| env::var("LANGFUSE_INIT_PROJECT_SECRET_KEY"))
        .unwrap_or_else(|_| "secretkey-local".to_string());

    Some(LangfuseLayer::new(public_key, secret_key))
}