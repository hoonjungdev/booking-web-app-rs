//! 업체 호출 실패 분류 (docs/03-resilience.md §1)
//!
//! 이후의 판단(재시도할지, 결과 불명으로 볼지, 서킷에 실패로 셀지)은 모두 이 분류만 보고 한다.

use std::time::Duration;

use domain::RejectReason;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// 응답 시간 초과. 요청이 업체에 닿았는지 알 수 없다
    #[error("응답 시간 초과")]
    Timeout,
    /// 연결 실패. 요청이 업체에 닿지 않았다
    #[error("연결 실패: {detail}")]
    Connect { detail: String },
    /// 요청을 보내는 중이나 응답을 받는 중에 연결이 끊김. 업체가 처리했는지 알 수 없다
    #[error("전송 중 연결 오류: {detail}")]
    Transport { detail: String },
    /// 업체가 호출 한도 초과를 알림
    #[error("호출 한도 초과")]
    RateLimited { retry_after: Option<Duration> },
    /// 업체 서버 오류 (5xx)
    #[error("업체 서버 오류 (HTTP {status})")]
    Server { status: u16 },
    /// 업무적 거절 (슬롯 마감 등). 기술적으로는 정상 응답
    #[error("업체가 거절함: {}", reason.code())]
    Rejected { reason: RejectReason },
    /// 예상하지 못한 4xx. 대개 우리 쪽 버그
    #[error("예상하지 못한 응답 (HTTP {status}): {body}")]
    Client { status: u16, body: String },
    /// 응답 형식이 계약과 다름
    #[error("응답 형식 오류: {detail}")]
    Decode { detail: String },
}

impl ProviderError {
    /// 메트릭 · 호출 기록에 쓰는 분류 이름
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connect { .. } => "connect_error",
            Self::Transport { .. } => "transport_error",
            Self::RateLimited { .. } => "rate_limited",
            Self::Server { .. } => "server_error",
            Self::Rejected { .. } => "rejected",
            Self::Client { .. } => "client_error",
            Self::Decode { .. } => "decode_error",
        }
    }

    /// 업체가 요청을 처리하지 **않았다고** 확신할 수 있는가.
    ///
    /// `true`면 멱등키가 없는 업체에도 다시 보내도 중복 생성 위험이 없다.
    /// 타임아웃 · 5xx · 전송 오류 · 응답 해석 실패는 업체가 이미 처리했을 수 있으므로 `false`.
    pub fn definitely_not_processed(&self) -> bool {
        matches!(
            self,
            Self::Connect { .. }
                | Self::RateLimited { .. }
                | Self::Rejected { .. }
                | Self::Client { .. }
        )
    }
}
