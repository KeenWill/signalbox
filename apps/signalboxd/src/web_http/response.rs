use super::{
    IntoResponse, Json, Response, StatusCode, WebApiError, WebApiErrorKind, WebApiErrorResponse,
};

pub(crate) fn transport_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> Response {
    api_error(status, WebApiErrorKind::Transport, code, message)
}

pub(crate) fn application_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> Response {
    api_error(status, WebApiErrorKind::Application, code, message)
}

fn api_error(
    status: StatusCode,
    kind: WebApiErrorKind,
    code: &'static str,
    message: &'static str,
) -> Response {
    let body = Json(WebApiErrorResponse {
        error: WebApiError {
            kind,
            code: code.to_owned(),
            message: message.to_owned(),
        },
    });
    (status, body).into_response()
}

pub(super) async fn api_not_found() -> Response {
    transport_error(
        StatusCode::NOT_FOUND,
        "api_route_not_found",
        "API route does not exist in this contract",
    )
}

pub(super) async fn static_assets_not_configured() -> Response {
    StatusCode::NOT_FOUND.into_response()
}
