use super::*;
use crate::config::GatewayConfig;
use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::protocol::ids::{
    GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
};
use crate::protocol::ir::AiResponse;
use crate::provider::inbound::InboundResponse;
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::VendorScope;
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::VendorCtx;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

/// The hook runs only after StreamInput has buffered the entire HTTP chunk.
/// Block its marker line so the test can drop the downstream at that exact point.
struct CancellationBarrier {
    buffered: Mutex<Option<oneshot::Sender<()>>>,
    resume: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl Vendor for CancellationBarrier {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "cancellation-fixture",
        }
    }
    fn vendor_id(&self) -> &'static str {
        "cancellation-fixture"
    }
    fn supported_protocols(&self) -> &'static [ProtocolEndpoint] {
        &[GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA]
    }
    async fn build_request(
        &self,
        _: &mut AiRequest,
        _: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        unreachable!("response-only fixture")
    }
    async fn parse_response(
        &self,
        _: InboundResponse,
        _: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        unreachable!("response-only fixture")
    }
    fn map_error(&self, status: u16, _: Value) -> GatewayError {
        GatewayError::upstream_status("cancellation-fixture", status, None)
    }
    async fn on_stream_raw_chunk(
        &self,
        _: &VendorCtx<'_>,
        chunk: &mut String,
    ) -> anyhow::Result<()> {
        if chunk.contains(": cancellation-barrier") {
            self.buffered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            self.resume.lock().await.take().unwrap().await.unwrap();
        }
        Ok(())
    }
}

#[tokio::test]
async fn downstream_cancellation_does_not_flush_an_incomplete_utf8_tail() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stream", listener.local_addr().unwrap());
        let (next_tx, next_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            loop {
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buf[..count]);
                if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                    }).unwrap_or(0);
                    if request.len() >= end + 4 + length { break; }
                }
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            let first = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"first\"}]}}]}\n\n";
            socket.write_all(format!("{:x}\r\n", first.len()).as_bytes()).await.unwrap();
            socket.write_all(first).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            next_rx.await.unwrap();
            // A complete comment permits synchronization after the incomplete
            // UTF-8 tail has reached StreamInput, without yielding model deltas.
            let tail = b": cancellation-barrier\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"\xe4";
            let mut frame = format!("{:x}\r\n", tail.len()).into_bytes();
            frame.extend_from_slice(tail);
            frame.extend_from_slice(b"\r\n");
            socket.write_all(&frame).await.unwrap();
            std::future::pending::<()>().await;
        });
        let dir = tempfile::tempdir().unwrap();
        let (gw, mut logs) = crate::Gateway::new(GatewayConfig {
            data_dir: dir.path().to_path_buf(),
            config_poll_interval: Duration::ZERO,
            ..Default::default()
        }).await.unwrap();
        let provider: Provider = serde_json::from_value(json!({
            "id":"fixture","name":"fixture","vendor":"google","protocol":"google-gemini",
            "base_url":url,"api_key":"fixture","is_enabled":true,"created_at":"","updated_at":""
        })).unwrap();
        let ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
        let egress = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
        let ctx = RequestContext::new(ingress, Duration::from_secs(10));
        let performance = crate::performance::Attempt::new(gw.log_tx.clone(), ctx.cancellation.clone(), ctx.deadline.clone());
        performance.correlate(&ctx.request_id, 1);
        performance.select_final(1);
        let call_ctx = CallCtx {
            performance: Some(performance.clone()), gw: gw.clone(), provider: &provider,
            model_id:"fixture", model_name:"fixture", egress, ingress,
            ingress_str:"openai-compatible",egress_str:"google-gemini",request_model:"fixture",
            actual_model:"fixture",backend_model:"fixture",api_key_id:None,api_key_name:None,
            provider_key_name:None,route_decision:None,is_stream:true,enable_payload:Some(false),
            reasoning_effort:None,start:Instant::now(),req_ext:ctx.extensions.clone(),
        };
        let extras = RequestExtras { method:"POST".into(),path:"/stream".into(),headers:None,body:None };
        let dispatch_producer = performance.producer();
        // Mirror dispatcher's fallback row so cancellation can finalize without
        // the producer ever reaching its normal final log call.
        LogBuilder::from_ctx(&call_ctx).emit();
        let (buffered_tx, buffered_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let hook = StreamRawChunkHook {
            vendor: Arc::new(CancellationBarrier {buffered:Mutex::new(Some(buffered_tx)),resume:tokio::sync::Mutex::new(Some(resume_rx))}),
            provider:provider.clone(),protocol_id:egress,api_key:"fixture".into(),actual_model:"fixture".into(),credential:None,
        };
        let response = handle_stream(
            ProxyClient::new(reqwest::Client::builder().no_proxy().build().unwrap()).with_performance(performance.clone()),
            &url,ReqwestHeaderMap::new(),json!({"stream":true}),&call_ctx,&extras,false,
            ToolRoutePlan::default(),&ctx,&AiRequest::new("fixture",vec![]),Some(hook),
        ).await;
        let mut body = performance.wrap(response).into_body().into_data_stream();
        dispatch_producer.finish();
        drop(dispatch_producer);
        loop {
            let bytes = body.next().await.unwrap().unwrap();
            if std::str::from_utf8(&bytes).unwrap().contains("first") {break;}
        }
        next_tx.send(()).unwrap();
        buffered_rx.await.unwrap();
        drop(body);
        resume_tx.send(()).unwrap();
        let entry = logs.recv().await.unwrap();
        server.abort();
        assert_eq!(entry.diagnostic.attempt_outcome, "cancelled", "{:?}", entry.diagnostic);
        assert_ne!(entry.diagnostic.failure_kind.as_deref(), Some("conversion_parse_error"));
    }).await.expect("cancellation must terminate without waiting for upstream EOF");
}
