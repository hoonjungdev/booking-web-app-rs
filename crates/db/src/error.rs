use domain::InvalidTransition;

/// 저장소 에러.
///
/// `#[from]`을 붙이면 `sqlx::Error` 등을 이 타입으로 바꾸는 `From` 구현이 생긴다.
/// 그래서 함수 안에서 `?`를 쓰면 sqlx 에러가 자동으로 `DbError::Sqlx`로 감싸져 돌아간다.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    /// 허용되지 않는 상태 전이를 저장하려 함 (도메인 규칙 위반)
    #[error(transparent)]
    InvalidTransition(#[from] InvalidTransition),
    /// DB에 저장된 값이 도메인 타입으로 바뀌지 않음 (데이터가 깨졌거나 코드와 스키마가 어긋남)
    #[error("DB 값이 도메인 규칙에 맞지 않습니다: {0}")]
    Corrupt(String),
}

impl DbError {
    pub(crate) fn corrupt(what: impl std::fmt::Display) -> Self {
        Self::Corrupt(what.to_string())
    }
}
