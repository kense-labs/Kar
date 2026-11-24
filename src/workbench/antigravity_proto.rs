use super::native_sqlite::invalid;
use std::{collections::BTreeMap, io};
#[derive(Clone, Copy)]
enum Field<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}
#[derive(Default)]
pub(super) struct Fields<'a>(BTreeMap<u32, Vec<Field<'a>>>);
fn varint(bytes: &[u8], offset: &mut usize) -> io::Result<u64> {
    let mut value = 0;
    for shift in (0..=63).step_by(7) {
        let b = *bytes
            .get(*offset)
            .ok_or_else(|| invalid("truncated Antigravity varint"))?;
        *offset += 1;
        if shift == 63 && b > 1 {
            return Err(invalid("invalid Antigravity varint"));
        }
        value |= u64::from(b & 127) << shift;
        if b < 128 {
            return Ok(value);
        }
    }
    Err(invalid("overflowing Antigravity varint"))
}
impl<'a> Fields<'a> {
    pub(super) fn decode(bytes: &'a [u8]) -> io::Result<Self> {
        let mut at = 0;
        let mut result = Self::default();
        while at < bytes.len() {
            let tag = varint(bytes, &mut at)?;
            let number = u32::try_from(tag >> 3)
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| invalid("invalid Antigravity tag"))?;
            let field = match tag & 7 {
                0 => Field::Int(varint(bytes, &mut at)?),
                wire @ (1 | 2 | 5) => {
                    let len = match wire {
                        1 => 8,
                        5 => 4,
                        _ => usize::try_from(varint(bytes, &mut at)?)
                            .map_err(|_| invalid("overflowing Antigravity field"))?,
                    };
                    let end = at
                        .checked_add(len)
                        .filter(|n| *n <= bytes.len())
                        .ok_or_else(|| invalid("truncated Antigravity field"))?;
                    let slice = &bytes[at..end];
                    at = end;
                    if wire != 2 {
                        continue;
                    }
                    Field::Bytes(slice)
                }
                _ => return Err(invalid("unsupported Antigravity wire type")),
            };
            result.0.entry(number).or_default().push(field);
        }
        Ok(result)
    }
    pub(super) fn int(&self, k: u32) -> u64 {
        match self.0.get(&k).and_then(|a| a.first()) {
            Some(Field::Int(v)) => *v,
            _ => 0,
        }
    }
    pub(super) fn bytes(&self, k: u32) -> Option<&'a [u8]> {
        match self.0.get(&k)?.first()? {
            Field::Bytes(v) => Some(*v),
            _ => None,
        }
    }
    pub(super) fn nested(&self, k: u32) -> io::Result<Self> {
        Self::decode(self.bytes(k).unwrap_or(&[]))
    }
    pub(super) fn text(&self, k: u32) -> io::Result<&str> {
        std::str::from_utf8(self.bytes(k).unwrap_or(&[]))
            .map(str::trim)
            .map_err(|_| invalid("invalid Antigravity text"))
    }
    pub(super) fn repeated(&self, k: u32) -> impl Iterator<Item = io::Result<&'a [u8]>> + '_ {
        self.0
            .get(&k)
            .into_iter()
            .flatten()
            .map(|field| match field {
                Field::Bytes(v) => Ok(*v),
                _ => Err(invalid("unexpected Antigravity wire type")),
            })
    }
    pub(super) fn timestamp(&self) -> Option<i64> {
        let s = self.int(1);
        if s == 0 {
            return None;
        }
        i64::try_from(s.checked_mul(1000)?.checked_add(self.int(2) / 1_000_000)?).ok()
    }
}
