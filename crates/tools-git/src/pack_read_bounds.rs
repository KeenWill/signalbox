//! Metadata-only bounds for the objects and delta programs a packed read decodes.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Read,
};

use flate2::read::ZlibDecoder;
use git2::Oid;

use crate::limits::MAX_OBJECT_BYTES;

/// Adds objects whose full delta dependency chain cannot be read within the content bound.
pub(super) fn collect_unreadable_objects(
    pack: &[u8],
    indexed: &[(Oid, usize)],
    unreadable: &mut BTreeSet<Oid>,
) {
    let offsets = indexed
        .iter()
        .enumerate()
        .map(|(index, &(_, offset))| (offset, index))
        .collect::<HashMap<_, _>>();
    let ids = indexed
        .iter()
        .enumerate()
        .map(|(index, &(oid, _))| (oid, index))
        .collect::<BTreeMap<_, _>>();
    let mut dependents = vec![Vec::new(); indexed.len()];
    let mut pending = Vec::new();
    for (index, &(_, offset)) in indexed.iter().enumerate() {
        match bounded_entry_base(pack, offset, &offsets, &ids) {
            Ok(Some(base)) => dependents[base].push(index),
            Ok(None) => pending.push(index),
            Err(()) => {}
        }
    }
    let mut readable = vec![false; indexed.len()];
    while let Some(index) = pending.pop() {
        if !readable[index] {
            readable[index] = true;
            pending.extend_from_slice(&dependents[index]);
        }
    }
    for ((oid, _), readable) in indexed.iter().zip(readable) {
        if !readable {
            unreadable.insert(*oid);
        }
    }
}

// Git pack-format: https://git-scm.com/docs/pack-format. Both the packed entry's
// inflated size and each delta's reconstructed size allocate buffers in libgit2.
fn bounded_entry_base(
    pack: &[u8],
    offset: usize,
    offsets: &HashMap<usize, usize>,
    ids: &BTreeMap<Oid, usize>,
) -> Result<Option<usize>, ()> {
    if offset < 12 {
        return Err(());
    }
    let mut bytes = pack.get(offset..).ok_or(())?;
    let first = read_byte(&mut bytes)?;
    let kind = (first >> 4) & 7;
    let mut size = u64::from(first & 0x0f);
    if first & 0x80 != 0 {
        let upper = read_size(&mut bytes)?;
        size = upper
            .checked_mul(16)
            .and_then(|upper| upper.checked_add(size))
            .ok_or(())?;
    }
    if size > MAX_OBJECT_BYTES as u64 {
        return Err(());
    }
    let base = match kind {
        1..=4 => return Ok(None),
        6 => {
            let mut byte = read_byte(&mut bytes)?;
            let mut distance = usize::from(byte & 0x7f);
            while byte & 0x80 != 0 {
                byte = read_byte(&mut bytes)?;
                distance = distance
                    .checked_add(1)
                    .and_then(|value| value.checked_mul(128))
                    .and_then(|value| value.checked_add(usize::from(byte & 0x7f)))
                    .ok_or(())?;
            }
            if distance == 0 {
                return Err(());
            }
            *offsets
                .get(&offset.checked_sub(distance).ok_or(())?)
                .ok_or(())?
        }
        7 => {
            let oid_bytes = ids.keys().next().ok_or(())?.as_bytes().len();
            let oid = Oid::from_bytes(bytes.get(..oid_bytes).ok_or(())?).map_err(|_| ())?;
            bytes = bytes.get(oid_bytes..).ok_or(())?;
            *ids.get(&oid).ok_or(())?
        }
        _ => return Err(()),
    };
    let mut header = ZlibDecoder::new(bytes).take(size);
    let source_size = read_size(&mut header)?;
    let target_size = read_size(&mut header)?;
    if source_size > MAX_OBJECT_BYTES as u64 || target_size > MAX_OBJECT_BYTES as u64 {
        return Err(());
    }
    Ok(Some(base))
}

fn read_size(reader: &mut impl Read) -> Result<u64, ()> {
    let mut size = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = read_byte(reader)?;
        let value = u64::from(byte & 0x7f);
        if value > u64::MAX >> shift {
            return Err(());
        }
        size |= value << shift;
        if byte & 0x80 == 0 {
            return Ok(size);
        }
    }
    Err(())
}

fn read_byte(reader: &mut impl Read) -> Result<u8, ()> {
    let mut byte = [0];
    reader.read_exact(&mut byte).map_err(|_| ())?;
    Ok(byte[0])
}
