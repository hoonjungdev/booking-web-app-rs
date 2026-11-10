//! API 키 인증
//!
//! 클라이언트는 `X-Api-Key` 헤더에 발급받은 키를 넣어 보낸다.
//! DB에는 키 원문이 아니라 SHA-256 해시만 저장하고, 요청이 오면 헤더 값을 해시해서 찾는다.

use std::fmt::Write;

use axum::{extract::FromRequestParts, http::request::Parts};
use domain::ClientId;
use rand::RngExt;
use sha2::{Digest, Sha256};

use crate::api::{ApiError, ApiState};

pub const API_KEY_HEADER: &str = "x-api-key";
const API_KEY_PREFIX: &str = "bk_";

/// 새 API 키: `bk_` + 무작위 32바이트(256비트)를 16진수로 적은 것
pub fn generate_api_key() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    format!("{API_KEY_PREFIX}{}", hex(&bytes))
}

/// API 키의 SHA-256 해시 (16진수 문자열). DB에는 이 값만 저장한다.
///
/// 비밀번호는 사람이 정해 짧고 추측하기 쉬워서 느린 해시(argon2 등)가 필요하지만,
/// API 키는 256비트 무작위 값이라 빠른 해시로도 역산 · 추측이 불가능하다. → docs/decisions.md D-14
pub fn hash_api_key(key: &str) -> String {
    hex(&Sha256::digest(key.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            // String에 쓰는 write!는 실패하지 않는다
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// 인증된 클라이언트.
///
/// 핸들러 매개변수에 이 타입을 넣으면, axum이 핸들러를 부르기 전에 아래 `from_request_parts`로 인증한다.
/// 인증에 실패하면 핸들러는 실행되지 않고 401이 나간다. (≈ ASP.NET Core의 인증 미들웨어 + `[Authorize]`)
#[derive(Debug, Clone)]
pub struct AuthenticatedClient {
    pub id: ClientId,
    pub name: String,
}

impl FromRequestParts<ApiState> for AuthenticatedClient {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get(API_KEY_HEADER)
            .and_then(|value| value.to_str().ok())
            .ok_or(ApiError::Unauthorized)?;
        let client = db::clients::find_by_key_hash(&state.pool, &hash_api_key(key))
            .await?
            .ok_or(ApiError::Unauthorized)?;
        Ok(Self {
            id: client.id,
            name: client.name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 발급한_키는_매번_다르고_형식이_같다() {
        let a = generate_api_key();
        let b = generate_api_key();
        assert_ne!(a, b);
        assert!(a.starts_with("bk_"));
        assert_eq!(a.len(), 3 + 64);
    }

    #[test]
    fn 해시는_같은_키에_항상_같은_값이다() {
        let key = "bk_test";
        assert_eq!(hash_api_key(key), hash_api_key(key));
        assert_ne!(hash_api_key(key), hash_api_key("bk_other"));
        assert_eq!(hash_api_key(key).len(), 64);
        assert!(!hash_api_key(key).contains(key));
    }
}
