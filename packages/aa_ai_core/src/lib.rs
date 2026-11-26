//! `aa_ai_core` — 与会话无关的核心抽象：agent loop、取消信号、工具接口。
//!
//! - [`tool_loop`]：对模型上下文跑工具循环，只输出事件，不认识会话/持久化；
//! - [`tool`]：工具定义/执行接口与注册表（数据半在 `aa_language_model::request::tools`）；
//! - [`CancelToken`] / [`CancelReason`]：协作式取消信号。
//!
//! 协作式取消：agent loop 的停止信号。
//!
//! 取消是 **loop 的契约**，不是单个工具的能力：循环持有 future、决定还发不发
//! 下一轮请求；工具只能通过 [`ToolEvents::cancelled`](crate::tool::ToolEvents::cancelled)
//! 观察信号并自行退出（Rust 不能抢占正在执行的 future）。
//!
//! 设计取舍见 [`docs/decisions/0018-agent-loop-cancellation.md`](../../../docs/decisions/0018-agent-loop-cancellation.md)：
//!
//! - **同步读 + 异步等**：工具侧是同步接口，用 `AtomicU8` 直接读；
//!   loop 侧用 [`CancelToken::cancelled`] 等通知，取消时立即 drop 流。
//! - **首个原因胜出**：用户取消后，超时/预算再触发不会覆盖原因。
//! - **不做自下而上**：工具不能把整个 run 取消；它只写自己的结果。

pub mod tool;
pub mod tool_loop;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio::sync::Notify;

/// 取消原因。分类对齐 dsh 的 `AgentCancelCause`，按我们的场景裁剪。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// 用户主动停止（Ctrl-C / UI 停止按钮）。
    User,
    /// 协作式超时。硬超时由上层 `tokio::time::timeout` 直接 drop future。
    Timeout,
    /// 上层 step / 父 agent 取消。
    Upstream,
    /// 轮数或预算用尽（`ToolLoop::max_turns`）。
    Budget,
}

impl CancelReason {
    fn code(self) -> u8 {
        match self {
            Self::User => 1,
            Self::Timeout => 2,
            Self::Upstream => 3,
            Self::Budget => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::User),
            2 => Some(Self::Timeout),
            3 => Some(Self::Upstream),
            4 => Some(Self::Budget),
            _ => None,
        }
    }
}

impl fmt::Display for CancelReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::User => "user",
            Self::Timeout => "timeout",
            Self::Upstream => "upstream",
            Self::Budget => "budget",
        })
    }
}

#[derive(Debug, Default)]
struct State {
    /// 0 = 未取消；其余见 [`CancelReason::code`]。
    reason: AtomicU8,
    notify: Notify,
}

/// 可克隆的取消令牌：所有克隆共享同一个原因。
///
/// 默认是「未取消」。同一个 token 可以被 loop、工具和上层 watcher 同时持有。
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    state: Arc<State>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// 置位取消。第一个原因胜出；重复调用是幂等的。
    pub fn cancel(&self, reason: CancelReason) {
        if self
            .state
            .reason
            .compare_exchange(0, reason.code(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // `notify_waiters` 不保存许可，但 `cancelled()` 在等待前会先读原子，
            // 所以「先 cancel 后等待」和「先等待后 cancel」都不会漏。
            self.state.notify.notify_waiters();
        }
    }

    /// 同步检查（工具侧用）。热路径只有一次原子读。
    pub fn is_cancelled(&self) -> bool {
        self.state.reason.load(Ordering::Acquire) != 0
    }

    /// 取消原因；未取消时为 `None`。
    pub fn reason(&self) -> Option<CancelReason> {
        CancelReason::from_code(self.state.reason.load(Ordering::Acquire))
    }

    /// 未取消时 `Ok(())`，否则返回原因。
    pub fn check(&self) -> Result<(), CancelReason> {
        match self.reason() {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// 等取消发生，返回原因。取消后立即返回；可以放进 `tokio::select!`。
    ///
    /// 先 `enable()` 再读原子，避免「检查通过 → cancel 置位 → 注册等待」的
    /// 丢唤醒窗口。
    pub async fn cancelled(&self) -> CancelReason {
        let notified = self.state.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(reason) = self.reason() {
            return reason;
        }
        notified.await;
        self.reason().expect("notified only fires after cancel()")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_is_not_cancelled() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        assert_eq!(token.reason(), None);
        assert_eq!(token.check(), Ok(()));
    }

    #[tokio::test]
    async fn first_reason_wins() {
        let token = CancelToken::new();
        token.cancel(CancelReason::User);
        token.cancel(CancelReason::Timeout);
        assert!(token.is_cancelled());
        assert_eq!(token.reason(), Some(CancelReason::User));
        assert_eq!(token.check(), Err(CancelReason::User));
    }

    #[tokio::test]
    async fn clones_share_the_signal() {
        let token = CancelToken::new();
        let clone = token.clone();
        token.cancel(CancelReason::Upstream);
        assert!(clone.is_cancelled());
        assert_eq!(clone.reason(), Some(CancelReason::Upstream));
    }

    #[tokio::test]
    async fn waiting_after_cancel_returns_immediately() {
        let token = CancelToken::new();
        token.cancel(CancelReason::Budget);
        assert_eq!(token.cancelled().await, CancelReason::Budget);
    }

    #[tokio::test]
    async fn waiting_is_woken_by_cancel() {
        let token = CancelToken::new();
        let waiter = token.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        // 让等待者先注册。
        tokio::task::yield_now().await;
        token.cancel(CancelReason::Timeout);
        assert_eq!(handle.await.unwrap(), CancelReason::Timeout);
    }
}

pub use tool_loop::{ToolLoop, ToolLoopEvent, ToolLoopStream};
