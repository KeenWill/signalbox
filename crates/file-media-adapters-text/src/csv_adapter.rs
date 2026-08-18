use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ValidationEvidence, VerifiedBlobSource,
};

use crate::{
    CSV_MEDIA_TYPE, MAX_TEXT_FAMILY_BYTES, STRUCTURED_VIEW_NAME, options_are_empty, source,
};

// Hard safety ceiling preventing one record from causing runaway allocation.
const MAX_COLUMNS: usize = 256;
// Hard safety ceiling bounding table allocation and parse latency.
const MAX_ROWS: usize = 10_000;

pub(crate) async fn probe(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorProbeOutput, ProcessorFailure> {
    let prefix = source::read_probe_prefix(source, cancellation).await?;
    let candidate = source::probe_utf8(&prefix).is_some_and(has_record_structure);
    if candidate {
        Ok(ProcessorProbeOutput::Candidate {
            media_type: String::from(CSV_MEDIA_TYPE),
            strength: ProbeStrength::StructuralCandidate,
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
    let Some(bytes) = source::read_complete(source, cancellation).await? else {
        return Ok(validation_failure(request.evidence, "source_too_large"));
    };
    let text = match source::checked_utf8(bytes) {
        Ok(text) => text,
        Err(reason) => return Ok(validation_failure(request.evidence, reason)),
    };
    let table = match parse_table(&text) {
        Ok(table) => table,
        Err(reason) => return Ok(validation_failure(request.evidence, reason)),
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
    if request.view.as_str() != STRUCTURED_VIEW_NAME || !options_are_empty(&request.options) {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    }
    let Some(bytes) = source::read_complete(source, cancellation).await? else {
        return Ok(ProcessorReadOutput::SourceTooLarge {
            maximum_bytes: MAX_TEXT_FAMILY_BYTES,
        });
    };
    let text = source::checked_utf8(bytes).map_err(|_| ProcessorFailure::Failed)?;
    let table = parse_table(&text).map_err(|_| ProcessorFailure::Failed)?;
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

fn parse_table(text: &str) -> Result<CsvTable, &'static str> {
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
    if headers.len() < 2 {
        return Err("malformed_csv");
    }
    if headers.len() > MAX_COLUMNS {
        return Err("column_limit_exceeded");
    }
    let mut rows = Vec::new();
    for record in reader.records() {
        if rows.len() == MAX_ROWS {
            return Err("row_limit_exceeded");
        }
        let record = record.map_err(|_| "malformed_csv")?;
        rows.push(record.iter().map(String::from).collect());
    }
    if rows.is_empty() {
        return Err("malformed_csv");
    }
    Ok(CsvTable { headers, rows })
}

fn has_record_structure(text: &str) -> bool {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(text.as_bytes());
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
