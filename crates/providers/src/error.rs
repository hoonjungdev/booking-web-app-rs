//! 업체 호출 실패 분류 (docs/03-resilience.md §1)
//!
//! 이후의 판단(재시도할지, 결과 불명으로 볼지, 서킷에 실패로 셀지)은 모두 이 분류만 보고 한다.
//!
//! **업체가 보낸 글자는 오류 메시지(`Display`)에 넣지 않는다.** 오류 메시지는 로그 · 추적 · 예약의 실패 사유로
//! 퍼져 나가는데, 업체가 오류 본문에 요청 값(고객 이름 · 전화번호)을 되돌려 줄 수 있기 때문이다.
//! 원인을 찾는 데 필요한 그 내용은 [`ProviderError::detail`]로 꺼내 DB의 호출 기록에만 남긴다.

use std::time::{Duration, Instant};

use domain::RejectReason;

/// `Instant`는 단조 시계(monotonic clock)의 한 시점이다 (≈ C#의 `Stopwatch.GetTimestamp()` 값).
/// 벽시계(`DateTime<Utc>`)와 달리 시스템 시각이 바뀌어도 뒤로 가지 않아서, "몇 초 뒤"를 계산할 때 쓴다.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// 응답 시간 초과. 요청이 업체에 닿았는지 알 수 없다
    #[error("응답 시간 초과")]
    Timeout,
    /// 연결 실패. 요청이 업체에 닿지 않았다
    // HTTP 클라이언트의 오류 문구(`detail`)에는 업체 주소 같은 내부 정보가 들어 있다. 메시지는 로그 · 예약의 실패 사유로
    // 퍼져 나가 클라이언트에게까지 보일 수 있으므로 문구는 넣지 않고, `detail()`로 호출 기록에만 남긴다.
    #[error("연결 실패")]
    Connect { detail: String },
    /// 요청을 보내는 중이나 응답을 받는 중에 연결이 끊김. 업체가 처리했는지 알 수 없다
    #[error("전송 중 연결 오류")]
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
    /// 예상하지 못한 4xx. 대개 우리 쪽 버그. `body`는 업체가 보낸 본문이라 메시지에 넣지 않는다
    #[error("예상하지 못한 응답 (HTTP {status})")]
    Client { status: u16, body: String },
    /// 응답 형식이 계약과 다름. `detail`에는 응답 내용 일부가 들어갈 수 있어 메시지에 넣지 않는다
    #[error("응답 형식 오류")]
    Decode { detail: String },
    /// 서킷이 열려 있어서 호출하지 않음. `retry_at`이 지나면 다시 시도해 볼 수 있다
    #[error("서킷이 열려 있어 호출하지 않음")]
    CircuitOpen { retry_at: Instant },
    /// 우리 쪽 rate limiter · bulkhead에서 대기 한도를 넘었거나, 업체가 쉬라고 해서(pause) 호출하지 않음
    #[error("호출 대기 한도 초과 또는 업체 pause 중이라 호출하지 않음")]
    Throttled { retry_at: Instant },
}

/// 서킷 브레이커가 이 결과를 어떻게 셀지 (docs/03-resilience.md §1 표의 "서킷 브레이커 집계" 열)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitVerdict {
    /// 업체가 정상이라는 신호
    Success,
    /// 업체가 아프다는 신호
    Failure,
    /// 업체 상태와 상관없는 결과라 세지 않는다
    Ignored,
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
            Self::CircuitOpen { .. } => "circuit_open",
            Self::Throttled { .. } => "throttled",
        }
    }

    /// 호출 기록(`provider_calls.error_detail`)에 남길 자세한 내용: 메시지 + 업체가 보낸 글자.
    ///
    /// 여기서 나온 값은 로그나 span에 넣지 않는다 (모듈 설명 참고). DB의 호출 기록에만 쓴다.
    /// 그곳은 고객 정보가 원래 저장되는 곳이고, 30일 뒤에 지운다.
    pub fn detail(&self) -> String {
        match self {
            Self::Client { body, .. } => format!("{self}: {body}"),
            Self::Decode { detail } | Self::Connect { detail } | Self::Transport { detail } => {
                format!("{self}: {detail}")
            }
            other => other.to_string(),
        }
    }

    /// 업체가 요청을 처리하지 **않았다고** 확신할 수 있는가.
    ///
    /// `true`면 멱등키가 없는 업체에도 다시 보내도 중복 생성 위험이 없다.
    /// 타임아웃 · 5xx · 전송 오류 · 응답 해석 실패는 업체가 이미 처리했을 수 있으므로 `false`.
    pub fn definitely_not_processed(&self) -> bool {
        match self {
            Self::Connect { .. }
            | Self::RateLimited { .. }
            | Self::Rejected { .. }
            | Self::Client { .. }
            | Self::CircuitOpen { .. }
            | Self::Throttled { .. } => true,
            Self::Timeout | Self::Transport { .. } | Self::Server { .. } | Self::Decode { .. } => {
                false
            }
        }
    }

    /// 업체를 부르지 않고 우리 쪽에서 막았다면 다시 시도해 볼 시각.
    ///
    /// 이런 실패는 작업 시도 횟수로 세지 않는다 (docs/03-resilience.md §4).
    pub fn blocked_until(&self) -> Option<Instant> {
        match self {
            Self::CircuitOpen { retry_at } | Self::Throttled { retry_at } => Some(*retry_at),
            _ => None,
        }
    }

    /// 서킷 브레이커 집계 방법
    pub fn circuit_verdict(&self) -> CircuitVerdict {
        match self {
            // 업무적 거절은 업체가 정상적으로 일했다는 뜻이다
            Self::Rejected { .. } => CircuitVerdict::Success,
            Self::Timeout
            | Self::Connect { .. }
            | Self::Transport { .. }
            | Self::Server { .. }
            | Self::Decode { .. } => CircuitVerdict::Failure,
            // 호출 제한과 우리 버그(4xx)는 업체 건강 상태와 상관없다. 우리 쪽에서 막은 것도 마찬가지
            Self::RateLimited { .. }
            | Self::Client { .. }
            | Self::CircuitOpen { .. }
            | Self::Throttled { .. } => CircuitVerdict::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 모든 분류를 하나씩 (새 분류를 추가하면 아래 match가 컴파일되지 않아 여기도 고치게 된다)
    fn all_errors() -> Vec<ProviderError> {
        let now = Instant::now();
        let errors = vec![
            ProviderError::Timeout,
            ProviderError::Connect { detail: "x".into() },
            ProviderError::Transport { detail: "x".into() },
            ProviderError::RateLimited { retry_after: None },
            ProviderError::Server { status: 503 },
            ProviderError::Rejected {
                reason: RejectReason::SlotUnavailable,
            },
            ProviderError::Client {
                status: 422,
                body: String::new(),
            },
            ProviderError::Decode { detail: "x".into() },
            ProviderError::CircuitOpen { retry_at: now },
            ProviderError::Throttled { retry_at: now },
        ];
        for error in &errors {
            match error {
                ProviderError::Timeout
                | ProviderError::Connect { .. }
                | ProviderError::Transport { .. }
                | ProviderError::RateLimited { .. }
                | ProviderError::Server { .. }
                | ProviderError::Rejected { .. }
                | ProviderError::Client { .. }
                | ProviderError::Decode { .. }
                | ProviderError::CircuitOpen { .. }
                | ProviderError::Throttled { .. } => {}
            }
        }
        errors
    }

    #[test]
    fn 분류_표와_일치한다() {
        // (outcome, 처리 안 됐다고 확신, 서킷 집계) — docs/03-resilience.md §1
        let expected = [
            ("timeout", false, CircuitVerdict::Failure),
            ("connect_error", true, CircuitVerdict::Failure),
            ("transport_error", false, CircuitVerdict::Failure),
            ("rate_limited", true, CircuitVerdict::Ignored),
            ("server_error", false, CircuitVerdict::Failure),
            ("rejected", true, CircuitVerdict::Success),
            ("client_error", true, CircuitVerdict::Ignored),
            ("decode_error", false, CircuitVerdict::Failure),
            ("circuit_open", true, CircuitVerdict::Ignored),
            ("throttled", true, CircuitVerdict::Ignored),
        ];
        let errors = all_errors();
        assert_eq!(errors.len(), expected.len());
        for (error, (outcome, not_processed, verdict)) in errors.iter().zip(expected) {
            assert_eq!(error.outcome(), outcome);
            assert_eq!(error.definitely_not_processed(), not_processed, "{outcome}");
            assert_eq!(error.circuit_verdict(), verdict, "{outcome}");
        }
    }

    #[test]
    fn 업체가_보낸_글자는_메시지에_넣지_않고_호출_기록용_내용에만_넣는다() {
        // 업체가 검증 오류 본문에 요청 값을 되돌려 준 경우
        let echoed = r#"{"error":{"code":"invalid","message":"홍길동 010-1234-5678"}}"#;
        let client = ProviderError::Client {
            status: 422,
            body: echoed.into(),
        };
        assert_eq!(client.to_string(), "예상하지 못한 응답 (HTTP 422)");
        assert!(client.detail().contains(echoed));

        let decode = ProviderError::Decode {
            detail: r#"invalid type: string "홍길동", expected u32"#.into(),
        };
        assert_eq!(decode.to_string(), "응답 형식 오류");
        assert!(decode.detail().contains("홍길동"));

        // HTTP 클라이언트의 오류 문구에는 업체 주소가 들어 있다. 메시지(예약의 실패 사유로 클라이언트에게도 보임)에는 넣지 않는다.
        let connect = ProviderError::Connect {
            detail: "error sending request for url (http://10.1.2.3/alpha/v1/reservations)".into(),
        };
        assert_eq!(connect.to_string(), "연결 실패");
        assert!(connect.detail().contains("10.1.2.3"));
        let transport = ProviderError::Transport {
            detail: "connection closed before message completed".into(),
        };
        assert_eq!(transport.to_string(), "전송 중 연결 오류");
        assert!(transport.detail().contains("connection closed"));

        // 따로 남길 내용이 없는 분류는 메시지와 같다
        for error in all_errors() {
            if !matches!(
                error,
                ProviderError::Client { .. }
                    | ProviderError::Decode { .. }
                    | ProviderError::Connect { .. }
                    | ProviderError::Transport { .. }
            ) {
                assert_eq!(error.detail(), error.to_string());
            }
        }
    }

    #[test]
    fn 우리_쪽에서_막은_실패만_재시도_시각을_알려_준다() {
        for error in all_errors() {
            let blocked = matches!(
                error,
                ProviderError::CircuitOpen { .. } | ProviderError::Throttled { .. }
            );
            assert_eq!(error.blocked_until().is_some(), blocked, "{error:?}");
        }
    }
}
