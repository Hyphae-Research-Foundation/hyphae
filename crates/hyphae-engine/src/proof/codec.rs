// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use hyphae_query::{
    AggregationPlan, AggregationResult, CompareOperator, Cursor, ExecutionLimits, FieldPath,
    Filter, GroupResult, Metric, MetricValue, NamedMetric, NamedMetricValue, NullPlacement, Query,
    QueryResult, Record, SortDirection, SortField, Value, validate_query,
};
use hyphae_storage::MAX_KEY_BYTES;

use super::{
    MAX_RESULT_PROOF_BYTES, ProofAnchor, ProofError, ProvenOperation, ProvenResult,
    RESULT_PROOF_FORMAT_VERSION, ResultProof,
};
use crate::{MAX_DOCUMENT_BYTES, decode_document, encode_document};

const MAGIC: [u8; 8] = *b"HYPRF001";
const HEADER_LENGTH: usize = 128;
const CHECKSUM_PREFIX_LENGTH: usize = 92;
const DIGEST_PREFIX_LENGTH: usize = 96;
const PROOF_DIGEST_OFFSET: usize = 96;
const DIGEST_DOMAIN: &[u8] = b"hyphae-result-proof-v1";

const GET_OPERATION: u8 = 1;
const QUERY_OPERATION: u8 = 2;
const ABSENT: u8 = 0;
const PRESENT: u8 = 1;
const MAX_FILTER_DEPTH: usize = 64;
const MAX_FILTER_NODES: usize = 4_096;
const MAX_QUERY_FIELDS: usize = 256;
const MAX_RESULT_ROWS: usize = 100_000;
const MAX_RESULT_GROUPS: usize = 100_000;
const MAX_RESULT_METRICS: usize = 1_024;

pub(crate) fn finalize_proof(
    anchor: ProofAnchor,
    operation: ProvenOperation,
    result: ProvenResult,
) -> Result<ResultProof, ProofError> {
    validate_models(&operation, &result)?;
    let mut proof = ResultProof {
        anchor,
        operation,
        result,
        proof_digest: [0; 32],
    };
    let encoded = encode_proof(&proof)?;
    proof.proof_digest = copy_array(&encoded[PROOF_DIGEST_OFFSET..HEADER_LENGTH]);
    Ok(proof)
}

pub(crate) fn encode_proof(proof: &ResultProof) -> Result<Vec<u8>, ProofError> {
    validate_anchor(&proof.anchor)?;
    validate_models(&proof.operation, &proof.result)?;

    let (operation_tag, request) = encode_operation(&proof.operation)?;
    let result = encode_result(&proof.result)?;
    let mut payload = Encoder::default();
    payload.byte(operation_tag);
    payload.length_u64(request.len())?;
    payload.extend(&request);
    payload.length_u64(result.len())?;
    payload.extend(&result);

    let file_length = HEADER_LENGTH
        .checked_add(payload.bytes.len())
        .ok_or(ProofError::LengthOverflow)?;
    let file_length_u64 = u64::try_from(file_length).map_err(|_| ProofError::LengthOverflow)?;
    if file_length_u64 > MAX_RESULT_PROOF_BYTES {
        return Err(ProofError::ProofLimitExceeded {
            actual: file_length_u64,
            maximum: MAX_RESULT_PROOF_BYTES,
        });
    }

    let mut header = [0_u8; HEADER_LENGTH];
    header[..8].copy_from_slice(&MAGIC);
    header[8..10].copy_from_slice(&RESULT_PROOF_FORMAT_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&0_u16.to_le_bytes());
    header[12..20].copy_from_slice(&proof.anchor.checkpoint_sequence.to_le_bytes());
    header[20..52].copy_from_slice(&proof.anchor.checkpoint_digest.unwrap_or([0; 32]));
    header[52..84].copy_from_slice(&proof.anchor.snapshot_digest);
    header[84..92].copy_from_slice(
        &u64::try_from(payload.bytes.len())
            .map_err(|_| ProofError::LengthOverflow)?
            .to_le_bytes(),
    );

    let checksum = crc32c::crc32c_append(
        crc32c::crc32c(&header[..CHECKSUM_PREFIX_LENGTH]),
        &payload.bytes,
    );
    header[92..96].copy_from_slice(&checksum.to_le_bytes());
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(&header[..DIGEST_PREFIX_LENGTH]);
    hasher.update(&payload.bytes);
    header[96..128].copy_from_slice(hasher.finalize().as_bytes());

    let mut encoded = Vec::with_capacity(file_length);
    encoded.extend_from_slice(&header);
    encoded.extend_from_slice(&payload.bytes);
    Ok(encoded)
}

pub(crate) fn decode_proof(encoded: &[u8]) -> Result<ResultProof, ProofError> {
    let encoded_length = u64::try_from(encoded.len()).map_err(|_| ProofError::LengthOverflow)?;
    if encoded_length > MAX_RESULT_PROOF_BYTES {
        return Err(ProofError::ProofLimitExceeded {
            actual: encoded_length,
            maximum: MAX_RESULT_PROOF_BYTES,
        });
    }
    if encoded.len() < HEADER_LENGTH {
        return Err(invalid("truncated header"));
    }
    if encoded[..8] != MAGIC {
        return Err(invalid("bad magic"));
    }
    let version = u16::from_le_bytes(copy_array(&encoded[8..10]));
    if version != RESULT_PROOF_FORMAT_VERSION {
        return Err(ProofError::UnsupportedVersion {
            found: version,
            supported: RESULT_PROOF_FORMAT_VERSION,
        });
    }
    if u16::from_le_bytes(copy_array(&encoded[10..12])) != 0 {
        return Err(invalid("unsupported flags"));
    }
    let payload_length = usize::try_from(u64::from_le_bytes(copy_array(&encoded[84..92])))
        .map_err(|_| ProofError::LengthOverflow)?;
    let expected_length = HEADER_LENGTH
        .checked_add(payload_length)
        .ok_or(ProofError::LengthOverflow)?;
    if encoded.len() != expected_length {
        return Err(invalid("file length mismatch"));
    }
    let payload = &encoded[HEADER_LENGTH..];
    let expected_checksum = u32::from_le_bytes(copy_array(&encoded[92..96]));
    let actual_checksum =
        crc32c::crc32c_append(crc32c::crc32c(&encoded[..CHECKSUM_PREFIX_LENGTH]), payload);
    if actual_checksum != expected_checksum {
        return Err(ProofError::ChecksumMismatch);
    }
    let expected_digest: [u8; 32] = copy_array(&encoded[96..128]);
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(&encoded[..DIGEST_PREFIX_LENGTH]);
    hasher.update(payload);
    if *hasher.finalize().as_bytes() != expected_digest {
        return Err(ProofError::DigestMismatch);
    }

    let checkpoint_sequence = u64::from_le_bytes(copy_array(&encoded[12..20]));
    let raw_checkpoint_digest = copy_array(&encoded[20..52]);
    let checkpoint_digest = if checkpoint_sequence == 0 {
        if raw_checkpoint_digest != [0; 32] {
            return Err(invalid("empty checkpoint has a digest"));
        }
        None
    } else {
        Some(raw_checkpoint_digest)
    };
    let anchor = ProofAnchor {
        checkpoint_sequence,
        checkpoint_digest,
        snapshot_digest: copy_array(&encoded[52..84]),
    };
    validate_anchor(&anchor)?;

    let mut payload = Decoder::new(payload);
    let operation_tag = payload.byte()?;
    let request_length = payload.length_u64()?;
    let request = payload.take(request_length)?;
    let result_length = payload.length_u64()?;
    let result = payload.take(result_length)?;
    payload.finish()?;

    let operation = decode_operation(operation_tag, request)?;
    let result = decode_result(operation_tag, result)?;
    validate_models(&operation, &result)?;
    Ok(ResultProof {
        anchor,
        operation,
        result,
        proof_digest: expected_digest,
    })
}

fn validate_anchor(anchor: &ProofAnchor) -> Result<(), ProofError> {
    if (anchor.checkpoint_sequence == 0) != anchor.checkpoint_digest.is_none() {
        return Err(invalid("noncanonical checkpoint identity"));
    }
    Ok(())
}

fn validate_operation_result(
    operation: &ProvenOperation,
    result: &ProvenResult,
) -> Result<(), ProofError> {
    if matches!(
        (operation, result),
        (ProvenOperation::Get { .. }, ProvenResult::Get(_))
            | (ProvenOperation::Query(_), ProvenResult::Query(_))
    ) {
        Ok(())
    } else {
        Err(ProofError::OperationResultMismatch)
    }
}

fn validate_models(operation: &ProvenOperation, result: &ProvenResult) -> Result<(), ProofError> {
    validate_operation_result(operation, result)?;
    match (operation, result) {
        (ProvenOperation::Get { key }, ProvenResult::Get(record)) => {
            if key.is_empty() || key.len() > MAX_KEY_BYTES {
                return Err(invalid("invalid key length"));
            }
            if record.as_ref().is_some_and(|record| &record.key != key) {
                return Err(invalid("get result key differs from request"));
            }
        }
        (ProvenOperation::Query(query), ProvenResult::Query(result)) => {
            validate_query(query, &proof_validation_limits())?;
            if result.rows.len() > MAX_RESULT_ROWS {
                return Err(invalid("result rows exceed proof bound"));
            }
            if result
                .aggregation
                .as_ref()
                .is_some_and(|aggregation| aggregation.groups.len() > MAX_RESULT_GROUPS)
            {
                return Err(invalid("aggregation groups exceed proof bound"));
            }
        }
        _ => return Err(ProofError::OperationResultMismatch),
    }
    Ok(())
}

fn proof_validation_limits() -> ExecutionLimits {
    ExecutionLimits {
        max_scanned_records: u64::MAX,
        max_matched_records: u64::MAX,
        max_returned_records: MAX_RESULT_ROWS,
        max_groups: MAX_RESULT_GROUPS,
        max_filter_nodes: MAX_FILTER_NODES,
        max_filter_depth: MAX_FILTER_DEPTH,
        max_sort_fields: MAX_QUERY_FIELDS,
        max_group_fields: MAX_QUERY_FIELDS,
        max_metrics: MAX_RESULT_METRICS,
        timeout: Duration::MAX,
    }
}

fn encode_operation(operation: &ProvenOperation) -> Result<(u8, Vec<u8>), ProofError> {
    let mut encoder = Encoder::default();
    let tag = match operation {
        ProvenOperation::Get { key } => {
            encoder.key(key)?;
            GET_OPERATION
        }
        ProvenOperation::Query(query) => {
            encoder.query(query)?;
            QUERY_OPERATION
        }
    };
    Ok((tag, encoder.bytes))
}

fn decode_operation(tag: u8, encoded: &[u8]) -> Result<ProvenOperation, ProofError> {
    let mut decoder = Decoder::new(encoded);
    let operation = match tag {
        GET_OPERATION => ProvenOperation::Get {
            key: decoder.key()?,
        },
        QUERY_OPERATION => ProvenOperation::Query(decoder.query()?),
        _ => return Err(invalid("unknown operation tag")),
    };
    decoder.finish()?;
    Ok(operation)
}

fn encode_result(result: &ProvenResult) -> Result<Vec<u8>, ProofError> {
    let mut encoder = Encoder::default();
    match result {
        ProvenResult::Get(record) => encoder.optional_record(record.as_ref())?,
        ProvenResult::Query(result) => encoder.query_result(result)?,
    }
    Ok(encoder.bytes)
}

fn decode_result(tag: u8, encoded: &[u8]) -> Result<ProvenResult, ProofError> {
    let mut decoder = Decoder::new(encoded);
    let result = match tag {
        GET_OPERATION => ProvenResult::Get(decoder.optional_record()?),
        QUERY_OPERATION => ProvenResult::Query(decoder.query_result()?),
        _ => return Err(invalid("unknown operation tag")),
    };
    decoder.finish()?;
    Ok(result)
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.extend(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.extend(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.extend(&value.to_le_bytes());
    }

    fn i128(&mut self, value: i128) {
        self.extend(&value.to_le_bytes());
    }

    fn extend(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn length_u16(&mut self, value: usize) -> Result<(), ProofError> {
        self.u16(u16::try_from(value).map_err(|_| ProofError::LengthOverflow)?);
        Ok(())
    }

    fn length_u32(&mut self, value: usize) -> Result<(), ProofError> {
        self.u32(u32::try_from(value).map_err(|_| ProofError::LengthOverflow)?);
        Ok(())
    }

    fn length_u64(&mut self, value: usize) -> Result<(), ProofError> {
        self.u64(u64::try_from(value).map_err(|_| ProofError::LengthOverflow)?);
        Ok(())
    }

    fn key(&mut self, key: &[u8]) -> Result<(), ProofError> {
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Err(invalid("invalid key length"));
        }
        self.length_u32(key.len())?;
        self.extend(key);
        Ok(())
    }

    fn string(&mut self, value: &str) -> Result<(), ProofError> {
        self.length_u32(value.len())?;
        self.extend(value.as_bytes());
        Ok(())
    }

    fn document(&mut self, value: &Value) -> Result<(), ProofError> {
        let document = encode_document(value)?;
        self.length_u64(document.len())?;
        self.extend(&document);
        Ok(())
    }

    fn optional_document(&mut self, value: Option<&Value>) -> Result<(), ProofError> {
        match value {
            None => self.byte(ABSENT),
            Some(value) => {
                self.byte(PRESENT);
                self.document(value)?;
            }
        }
        Ok(())
    }

    fn field_path(&mut self, path: &FieldPath) -> Result<(), ProofError> {
        self.length_u16(path.segments().len())?;
        for segment in path.segments() {
            if segment.is_empty() {
                return Err(invalid("field path contains an empty segment"));
            }
            self.string(segment)?;
        }
        Ok(())
    }

    fn filter(&mut self, filter: &Filter) -> Result<(), ProofError> {
        match filter {
            Filter::MatchAll => self.byte(1),
            Filter::Exists(path) => {
                self.byte(2);
                self.field_path(path)?;
            }
            Filter::Compare {
                path,
                operator,
                value,
            } => {
                self.byte(3);
                self.field_path(path)?;
                self.byte(compare_operator_tag(*operator));
                self.document(value)?;
            }
            Filter::Prefix { path, prefix } => {
                self.byte(4);
                self.field_path(path)?;
                self.document(prefix)?;
            }
            Filter::Contains { path, needle } => {
                self.byte(5);
                self.field_path(path)?;
                self.document(needle)?;
            }
            Filter::All(children) => {
                self.byte(6);
                self.length_u16(children.len())?;
                for child in children {
                    self.filter(child)?;
                }
            }
            Filter::Any(children) => {
                self.byte(7);
                self.length_u16(children.len())?;
                for child in children {
                    self.filter(child)?;
                }
            }
            Filter::Not(child) => {
                self.byte(8);
                self.filter(child)?;
            }
        }
        Ok(())
    }

    fn sort_field(&mut self, field: &SortField) -> Result<(), ProofError> {
        self.field_path(&field.path)?;
        self.byte(match field.direction {
            SortDirection::Ascending => 1,
            SortDirection::Descending => 2,
        });
        self.byte(match field.nulls {
            NullPlacement::First => 1,
            NullPlacement::Last => 2,
        });
        Ok(())
    }

    fn cursor(&mut self, cursor: &Cursor) -> Result<(), ProofError> {
        self.length_u16(cursor.sort_values.len())?;
        for value in &cursor.sort_values {
            self.optional_document(value.as_ref())?;
        }
        self.key(&cursor.key)
    }

    fn optional_cursor(&mut self, cursor: Option<&Cursor>) -> Result<(), ProofError> {
        match cursor {
            None => self.byte(ABSENT),
            Some(cursor) => {
                self.byte(PRESENT);
                self.cursor(cursor)?;
            }
        }
        Ok(())
    }

    fn metric(&mut self, metric: &NamedMetric) -> Result<(), ProofError> {
        self.string(&metric.name)?;
        match &metric.metric {
            Metric::Count => self.byte(1),
            Metric::Sum(path) => {
                self.byte(2);
                self.field_path(path)?;
            }
            Metric::Min(path) => {
                self.byte(3);
                self.field_path(path)?;
            }
            Metric::Max(path) => {
                self.byte(4);
                self.field_path(path)?;
            }
        }
        Ok(())
    }

    fn aggregation_plan(&mut self, plan: &AggregationPlan) -> Result<(), ProofError> {
        self.length_u16(plan.group_by.len())?;
        for path in &plan.group_by {
            self.field_path(path)?;
        }
        self.length_u16(plan.metrics.len())?;
        for metric in &plan.metrics {
            self.metric(metric)?;
        }
        Ok(())
    }

    fn optional_aggregation_plan(
        &mut self,
        plan: Option<&AggregationPlan>,
    ) -> Result<(), ProofError> {
        match plan {
            None => self.byte(ABSENT),
            Some(plan) => {
                self.byte(PRESENT);
                self.aggregation_plan(plan)?;
            }
        }
        Ok(())
    }

    fn query(&mut self, query: &Query) -> Result<(), ProofError> {
        self.filter(&query.filter)?;
        self.length_u16(query.sort.len())?;
        for field in &query.sort {
            self.sort_field(field)?;
        }
        self.optional_cursor(query.cursor.as_ref())?;
        self.u64(u64::try_from(query.limit).map_err(|_| ProofError::LengthOverflow)?);
        self.optional_aggregation_plan(query.aggregation.as_ref())
    }

    fn record(&mut self, record: &Record) -> Result<(), ProofError> {
        self.key(&record.key)?;
        self.document(&record.value)
    }

    fn optional_record(&mut self, record: Option<&Record>) -> Result<(), ProofError> {
        match record {
            None => self.byte(ABSENT),
            Some(record) => {
                self.byte(PRESENT);
                self.record(record)?;
            }
        }
        Ok(())
    }

    fn metric_value(&mut self, metric: &NamedMetricValue) -> Result<(), ProofError> {
        self.string(&metric.name)?;
        match &metric.value {
            MetricValue::Count(value) => {
                self.byte(1);
                self.u64(*value);
            }
            MetricValue::Integer(value) => {
                self.byte(2);
                match value {
                    None => self.byte(ABSENT),
                    Some(value) => {
                        self.byte(PRESENT);
                        self.i128(*value);
                    }
                }
            }
            MetricValue::Value(value) => {
                self.byte(3);
                self.optional_document(value.as_ref())?;
            }
        }
        Ok(())
    }

    fn aggregation_result(&mut self, result: &AggregationResult) -> Result<(), ProofError> {
        self.byte(u8::from(result.grouped));
        self.length_u32(result.groups.len())?;
        for group in &result.groups {
            self.length_u16(group.key.len())?;
            for value in &group.key {
                self.optional_document(value.as_ref())?;
            }
            self.length_u16(group.metrics.len())?;
            for metric in &group.metrics {
                self.metric_value(metric)?;
            }
        }
        Ok(())
    }

    fn optional_aggregation_result(
        &mut self,
        result: Option<&AggregationResult>,
    ) -> Result<(), ProofError> {
        match result {
            None => self.byte(ABSENT),
            Some(result) => {
                self.byte(PRESENT);
                self.aggregation_result(result)?;
            }
        }
        Ok(())
    }

    fn query_result(&mut self, result: &QueryResult) -> Result<(), ProofError> {
        self.length_u32(result.rows.len())?;
        for row in &result.rows {
            self.record(row)?;
        }
        self.optional_cursor(result.next_cursor.as_ref())?;
        self.optional_aggregation_result(result.aggregation.as_ref())?;
        self.u64(result.scanned_records);
        self.u64(result.matched_records);
        Ok(())
    }
}

struct Decoder<'bytes> {
    bytes: &'bytes [u8],
    position: usize,
}

impl<'bytes> Decoder<'bytes> {
    fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'bytes [u8], ProofError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(ProofError::LengthOverflow)?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid("truncated payload"))?;
        self.position = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, ProofError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProofError> {
        Ok(u16::from_le_bytes(copy_array(self.take(2)?)))
    }

    fn u32(&mut self) -> Result<u32, ProofError> {
        Ok(u32::from_le_bytes(copy_array(self.take(4)?)))
    }

    fn u64(&mut self) -> Result<u64, ProofError> {
        Ok(u64::from_le_bytes(copy_array(self.take(8)?)))
    }

    fn i128(&mut self) -> Result<i128, ProofError> {
        Ok(i128::from_le_bytes(copy_array(self.take(16)?)))
    }

    fn length_u64(&mut self) -> Result<usize, ProofError> {
        usize::try_from(self.u64()?).map_err(|_| ProofError::LengthOverflow)
    }

    fn key(&mut self) -> Result<Vec<u8>, ProofError> {
        let length = usize::try_from(self.u32()?).map_err(|_| ProofError::LengthOverflow)?;
        if length == 0 || length > MAX_KEY_BYTES {
            return Err(invalid("invalid key length"));
        }
        Ok(self.take(length)?.to_vec())
    }

    fn string(&mut self) -> Result<String, ProofError> {
        let length = usize::try_from(self.u32()?).map_err(|_| ProofError::LengthOverflow)?;
        let encoded = self.take(length)?;
        String::from_utf8(encoded.to_vec()).map_err(|_| invalid("invalid UTF-8"))
    }

    fn document(&mut self) -> Result<Value, ProofError> {
        let length = self.length_u64()?;
        let maximum = MAX_DOCUMENT_BYTES
            .checked_add(56)
            .ok_or(ProofError::LengthOverflow)?;
        if length > maximum {
            return Err(invalid("document exceeds proof bound"));
        }
        Ok(decode_document(self.take(length)?)?)
    }

    fn optional_document(&mut self) -> Result<Option<Value>, ProofError> {
        match self.byte()? {
            ABSENT => Ok(None),
            PRESENT => Ok(Some(self.document()?)),
            _ => Err(invalid("invalid optional value tag")),
        }
    }

    fn field_path(&mut self) -> Result<FieldPath, ProofError> {
        let count = usize::from(self.u16()?);
        if count > MAX_QUERY_FIELDS {
            return Err(invalid("field path exceeds proof bound"));
        }
        let mut segments = Vec::with_capacity(count);
        for _ in 0..count {
            let segment = self.string()?;
            if segment.is_empty() {
                return Err(invalid("field path contains an empty segment"));
            }
            segments.push(segment);
        }
        Ok(FieldPath::new(segments))
    }

    fn filter(&mut self) -> Result<Filter, ProofError> {
        let mut nodes = 0_usize;
        self.filter_at_depth(0, &mut nodes)
    }

    fn filter_at_depth(&mut self, depth: usize, nodes: &mut usize) -> Result<Filter, ProofError> {
        if depth > MAX_FILTER_DEPTH {
            return Err(invalid("filter exceeds proof depth bound"));
        }
        *nodes = nodes.checked_add(1).ok_or(ProofError::LengthOverflow)?;
        if *nodes > MAX_FILTER_NODES {
            return Err(invalid("filter exceeds proof node bound"));
        }
        match self.byte()? {
            1 => Ok(Filter::MatchAll),
            2 => Ok(Filter::Exists(self.field_path()?)),
            3 => Ok(Filter::Compare {
                path: self.field_path()?,
                operator: decode_compare_operator(self.byte()?)?,
                value: self.document()?,
            }),
            4 => Ok(Filter::Prefix {
                path: self.field_path()?,
                prefix: self.document()?,
            }),
            5 => Ok(Filter::Contains {
                path: self.field_path()?,
                needle: self.document()?,
            }),
            6 => Ok(Filter::All(self.filter_children(depth, nodes)?)),
            7 => Ok(Filter::Any(self.filter_children(depth, nodes)?)),
            8 => Ok(Filter::Not(Box::new(
                self.filter_at_depth(depth.saturating_add(1), nodes)?,
            ))),
            _ => Err(invalid("unknown filter tag")),
        }
    }

    fn filter_children(
        &mut self,
        depth: usize,
        nodes: &mut usize,
    ) -> Result<Vec<Filter>, ProofError> {
        let count = usize::from(self.u16()?);
        if count > MAX_FILTER_NODES {
            return Err(invalid("filter children exceed proof bound"));
        }
        let mut children = Vec::with_capacity(count);
        for _ in 0..count {
            children.push(self.filter_at_depth(depth.saturating_add(1), nodes)?);
        }
        Ok(children)
    }

    fn sort_field(&mut self) -> Result<SortField, ProofError> {
        let path = self.field_path()?;
        let direction = match self.byte()? {
            1 => SortDirection::Ascending,
            2 => SortDirection::Descending,
            _ => return Err(invalid("unknown sort direction")),
        };
        let nulls = match self.byte()? {
            1 => NullPlacement::First,
            2 => NullPlacement::Last,
            _ => return Err(invalid("unknown null placement")),
        };
        Ok(SortField {
            path,
            direction,
            nulls,
        })
    }

    fn cursor(&mut self) -> Result<Cursor, ProofError> {
        let count = usize::from(self.u16()?);
        if count > MAX_QUERY_FIELDS {
            return Err(invalid("cursor exceeds proof bound"));
        }
        let mut sort_values = Vec::with_capacity(count);
        for _ in 0..count {
            sort_values.push(self.optional_document()?);
        }
        Ok(Cursor {
            sort_values,
            key: self.key()?,
        })
    }

    fn optional_cursor(&mut self) -> Result<Option<Cursor>, ProofError> {
        match self.byte()? {
            ABSENT => Ok(None),
            PRESENT => Ok(Some(self.cursor()?)),
            _ => Err(invalid("invalid optional cursor tag")),
        }
    }

    fn metric(&mut self) -> Result<NamedMetric, ProofError> {
        let name = self.string()?;
        let metric = match self.byte()? {
            1 => Metric::Count,
            2 => Metric::Sum(self.field_path()?),
            3 => Metric::Min(self.field_path()?),
            4 => Metric::Max(self.field_path()?),
            _ => return Err(invalid("unknown metric tag")),
        };
        Ok(NamedMetric { name, metric })
    }

    fn aggregation_plan(&mut self) -> Result<AggregationPlan, ProofError> {
        let group_count = usize::from(self.u16()?);
        if group_count > MAX_QUERY_FIELDS {
            return Err(invalid("aggregation group fields exceed proof bound"));
        }
        let mut group_by = Vec::with_capacity(group_count);
        for _ in 0..group_count {
            group_by.push(self.field_path()?);
        }
        let metric_count = usize::from(self.u16()?);
        if metric_count > MAX_RESULT_METRICS {
            return Err(invalid("aggregation metrics exceed proof bound"));
        }
        let mut metrics = Vec::with_capacity(metric_count);
        for _ in 0..metric_count {
            metrics.push(self.metric()?);
        }
        Ok(AggregationPlan { group_by, metrics })
    }

    fn optional_aggregation_plan(&mut self) -> Result<Option<AggregationPlan>, ProofError> {
        match self.byte()? {
            ABSENT => Ok(None),
            PRESENT => Ok(Some(self.aggregation_plan()?)),
            _ => Err(invalid("invalid optional aggregation plan tag")),
        }
    }

    fn query(&mut self) -> Result<Query, ProofError> {
        let filter = self.filter()?;
        let sort_count = usize::from(self.u16()?);
        if sort_count > MAX_QUERY_FIELDS {
            return Err(invalid("sort fields exceed proof bound"));
        }
        let mut sort = Vec::with_capacity(sort_count);
        for _ in 0..sort_count {
            sort.push(self.sort_field()?);
        }
        let cursor = self.optional_cursor()?;
        let limit = usize::try_from(self.u64()?).map_err(|_| ProofError::LengthOverflow)?;
        let aggregation = self.optional_aggregation_plan()?;
        Ok(Query {
            filter,
            sort,
            cursor,
            limit,
            aggregation,
        })
    }

    fn record(&mut self) -> Result<Record, ProofError> {
        Ok(Record {
            key: self.key()?,
            value: self.document()?,
        })
    }

    fn optional_record(&mut self) -> Result<Option<Record>, ProofError> {
        match self.byte()? {
            ABSENT => Ok(None),
            PRESENT => Ok(Some(self.record()?)),
            _ => Err(invalid("invalid optional record tag")),
        }
    }

    fn metric_value(&mut self) -> Result<NamedMetricValue, ProofError> {
        let name = self.string()?;
        let value = match self.byte()? {
            1 => MetricValue::Count(self.u64()?),
            2 => MetricValue::Integer(match self.byte()? {
                ABSENT => None,
                PRESENT => Some(self.i128()?),
                _ => return Err(invalid("invalid optional integer tag")),
            }),
            3 => MetricValue::Value(self.optional_document()?),
            _ => return Err(invalid("unknown metric value tag")),
        };
        Ok(NamedMetricValue { name, value })
    }

    fn aggregation_result(&mut self) -> Result<AggregationResult, ProofError> {
        let grouped = match self.byte()? {
            0 => false,
            1 => true,
            _ => return Err(invalid("invalid grouped tag")),
        };
        let group_count = usize::try_from(self.u32()?).map_err(|_| ProofError::LengthOverflow)?;
        if group_count > MAX_RESULT_GROUPS {
            return Err(invalid("aggregation groups exceed proof bound"));
        }
        let mut groups = Vec::with_capacity(group_count);
        for _ in 0..group_count {
            let key_count = usize::from(self.u16()?);
            if key_count > MAX_QUERY_FIELDS {
                return Err(invalid("aggregation key exceeds proof bound"));
            }
            let mut key = Vec::with_capacity(key_count);
            for _ in 0..key_count {
                key.push(self.optional_document()?);
            }
            let metric_count = usize::from(self.u16()?);
            if metric_count > MAX_RESULT_METRICS {
                return Err(invalid("result metrics exceed proof bound"));
            }
            let mut metrics = Vec::with_capacity(metric_count);
            for _ in 0..metric_count {
                metrics.push(self.metric_value()?);
            }
            groups.push(GroupResult { key, metrics });
        }
        Ok(AggregationResult { grouped, groups })
    }

    fn optional_aggregation_result(&mut self) -> Result<Option<AggregationResult>, ProofError> {
        match self.byte()? {
            ABSENT => Ok(None),
            PRESENT => Ok(Some(self.aggregation_result()?)),
            _ => Err(invalid("invalid optional aggregation result tag")),
        }
    }

    fn query_result(&mut self) -> Result<QueryResult, ProofError> {
        let row_count = usize::try_from(self.u32()?).map_err(|_| ProofError::LengthOverflow)?;
        if row_count > MAX_RESULT_ROWS {
            return Err(invalid("result rows exceed proof bound"));
        }
        let mut rows = Vec::with_capacity(row_count);
        for _ in 0..row_count {
            rows.push(self.record()?);
        }
        Ok(QueryResult {
            rows,
            next_cursor: self.optional_cursor()?,
            aggregation: self.optional_aggregation_result()?,
            scanned_records: self.u64()?,
            matched_records: self.u64()?,
        })
    }

    fn finish(&self) -> Result<(), ProofError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid("trailing payload bytes"))
        }
    }
}

fn compare_operator_tag(operator: CompareOperator) -> u8 {
    match operator {
        CompareOperator::Equal => 1,
        CompareOperator::NotEqual => 2,
        CompareOperator::Less => 3,
        CompareOperator::LessOrEqual => 4,
        CompareOperator::Greater => 5,
        CompareOperator::GreaterOrEqual => 6,
    }
}

fn decode_compare_operator(tag: u8) -> Result<CompareOperator, ProofError> {
    match tag {
        1 => Ok(CompareOperator::Equal),
        2 => Ok(CompareOperator::NotEqual),
        3 => Ok(CompareOperator::Less),
        4 => Ok(CompareOperator::LessOrEqual),
        5 => Ok(CompareOperator::Greater),
        6 => Ok(CompareOperator::GreaterOrEqual),
        _ => Err(invalid("unknown compare operator")),
    }
}

fn invalid(reason: &'static str) -> ProofError {
    ProofError::Invalid { reason }
}

fn copy_array<const N: usize>(source: &[u8]) -> [u8; N] {
    let mut output = [0_u8; N];
    output.copy_from_slice(source);
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hyphae_query::{
        AggregationPlan, AggregationResult, CompareOperator, Cursor, FieldPath, Filter,
        GroupResult, Metric, MetricValue, NamedMetric, NamedMetricValue, NullPlacement, Query,
        QueryResult, Record, SortDirection, SortField, Value,
    };

    use super::{
        CHECKSUM_PREFIX_LENGTH, HEADER_LENGTH, copy_array, decode_proof, encode_proof,
        finalize_proof,
    };
    use crate::{ProofAnchor, ProofError, ProvenOperation, ProvenResult};

    fn query() -> Query {
        Query {
            filter: Filter::All(vec![
                Filter::Exists(FieldPath::field("score")),
                Filter::Not(Box::new(Filter::Compare {
                    path: FieldPath::field("deleted"),
                    operator: CompareOperator::Equal,
                    value: Value::Boolean(true),
                })),
            ]),
            sort: vec![SortField {
                path: FieldPath::field("score"),
                direction: SortDirection::Descending,
                nulls: NullPlacement::Last,
            }],
            cursor: Some(Cursor {
                sort_values: vec![Some(Value::Integer(10))],
                key: b"before".to_vec(),
            }),
            limit: 5,
            aggregation: Some(AggregationPlan {
                group_by: vec![FieldPath::field("group")],
                metrics: vec![NamedMetric {
                    name: "count".to_owned(),
                    metric: Metric::Count,
                }],
            }),
        }
    }

    fn record() -> Record {
        Record::new(
            b"row",
            Value::Object(BTreeMap::from([("score".to_owned(), Value::Integer(11))])),
        )
    }

    #[test]
    fn proof_codec_round_trips_complete_query_and_result() -> Result<(), ProofError> {
        let proof = finalize_proof(
            ProofAnchor {
                checkpoint_sequence: 7,
                checkpoint_digest: Some([2; 32]),
                snapshot_digest: [3; 32],
            },
            ProvenOperation::Query(query()),
            ProvenResult::Query(QueryResult {
                rows: vec![record()],
                next_cursor: None,
                aggregation: Some(AggregationResult {
                    grouped: true,
                    groups: vec![GroupResult {
                        key: vec![Some(Value::String("x".to_owned()))],
                        metrics: vec![NamedMetricValue {
                            name: "count".to_owned(),
                            value: MetricValue::Count(1),
                        }],
                    }],
                }),
                scanned_records: 3,
                matched_records: 1,
            }),
        )?;
        let encoded = encode_proof(&proof)?;
        assert_eq!(decode_proof(&encoded)?, proof);
        assert_eq!(proof.proof_digest(), copy_array(&encoded[96..128]));
        Ok(())
    }

    #[test]
    fn proof_codec_round_trips_get_and_detects_bit_flip() -> Result<(), ProofError> {
        let proof = finalize_proof(
            ProofAnchor {
                checkpoint_sequence: 0,
                checkpoint_digest: None,
                snapshot_digest: [4; 32],
            },
            ProvenOperation::Get {
                key: b"missing".to_vec(),
            },
            ProvenResult::Get(None),
        )?;
        let mut encoded = encode_proof(&proof)?;
        assert_eq!(decode_proof(&encoded)?, proof);
        let last = encoded.len().saturating_sub(1);
        encoded[last] ^= 1;
        assert!(matches!(
            decode_proof(&encoded),
            Err(ProofError::ChecksumMismatch)
        ));

        let mut digest_tamper = encode_proof(&proof)?;
        let last = digest_tamper.len().saturating_sub(1);
        digest_tamper[last] ^= 1;
        let checksum = crc32c::crc32c_append(
            crc32c::crc32c(&digest_tamper[..CHECKSUM_PREFIX_LENGTH]),
            &digest_tamper[HEADER_LENGTH..],
        );
        digest_tamper[92..96].copy_from_slice(&checksum.to_le_bytes());
        assert!(matches!(
            decode_proof(&digest_tamper),
            Err(ProofError::DigestMismatch)
        ));
        Ok(())
    }
}
