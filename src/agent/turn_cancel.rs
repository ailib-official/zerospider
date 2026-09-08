//! Shared turn-cancel contract for CLI and Web (VL-UX-CANCEL-002).
//! CLI 与 Web 共用的 turn 取消合同：完成 / 取消 / 失败，禁止分叉 persist。

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::loop_::is_tool_loop_cancelled;

/// User-facing stop copy for CLI stderr and WS `cancelled` frames.
pub const STOPPED_USER_MESSAGE: &str = "Stopped.";

/// Outcome of one agent turn after the cancellation token is considered.
#[derive(Debug)]
pub enum TurnFinish<T> {
    Completed(T),
    Cancelled,
    Failed(anyhow::Error),
}

impl<T> TurnFinish<T> {
    /// Persist a successful internodal/parlor assistant body only on Completed.
    /// Cancelled turns persist a separate `Stopped.` tombstone (VL-NA-043).
    #[must_use]
    pub fn should_persist_assistant(&self) -> bool {
        matches!(self, Self::Completed(_))
    }
}

/// Classify `run_tool_call_loop` / `Agent::turn` results (GOV-007 single loop).
#[must_use]
pub fn classify_turn_result<T>(result: Result<T, anyhow::Error>) -> TurnFinish<T> {
    match result {
        Ok(value) => TurnFinish::Completed(value),
        Err(err) if is_tool_loop_cancelled(&err) => TurnFinish::Cancelled,
        Err(err) => TurnFinish::Failed(err),
    }
}

/// WebSocket inbound during an in-flight turn: Stop, Close, or recv error.
#[must_use]
pub fn ws_inbound_cancels_turn(socket_closed: bool, cancel_frame: bool, recv_error: bool) -> bool {
    socket_closed || cancel_frame || recv_error
}

/// Double-Esc watcher + token for one CLI interactive turn.
pub struct CliTurnCancel {
    token: CancellationToken,
    watch: JoinHandle<()>,
}

impl CliTurnCancel {
    /// Start a turn-scoped token and the TTY Esc watcher (no-op on non-TTY).
    #[must_use]
    pub fn begin() -> Self {
        let token = CancellationToken::new();
        let watch = crate::agent::double_esc::spawn_double_esc_watcher(token.clone());
        Self { token, watch }
    }

    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Stop the watcher and classify the turn result (CLI and Web share classify).
    pub async fn conclude<T>(self, result: Result<T, anyhow::Error>) -> TurnFinish<T> {
        self.token.cancel();
        let _ = self.watch.await;
        classify_turn_result(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velaclaw_agent_runtime::ToolLoopCancelled;

    #[test]
    fn completed_turn_persists() {
        let finish = classify_turn_result(Ok("hello".to_string()));
        assert!(finish.should_persist_assistant());
    }

    #[test]
    fn cancelled_turn_does_not_persist() {
        let err = anyhow::Error::from(ToolLoopCancelled);
        let finish = classify_turn_result::<String>(Err(err));
        assert!(!finish.should_persist_assistant());
        assert!(matches!(finish, TurnFinish::Cancelled));
    }

    #[test]
    fn failed_turn_does_not_persist() {
        let finish = classify_turn_result::<String>(Err(anyhow::anyhow!("provider down")));
        assert!(!finish.should_persist_assistant());
        assert!(matches!(finish, TurnFinish::Failed(_)));
    }

    #[test]
    fn ws_close_and_cancel_frame_both_stop() {
        assert!(ws_inbound_cancels_turn(true, false, false));
        assert!(ws_inbound_cancels_turn(false, true, false));
        assert!(ws_inbound_cancels_turn(false, false, true));
        assert!(!ws_inbound_cancels_turn(false, false, false));
    }
}
