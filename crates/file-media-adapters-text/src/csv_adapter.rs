use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    MAX_OBSERVED_CONTAINER_ENTRIES, ProbeStrength, ProcessorFailure, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ValidationEvidence, VerifiedBlobSource,
};

use crate::{
    CSV_MEDIA_TYPE, MAX_TEXT_FAMILY_BYTES, PROBE_PREFIX_BYTES, STRUCTURED_VIEW_NAME,
    json_adapter::{self, ProbeExtent},
    read_input_is_empty, source,
};

// Hard safety ceiling preventing one record from causing runaway allocation.
const MAX_COLUMNS: usize = 256;

pub(crate) async fn probe(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorProbeOutput, ProcessorFailure> {
    let prefix = source::read_probe_prefix(source, cancellation).await?;
    let extent = if source.byte_length().get() <= prefix.len() as u64 {
        ProbeExtent::CompleteSource
    } else {
        ProbeExtent::TruncatedPrefix
    };
    let json_suppresses_csv = matches!(extent, ProbeExtent::CompleteSource)
        && json_adapter::is_complete_json_document(&prefix);
    let probe_text = match extent {
        ProbeExtent::CompleteSource => std::str::from_utf8(&prefix).ok(),
        ProbeExtent::TruncatedPrefix => source::probe_utf8(&prefix),
    };
    let candidate =
        !json_suppresses_csv && probe_text.is_some_and(|text| has_record_structure(text, extent));
    if candidate {
        Ok(ProcessorProbeOutput::Candidate {
            media_type: String::from(CSV_MEDIA_TYPE),
            strength: match extent {
                ProbeExtent::CompleteSource => ProbeStrength::StructuralCandidate,
                ProbeExtent::TruncatedPrefix => ProbeStrength::ProvisionalStructuralCandidate,
            },
        })
    } else {
        Ok(ProcessorProbeOutput::NoMatch)
    }
}

pub(crate) async fn inspect(
    request: FileMediaProviderValidationRequest,
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    if request.media_type.as_str() != CSV_MEDIA_TYPE {
        return Err(ProcessorFailure::Protocol);
    }
    let Some(bytes) =
        source::read_complete(source, cancellation, request.maximum_source_bytes).await?
    else {
        if matches!(
            request.evidence,
            ValidationEvidence::DeclaredCandidateStructurallyValidated
        ) {
            let prefix =
                source::read_validation_prefix(source, cancellation, request.maximum_source_bytes)
                    .await?;
            if source::probe_utf8(&prefix).is_some_and(has_declared_record_structure) {
                return Ok(malformed("source_too_large"));
            }
        }
        return Ok(validation_failure(request.evidence, "source_too_large"));
    };
    let text = match source::checked_utf8(bytes) {
        Ok(text) => text,
        Err(reason) => return Ok(validation_failure(request.evidence, reason)),
    };
    let table = match parse_table(&text, MAX_OBSERVED_CONTAINER_ENTRIES) {
        Ok(table) => table,
        Err(reason) => {
            if matches!(request.evidence, ValidationEvidence::StructuralValidation)
                && initial_probe_was_provisional(text.as_bytes())
            {
                return Ok(ProcessorValidationOutput::NoMatch);
            }
            let declared_csv_shape = matches!(
                request.evidence,
                ValidationEvidence::DeclaredCandidateStructurallyValidated
            ) && (has_declared_record_evidence(&text)
                || reason == "column_limit_exceeded" && is_header_only_csv(&text));
            if declared_csv_shape {
                return Ok(malformed(reason));
            }
            return Ok(validation_failure(request.evidence, reason));
        }
    };
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(CSV_MEDIA_TYPE),
        evidence: request.evidence,
        metadata_json: serde_json::json!({
            "columns": table.headers.len(),
            "rows": table.rows.len()
        })
        .to_string(),
    })
}

pub(crate) async fn read(
    request: FileMediaProviderReadRequest,
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    if request.view.as_str() != STRUCTURED_VIEW_NAME || !read_input_is_empty(&request.input) {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    }
    let Some(bytes) = source::read_complete(source, cancellation, MAX_TEXT_FAMILY_BYTES).await?
    else {
        return Ok(ProcessorReadOutput::SourceTooLarge {
            maximum_bytes: MAX_TEXT_FAMILY_BYTES,
        });
    };
    let text = source::checked_utf8(bytes).map_err(|_| ProcessorFailure::Failed)?;
    if request.maximum_container_entries < 2 {
        return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
            limit_kind: String::from("container_entry_limit_exceeded"),
        });
    }
    let table = match parse_table(&text, request.maximum_container_entries) {
        Ok(table) => table,
        Err("row_limit_exceeded") | Err("column_limit_exceeded") => {
            return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                limit_kind: String::from("container_entry_limit_exceeded"),
            });
        }
        Err(_) => return Err(ProcessorFailure::Failed),
    };
    let body_json = serde_json::to_string(&serde_json::json!({
        "headers": table.headers,
        "rows": table.rows
    }))
    .map_err(|_| ProcessorFailure::Failed)?;
    if body_json.len() > MAX_TEXT_FAMILY_BYTES as usize {
        return Ok(ProcessorReadOutput::OutputUnitTooLarge);
    }
    Ok(ProcessorReadOutput::Structured {
        body_json,
        truncated: false,
        cursor: None,
    })
}

struct CsvTable {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

fn parse_table(text: &str, maximum_container_entries: u64) -> Result<CsvTable, &'static str> {
    if !quotes_are_well_formed(text) || has_blank_record(text) {
        return Err("malformed_csv");
    }
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|_| "malformed_csv")?
        .iter()
        .map(String::from)
        .collect::<Vec<_>>();
    if headers.is_empty() {
        return Err("malformed_csv");
    }
    if headers.len() > MAX_COLUMNS || headers.len() as u64 > maximum_container_entries {
        return Err("column_limit_exceeded");
    }
    let mut rows = Vec::new();
    for record in reader.records() {
        if rows.len() as u64 == maximum_container_entries {
            return Err("row_limit_exceeded");
        }
        let record = record.map_err(|_| "malformed_csv")?;
        if record.len() as u64 > maximum_container_entries {
            return Err("column_limit_exceeded");
        }
        rows.push(record.iter().map(String::from).collect());
    }
    if rows.is_empty() && !text.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return Err("malformed_csv");
    }
    Ok(CsvTable { headers, rows })
}

pub(crate) fn has_record_structure(text: &str, extent: ProbeExtent) -> bool {
    let evidence = match extent {
        ProbeExtent::CompleteSource => text,
        ProbeExtent::TruncatedPrefix => {
            let Some(evidence) = first_two_strict_records(text, extent) else {
                return false;
            };
            evidence
        }
    };
    if !quotes_are_well_formed(evidence) || has_blank_record(evidence) {
        return false;
    }
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(evidence.as_bytes());
    let mut records = reader.records();
    let Some(Ok(first)) = records.next() else {
        return false;
    };
    if first.len() < 2 {
        return false;
    }
    let Some(Ok(second)) = records.next() else {
        return false;
    };
    second.len() == first.len()
        && records.all(|record| record.is_ok_and(|record| record.len() == first.len()))
}

fn initial_probe_was_provisional(bytes: &[u8]) -> bool {
    usize::try_from(PROBE_PREFIX_BYTES)
        .ok()
        .and_then(|length| bytes.get(..length))
        .and_then(source::probe_utf8)
        .is_some_and(|text| has_record_structure(text, ProbeExtent::TruncatedPrefix))
}

fn has_declared_record_structure(text: &str) -> bool {
    let Some(evidence) = first_two_strict_records(text, ProbeExtent::TruncatedPrefix) else {
        return false;
    };
    if !quotes_are_well_formed(evidence) || has_blank_record(evidence) {
        return false;
    }
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(evidence.as_bytes());
    let mut records = reader.records();
    let Some(Ok(first)) = records.next() else {
        return false;
    };
    let Some(Ok(second)) = records.next() else {
        return false;
    };
    !first.is_empty() && second.len() == first.len()
}

fn has_declared_record_evidence(text: &str) -> bool {
    if has_declared_record_structure(text) {
        return true;
    }
    let Some(first_end) = text.find(['\r', '\n']) else {
        return false;
    };
    if first_end == 0 {
        return false;
    }
    let remainder = text[first_end..].trim_start_matches(['\r', '\n']);
    !remainder.is_empty()
}

fn is_header_only_csv(text: &str) -> bool {
    let Some(first_end) = text.find(['\r', '\n']) else {
        return false;
    };
    first_end > 0
        && text[first_end..].trim_matches(['\r', '\n']).is_empty()
        && quotes_are_well_formed(text)
        && !has_blank_record(text)
}

fn first_two_strict_records(text: &str, extent: ProbeExtent) -> Option<&str> {
    let mut state = QuoteState::FieldStart;
    let mut completed_records = 0_u8;
    let mut bytes = text.as_bytes().iter().copied().enumerate().peekable();
    while let Some((index, byte)) = bytes.next() {
        state = match (state, byte) {
            (QuoteState::FieldStart, b'"') => QuoteState::Quoted,
            (QuoteState::FieldStart, b',') => QuoteState::FieldStart,
            (QuoteState::FieldStart, b'\r' | b'\n')
            | (QuoteState::Unquoted, b'\r' | b'\n')
            | (QuoteState::AfterQuote, b'\r' | b'\n') => {
                completed_records = completed_records.saturating_add(1);
                let mut end = index + 1;
                if byte == b'\r' && bytes.peek().is_some_and(|(_, next)| *next == b'\n') {
                    let _ = bytes.next();
                    end += 1;
                }
                if completed_records == 2 {
                    return text.get(..end);
                }
                QuoteState::FieldStart
            }
            (QuoteState::FieldStart, _) => QuoteState::Unquoted,
            (QuoteState::Unquoted, b'"') => return None,
            (QuoteState::Unquoted, b',') => QuoteState::FieldStart,
            (QuoteState::Unquoted, _) => QuoteState::Unquoted,
            (QuoteState::Quoted, b'"') if bytes.peek().is_some_and(|(_, next)| *next == b'"') => {
                let _ = bytes.next();
                QuoteState::Quoted
            }
            (QuoteState::Quoted, b'"') => QuoteState::AfterQuote,
            (QuoteState::Quoted, _) => QuoteState::Quoted,
            (QuoteState::AfterQuote, b',') => QuoteState::FieldStart,
            (QuoteState::AfterQuote, _) => return None,
        };
    }
    if matches!(extent, ProbeExtent::CompleteSource)
        && completed_records == 1
        && !matches!(state, QuoteState::Quoted)
    {
        Some(text)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
enum QuoteState {
    FieldStart,
    Unquoted,
    Quoted,
    AfterQuote,
}

fn quotes_are_well_formed(text: &str) -> bool {
    let mut state = QuoteState::FieldStart;
    let mut bytes = text.bytes().peekable();
    while let Some(byte) = bytes.next() {
        state = match (state, byte) {
            (QuoteState::FieldStart, b'"') => QuoteState::Quoted,
            (QuoteState::FieldStart, b',' | b'\r' | b'\n') => QuoteState::FieldStart,
            (QuoteState::FieldStart, _) => QuoteState::Unquoted,
            (QuoteState::Unquoted, b'"') => return false,
            (QuoteState::Unquoted, b',' | b'\r' | b'\n') => QuoteState::FieldStart,
            (QuoteState::Unquoted, _) => QuoteState::Unquoted,
            (QuoteState::Quoted, b'"') if bytes.peek() == Some(&b'"') => {
                let _ = bytes.next();
                QuoteState::Quoted
            }
            (QuoteState::Quoted, b'"') => QuoteState::AfterQuote,
            (QuoteState::Quoted, _) => QuoteState::Quoted,
            (QuoteState::AfterQuote, b',' | b'\r' | b'\n') => QuoteState::FieldStart,
            (QuoteState::AfterQuote, _) => return false,
        };
    }
    !matches!(state, QuoteState::Quoted)
}

fn has_blank_record(text: &str) -> bool {
    let mut in_quotes = false;
    let mut record_has_content = false;
    let mut bytes = text.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if in_quotes {
            if byte == b'"' && bytes.peek() == Some(&b'"') {
                let _ = bytes.next();
            } else if byte == b'"' {
                in_quotes = false;
            }
            record_has_content = true;
        } else {
            match byte {
                b'"' => {
                    in_quotes = true;
                    record_has_content = true;
                }
                b'\r' | b'\n' => {
                    if byte == b'\r' && bytes.peek() == Some(&b'\n') {
                        let _ = bytes.next();
                    }
                    if !record_has_content {
                        return true;
                    }
                    record_has_content = false;
                }
                _ => record_has_content = true,
            }
        }
    }
    false
}

fn malformed(reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(CSV_MEDIA_TYPE),
        reason_code: String::from(reason),
    }
}

fn validation_failure(evidence: ValidationEvidence, reason: &str) -> ProcessorValidationOutput {
    match evidence {
        ValidationEvidence::DeclaredCandidateStructurallyValidated => {
            ProcessorValidationOutput::NoMatch
        }
        ValidationEvidence::StrongSignature
        | ValidationEvidence::StructuralValidation
        | ValidationEvidence::StreamingTextValidation => malformed(reason),
    }
}
