//! GET `/ws` — WebSocket streaming chat via agent loop (VL-UI-002).
//! GET `/ws` — 经 agent 循环的 WebSocket 流式对话（VL-UI-002）。

use super::local_control::auth::{check_pairing_auth, unauthorized_response};
use super::local_control::runner::{
    chunk_text_for_stream, persist_assistant_message, persist_user_message, run_agent_chat,
    user_facing_turn_error,
};
use super::local_control::types::{ChatApiRequest, WsClientMessage, WsDagNode, WsServerMessage};
use super::AppState;
use crate::agent::turn_cancel::{classify_turn_result, ws_inbound_cancels_turn, TurnFinish};
use crate::agent::turn_progress::TurnProgress;
use crate::approval::HumanInputKind;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const WS_CHUNK_SIZE: usize = 48;

type WsSink = futures_util::stream::SplitSink<WebSocket, Message>;

#[derive(Debug, Deserialize, Default)]
pub struct WsQuery {
    #[serde(default)]
    pub token: Option<String>,
}

fn human_input_kind_label(kind: HumanInputKind) -> &'static str {
    match kind {
        HumanInputKind::Choice => "choice",
        HumanInputKind::Text => "text",
        HumanInputKind::Secret => "secret",
        HumanInputKind::Handoff => "handoff",
    }
}

fn progress_frame(progress: TurnProgress) -> WsServerMessage {
    match progress {
        TurnProgress::Status { phase, detail } => WsServerMessage::Status {
            phase,
            detail: Some(detail),
        },
        TurnProgress::Step {
            kind,
            tool,
            ok,
            summary,
            expand,
        } => WsServerMessage::Step {
            kind,
            tool,
            ok,
            summary,
            expand,
        },
        TurnProgress::Dag {
            dag_id,
            fallback,
            outline,
            nodes,
        } => WsServerMessage::Dag {
            dag_id,
            fallback,
            outline,
            nodes: nodes
                .into_iter()
                .map(|n| WsDagNode {
                    id: n.id,
                    label: n.label,
                    task_type: n.task_type,
                    caps: n.caps,
                    contact: n.contact,
                    status: n.status,
                })
                .collect(),
        },
        TurnProgress::Note { text } => WsServerMessage::Delta { content: text },
    }
}

/// GET /ws — upgrade to WebSocket for streaming chat.
pub async fn handle_ws_chat(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    if check_pairing_auth(&state.pairing, &headers, query.token.as_deref()).is_err() {
        return unauthorized_response().into_response();
    }

    ws.on_upgrade(move |socket| handle_ws_socket(socket, state))
}

async fn handle_ws_socket(socket: WebSocket, state: AppState) {
    let (sink, mut stream) = socket.split();
    let sink = Arc::new(Mutex::new(sink));

    let mut title_sub = state.session_title_hub.subscribe();
    let title_sink = sink.clone();
    let title_forwarder = tokio::spawn(async move {
        loop {
            match title_sub.recv().await {
                Ok(ev) => {
                    let frame = WsServerMessage::SessionTitle {
                        session_id: ev.session_id,
                        title: ev.title,
                    };
                    if send_frame(title_sink.clone(), &frame).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "session title subscriber lagged; continuing");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    loop {
        let msg = match stream.next().await {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                tracing::warn!("WebSocket receive error: {e}");
                break;
            }
        };

        let client: WsClientMessage = match serde_json::from_str(&msg) {
            Ok(v) => v,
            Err(e) => {
                let frame = WsServerMessage::Error {
                    message: format!("Invalid JSON: {e}"),
                };
                if send_frame(sink.clone(), &frame).await.is_err() {
                    break;
                }
                continue;
            }
        };

        if client.msg_type == "listen" {
            // Park until the client closes; title_forwarder keeps pushing.
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            break;
        }

        if client.msg_type == "cancel" {
            continue;
        }

        if client.msg_type != "chat" {
            let frame = WsServerMessage::Error {
                message: format!("Unsupported message type: {}", client.msg_type),
            };
            if send_frame(sink.clone(), &frame).await.is_err() {
                break;
            }
            continue;
        }

        if client.messages.is_empty() {
            let frame = WsServerMessage::Error {
                message: "messages must not be empty".into(),
            };
            if send_frame(sink.clone(), &frame).await.is_err() {
                break;
            }
            continue;
        }

        let req = ChatApiRequest {
            messages: client.messages,
            session_id: client.session_id,
            model_id: client.model_id,
            temperature: client.temperature,
            max_tokens: None,
            host_phase: client.host_phase,
        };

        let config = state.config.lock().clone();
        let hub = state.approval_hub.clone();
        let human_hub = state.human_input_hub.clone();
        let mut approval_sub = hub.subscribe();
        let mut human_sub = human_hub.subscribe();
        let sock_fwd = sink.clone();
        let sock_hitl = sink.clone();
        let forwarder = tokio::spawn(async move {
            loop {
                match approval_sub.recv().await {
                    Ok(ev) => {
                        let frame = WsServerMessage::ApprovalRequired {
                            id: ev.id,
                            tool_name: ev.tool_name,
                            arguments_summary: ev.arguments_summary,
                            elevation: ev.elevation,
                        };
                        if send_frame(sock_fwd.clone(), &frame).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "approval hub subscriber lagged; continuing");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let hitl_forwarder = tokio::spawn(async move {
            loop {
                match human_sub.recv().await {
                    Ok(ev) => {
                        let frame = WsServerMessage::InputRequired {
                            id: ev.id,
                            kind: human_input_kind_label(ev.kind).to_string(),
                            prompt: ev.prompt,
                            options: ev.options,
                            risk_note: ev.risk_note,
                        };
                        if send_frame(sock_hitl.clone(), &frame).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            skipped = n,
                            "human input hub subscriber lagged; continuing"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let cancel = CancellationToken::new();
        if let Err(e) = persist_user_message(
            &config,
            req.session_id.as_deref(),
            &req,
            Some(state.session_title_hub.clone()),
        )
        .await
        {
            tracing::warn!("session persist user failed: {e:#}");
        }
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(128);
        let mut chat_fut = Box::pin(run_agent_chat(
            &config,
            &req,
            Some(&hub),
            Some(&human_hub),
            Some(cancel.clone()),
            Some(progress_tx),
        ));

        let mut streamed_operator = String::new();
        let chat_result = loop {
            tokio::select! {
                result = &mut chat_fut => {
                    break result;
                }
                progress = progress_rx.recv() => {
                    if let Some(progress) = progress {
                        if let crate::agent::turn_progress::TurnProgress::Note { text } = &progress {
                            streamed_operator.push_str(text);
                        }
                        let frame = progress_frame(progress);
                        if send_frame(sink.clone(), &frame).await.is_err() {
                            cancel.cancel();
                        }
                    }
                }
                incoming = stream.next() => {
                    match incoming {
                        None | Some(Ok(Message::Close(_))) => {
                            cancel.cancel();
                        }
                        Some(Ok(Message::Text(text))) => {
                            let cancel_frame = serde_json::from_str::<WsClientMessage>(&text)
                                .is_ok_and(|frame| frame.msg_type == "cancel");
                            if ws_inbound_cancels_turn(false, cancel_frame, false) {
                                cancel.cancel();
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!("WebSocket receive error during turn: {e}");
                            cancel.cancel();
                        }
                        Some(Ok(_)) => {}
                    }
                }
            }
        };

        forwarder.abort();
        hitl_forwarder.abort();

        match classify_turn_result(chat_result) {
            TurnFinish::Completed(resp) => {
                if let Err(e) = persist_assistant_message(
                    &config,
                    req.session_id.as_deref(),
                    &req,
                    &resp.content,
                )
                .await
                {
                    tracing::warn!("session persist failed: {e:#}");
                }
                for chunk in chunk_text_for_stream(
                    crate::agent::bounded_dag_delivery::remaining_operator_delta(
                        &streamed_operator,
                        &resp.content,
                    ),
                    WS_CHUNK_SIZE,
                ) {
                    let delta = WsServerMessage::Delta { content: chunk };
                    if send_frame(sink.clone(), &delta).await.is_err() {
                        return;
                    }
                }
                let done = WsServerMessage::Done {
                    usage: resp.usage,
                    cost: resp.cost,
                    selected_model: resp.selected_model,
                    model_selection_reason: resp.model_selection_reason,
                };
                if send_frame(sink.clone(), &done).await.is_err() {
                    return;
                }
            }
            TurnFinish::Cancelled => {
                if let Err(e) = persist_assistant_message(
                    &config,
                    req.session_id.as_deref(),
                    &req,
                    crate::agent::turn_cancel::STOPPED_USER_MESSAGE,
                )
                .await
                {
                    tracing::warn!("session persist cancelled tombstone failed: {e:#}");
                }
                let frame = WsServerMessage::Cancelled {
                    message: Some(crate::agent::turn_cancel::STOPPED_USER_MESSAGE.into()),
                };
                if send_frame(sink.clone(), &frame).await.is_err() {
                    break;
                }
            }
            TurnFinish::Failed(e) => {
                tracing::warn!(error = %format!("{e:#}"), "websocket chat turn failed");
                let frame = WsServerMessage::Error {
                    message: user_facing_turn_error(&e, req.model_id.as_deref()),
                };
                if send_frame(sink.clone(), &frame).await.is_err() {
                    break;
                }
            }
        }
    }
    title_forwarder.abort();
}

async fn send_frame(sink: Arc<Mutex<WsSink>>, frame: &WsServerMessage) -> Result<(), ()> {
    let text = serde_json::to_string(frame).map_err(|_| ())?;
    let mut guard = sink.lock().await;
    guard.send(Message::Text(text.into())).await.map_err(|_| ())
}
