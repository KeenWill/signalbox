//! Rendered-frontier attachment authority shared by blob and typed file tools.

use super::*;
use signalbox_application::RenderedAttachmentSelector;
use signalbox_domain::{
    AttachmentDisplayFilename, AttachmentKind, BlobDigest, DeclaredMediaType,
    SemanticTranscriptEntryId, UserContentPart,
};

/// Exact visible semantic use returned after the catalog transaction ends.
#[derive(Clone, Debug)]
pub struct VisibleToolAttachment {
    /// Identity of this occurrence in the rendered frontier.
    pub selector: RenderedAttachmentSelector,
    /// Checked semantic attachment metadata, independent of blob placement.
    pub part: UserContentPart,
}

impl PostgresToolLoopRepository {
    /// Resolves one use through the same projected frontier proof as blob preauthorization.
    /// An omitted selector succeeds only when exactly one occurrence exists.
    pub async fn resolve_visible_attachment(
        &self,
        request: &signalbox_domain::ToolRequest,
        digest: BlobDigest,
        selector: Option<RenderedAttachmentSelector>,
    ) -> Result<Option<VisibleToolAttachment>, ToolLoopRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let rows = visible_attachment_rows(
            &mut transaction,
            request.session(),
            request.turn(),
            request.id(),
            digest,
            selector,
        )
        .await?;
        let result = match rows.as_slice() {
            [row] => Some(decode_visible_attachment(row, digest)?),
            _ => None,
        };
        transaction.commit().await?;
        Ok(result)
    }
}

fn decode_visible_attachment(
    row: &PgRow,
    digest: BlobDigest,
) -> Result<VisibleToolAttachment, ToolLoopRepositoryError> {
    let malformed = || ToolLoopCorruption::Inconsistent("visible attachment metadata");
    let entry = SemanticTranscriptEntryId::from_uuid(required(row, "semantic_entry_id")?);
    let ordinal = u8::try_from(required::<i16>(row, "position")?).map_err(|_| malformed())?;
    let kind = match required::<String>(row, "attachment_kind")?.as_str() {
        "image" => AttachmentKind::Image,
        "document" => AttachmentKind::Document,
        "file" => AttachmentKind::File,
        _ => return Err(malformed().into()),
    };
    let media_type = DeclaredMediaType::try_new(required::<String>(row, "declared_media_type")?)
        .map_err(|_| malformed())?;
    let display_filename = row
        .try_get::<Option<String>, _>("display_filename")?
        .map(AttachmentDisplayFilename::try_new)
        .transpose()
        .map_err(|_| malformed())?;
    Ok(VisibleToolAttachment {
        selector: RenderedAttachmentSelector::new(entry, ordinal),
        part: UserContentPart::Attachment {
            digest,
            kind,
            media_type,
            display_filename,
        },
    })
}

pub(super) async fn visible_attachment_rows(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    request: ToolRequestId,
    digest: BlobDigest,
    selector: Option<RenderedAttachmentSelector>,
) -> Result<Vec<PgRow>, ToolLoopRepositoryError> {
    let frontier: Uuid = sqlx::query_scalar(
        "SELECT call.context_frontier_id FROM tool_request AS request
           JOIN model_call AS call
             ON call.model_call_id = request.producing_model_call_id
            AND call.session_id = request.session_id
          WHERE request.request_id = $1 AND request.session_id = $2 AND request.turn_id = $3",
    )
    .bind(tool_request_id_to_uuid(request))
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ToolLoopCorruption::Missing(
        "blob authorization producing frontier",
    ))?;
    let members = crate::context_compaction::projected_frontier_membership(
        connection,
        session,
        signalbox_domain::ContextFrontierId::from_uuid(frontier),
    )
    .await
    .map_err(crate::model_execution::map_projected_membership_error)
    .map_err(map_model_call_error)?;
    let sources = members
        .iter()
        .map(|member| member.source_session().into_uuid())
        .collect::<Vec<_>>();
    let entries = members
        .iter()
        .map(|member| member.entry().into_uuid())
        .collect::<Vec<_>>();
    sqlx::query(
        "SELECT entry.semantic_entry_id, part.position, part.attachment_kind,
                part.declared_media_type, part.display_filename
              FROM unnest($1::uuid[], $2::uuid[]) AS member(source_session_id, semantic_entry_id)
              JOIN semantic_transcript_entry AS entry
                ON entry.source_session_id = member.source_session_id
               AND entry.semantic_entry_id = member.semantic_entry_id
              JOIN accepted_input_content_part AS part
                ON part.accepted_input_id = entry.origin_accepted_input_id
             WHERE part.part_kind = 'attachment' AND part.blob_digest = $3
               AND ($4::uuid IS NULL OR entry.semantic_entry_id = $4)
               AND ($5::smallint IS NULL OR part.position = $5)
             ORDER BY entry.semantic_entry_id, part.position LIMIT 2",
    )
    .bind(&sources)
    .bind(&entries)
    .bind(digest.as_bytes().as_slice())
    .bind(selector.map(|value| value.entry().into_uuid()))
    .bind(selector.map(|value| i16::from(value.part_ordinal())))
    .fetch_all(&mut *connection)
    .await
    .map_err(Into::into)
}
