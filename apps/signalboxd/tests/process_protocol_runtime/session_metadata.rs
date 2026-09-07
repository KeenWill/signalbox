//! Session metadata coverage.

use super::*;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_lists_the_alias_session_projection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let alias_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let expected_placement = SessionPlacement::Pathless {};
    let session_id =
        create_alias_session_with(&mut connection, alias_id, expected_placement.clone()).await?;

    connection
        .request(2, ClientRequest::ListSessions {})
        .await?;

    let start = response_within(&mut connection).await?;
    assert_eq!(start.message(), &ServerMessage::SessionsStart {});
    let summary = response_within(&mut connection).await?;
    assert_eq!(
        summary.message(),
        &ServerMessage::SessionSummary {
            session_id,
            defaults_version: CanonicalU64::new(
                SessionConfigurationDefaultsVersion::first().as_u64(),
            ),
            model_selection: ModelSelection::Alias { alias_id },
            placement_version: CanonicalU64::new(
                signalbox_domain::SessionPlacementVersion::INITIAL.as_u64(),
            ),
            placement: expected_placement,
            runner: None,
        }
    );
    let end = response_within(&mut connection).await?;
    assert_eq!(
        end.message(),
        &ServerMessage::SessionsEnd {
            session_count: CanonicalU64::new(1),
        }
    );

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_reads_an_empty_operator_status_snapshot() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request(1, ClientRequest::ReadOperatorStatus {})
        .await?;

    let start = response_within(&mut connection).await?;
    assert_eq!(
        start.message(),
        &ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::Start {}))
    );
    let end = response_within(&mut connection).await?;
    assert_eq!(
        end.message(),
        &ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::End(Box::new(
            OperatorStatusEndMessage {
                lifecycle_week_count: CanonicalU64::new(0),
                lifecycle_deadline_violation_count: CanonicalU64::new(0),
            },
        ))))
    );

    drop(connection);
    runtime.stop().await
}

/// metadata wire-shape failures are malformed frames, not application
/// request rejections.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn metadata_shape_failure_is_a_malformed_frame() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let required_tags = vec!["x".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES + 1)];
    let frame = format!(
        "{{\"version\":1,\"request_id\":\"21\",\"request\":{{\"type\":\"list_session_metadata\",\"required_tags\":{},\"title_contains\":null,\"include_archived\":false,\"page_size\":\"50\",\"after_session_id\":null}}}}\n",
        serde_json::to_string(&required_tags)?
    );

    connection.raw_request(&frame).await?;

    let response = response_within(&mut connection).await?;
    assert_eq!(response.version(), ProtocolVersion::One);
    assert!(matches!(
        response.message(),
        ServerMessage::Error {
            code: ErrorCode::MalformedFrame,
            ..
        }
    ));

    drop(connection);
    runtime.stop().await
}

/// version four exposes the canonical initial metadata projection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reads_initial_metadata_projection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            10,
            ClientRequest::ReadSessionMetadata {
                session_id: first_session,
            },
        )
        .await?;
    let initial = response_within(&mut connection).await?;
    assert_eq!(initial.version(), ProtocolVersion::One);
    let ServerMessage::SessionMetadata {
        session_id,
        metadata,
        last_writer: None,
    } = initial.message()
    else {
        panic!(
            "fixture expected initial metadata, got {:?}",
            initial.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert_eq!(metadata, &SessionMetadata::empty());

    drop(connection);
    runtime.stop().await
}

/// a durable snapshot whose last writer is tool execution projects onto
/// both metadata read surfaces. The tool-facing replacement constructor is
/// production-registered, so this row shape exists in ordinary operation; a
/// missing wire projection would fail the read as an encode invariant, which is
/// fatal to the daemon and repeats on every later read of the same row.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reads_back_tool_written_metadata() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session = create_alias_session(&mut connection).await?;
    let tool_request = ToolRequestId::from_uuid(Uuid::now_v7());

    let replacement = SessionMetadataContent::try_new(
        Some(String::from("Status from the tool")),
        vec![String::from("automated")],
        Vec::new(),
        false,
    )
    .map_err(|error| io::Error::other(format!("metadata fixture is invalid: {error:?}")))?;
    let write = ReplaceSessionMetadataRequest::try_new_for_tool(
        DurableCommandId::from_uuid(Uuid::now_v7()),
        SessionId::from_uuid(session.into_uuid()),
        tool_request,
        replacement,
    )?;
    let mut writer =
        ReplaceSessionMetadataService::new(SessionMetadataRepository::new(runtime.pool.clone()));
    let ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Applied(applied)) =
        writer.execute(write).await?
    else {
        panic!("fixture expected the tool replacement to apply");
    };
    assert_eq!(
        applied.snapshot().last_writer().map(|last| last.actor()),
        Some(Actor::Tool {
            request: tool_request,
        })
    );

    connection
        .request(
            11,
            ClientRequest::ReadSessionMetadata {
                session_id: session,
            },
        )
        .await?;
    let read = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadata {
        last_writer: Some(last_writer),
        ..
    } = read.message()
    else {
        panic!(
            "fixture expected tool-written metadata, got {:?}",
            read.message()
        );
    };
    assert_eq!(
        last_writer.actor(),
        MetadataActor::Tool {
            tool_request_id: CanonicalUuid::from_uuid(tool_request.into_uuid()),
        }
    );

    connection
        .request(
            12,
            ClientRequest::ListSessionMetadata {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: false,
                page_size: CanonicalU64::new(50),
                after_session_id: None,
            },
        )
        .await?;
    let page_start = response_within(&mut connection).await?;
    assert!(matches!(
        page_start.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    let summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        last_writer: Some(listed_writer),
        ..
    } = summary.message()
    else {
        panic!(
            "fixture expected the tool-written summary, got {:?}",
            summary.message()
        );
    };
    assert_eq!(listed_writer.actor(), last_writer.actor());
    let page_end = response_within(&mut connection).await?;
    assert!(matches!(
        page_end.message(),
        ServerMessage::SessionMetadataPageEnd { .. }
    ));

    // The daemon survives both reads: a later request on a fresh connection is
    // still served, which a fatal encode invariant would have prevented.
    let mut later = Connection::connect(runtime.socket()).await?;
    later
        .request(
            13,
            ClientRequest::ReadSessionMetadata {
                session_id: session,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut later).await?.message(),
        ServerMessage::SessionMetadata { .. }
    ));

    drop(later);
    drop(connection);
    runtime.stop().await
}

/// one metadata command identity applies once, replays exactly, and
/// rejects a structurally different reuse.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn enforces_metadata_command_identity() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;

    let replacement_command = command()?;
    let replacement = SessionMetadata::try_new(
        Some(String::from("Archived plan")),
        vec![String::from("work"), String::from("daily")],
        vec![(String::from("run"), String::from("17"))],
        true,
    )?;
    connection
        .request_version(
            ProtocolVersion::One,
            11,
            ClientRequest::ReplaceSessionMetadata {
                command_id: replacement_command,
                session_id: first_session,
                metadata: replacement.clone(),
            },
        )
        .await?;
    let applied = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        last_writer,
    } = applied.message()
    else {
        panic!(
            "fixture expected replaced metadata, got {:?}",
            applied.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert_eq!(metadata, &replacement);
    assert!(matches!(last_writer.actor(), MetadataActor::User {}));

    connection
        .request_version(
            ProtocolVersion::One,
            12,
            ClientRequest::ReplaceSessionMetadata {
                command_id: replacement_command,
                session_id: first_session,
                metadata: replacement.clone(),
            },
        )
        .await?;
    let replay = response_within(&mut connection).await?;
    assert_eq!(replay.message(), applied.message());

    connection
        .request_version(
            ProtocolVersion::One,
            13,
            ClientRequest::ReplaceSessionMetadata {
                command_id: replacement_command,
                session_id: first_session,
                metadata: SessionMetadata::empty(),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::ConflictingReuse,
            ..
        }
    ));

    drop(connection);
    runtime.stop().await
}

/// the default metadata list applies exact filters while excluding an
/// archived match.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn metadata_list_applies_default_visibility_filters() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;
    let second_session = create_alias_session(&mut connection).await?;
    let archived_metadata = SessionMetadata::try_new(
        Some(String::from("Active archived plan")),
        vec![String::from("daily")],
        Vec::new(),
        true,
    )?;
    connection
        .request_version(
            ProtocolVersion::One,
            10,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: archived_metadata,
            },
        )
        .await?;
    let archived_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        ..
    } = archived_receipt.message()
    else {
        panic!(
            "fixture expected archived metadata receipt, got {:?}",
            archived_receipt.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert!(metadata.archived());

    let second_metadata = SessionMetadata::try_new(
        Some(String::from("Active plan")),
        vec![String::from("daily")],
        Vec::new(),
        false,
    )?;
    connection
        .request_version(
            ProtocolVersion::One,
            14,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: second_session,
                metadata: second_metadata.clone(),
            },
        )
        .await?;
    let second_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        ..
    } = second_receipt.message()
    else {
        panic!(
            "fixture expected second metadata receipt, got {:?}",
            second_receipt.message()
        );
    };
    assert_eq!(*session_id, second_session);
    assert_eq!(metadata, &second_metadata);

    connection
        .request_version(
            ProtocolVersion::One,
            15,
            ClientRequest::ListSessionMetadata {
                required_tags: vec![String::from("daily")],
                title_contains: Some(String::from("Active")),
                include_archived: false,
                page_size: CanonicalU64::new(10),
                after_session_id: None,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    let summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        session_id,
        dangerous_tool_auto_approval: false,
        title: Some(title),
        tags,
        archived: false,
        ..
    } = summary.message()
    else {
        panic!(
            "fixture expected active metadata summary, got {:?}",
            summary.message()
        );
    };
    assert_eq!(*session_id, second_session);
    assert_eq!(
        title.as_str(),
        second_metadata
            .title()
            .expect("the fixture metadata states its title")
    );
    assert!(tags.iter().map(String::as_str).eq(second_metadata.tags()));
    let page_end = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataPageEnd {
        session_count,
        next_after_session_id: None,
    } = page_end.message()
    else {
        panic!(
            "fixture expected terminal metadata page, got {:?}",
            page_end.message()
        );
    };
    assert_eq!(session_count.value(), 1);

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn metadata_list_uses_bounded_keyset_pages() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;
    let second_session = create_alias_session(&mut connection).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            16,
            ClientRequest::ListSessionMetadata {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: true,
                page_size: CanonicalU64::new(1),
                after_session_id: None,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    let first_summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        session_id: first_page_session,
        ..
    } = first_summary.message()
    else {
        panic!(
            "unexpected first metadata-page summary: {:?}",
            first_summary.message()
        );
    };
    let first_page_session = *first_page_session;
    let first_end = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataPageEnd {
        session_count,
        next_after_session_id: Some(next),
    } = first_end.message()
    else {
        panic!(
            "unexpected first metadata-page end: {:?}",
            first_end.message()
        );
    };
    assert_eq!(session_count.value(), 1);
    let next = *next;
    assert_eq!(next, first_page_session);

    connection
        .request_version(
            ProtocolVersion::One,
            17,
            ClientRequest::ListSessionMetadata {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: true,
                page_size: CanonicalU64::new(1),
                after_session_id: Some(next),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    let second_summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        session_id: second_page_session,
        ..
    } = second_summary.message()
    else {
        panic!(
            "unexpected second metadata-page summary: {:?}",
            second_summary.message()
        );
    };
    let second_page_session = *second_page_session;
    assert_ne!(second_page_session, first_page_session);
    assert!(
        [first_page_session, second_page_session].contains(&first_session)
            && [first_page_session, second_page_session].contains(&second_session)
    );
    let page_end = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataPageEnd {
        session_count,
        next_after_session_id: None,
    } = page_end.message()
    else {
        panic!(
            "fixture expected terminal metadata page, got {:?}",
            page_end.message()
        );
    };
    assert_eq!(session_count.value(), 1);

    drop(connection);
    runtime.stop().await
}

/// a metadata read returns the complete current wire
/// projection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reads_current_metadata_projection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;
    let replacement = SessionMetadata::try_new(
        Some(String::from("Current plan")),
        vec![String::from("daily")],
        vec![(String::from("run"), String::from("17"))],
        false,
    )?;
    connection
        .request_version(
            ProtocolVersion::One,
            10,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: replacement.clone(),
            },
        )
        .await?;
    let replacement_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced { session_id, .. } = replacement_receipt.message()
    else {
        panic!(
            "fixture expected metadata replacement, got {:?}",
            replacement_receipt.message()
        );
    };
    assert_eq!(*session_id, first_session);

    connection
        .request_version(
            ProtocolVersion::One,
            18,
            ClientRequest::ReadSessionMetadata {
                session_id: first_session,
            },
        )
        .await?;
    let read = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadata {
        session_id,
        metadata,
        last_writer: Some(last_writer),
    } = read.message()
    else {
        panic!(
            "fixture expected current metadata, got {:?}",
            read.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert_eq!(metadata, &replacement);
    assert!(matches!(last_writer.actor(), MetadataActor::User {}));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn metadata_read_maps_a_missing_session() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let absent = CanonicalUuid::from_uuid(Uuid::from_u128(0xdead));
    connection
        .request_version(
            ProtocolVersion::One,
            19,
            ClientRequest::ReadSessionMetadata { session_id: absent },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::NotFound,
            ..
        }
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn metadata_replace_maps_a_missing_session() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let absent = CanonicalUuid::from_uuid(Uuid::from_u128(0xdead));
    connection
        .request_version(
            ProtocolVersion::One,
            20,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: absent,
                metadata: SessionMetadata::empty(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::SessionNotFound { session_id: absent }
    );

    drop(connection);
    runtime.stop().await
}

/// replacing an archived snapshot with `archived = false` returns the
/// same session to the default list.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn metadata_restore_returns_session_to_default_list() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;
    let archived = SessionMetadata::try_new(
        Some(String::from("Archived plan")),
        vec![String::from("work"), String::from("daily")],
        vec![(String::from("run"), String::from("17"))],
        true,
    )?;
    connection
        .request_version(
            ProtocolVersion::One,
            10,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: archived,
            },
        )
        .await?;
    let archived_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        ..
    } = archived_receipt.message()
    else {
        panic!(
            "fixture expected archived metadata receipt, got {:?}",
            archived_receipt.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert!(metadata.archived());

    let restored = SessionMetadata::try_new(
        Some(String::from("Archived plan")),
        vec![String::from("work"), String::from("daily")],
        vec![(String::from("run"), String::from("17"))],
        false,
    )?;
    connection
        .request_version(
            ProtocolVersion::One,
            21,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: restored.clone(),
            },
        )
        .await?;
    let restored_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        ..
    } = restored_receipt.message()
    else {
        panic!(
            "fixture expected restored metadata, got {:?}",
            restored_receipt.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert_eq!(metadata, &restored);

    connection
        .request_version(
            ProtocolVersion::One,
            22,
            ClientRequest::ListSessionMetadata {
                required_tags: vec![String::from("daily")],
                title_contains: Some(String::from("Archived")),
                include_archived: false,
                page_size: CanonicalU64::new(10),
                after_session_id: None,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    let summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        session_id,
        archived: false,
        ..
    } = summary.message()
    else {
        panic!(
            "fixture expected restored metadata summary, got {:?}",
            summary.message()
        );
    };
    assert_eq!(*session_id, first_session);
    let page_end = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataPageEnd {
        session_count,
        next_after_session_id: None,
    } = page_end.message()
    else {
        panic!(
            "fixture expected terminal metadata page, got {:?}",
            page_end.message()
        );
    };
    assert_eq!(session_count.value(), 1);

    drop(connection);
    runtime.stop().await
}
