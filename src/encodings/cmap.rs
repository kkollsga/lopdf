use crate::cmap_section::{CMapParseError, CMapSection, CodeLen, SourceCode};
use crate::parser::cmap_parser::parse;

use log::error;
use rangemap::RangeInclusiveMap;
use std::collections::HashMap;
use thiserror::Error;

/// Unicode Cmap is implemented by 4 maps.
/// Each map contains a mappings from source codes to unicode values for a different length of codes.
/// Codes vary from 1 byte to 4 bytes so they are always in limits of u32.
/// However to map a code to a unicode value an additional knowledge about the number of bytes is needed,
/// as 2 byte code <0000> shouldn't be matched with a single byte <00> even though they have the same integer value.
#[derive(Debug, Default)]
pub struct ToUnicodeCMap {
    pub bf_ranges: [RangeInclusiveMap<SourceCode, BfRangeTarget>; 4],
    reverse_map: Option<HashMap<Vec<u16>, Vec<ReverseCMapEntry>>>,
}
/// Represents the information needed to map a Unicode sequence back to a source code.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReverseCMapEntry {
    pub source_code: SourceCode,
    pub code_len: CodeLen,
    // Optionally, add priority if multiple source codes map to the same Unicode sequence
    // pub priority: u8,
}

#[derive(Debug, Error)]
pub enum UnicodeCMapError {
    #[error("could not parse ToUnicode CMap: {0:#?}")]
    Parse(CMapParseError),
    #[error("invalid code range")]
    InvalidCodeRange,
}

impl From<CMapParseError> for UnicodeCMapError {
    fn from(err: CMapParseError) -> Self {
        UnicodeCMapError::Parse(err)
    }
}

impl ToUnicodeCMap {
    const REPLACEMENT_CHAR: u16 = 0xfffd;
    // `RangeInclusiveMap` stores ranges in a private `BTreeMap`. A sparse map
    // still owns a full node allocation: on supported Rust targets the node can
    // hold 11 `(u32, (u32, BfRangeTarget))` key/value slots, and an internal
    // node additionally holds 12 pointer-width edges plus its header. Two KiB
    // covers that complete allocation, including alignment and allocator
    // overhead, on both 32- and 64-bit targets. Charge the whole envelope for
    // every live range so this remains conservative at any node occupancy.
    // The stored tuple is already included and must not be added separately.
    const RANGE_NODE_ALLOCATION_ENVELOPE: usize = 2 * 1024;
    const REVERSE_SLOT_ENVELOPE: usize = 128;

    pub fn new() -> ToUnicodeCMap {
        ToUnicodeCMap {
            bf_ranges: [(); 4].map(|_| RangeInclusiveMap::new()),
            reverse_map: None,
        }
    }

    pub(crate) fn parse(stream_content: Vec<u8>) -> Result<ToUnicodeCMap, UnicodeCMapError> {
        let cmap_sections = parse(&stream_content[..])?;
        Self::from_sections(cmap_sections, true)
    }

    /// Parse the exact forward byte-to-Unicode map without constructing the reverse map used
    /// only by Unicode-to-byte encoding.
    pub(crate) fn parse_forward_only(stream_content: Vec<u8>) -> Result<ToUnicodeCMap, UnicodeCMapError> {
        let cmap_sections = parse(&stream_content[..])?;
        Self::from_sections(cmap_sections, false)
    }

    fn from_sections(
        cmap_sections: Vec<CMapSection>, build_reverse_map: bool,
    ) -> Result<ToUnicodeCMap, UnicodeCMapError> {
        let mut cmap = Self::new();
        for section in cmap_sections {
            match section {
                CMapSection::CsRange(_) => (), // currently no additional validation is implemented for code ranges
                CMapSection::BfChar(char_mappings) => {
                    for ((code, code_len), dst) in char_mappings {
                        cmap.put_char(code, code_len, dst);
                    }
                }
                CMapSection::BfRange(range_mappings) => {
                    for ((start, end, code_len), dst_vec) in range_mappings {
                        if end < start {
                            return Err(UnicodeCMapError::InvalidCodeRange);
                        }
                        match dst_vec.len() {
                            1 if dst_vec[0].len() == 1 => cmap.put(
                                start,
                                end,
                                code_len,
                                BfRangeTarget::UTF16CodePoint {
                                    offset: u32::wrapping_sub(dst_vec[0][0] as u32, start),
                                },
                            ),
                            1 => cmap.put(start, end, code_len, BfRangeTarget::HexString(dst_vec[0].clone())),
                            0 => return Err(UnicodeCMapError::InvalidCodeRange),
                            _ => cmap.put(start, end, code_len, BfRangeTarget::ArrayOfHexStrings(dst_vec.clone())),
                        }
                    }
                }
            }
        }

        if !build_reverse_map {
            return Ok(cmap);
        }

        let mut rev_map = HashMap::new();
        for code_len_idx in 0..cmap.bf_ranges.len() {
            let code_len = (code_len_idx + 1) as u8;
            for (range, target) in cmap.bf_ranges[code_len_idx].iter() {
                for src_code in range.clone() {
                    let unicode_sequence: Option<Vec<u16>> = match target {
                        BfRangeTarget::UTF16CodePoint { offset } => {
                            Some(vec![u32::wrapping_add(src_code, *offset) as u16])
                        }
                        BfRangeTarget::HexString(hex_str_vec) => {
                            // If the hex_str_vec itself is the target for a single src_code in a bfchar-like mapping
                            // or if it's a base for a bfrange where only the last element increments.
                            if src_code == *range.start() {
                                // Simplified: assume direct mapping for start of range
                                Some(hex_str_vec.clone())
                            } else if hex_str_vec.len() == 1 {
                                // For ranges like <01> <05> <0041>
                                Some(vec![hex_str_vec[0].wrapping_add((src_code - range.start()) as u16)])
                            } else if !hex_str_vec.is_empty() {
                                // For ranges like <01> <05> [<0041> <0042> ...]
                                let mut current_hex_str = hex_str_vec.clone();
                                if let Some(last_val) = current_hex_str.last_mut() {
                                    *last_val = last_val.wrapping_add((src_code - range.start()) as u16);
                                    Some(current_hex_str)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        BfRangeTarget::ArrayOfHexStrings(array_of_hex_str) => {
                            let index = (src_code - range.start()) as usize;
                            if index < array_of_hex_str.len() {
                                Some(array_of_hex_str[index].clone())
                            } else {
                                None
                            }
                        }
                    };

                    if let Some(uni_seq) = unicode_sequence
                        && !uni_seq.is_empty()
                    {
                        rev_map.entry(uni_seq).or_insert_with(Vec::new).push(ReverseCMapEntry {
                            source_code: src_code,
                            code_len,
                        });
                    }
                }
            }
        }
        cmap.reverse_map = Some(rev_map);

        Ok(cmap)
    }

    pub fn get(&self, code: SourceCode, code_len: CodeLen) -> Option<Vec<u16>> {
        if code_len > 4 || code_len == 0 {
            error!("Code lenght should be between l and 4 bytes, got {code_len}");
            return None;
        }
        use BfRangeTarget::*;

        let bf_ranges_map = &self.bf_ranges[(code_len - 1) as usize];

        bf_ranges_map.get_key_value(&code).map(|(range, value)| match value {
            HexString(vec) => {
                let mut ret_vec = vec.clone();
                *(ret_vec.last_mut().unwrap()) += (code - range.start()) as u16;
                ret_vec
            }
            UTF16CodePoint { offset } => vec![u32::wrapping_add(code, *offset) as u16],
            ArrayOfHexStrings(vec_of_strings) => {
                let idx = (code - range.start()) as usize;
                if idx < vec_of_strings.len() {
                    vec_of_strings[idx].clone()
                } else {
                    vec![ToUnicodeCMap::REPLACEMENT_CHAR]
                }
            }
        })
    }

    pub fn get_or_replacement_char(&self, code: SourceCode, code_len: CodeLen) -> Vec<u16> {
        self.get(code, code_len)
            .unwrap_or(vec![ToUnicodeCMap::REPLACEMENT_CHAR])
    }

    pub fn put(&mut self, src_code_lo: SourceCode, src_code_hi: SourceCode, code_len: CodeLen, target: BfRangeTarget) {
        if code_len > 4 || code_len == 0 {
            error!("Code lenght should be between l and 4 bytes, got {code_len}, ignoring");
            return;
        }
        self.bf_ranges[(code_len - 1) as usize].insert(src_code_lo..=src_code_hi, target)
    }

    pub fn put_char(&mut self, code: SourceCode, code_len: CodeLen, dst: Vec<u16>) {
        let target = if dst.len() == 1 {
            BfRangeTarget::UTF16CodePoint {
                offset: u32::wrapping_sub(dst[0] as u32, code),
            }
        } else {
            BfRangeTarget::HexString(dst)
        };
        self.put(code, code, code_len, target)
    }

    /// Gets the source code(s) for a given Unicode sequence.
    /// Prioritizes shorter byte sequences if multiple mappings exist.
    pub fn get_source_codes_for_unicode(&self, unicode_sequence: &[u16]) -> Option<&[ReverseCMapEntry]> {
        if let Some(map) = &self.reverse_map {
            // TODO: Add prioritization logic if needed (e.g., prefer shorter code_len)
            map.get(unicode_sequence).map(|v| v.as_slice())
        } else {
            None
        }
    }

    pub(crate) fn retained_heap_bytes(&self) -> usize {
        let forward = self.bf_ranges.iter().fold(0usize, |bytes, ranges| {
            ranges.iter().fold(bytes, |bytes, (_, target)| {
                bytes
                    .saturating_add(Self::RANGE_NODE_ALLOCATION_ENVELOPE)
                    .saturating_add(target.retained_heap_bytes())
            })
        });
        self.reverse_map.as_ref().map_or(forward, |reverse| {
            let slots = reverse.capacity().saturating_mul(
                std::mem::size_of::<(Vec<u16>, Vec<ReverseCMapEntry>)>().saturating_add(Self::REVERSE_SLOT_ENVELOPE),
            );
            reverse
                .iter()
                .fold(forward.saturating_add(slots), |bytes, (unicode, entries)| {
                    bytes
                        .saturating_add(unicode.capacity().saturating_mul(std::mem::size_of::<u16>()))
                        .saturating_add(
                            entries
                                .capacity()
                                .saturating_mul(std::mem::size_of::<ReverseCMapEntry>()),
                        )
                })
        })
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BfRangeTarget {
    // UTF16-BE encoding is used
    HexString(Vec<u16>),
    // don't store the actual codepoint but rather an offset to the src_code_lo
    // so that consecutive ranges can be mapped to the same value in the range map
    UTF16CodePoint { offset: u32 },
    ArrayOfHexStrings(Vec<Vec<u16>>),
}

impl BfRangeTarget {
    fn retained_heap_bytes(&self) -> usize {
        match self {
            Self::UTF16CodePoint { .. } => 0,
            Self::HexString(values) => values.capacity().saturating_mul(std::mem::size_of::<u16>()),
            Self::ArrayOfHexStrings(values) => values
                .capacity()
                .saturating_mul(std::mem::size_of::<Vec<u16>>())
                .saturating_add(
                    values
                        .iter()
                        .map(|value| value.capacity().saturating_mul(std::mem::size_of::<u16>()))
                        .fold(0, usize::saturating_add),
                ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_char_can_be_retrieved() {
        let mut cmap = ToUnicodeCMap::new();
        let char_code = 0x01;
        let char_value = vec![0x1234];
        cmap.put_char(char_code, 2, char_value.clone());

        assert_eq!(cmap.get(char_code, 2), Some(char_value))
    }

    #[test]
    fn char_can_be_retrieved_only_by_appropriate_len() {
        let mut cmap = ToUnicodeCMap::new();
        let char_code = 0x1;
        let code_len = 4;
        let char_value = vec![0x1234];
        cmap.put_char(char_code, code_len, char_value.clone());

        for i in 1..=3 {
            assert_eq!(cmap.get(char_code, i), None);
        }

        assert_eq!(cmap.get(char_code, code_len), Some(char_value));
    }

    #[test]
    fn wrong_code_len_does_not_panic() {
        let mut cmap = ToUnicodeCMap::new();
        let char_code = 0x1;
        let char_value = vec![0x1234];

        cmap.put_char(char_code, 5, char_value.clone());
        cmap.put_char(char_code, 0, char_value.clone());
    }

    #[test]
    fn array_of_hex_strings_out_of_bounds_returns_replacement() {
        // Simulate a malformed CMap bfrange where the array has fewer entries
        // than the declared source code range covers.
        let mut cmap = ToUnicodeCMap::new();
        let array = BfRangeTarget::ArrayOfHexStrings(vec![
            vec![0x0041], // 'A' — for code 0x10
            vec![0x0042], // 'B' — for code 0x11
        ]);
        // Range 0x10..=0x14 but only 2 entries in array (needs 5)
        cmap.put(0x10, 0x14, 2, array);

        // In-bounds lookups work
        assert_eq!(cmap.get(0x10, 2), Some(vec![0x0041]));
        assert_eq!(cmap.get(0x11, 2), Some(vec![0x0042]));

        // Out-of-bounds lookups return replacement char instead of panicking
        assert_eq!(cmap.get(0x12, 2), Some(vec![ToUnicodeCMap::REPLACEMENT_CHAR]));
        assert_eq!(cmap.get(0x13, 2), Some(vec![ToUnicodeCMap::REPLACEMENT_CHAR]));
        assert_eq!(cmap.get(0x14, 2), Some(vec![ToUnicodeCMap::REPLACEMENT_CHAR]));
    }

    #[test]
    fn forward_only_full_four_byte_range_stays_compact() {
        let content = b"/CIDInit /ProcSet findresource begin\n\
            12 dict begin\n\
            begincmap\n\
            /CMapName /Full-Four-Byte-Range def\n\
            /CMapType 2 def\n\
            1 begincodespacerange\n\
            <00000000> <FFFFFFFF>\n\
            endcodespacerange\n\
            1 beginbfrange\n\
            <00000000> <FFFFFFFF> <0041>\n\
            endbfrange\n\
            endcmap\n\
            CMapName currentdict /CMap defineresource pop\n\
            end\n\
            end\n"
            .to_vec();

        let cmap = ToUnicodeCMap::parse_forward_only(content).unwrap();

        assert!(cmap.reverse_map.is_none());
        assert_eq!(cmap.bf_ranges[3].iter().count(), 1);
        assert_eq!(cmap.get(0, 4), Some(vec![0x0041]));
        assert_eq!(cmap.get(1, 4), Some(vec![0x0042]));
        assert_eq!(cmap.get(u32::MAX, 4), Some(vec![0x0040]));
    }

    #[test]
    fn sparse_single_range_has_absolute_full_node_lower_bound() {
        let mut cmap = ToUnicodeCMap::new();
        cmap.put(0x10, 0x10, 1, BfRangeTarget::UTF16CodePoint { offset: 0 });

        assert_eq!(cmap.bf_ranges[0].iter().count(), 1);
        assert!(cmap.retained_heap_bytes() >= 2 * 1024);
    }

    #[test]
    fn retained_weight_tracks_multiple_ranges_and_target_capacity() {
        let mut target = Vec::with_capacity(64);
        target.push(0x0041);
        let mut cmap = ToUnicodeCMap::new();
        cmap.put(0x10, 0x10, 1, BfRangeTarget::HexString(target));
        cmap.put(0x20, 0x2f, 1, BfRangeTarget::UTF16CodePoint { offset: 0 });

        assert_eq!(cmap.bf_ranges[0].iter().count(), 2);
        assert!(cmap.retained_heap_bytes() >= 2 * (2 * 1024) + 64 * std::mem::size_of::<u16>());
    }
}
