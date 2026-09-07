use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PresentTokenTotal {
    tokens: u128,
    pub(super) present_calls: u64,
}

impl PresentTokenTotal {
    fn add(
        &mut self,
        value: Option<signalbox_process_protocol::CanonicalU64>,
    ) -> Result<(), ClientError> {
        let Some(value) = value else {
            return Ok(());
        };
        self.tokens = self
            .tokens
            .checked_add(u128::from(value.value()))
            .ok_or(ClientError::Protocol("token usage total overflowed"))?;
        self.present_calls = self
            .present_calls
            .checked_add(1)
            .ok_or(ClientError::Protocol("token usage coverage overflowed"))?;
        Ok(())
    }

    pub(super) fn label(self) -> String {
        if self.present_calls == 0 {
            String::from("unreported")
        } else {
            self.tokens.to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct TokenUsageTotal {
    pub(super) terminal_calls: u64,
    pub(super) input: PresentTokenTotal,
    pub(super) output: PresentTokenTotal,
    pub(super) cache_creation_input: PresentTokenTotal,
    pub(super) cache_read_input: PresentTokenTotal,
}

impl TokenUsageTotal {
    fn add(
        &mut self,
        usage: signalbox_process_protocol::ModelCallTokenUsage,
    ) -> Result<(), ClientError> {
        self.terminal_calls = self
            .terminal_calls
            .checked_add(1)
            .ok_or(ClientError::Protocol(
                "terminal model-call count overflowed",
            ))?;
        self.input.add(usage.input_tokens)?;
        self.output.add(usage.output_tokens)?;
        self.cache_creation_input
            .add(usage.cache_creation_input_tokens)?;
        self.cache_read_input.add(usage.cache_read_input_tokens)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct CostAggregateKey {
    pub(super) provenance: UsageProvenance,
    pub(super) label: ModelCallCostLabel,
    pub(super) rate_version: String,
}

const COST_KEY_WIDTH: usize = 2 + MAX_RATE_VERSION_UTF8_BYTES;
const COST_TOTAL_WIDTH: usize = 16 + 8;
const COST_SLOT_WIDTH: usize = 1 + COST_KEY_WIDTH + COST_TOTAL_WIDTH;

pub(crate) struct DiskCostTotals {
    file: File,
    len: u64,
    pub(super) capacity: u64,
}

impl DiskCostTotals {
    fn new() -> io::Result<Self> {
        Self::with_capacity(16)
    }

    pub(super) fn with_capacity(capacity: u64) -> io::Result<Self> {
        let file = tempfile::tempfile()?;
        file.set_len(cost_slot_offset(capacity)?)?;
        Ok(Self {
            file,
            len: 0,
            capacity,
        })
    }

    pub(super) fn add(
        &mut self,
        key: &CostAggregateKey,
        amount: Decimal,
    ) -> Result<(), ClientError> {
        let next_len = self
            .len
            .checked_add(1)
            .ok_or(ClientError::Protocol("cost aggregate count overflowed"))?;
        if next_len
            .checked_mul(10)
            .is_none_or(|scaled| scaled >= self.capacity.saturating_mul(7))
        {
            self.grow()?;
        }
        let encoded = encode_cost_key(key);
        let start = stable_cost_hash(&encoded) % self.capacity;
        let mut candidate = [0_u8; COST_KEY_WIDTH];
        for displacement in 0..self.capacity {
            let index = (start + displacement) % self.capacity;
            let Some(mut total) = self.read_slot(index, &mut candidate)? else {
                self.write_slot(
                    index,
                    &encoded,
                    CostTotal {
                        amount_usd: amount,
                        calls: 1,
                    },
                )?;
                self.len = next_len;
                return Ok(());
            };
            if candidate == encoded {
                let next_amount = total
                    .amount_usd
                    .checked_add(amount)
                    .ok_or(ClientError::Protocol("dollar cost total overflowed"))?;
                if next_amount.checked_sub(total.amount_usd) != Some(amount)
                    || next_amount.checked_sub(amount) != Some(total.amount_usd)
                {
                    return Err(ClientError::Protocol("dollar cost total was inexact"));
                }
                total.amount_usd = next_amount;
                total.calls = total
                    .calls
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("dollar cost coverage overflowed"))?;
                self.write_slot(index, &encoded, total)?;
                return Ok(());
            }
        }
        Err(ClientError::Io(io::Error::other(
            "disk cost aggregate was unexpectedly full",
        )))
    }

    fn grow(&mut self) -> io::Result<()> {
        let new_capacity = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("disk cost capacity overflowed"))?;
        let mut replacement = Self::with_capacity(new_capacity)?;
        let mut key = [0_u8; COST_KEY_WIDTH];
        for index in 0..self.capacity {
            if let Some(total) = self.read_slot(index, &mut key)? {
                replacement.insert_stored(key, total)?;
            }
        }
        *self = replacement;
        Ok(())
    }

    fn insert_stored(&mut self, key: [u8; COST_KEY_WIDTH], total: CostTotal) -> io::Result<()> {
        let start = stable_cost_hash(&key) % self.capacity;
        let mut candidate = [0_u8; COST_KEY_WIDTH];
        for displacement in 0..self.capacity {
            let index = (start + displacement) % self.capacity;
            if self.read_slot(index, &mut candidate)?.is_none() {
                self.write_slot(index, &key, total)?;
                self.len = self
                    .len
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("disk cost count overflowed"))?;
                return Ok(());
            }
        }
        Err(io::Error::other(
            "replacement disk cost aggregate was unexpectedly full",
        ))
    }

    #[cfg(test)]
    pub(super) fn get(&mut self, key: &CostAggregateKey) -> io::Result<Option<CostTotal>> {
        let encoded = encode_cost_key(key);
        let start = stable_cost_hash(&encoded) % self.capacity;
        let mut candidate = [0_u8; COST_KEY_WIDTH];
        for displacement in 0..self.capacity {
            let index = (start + displacement) % self.capacity;
            let Some(total) = self.read_slot(index, &mut candidate)? else {
                return Ok(None);
            };
            if candidate == encoded {
                return Ok(Some(total));
            }
        }
        Ok(None)
    }

    pub(super) fn entry_at(
        &mut self,
        index: u64,
    ) -> io::Result<Option<(CostAggregateKey, CostTotal)>> {
        let mut encoded = [0_u8; COST_KEY_WIDTH];
        self.read_slot(index, &mut encoded)?
            .map(|total| Ok((decode_cost_key(&encoded)?, total)))
            .transpose()
    }

    fn read_slot(
        &mut self,
        index: u64,
        key: &mut [u8; COST_KEY_WIDTH],
    ) -> io::Result<Option<CostTotal>> {
        self.file.seek(SeekFrom::Start(cost_slot_offset(index)?))?;
        let mut occupied = [0_u8; 1];
        self.file.read_exact(&mut occupied)?;
        if occupied[0] == 0 {
            return Ok(None);
        }
        self.file.read_exact(key)?;
        let mut amount = [0_u8; 16];
        self.file.read_exact(&mut amount)?;
        let mut calls = [0_u8; 8];
        self.file.read_exact(&mut calls)?;
        Ok(Some(CostTotal {
            amount_usd: Decimal::deserialize(amount),
            calls: u64::from_le_bytes(calls),
        }))
    }

    fn write_slot(
        &mut self,
        index: u64,
        key: &[u8; COST_KEY_WIDTH],
        total: CostTotal,
    ) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(cost_slot_offset(index)?))?;
        self.file.write_all(&[1])?;
        self.file.write_all(key)?;
        self.file.write_all(&total.amount_usd.serialize())?;
        self.file.write_all(&total.calls.to_le_bytes())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct CostTotal {
    pub(super) amount_usd: Decimal,
    pub(super) calls: u64,
}

pub(super) struct UsageAggregate {
    pub(super) reported: TokenUsageTotal,
    pub(super) estimated: TokenUsageTotal,
    pub(super) costs: DiskCostTotals,
}

impl UsageAggregate {
    pub(super) fn new() -> Result<Self, ClientError> {
        Ok(Self {
            reported: TokenUsageTotal::default(),
            estimated: TokenUsageTotal::default(),
            costs: DiskCostTotals::new()?,
        })
    }

    pub(super) fn add(
        &mut self,
        evidence: &crate::transcript::SnapshotModelCallUsage,
    ) -> Result<(), ClientError> {
        match evidence.usage_provenance {
            UsageProvenance::Reported => self.reported.add(evidence.usage)?,
            UsageProvenance::Estimated => self.estimated.add(evidence.usage)?,
        }
        let Some(cost) = evidence.cost.as_ref() else {
            return Ok(());
        };
        let amount = Decimal::from_str(cost.amount_usd.as_str())
            .map_err(|_| ClientError::Protocol("dollar cost was not representable"))?;
        let key = CostAggregateKey {
            provenance: evidence.usage_provenance,
            label: cost.label,
            rate_version: cost.rate_version.as_str().to_owned(),
        };
        self.costs.add(&key, amount)
    }
}

fn encode_cost_key(key: &CostAggregateKey) -> [u8; COST_KEY_WIDTH] {
    let mut encoded = [0_u8; COST_KEY_WIDTH];
    encoded[0] = match key.provenance {
        UsageProvenance::Reported => 0,
        UsageProvenance::Estimated => 1,
    };
    encoded[1] = match key.label {
        ModelCallCostLabel::Real => 0,
        ModelCallCostLabel::MeteredEquivalent => 1,
    };
    let version = key.rate_version.as_bytes();
    debug_assert!(version.len() <= MAX_RATE_VERSION_UTF8_BYTES);
    encoded[2..2 + version.len()].copy_from_slice(version);
    encoded
}

fn decode_cost_key(encoded: &[u8; COST_KEY_WIDTH]) -> io::Result<CostAggregateKey> {
    let provenance = match encoded[0] {
        0 => UsageProvenance::Reported,
        1 => UsageProvenance::Estimated,
        _ => return Err(io::Error::other("disk cost provenance was invalid")),
    };
    let label = match encoded[1] {
        0 => ModelCallCostLabel::Real,
        1 => ModelCallCostLabel::MeteredEquivalent,
        _ => return Err(io::Error::other("disk cost label was invalid")),
    };
    let version_end = encoded[2..]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(MAX_RATE_VERSION_UTF8_BYTES);
    let rate_version = String::from_utf8(encoded[2..2 + version_end].to_vec())
        .map_err(|_| io::Error::other("disk rate version was not UTF-8"))?;
    Ok(CostAggregateKey {
        provenance,
        label,
        rate_version,
    })
}

fn cost_slot_offset(index: u64) -> io::Result<u64> {
    index
        .checked_mul(
            u64::try_from(COST_SLOT_WIDTH)
                .map_err(|_| io::Error::other("disk cost slot width overflowed"))?,
        )
        .ok_or_else(|| io::Error::other("disk cost slot offset overflowed"))
}

fn stable_cost_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(super) const fn usage_provenance_label(provenance: UsageProvenance) -> &'static str {
    match provenance {
        UsageProvenance::Reported => "reported",
        UsageProvenance::Estimated => "estimated",
    }
}

pub(super) const fn cost_label(label: ModelCallCostLabel) -> &'static str {
    match label {
        ModelCallCostLabel::Real => "real",
        ModelCallCostLabel::MeteredEquivalent => "metered_equivalent",
    }
}
