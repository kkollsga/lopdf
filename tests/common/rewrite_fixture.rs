// PDFs built byte by byte for the rewrite tests, so that what's in them doesn't come from a reader or a writer under test.

use std::collections::BTreeMap;
use std::io::{Read, Write};

/// One object: its number and what follows `N 0 obj`.
#[derive(Clone)]
pub struct Obj {
    pub number: u32,
    pub generation: u16,
    pub body: Vec<u8>,
    pub is_stream: bool,
}

pub fn obj(number: u32, body: &str) -> Obj {
    Obj {
        number,
        generation: 0,
        body: body.as_bytes().to_vec(),
        is_stream: false,
    }
}

/// A stream with a correct /Length, its content deflated.
pub fn deflated(number: u32, extra: &str, content: &[u8]) -> Obj {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(content).unwrap();
    raw_stream(number, &format!("/Filter /FlateDecode {extra}"), &encoder.finish().unwrap())
}

/// A stream with a correct /Length and these exact bytes.
pub fn raw_stream(number: u32, extra: &str, content: &[u8]) -> Obj {
    let mut body = format!("<< /Length {} {extra} >>\nstream\n", content.len()).into_bytes();
    body.extend_from_slice(content);
    body.extend_from_slice(b"\nendstream");
    Obj {
        number,
        generation: 0,
        body,
        is_stream: true,
    }
}

/// How a fixture holds its objects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Form {
    /// A classic table, every object at the top level.
    Table,
    /// A cross-reference stream, with objects that aren't streams in object streams.
    Compressed,
    /// A classic table for the streams, and a `/XRefStm` for the object streams.
    Hybrid,
}

pub const ALL_FORMS: [Form; 3] = [Form::Table, Form::Compressed, Form::Hybrid];

pub struct Builder {
    pub bytes: Vec<u8>,
    pub offsets: BTreeMap<u32, u64>,
    pub generations: BTreeMap<u32, u16>,
    pub compressed: BTreeMap<u32, (u32, u32)>,
    pub free: Vec<u32>,
    pub next_container: u32,
    pub last_xref: Option<u64>,
}

impl Builder {
    pub fn new(next_container: u32) -> Self {
        Self {
            bytes: b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec(),
            offsets: BTreeMap::new(),
            generations: BTreeMap::new(),
            compressed: BTreeMap::new(),
            free: Vec::new(),
            next_container,
            last_xref: None,
        }
    }

    pub fn write_obj(&mut self, object: &Obj) {
        self.offsets.insert(object.number, self.bytes.len() as u64);
        self.generations.insert(object.number, object.generation);
        self.bytes
            .extend_from_slice(format!("{} {} obj\n", object.number, object.generation).as_bytes());
        self.bytes.extend_from_slice(&object.body);
        self.bytes.extend_from_slice(b"\nendobj\n");
    }

    /// Put these members in one new object stream.
    pub fn object_stream(&mut self, members: &[&Obj]) {
        let container = self.next_container;
        self.next_container += 1;
        let mut head = String::new();
        let mut body = Vec::new();
        for (index, member) in members.iter().enumerate() {
            assert!(!member.is_stream);
            head.push_str(&format!("{} {} ", member.number, body.len()));
            body.extend_from_slice(&member.body);
            body.push(b'\n');
            self.compressed.insert(member.number, (container, index as u32));
            self.offsets.remove(&member.number);
        }
        let mut content = head.clone().into_bytes();
        content.extend_from_slice(&body);
        let stream = deflated(
            container,
            &format!("/Type /ObjStm /N {} /First {}", members.len(), head.len()),
            &content,
        );
        self.write_obj(&stream);
    }

    /// Place objects as this form holds them, up to `per_stream` in one object stream.
    pub fn place(&mut self, form: Form, objects: &[Obj], per_stream: usize) {
        let (streams, plain): (Vec<&Obj>, Vec<&Obj>) = objects.iter().partition(|object| object.is_stream);
        for stream in streams {
            self.write_obj(stream);
        }
        if form == Form::Table {
            for object in plain {
                self.write_obj(object);
            }
        } else {
            for members in plain.chunks(per_stream.max(1)) {
                self.object_stream(members);
            }
        }
    }

    fn table_rows(&self, numbers: &[u32], with_head: bool) -> String {
        let mut table = String::from("xref\n");
        if with_head {
            table.push_str("0 1\n0000000000 65535 f \n");
        }
        for number in numbers {
            if self.free.contains(number) {
                table.push_str(&format!("{number} 1\n0000000000 00001 f \n"));
            } else {
                table.push_str(&format!(
                    "{number} 1\n{:010} {:05} n \n",
                    self.offsets[number],
                    self.generations.get(number).copied().unwrap_or(0)
                ));
            }
        }
        table
    }

    fn numbers(&self) -> Vec<u32> {
        let mut numbers: Vec<u32> = self.offsets.keys().copied().chain(self.free.iter().copied()).collect();
        numbers.sort_unstable();
        numbers.dedup();
        numbers
    }

    fn xref_stream_bytes(&mut self, number: u32, extras: &str, numbers: &[u32], list_itself: bool) -> u64 {
        let start = self.bytes.len() as u64;
        self.offsets.insert(number, start);
        self.generations.insert(number, 0);
        let mut listed: Vec<u32> = numbers.to_vec();
        if list_itself {
            listed.push(number);
        }
        listed.sort_unstable();
        listed.dedup();
        let mut index = String::new();
        let mut content = Vec::new();
        for entry in &listed {
            index.push_str(&format!("{entry} 1 "));
            if let Some((container, position)) = self.compressed.get(entry) {
                content.push(2_u8);
                content.extend_from_slice(&container.to_be_bytes());
                content.extend_from_slice(&(*position as u16).to_be_bytes());
            } else if self.free.contains(entry) {
                content.push(0_u8);
                content.extend_from_slice(&0_u32.to_be_bytes());
                content.extend_from_slice(&1_u16.to_be_bytes());
            } else {
                content.push(1_u8);
                content.extend_from_slice(&(self.offsets[entry] as u32).to_be_bytes());
                content.extend_from_slice(&self.generations.get(entry).copied().unwrap_or(0).to_be_bytes());
            }
        }
        let body = format!(
            "{number} 0 obj\n<< /Type /XRef /W [1 4 2] /Index [{index}] /Length {} {extras} >>\nstream\n",
            content.len()
        );
        self.bytes.extend_from_slice(body.as_bytes());
        self.bytes.extend_from_slice(&content);
        self.bytes.extend_from_slice(b"\nendstream\nendobj\n");
        start
    }

    /// Finish a revision. `trailer` goes in the trailer's dictionary as it is.
    pub fn finish_revision(&mut self, form: Form, trailer: &str) {
        let prev = self.last_xref.map_or(String::new(), |offset| format!("/Prev {offset} "));
        let size = self
            .offsets
            .keys()
            .chain(self.compressed.keys())
            .chain(self.free.iter())
            .max()
            .copied()
            .unwrap_or(0)
            + 2;
        let xref = match form {
            Form::Table => {
                let numbers = self.numbers();
                let start = self.bytes.len() as u64;
                let table = self.table_rows(&numbers, self.last_xref.is_none());
                self.bytes.extend_from_slice(table.as_bytes());
                self.bytes.extend_from_slice(
                    format!("trailer\n<< /Size {size} {prev}{trailer} >>\n").as_bytes(),
                );
                start
            }
            Form::Compressed => {
                let mut numbers: Vec<u32> = self.numbers();
                numbers.extend(self.compressed.keys().copied());
                let number = self.next_container;
                self.next_container += 1;
                self.xref_stream_bytes(number, &format!("/Size {size} {prev}{trailer}"), &numbers, true)
            }
            Form::Hybrid => {
                let compressed: Vec<u32> = self.compressed.keys().copied().collect();
                let number = self.next_container;
                self.next_container += 1;
                let supplement = self.xref_stream_bytes(number, &format!("/Size {size}"), &compressed, false);
                let numbers = self.numbers();
                let start = self.bytes.len() as u64;
                let table = self.table_rows(&numbers, self.last_xref.is_none());
                self.bytes.extend_from_slice(table.as_bytes());
                self.bytes.extend_from_slice(
                    format!("trailer\n<< /Size {size} {prev}/XRefStm {supplement} {trailer} >>\n").as_bytes(),
                );
                start
            }
        };
        self.bytes
            .extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
        self.last_xref = Some(xref);
        // A new revision lists only what it changes.
        self.offsets.clear();
        self.compressed.clear();
        self.free.clear();
    }
}

/// Every stream's bytes in this file, inflated where that works. What a reader can't see still counts, so this knows nothing about xrefs.
pub fn every_decoded_stream(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(position) = find(&bytes[at..], b"stream") {
        let start = at + position + b"stream".len();
        at = start;
        // `endstream` itself isn't the start of one.
        if start >= 9 && &bytes[start - 9..start] == b"endstream" {
            continue;
        }
        let mut data = start;
        if bytes.get(data) == Some(&b'\r') {
            data += 1;
        }
        if bytes.get(data) == Some(&b'\n') {
            data += 1;
        }
        let end = find(&bytes[data..], b"endstream").map_or(bytes.len(), |end| data + end);
        let mut inflated = Vec::new();
        let _ = flate2::read::ZlibDecoder::new(&bytes[data..end])
            .take(256 * 1_024 * 1_024)
            .read_to_end(&mut inflated);
        if !inflated.is_empty() {
            found.push(inflated);
        }
    }
    found
}

pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn spellings(marker: &str) -> Vec<Vec<u8>> {
    let hex: String = marker.bytes().map(|byte| format!("{byte:02X}")).collect();
    let utf16: Vec<u8> = marker.bytes().flat_map(|byte| [0, byte]).collect();
    let utf16_hex: String = marker.bytes().map(|byte| format!("00{byte:02X}")).collect();
    vec![
        marker.as_bytes().to_vec(),
        hex.clone().into_bytes(),
        hex.to_lowercase().into_bytes(),
        utf16,
        utf16_hex.clone().into_bytes(),
        utf16_hex.to_lowercase().into_bytes(),
    ]
}

/// Whether a marker is anywhere in this file: in its bytes or any stream, inflated or not, plainly, in hex, or as UTF-16.
pub fn holds_marker(bytes: &[u8], streams: &[Vec<u8>], marker: &str) -> bool {
    spellings(marker)
        .iter()
        .any(|spelling| find(bytes, spelling).is_some() || streams.iter().any(|stream| find(stream, spelling).is_some()))
}
