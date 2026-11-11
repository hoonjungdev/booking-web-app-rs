//! 관리자 웹 정적 파일 (docs/06-admin-frontend.md §5)
//!
//! `pnpm build`가 만든 `web/admin/dist`를 `/admin`에서 그대로 내려준다. 그래서 배포할 때는 프로세스 하나 · 주소 하나로
//! API와 화면을 함께 제공한다 (화면이 같은 주소의 API를 부르므로 CORS 설정이 필요 없다).
//!
//! 화면 경로(`/admin/bookings/…`)는 파일이 아니라 브라우저의 React Router가 처리하는 주소다. 그런 경로를 새로 고치면
//! 서버에 그 경로로 요청이 오므로, 파일이 없는 경로에는 `index.html`을 돌려준다 (SPA fallback).
//! 관리자 API(`/admin/api/...`)는 더 구체적인 경로라서 이 규칙보다 먼저 맞는다 (`api::router`).

use std::path::Path;

use axum::{Router, response::Redirect, routing::get};
use tower_http::services::{ServeDir, ServeFile};

/// 관리자 웹 경로. 없으면(아직 빌드하지 않음) `None`.
pub fn routes(dir: &Path) -> Option<Router> {
    let index = dir.join("index.html");
    if !index.is_file() {
        return None;
    }
    // ServeDir: 폴더의 파일을 경로 그대로 내려준다 (≈ ASP.NET Core의 UseStaticFiles).
    // fallback: 파일이 없으면 index.html을 200으로 돌려준다 (≈ MapFallbackToFile("index.html")).
    let files = ServeDir::new(dir).fallback(ServeFile::new(index));
    Some(
        Router::new()
            .nest_service("/admin", files)
            .route("/", get(|| async { Redirect::temporary("/admin/") })),
    )
}
