use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use signalbox_web_contract::WebImportListPage;
use tower::ServiceExt as _;

use super::production_router;

#[tokio::test]
async fn client_import_list_path_returns_the_catalog() -> Result<(), Box<dyn std::error::Error>> {
    let (_container, pool, _) =
        signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
    let models = crate::configuration::checked_in_example_configuration()?;
    let response = production_router(None, Some(pool), None, Some(models), None, None, None)
        .oneshot(
            Request::get("/api/imports?limit=1")
                .header(header::HOST, "localhost")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    let page: WebImportListPage = serde_json::from_slice(&body)?;
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
    Ok(())
}
