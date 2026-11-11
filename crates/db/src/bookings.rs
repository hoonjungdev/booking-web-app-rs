//! 예약 (`bookings`, `booking_events`)

use chrono::{DateTime, Utc};
use domain::{
    Booking, BookingEvent, BookingId, BookingRequest, BookingStatus, ClientId, Customer, ProviderId,
};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    DbError, PgPool, TraceRef,
    jobs::{self, JobKind},
};

/// 새로 접수할 예약
#[derive(Debug, Clone)]
pub struct NewBooking {
    pub id: BookingId,
    pub client_id: ClientId,
    pub idempotency_key: String,
    /// 요청 본문의 해시. 같은 멱등키로 다른 요청이 왔는지 가려낸다.
    pub request_hash: Vec<u8>,
    pub request: BookingRequest,
    pub deadline_at: DateTime<Utc>,
    /// 접수 요청의 trace. 상태 이력과 `submit_booking` 작업에 남는다.
    pub trace: TraceRef,
}

/// 접수 결과
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// 새로 저장함 (예약 + 상태 이력 + 작업)
    Created(Booking),
    /// 같은 클라이언트 · 같은 멱등키의 예약이 이미 있음. `same_request`는 요청 해시가 같은지
    Existing {
        booking: Booking,
        same_request: bool,
    },
}

/// 상태 전이 결과
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transitioned {
    /// 바뀜
    Applied(BookingStatus),
    /// 그사이 다른 곳에서 상태를 먼저 바꿔서 아무것도 하지 않음 (낙관적 동시성 충돌)
    Conflict,
}

/// 상태 이력 한 줄
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    pub from_status: Option<BookingStatus>,
    pub to_status: BookingStatus,
    pub reason: Option<String>,
    pub actor: String,
    /// 이 변경이 일어난 trace (추적 없이 바꿨으면 `None`)
    pub trace_id: Option<String>,
    pub at: DateTime<Utc>,
}

/// DB 행을 그대로 담는 구조체. sqlx 매크로가 컬럼 타입에 맞춰 채운다.
/// 도메인 타입([`Booking`])과 분리해 두고 `TryFrom`으로 바꾼다 (≈ EF 엔티티와 도메인 모델 사이의 매핑).
struct BookingRow {
    id: Uuid,
    client_id: Uuid,
    provider_id: String,
    resource_id: String,
    slot_start: DateTime<Utc>,
    party_size: i32,
    customer: serde_json::Value,
    status: String,
    status_reason: Option<String>,
    provider_booking_id: Option<String>,
    deadline_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    request_hash: Vec<u8>,
}

impl TryFrom<BookingRow> for Booking {
    type Error = DbError;

    fn try_from(row: BookingRow) -> Result<Self, Self::Error> {
        let customer: Customer = serde_json::from_value(row.customer).map_err(DbError::corrupt)?;
        Ok(Booking {
            id: BookingId::from_uuid(row.id),
            client_id: ClientId::from_uuid(row.client_id),
            request: BookingRequest {
                provider: ProviderId::parse(&row.provider_id).map_err(DbError::corrupt)?,
                resource_id: row.resource_id,
                slot_start: row.slot_start,
                party_size: u32::try_from(row.party_size).map_err(DbError::corrupt)?,
                customer,
            },
            status: row.status.parse().map_err(DbError::corrupt)?,
            status_reason: row.status_reason,
            provider_booking_id: row.provider_booking_id,
            deadline_at: row.deadline_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

/// 예약을 접수한다: 예약 · 상태 이력 · `submit_booking` 작업을 **한 트랜잭션**으로 저장한다.
///
/// 같은 클라이언트가 같은 멱등키로 이미 접수했으면 새로 만들지 않고 기존 예약을 돌려준다.
/// 두 요청이 동시에 와도 `UNIQUE (client_id, idempotency_key)` 제약 때문에 한쪽만 저장된다.
///
/// `#[tracing::instrument]`가 이 함수 실행 동안 `db.bookings.accept` span을 연다. trace에서 DB에 쓴 시간이 따로 보인다.
/// `skip_all`: 인자(고객 정보가 들어 있다)를 span에 기록하지 않는다.
#[tracing::instrument(name = "db.bookings.accept", skip_all)]
pub async fn accept(pool: &PgPool, new: &NewBooking) -> Result<AcceptOutcome, DbError> {
    let customer = serde_json::to_value(&new.request.customer).map_err(DbError::corrupt)?;
    let party_size = i32::try_from(new.request.party_size).map_err(DbError::corrupt)?;

    // `begin`으로 트랜잭션을 시작한다. `commit` 전에 함수가 끝나면(에러로 `?` 반환 등) 자동으로 롤백된다.
    // (≈ C#의 `using var tx = conn.BeginTransaction();` 후 Commit을 안 부르면 롤백되는 것)
    let mut tx = pool.begin().await?;

    let inserted = sqlx::query_as!(
        BookingRow,
        r#"
        INSERT INTO bookings (id, client_id, idempotency_key, request_hash, provider_id, resource_id,
                              slot_start, party_size, customer, status, deadline_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending', $10)
        ON CONFLICT ON CONSTRAINT bookings_idempotency_key DO NOTHING
        RETURNING id, client_id, provider_id, resource_id, slot_start, party_size, customer, status,
                  status_reason, provider_booking_id, deadline_at, created_at, updated_at, request_hash
        "#,
        new.id.as_uuid(),
        new.client_id.as_uuid(),
        new.idempotency_key,
        new.request_hash,
        new.request.provider.as_str(),
        new.request.resource_id,
        new.request.slot_start,
        party_size,
        customer,
        new.deadline_at,
    )
    .fetch_optional(&mut *tx)
    .await?;

    let outcome = match inserted {
        Some(row) => {
            insert_event(
                &mut tx,
                new.id,
                None,
                BookingStatus::Pending,
                None,
                "api",
                new.trace.trace_id.as_deref(),
            )
            .await?;
            jobs::enqueue(
                &mut tx,
                JobKind::SubmitBooking,
                Some(new.id),
                jobs::DEFAULT_MAX_ATTEMPTS,
                new.trace.context.as_ref(),
            )
            .await?;
            AcceptOutcome::Created(row.try_into()?)
        }
        None => {
            let row = sqlx::query_as!(
                BookingRow,
                r#"
                SELECT id, client_id, provider_id, resource_id, slot_start, party_size, customer, status,
                       status_reason, provider_booking_id, deadline_at, created_at, updated_at, request_hash
                FROM bookings WHERE client_id = $1 AND idempotency_key = $2
                "#,
                new.client_id.as_uuid(),
                new.idempotency_key,
            )
            .fetch_one(&mut *tx)
            .await?;
            let same_request = row.request_hash == new.request_hash;
            AcceptOutcome::Existing {
                booking: row.try_into()?,
                same_request,
            }
        }
    };

    tx.commit().await?;
    Ok(outcome)
}

/// 예약 하나 (클라이언트 구분 없이). 워커용.
#[tracing::instrument(name = "db.bookings.get", skip_all)]
pub async fn get(pool: &PgPool, id: BookingId) -> Result<Option<Booking>, DbError> {
    let row = sqlx::query_as!(
        BookingRow,
        r#"
        SELECT id, client_id, provider_id, resource_id, slot_start, party_size, customer, status,
               status_reason, provider_booking_id, deadline_at, created_at, updated_at, request_hash
        FROM bookings WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_optional(pool)
    .await?;
    row.map(Booking::try_from).transpose()
}

/// 예약 하나를 읽으면서 그 행을 잠근다 (`FOR UPDATE`). 트랜잭션이 끝날 때까지 다른 곳에서 이 예약을 바꾸지 못한다.
///
/// 관리자 재시도처럼 "상태를 확인하고 → 작업을 정리하고 → 상태를 바꾸는" 여러 단계를 한 예약에 대해 차례로 할 때,
/// 두 요청이 동시에 와서 작업이 둘 생기는 일을 막는다.
#[tracing::instrument(name = "db.bookings.get_for_update", skip_all)]
pub async fn get_for_update(
    conn: &mut PgConnection,
    id: BookingId,
) -> Result<Option<Booking>, DbError> {
    let row = sqlx::query_as!(
        BookingRow,
        r#"
        SELECT id, client_id, provider_id, resource_id, slot_start, party_size, customer, status,
               status_reason, provider_booking_id, deadline_at, created_at, updated_at, request_hash
        FROM bookings WHERE id = $1
        FOR UPDATE
        "#,
        id.as_uuid(),
    )
    .fetch_optional(conn)
    .await?;
    row.map(Booking::try_from).transpose()
}

/// 클라이언트 자신의 예약 하나. 다른 클라이언트의 예약이면 `None`.
#[tracing::instrument(name = "db.bookings.find_for_client", skip_all)]
pub async fn find_for_client(
    pool: &PgPool,
    client_id: ClientId,
    id: BookingId,
) -> Result<Option<Booking>, DbError> {
    let row = sqlx::query_as!(
        BookingRow,
        r#"
        SELECT id, client_id, provider_id, resource_id, slot_start, party_size, customer, status,
               status_reason, provider_booking_id, deadline_at, created_at, updated_at, request_hash
        FROM bookings WHERE id = $1 AND client_id = $2
        "#,
        id.as_uuid(),
        client_id.as_uuid(),
    )
    .fetch_optional(pool)
    .await?;
    row.map(Booking::try_from).transpose()
}

/// 관리자 예약 목록의 조건. `None`인 조건은 걸지 않는다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookingFilter {
    pub status: Option<BookingStatus>,
    pub provider: Option<ProviderId>,
    /// 접수 시각이 이 시각 이후 (포함)
    pub created_from: Option<DateTime<Utc>>,
    /// 접수 시각이 이 시각 전 (미포함)
    pub created_to: Option<DateTime<Utc>>,
    /// 이 ID보다 먼저 접수된 예약만. 앞 페이지의 마지막 ID를 넣으면 다음 페이지가 된다 (커서 페이지네이션).
    pub before: Option<BookingId>,
}

/// 예약 목록 (최근에 접수한 것부터, 최대 `limit`건). 관리자 화면용.
///
/// 정렬과 커서는 예약 ID 기준이다. ID가 UUID v7(앞부분이 생성 시각)이라 ID 순서가 곧 접수 순서다.
/// "N번째 페이지"(OFFSET) 대신 "이 ID 다음"(커서)으로 넘기므로, 목록을 보는 동안 새 예약이 들어와도
/// 같은 예약이 두 페이지에 걸쳐 나오거나 빠지지 않고, 뒤 페이지도 인덱스로 바로 찾는다.
///
/// `$1::text IS NULL OR status = $1`: 조건 값이 없으면(NULL) 그 조건을 건너뛴다. 조건 조합마다 SQL을 따로 만들지 않고
/// 문장 하나로 `query_as!`의 컴파일 시점 검사를 받는다.
#[tracing::instrument(name = "db.bookings.list", skip_all)]
pub async fn list(
    pool: &PgPool,
    filter: &BookingFilter,
    limit: i64,
) -> Result<Vec<Booking>, DbError> {
    let rows = sqlx::query_as!(
        BookingRow,
        r#"
        SELECT id, client_id, provider_id, resource_id, slot_start, party_size, customer, status,
               status_reason, provider_booking_id, deadline_at, created_at, updated_at, request_hash
        FROM bookings
        WHERE ($1::text IS NULL OR status = $1)
          AND ($2::text IS NULL OR provider_id = $2)
          AND ($3::timestamptz IS NULL OR created_at >= $3)
          AND ($4::timestamptz IS NULL OR created_at < $4)
          AND ($5::uuid IS NULL OR id < $5)
        ORDER BY id DESC
        LIMIT $6
        "#,
        filter.status.map(BookingStatus::as_str),
        filter.provider.as_ref().map(ProviderId::as_str),
        filter.created_from,
        filter.created_to,
        filter.before.map(|id| id.as_uuid()),
        limit,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(Booking::try_from).collect()
}

/// 상태를 바꾸고 이력을 남긴다.
///
/// 다음 상태는 도메인의 [`BookingStatus::apply`]로만 정한다. 그래서 규칙에 없는 전이는 저장 단계에서 막힌다.
/// `WHERE status = $from`: 읽은 뒤 다른 곳에서 상태가 바뀌었으면 아무 행도 바꾸지 않는다 (낙관적 동시성).
///
/// `trace_id`는 이 전이를 일으킨 요청 · 작업의 trace다. 상태 이력에 함께 남는다.
#[tracing::instrument(name = "db.bookings.transition", skip_all)]
pub async fn transition(
    conn: &mut PgConnection,
    id: BookingId,
    from: BookingStatus,
    event: &BookingEvent,
    actor: &str,
    trace_id: Option<&str>,
) -> Result<Transitioned, DbError> {
    let to = from.apply(event)?;
    let reason = event.reason();
    let provider_booking_id = event.provider_booking_id();

    let result = sqlx::query!(
        "UPDATE bookings
         SET status = $3, status_reason = $4,
             provider_booking_id = COALESCE($5, provider_booking_id), updated_at = now()
         WHERE id = $1 AND status = $2",
        id.as_uuid(),
        from.as_str(),
        to.as_str(),
        reason,
        provider_booking_id,
    )
    .execute(&mut *conn)
    .await?;

    if result.rows_affected() == 0 {
        return Ok(Transitioned::Conflict);
    }
    insert_event(conn, id, Some(from), to, reason, actor, trace_id).await?;
    Ok(Transitioned::Applied(to))
}

/// 확정 기한을 바꾼다. 관리자 재시도처럼 예약을 다시 처리하게 할 때, 기한도 그 시각부터 다시 잡는다
/// (그대로 두면 이미 지난 기한 때문에 워커가 바로 다시 포기한다). 상태는 바꾸지 않는다.
#[tracing::instrument(name = "db.bookings.set_deadline", skip_all)]
pub async fn set_deadline(
    conn: &mut PgConnection,
    id: BookingId,
    deadline_at: DateTime<Utc>,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE bookings SET deadline_at = $2, updated_at = now() WHERE id = $1",
        id.as_uuid(),
        deadline_at,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// 예약의 상태 이력 (오래된 것부터)
#[tracing::instrument(name = "db.bookings.events", skip_all)]
pub async fn events(pool: &PgPool, id: BookingId) -> Result<Vec<EventRecord>, DbError> {
    let rows = sqlx::query!(
        "SELECT from_status, to_status, reason, actor, trace_id, at
         FROM booking_events WHERE booking_id = $1 ORDER BY id",
        id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(EventRecord {
                from_status: r
                    .from_status
                    .map(|s| s.parse())
                    .transpose()
                    .map_err(DbError::corrupt)?,
                to_status: r.to_status.parse().map_err(DbError::corrupt)?,
                reason: r.reason,
                actor: r.actor,
                trace_id: r.trace_id,
                at: r.at,
            })
        })
        .collect()
}

/// 결과 불명(`unknown`) 상태로 가장 오래 머문 예약이 그 상태가 된 뒤 흐른 시간. 그런 예약이 없으면 0.
///
/// "결과 불명이 10분 넘게 유지" 알림에 쓰는 지표의 원천이다 (docs/05-observability-and-analytics.md §8).
/// `bookings_status_idx (status, updated_at)` 인덱스로 가장 오래된 한 건만 찾으므로 예약이 많아도 가볍다.
pub async fn oldest_unknown_age(pool: &PgPool) -> Result<std::time::Duration, DbError> {
    // `"age_secs!"`: sqlx는 계산식 컬럼을 NULL일 수 있다고 본다. 이름 끝의 `!`로 "NULL이 아니다"라고 알려 준다
    // (COALESCE로 0을 채웠으므로 실제로 NULL이 아니다).
    let age_secs = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0)::float8 AS "age_secs!"
        FROM bookings WHERE status = 'unknown'
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(crate::non_negative_secs(age_secs))
}

async fn insert_event(
    conn: &mut PgConnection,
    booking_id: BookingId,
    from: Option<BookingStatus>,
    to: BookingStatus,
    reason: Option<String>,
    actor: &str,
    trace_id: Option<&str>,
) -> Result<(), DbError> {
    let from = from.map(BookingStatus::as_str);
    sqlx::query!(
        "INSERT INTO booking_events (booking_id, from_status, to_status, reason, actor, trace_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
        booking_id.as_uuid(),
        from,
        to.as_str(),
        reason,
        actor,
        trace_id,
    )
    .execute(conn)
    .await?;
    Ok(())
}
