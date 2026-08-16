use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, VerifiedBlobSource,
};

use crate::{CSV_MEDIA_TYPE, MAX_TEXT_FAMILY_BYTES, options_are_empty, source};

const MAX_COLUMNS: usize = 256;
const MAX_ROWS: usize = 10_000;

pub(crate) async fn probe(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorProbeOutput, ProcessorFailure> {
    let prefix = source::read_probe_prefix(source, cancellation).await?;
    let candidate = std::str::from_utf8(&prefix)
        .ok()
        .is_some_and(|text| text.contains(',') && text.contains(['\n', '\r']));
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
        return Ok(malformed("source_too_large"));
    };
    let text = match source::checked_utf8(bytes) {
        Ok(text) => text,
        Err(reason) => return Ok(malformed(reason)),
    };
    let table = match parse_table(&text) {
        Ok(table) => table,
        Err(reason) => return Ok(malformed(reason)),
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
    if request.view.as_str() != "structured" || !options_are_empty(&request.options) {
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
    if !quotes_are_closed(text) {
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

fn quotes_are_closed(text: &str) -> bool {
    let mut quoted = false;
    let mut bytes = text.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte != b'"' {
            continue;
        }
        if quoted && bytes.peek() == Some(&b'"') {
            let _ = bytes.next();
        } else {
            quoted = !quoted;
        }
    }
    !quoted
}

fn malformed(reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(CSV_MEDIA_TYPE),
        reason_code: String::from(reason),
    }
}
