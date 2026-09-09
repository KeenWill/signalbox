//! Path-free evidence of the workspace bound by daemon-local tools.
use crate::mapping::{session_workspace_root_kind_from_str, session_workspace_root_kind_to_str};
use signalbox_domain::{RepoWatchDispatchId, SessionId, SessionWorkspaceRootKind};
use sqlx::{PgConnection, PgPool, Row};

/// Filesystem selection supplied by the daemon's binding authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceRootBinding {
    Configured,
    Derived {
        dispatch_marker: Option<RepoWatchDispatchId>,
    },
}

/// Records the selected root, recognizing a provisioned checkout only when its
/// marker names this session's retained dispatch.
pub async fn record_binding(
    pool: &PgPool,
    session: SessionId,
    binding: WorkspaceRootBinding,
) -> Result<SessionWorkspaceRootKind, sqlx::Error> {
    let (selected, marker) = match binding {
        WorkspaceRootBinding::Configured => (SessionWorkspaceRootKind::Configured, None),
        WorkspaceRootBinding::Derived { dispatch_marker } => {
            (SessionWorkspaceRootKind::Derived, dispatch_marker)
        }
    };
    let row = sqlx::query(
        "INSERT INTO session_workspace_binding (session_id, workspace_root_kind)
        SELECT session_id, CASE
            WHEN $2 = 'derived' AND dispatch_ref = $3 THEN 'provisioned' ELSE $2 END
        FROM session WHERE session_id = $1
        ON CONFLICT (session_id) DO UPDATE
            SET workspace_root_kind = EXCLUDED.workspace_root_kind
        RETURNING workspace_root_kind",
    )
    .bind(session.into_uuid())
    .bind(session_workspace_root_kind_to_str(selected))
    .bind(marker.map(|id| id.into_uuid()))
    .fetch_one(pool)
    .await?;
    decode(row.try_get("workspace_root_kind")?)?.ok_or(sqlx::Error::RowNotFound)
}

pub(crate) async fn read_binding(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<SessionWorkspaceRootKind>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT binding.workspace_root_kind FROM session
         LEFT JOIN session_workspace_binding AS binding USING (session_id)
         WHERE session.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(connection)
    .await?;
    decode(row.try_get("workspace_root_kind")?)
}

pub(crate) fn decode(
    value: Option<String>,
) -> Result<Option<SessionWorkspaceRootKind>, sqlx::Error> {
    value
        .map(|value| {
            session_workspace_root_kind_from_str(&value).ok_or_else(|| {
                sqlx::Error::Decode(Box::new(std::io::Error::other(
                    "invalid session workspace root kind",
                )))
            })
        })
        .transpose()
}
