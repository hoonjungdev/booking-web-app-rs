//! sqlx 기반 저장소 함수
//!
//! SQL은 `sqlx::query!` / `query_as!` 매크로로 쓴다. 매크로가 컴파일할 때 SQL을 DB에 보내 문법 · 컬럼 · 타입을 검사하므로,
//! 컬럼 이름 오타나 타입 불일치가 실행 전에 컴파일 에러로 드러난다 (≈ Dapper + 컴파일 시점 검사).
//! CI처럼 DB가 없는 곳에서는 미리 저장해 둔 검사 결과(`.sqlx/`)를 쓴다 (`SQLX_OFFLINE=true`).
//!
//! 쓰기 함수는 `&mut PgConnection`을 받는다. 트랜잭션(`Transaction`)도 연결로 바뀌어(`&mut *tx`) 들어갈 수 있어서,
//! 호출하는 쪽이 여러 함수를 한 트랜잭션으로 묶을 수 있다.

pub mod bookings;
pub mod clients;
mod error;
pub mod jobs;
pub mod providers;

use std::time::Duration;

use chrono::{DateTime, SubsecRound, Utc};

pub use error::DbError;
pub use sqlx::{PgPool, Postgres, Transaction};

/// 시각을 DB가 저장할 수 있는 정밀도(마이크로초)로 맞춘다. 그보다 작은 단위는 버린다.
///
/// PostgreSQL의 `timestamptz`는 마이크로초까지만 저장하는데, Linux에서 `Utc::now()`는 나노초까지 돌려준다.
/// 그대로 저장하면 "넣은 값"과 "다시 읽은 값"이 달라지므로, 저장할 시각은 경계(API 입구)에서 미리 맞춰 둔다.
/// (macOS는 시계가 마이크로초 단위라 이 차이가 드러나지 않아서, 로컬에서는 통과하고 CI(Linux)에서만 실패했었다)
pub fn normalize_timestamp(time: DateTime<Utc>) -> DateTime<Utc> {
    time.trunc_subsecs(6)
}

/// `migrations/` 폴더의 SQL을 컴파일할 때 실행 파일 안에 넣어 둔다. 서버 시작 시 `MIGRATOR.run(&pool)`로 적용한다.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// DB 연결 풀을 만든다 (≈ ADO.NET 연결 풀링, 여기서는 명시적인 객체)
pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, DbError> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await?;
    Ok(pool)
}
