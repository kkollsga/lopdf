//! Writing the new file from a settled plan, and checking it.

use std::io::BufWriter;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use super::append::MAX_TABLE_OFFSET;
use super::plan::RETAINED_CONTAINER_BYTES;
use super::*;
use crate::writer::Writer;

/// More objects than this and a table can't be read back: at a subsection line for every entry, it would pass the
/// 16 MiB the reader gives one section's window.
const MAX_TABLE_OBJECTS: u64 = 500_000;
/// Members of one new object stream. The common choice, and small enough that reading one object doesn't inflate much for nothing.
const MAX_OBJECT_STREAM_MEMBERS: usize = 100;
/// Serialized bytes of one new object stream before it goes out.
const MAX_OBJECT_STREAM_BYTES: usize = 64 * 1_024;

/// Where one object went, in 16 bytes, since there's one for every object in the file.
#[derive(Clone, Copy)]
pub(super) struct WrittenEntry {
    pub(super) number: u32,
    /// Generation, or for a compressed object its index in the object stream.
    second: u16,
    compressed: bool,
    /// Offset, or for a compressed object its object stream's number.
    first: u64,
}

/// What [`IndexedReader::verify_rewritten`] needs besides the summary.
pub(super) struct Written {
    entries: Vec<WrittenEntry>,
    dead: Bits,
    expected_pages: Option<usize>,
}

/// An object stream being filled.
#[derive(Default)]
struct OpenObjectStream {
    numbers: Vec<u32>,
    offsets: Vec<usize>,
    body: Vec<u8>,
}

struct Output<W: Write> {
    sink: Sink<W>,
    entries: Vec<WrittenEntry>,
    form: RewriteForm,
    open: OpenObjectStream,
    next_number: u32,
    compressed: u64,
    object_streams: u64,
    stream_bytes: u64,
    buffer: Vec<u8>,
}

impl<W: Write> Output<W> {
    fn emit(&mut self, bytes: &[u8]) -> RewriteResult<()> {
        self.sink.write_all(bytes)?;
        Ok(())
    }

    fn emit_object(&mut self, object: &Object) -> RewriteResult<()> {
        Writer::write_object(&mut self.sink, object)?;
        Ok(())
    }

    fn check_offset(&self, offset: u64) -> RewriteResult<()> {
        if self.form == RewriteForm::Table && offset > MAX_TABLE_OFFSET {
            return Err(RewriteError::OffsetTooLarge { offset });
        }
        Ok(())
    }

    fn take_number(&mut self) -> RewriteResult<u32> {
        let number = self.next_number;
        if number > MAX_OBJECT_NUMBER {
            return Err(RewriteError::ObjectNumberTooLarge {
                number: u64::from(number),
                limit: MAX_OBJECT_NUMBER,
            });
        }
        self.next_number = number + 1;
        Ok(number)
    }

    fn begin_object(&mut self, number: u32, generation: u16) -> RewriteResult<()> {
        let start = self.sink.position;
        self.check_offset(start)?;
        self.entries.push(WrittenEntry {
            number,
            second: generation,
            compressed: false,
            first: start,
        });
        self.emit(format!("{number} {generation} obj\n").as_bytes())
    }

    fn write_plain(&mut self, number: u32, generation: u16, object: &Object) -> RewriteResult<()> {
        self.begin_object(number, generation)?;
        self.emit_object(object)?;
        self.emit(b"\nendobj\n")
    }

    fn stream_head(&mut self, number: u32, generation: u16, dict: &mut Dictionary, length: u64) -> RewriteResult<()> {
        let length = i64::try_from(length).map_err(|_| RewriteError::InvalidObject {
            id: (number, generation),
            reason: "its length doesn't fit a PDF integer",
        })?;
        dict.set("Length", length);
        self.begin_object(number, generation)?;
        self.emit_object(&Object::Dictionary(std::mem::take(dict)))?;
        self.emit(b"\nstream\n")
    }

    fn stream_tail(&mut self) -> RewriteResult<()> {
        self.emit(b"\nendstream\nendobj\n")
    }

    /// Whether this object can go in an object stream.
    fn can_compress(&self, generation: u16, object: &Object) -> bool {
        self.form == RewriteForm::Stream
            && generation == 0
            // What a signature signs leaves a hole for it in the file's own bytes, so anything that reads one looks for it there.
            && !object.as_dict().is_ok_and(|dict| dict.has(b"ByteRange"))
    }

    fn add_to_object_stream(&mut self, number: u32, object: &Object) -> RewriteResult<()> {
        self.open.numbers.push(number);
        self.open.offsets.push(self.open.body.len());
        Writer::write_object(&mut self.open.body, object)?;
        self.open.body.push(b'\n');
        if self.open.numbers.len() >= MAX_OBJECT_STREAM_MEMBERS || self.open.body.len() >= MAX_OBJECT_STREAM_BYTES {
            self.close_object_stream()?;
        }
        Ok(())
    }

    fn close_object_stream(&mut self) -> RewriteResult<()> {
        if self.open.numbers.is_empty() {
            return Ok(());
        }
        let open = std::mem::take(&mut self.open);
        let container = self.take_number()?;
        let mut head = String::new();
        for (member, (number, offset)) in open.numbers.iter().zip(&open.offsets).enumerate() {
            head.push_str(&format!("{number} {offset} "));
            self.entries.push(WrittenEntry {
                number: *number,
                second: u16::try_from(member).unwrap_or(u16::MAX),
                compressed: true,
                first: u64::from(container),
            });
        }
        head.pop();
        head.push('\n');
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(head.as_bytes())?;
        encoder.write_all(&open.body)?;
        let content = encoder.finish()?;
        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(b"ObjStm".to_vec()));
        dict.set("N", open.numbers.len() as i64);
        dict.set("First", head.len() as i64);
        dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
        self.stream_head(container, 0, &mut dict, content.len() as u64)?;
        self.emit(&content)?;
        self.stream_tail()?;
        self.compressed += open.numbers.len() as u64;
        self.object_streams += 1;
        Ok(())
    }

    fn copy_from_source(&mut self, source: &dyn RandomAccessSource, start: u64, length: u64) -> RewriteResult<()> {
        let mut position = start;
        let mut left = length;
        let mut buffer = std::mem::take(&mut self.buffer);
        while left > 0 {
            let step = usize::try_from(left).map_or(COPY_CHUNK_BYTES, |left| left.min(COPY_CHUNK_BYTES));
            source.read_exact_at(position, &mut buffer[..step])?;
            self.emit(&buffer[..step])?;
            position += step as u64;
            left -= step as u64;
        }
        self.buffer = buffer;
        self.stream_bytes += length;
        Ok(())
    }

    fn copy_from_reader(&mut self, id: crate::ObjectId, length: u64, content: &mut dyn Read) -> RewriteResult<()> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let mut left = length;
        loop {
            let step = usize::try_from(left).map_or(COPY_CHUNK_BYTES, |left| left.min(COPY_CHUNK_BYTES));
            // With nothing left to copy this reads one more byte, to prove the reader is empty too.
            let probe = if step == 0 { 1 } else { step };
            let read = match content.read(&mut buffer[..probe]) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if (read == 0 && left > 0) || (read > 0 && left == 0) {
                return Err(RewriteError::StreamLengthMismatch { id, declared: length });
            }
            if read == 0 {
                break;
            }
            self.emit(&buffer[..read])?;
            left -= read as u64;
        }
        self.buffer = buffer;
        self.stream_bytes += length;
        Ok(())
    }

    fn write_table(&mut self, trailer: &mut Dictionary, size: u64) -> RewriteResult<u64> {
        let offset = self.sink.position;
        self.check_offset(offset)?;
        self.emit(b"xref\n0 1\n0000000000 65535 f \n")?;
        let entries = std::mem::take(&mut self.entries);
        let mut at = 0;
        while at < entries.len() {
            let mut end = at + 1;
            while end < entries.len() && entries[end].number == entries[end - 1].number + 1 {
                end += 1;
            }
            self.emit(format!("{} {}\n", entries[at].number, end - at).as_bytes())?;
            for entry in &entries[at..end] {
                self.emit(format!("{:010} {:05} n \n", entry.first, entry.second).as_bytes())?;
            }
            at = end;
        }
        self.entries = entries;
        self.emit(b"trailer\n")?;
        trailer.set("Size", i64::try_from(size).unwrap_or(i64::MAX));
        self.emit_object(&Object::Dictionary(std::mem::take(trailer)))?;
        self.emit(b"\n")?;
        Ok(offset)
    }

    fn write_xref_stream(&mut self, trailer: &mut Dictionary) -> RewriteResult<u64> {
        let number = self.take_number()?;
        let offset = self.sink.position;
        self.entries.push(WrittenEntry {
            number,
            second: 0,
            compressed: false,
            first: offset,
        });
        let largest = self.entries.iter().map(|entry| entry.first).max().unwrap_or(0).max(offset);
        let width = usize::try_from((u64::BITS - largest.leading_zeros()).div_ceil(8)).unwrap_or(8).max(1);
        let mut index = vec![Object::Integer(0), Object::Integer(1)];
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        // Object 0, the head of the free list.
        encoder.write_all(&[0])?;
        encoder.write_all(&vec![0; width])?;
        encoder.write_all(&[0xFF, 0xFF])?;
        let entries = std::mem::take(&mut self.entries);
        let mut at = 0;
        let mut row = Vec::with_capacity(width + 3);
        while at < entries.len() {
            let mut end = at + 1;
            while end < entries.len() && entries[end].number == entries[end - 1].number + 1 {
                end += 1;
            }
            index.push(Object::Integer(i64::from(entries[at].number)));
            index.push(Object::Integer((end - at) as i64));
            for entry in &entries[at..end] {
                row.clear();
                row.push(if entry.compressed { 2 } else { 1 });
                row.extend_from_slice(&entry.first.to_be_bytes()[8 - width..]);
                row.extend_from_slice(&entry.second.to_be_bytes());
                encoder.write_all(&row)?;
            }
            at = end;
        }
        self.entries = entries;
        let content = encoder.finish()?;
        trailer.set("Type", Object::Name(b"XRef".to_vec()));
        trailer.set("Size", i64::from(self.next_number));
        trailer.set(
            "W",
            Object::Array(vec![Object::Integer(1), Object::Integer(width as i64), Object::Integer(2)]),
        );
        trailer.set("Index", Object::Array(index));
        trailer.set("Filter", Object::Name(b"FlateDecode".to_vec()));
        trailer.set("Length", content.len() as i64);
        self.emit(format!("{number} 0 obj\n").as_bytes())?;
        self.emit_object(&Object::Dictionary(std::mem::take(trailer)))?;
        self.emit(b"\nstream\n")?;
        self.emit(&content)?;
        self.stream_tail()?;
        Ok(offset)
    }
}

/// A version the header can state, whatever the file said it was.
fn header_version(source: &str, form: RewriteForm) -> &'static str {
    const KNOWN: [&str; 9] = ["1.0", "1.1", "1.2", "1.3", "1.4", "1.5", "1.6", "1.7", "2.0"];
    let version = KNOWN.iter().copied().find(|known| *known == source).unwrap_or("1.7");
    // Object streams and cross-reference streams came in with 1.5.
    match (form, version) {
        (RewriteForm::Stream, "1.0" | "1.1" | "1.2" | "1.3" | "1.4") => "1.5",
        _ => version,
    }
}

/// Write everything a plan settled.
pub(super) fn write_plan<W: AppendOutput>(
    reader: &IndexedReader, mut plan: Plan<'_>, out: W,
) -> RewriteResult<(RewriteSummary, Written)> {
    let index = &reader.index;
    let source_uses_object_streams = index
        .locations
        .values()
        .any(|location| matches!(location, ObjectLocation64::Compressed { .. }));
    let mut form = match plan.policy.form {
        RewriteFormChoice::Force(form) => form,
        RewriteFormChoice::MatchSource if index.xref_type == IndexXrefType::Table && !source_uses_object_streams => {
            RewriteForm::Table
        }
        RewriteFormChoice::MatchSource => RewriteForm::Stream,
    };
    if plan.live.count() > MAX_TABLE_OBJECTS {
        form = RewriteForm::Stream;
    }
    let mut output = Output {
        sink: Sink {
            out: BufWriter::with_capacity(COPY_CHUNK_BYTES, out),
            position: 0,
            hash: Sha256::new(),
        },
        entries: Vec::with_capacity(plan.live.count() as usize),
        form,
        open: OpenObjectStream::default(),
        next_number: plan.next_number,
        compressed: 0,
        object_streams: 0,
        stream_bytes: 0,
        buffer: vec![0; COPY_CHUNK_BYTES],
    };
    output.emit(format!("%PDF-{}\n", header_version(reader.version(), form)).as_bytes())?;
    output.emit(b"%\xE2\xE3\xCF\xD3\n")?;

    // Numbers come out in order, so each list is settled as it goes.
    let numbers: Vec<u32> = plan.live.iter().collect();
    let source = Arc::clone(plan.view.whole_file_source());
    for number in numbers {
        let generation = plan.generation(number);
        let Some(loaded) = plan.load_for_output(number)? else {
            continue;
        };
        match loaded {
            Loaded::Plain(object) => {
                if output.can_compress(generation, &object) {
                    output.add_to_object_stream(number, &object)?;
                } else {
                    output.write_plain(number, generation, &object)?;
                }
            }
            Loaded::Stream(mut dict, StreamBody::Source { start, length }) => {
                output.stream_head(number, generation, &mut dict, length)?;
                output.copy_from_source(source.as_ref(), start, length)?;
                output.stream_tail()?;
            }
            Loaded::Stream(mut dict, StreamBody::Given) => match plan.objects.remove(&number) {
                Some(Change::Object {
                    object: Object::Stream(stream),
                    ..
                }) => {
                    output.stream_head(number, generation, &mut dict, stream.content.len() as u64)?;
                    output.emit(&stream.content)?;
                    output.stream_bytes += stream.content.len() as u64;
                    output.stream_tail()?;
                }
                Some(Change::Stream { length, mut content, .. }) => {
                    output.stream_head(number, generation, &mut dict, length)?;
                    output.copy_from_reader((number, generation), length, &mut content)?;
                    output.stream_tail()?;
                }
                _ => {
                    return Err(RewriteError::InvalidObject {
                        id: (number, generation),
                        reason: "its stream was already written",
                    });
                }
            },
        }
    }
    output.close_object_stream()?;

    let objects_written = plan.live.count();
    let objects_compressed = output.compressed;
    let mut trailer = Dictionary::new();
    trailer.set("Root", Object::Reference((plan.root, plan.generation(plan.root))));
    if let Some(info) = plan.info.take() {
        match info {
            Object::Reference((number, generation)) if plan.live.get(number) => {
                trailer.set("Info", Object::Reference((number, generation)));
            }
            Object::Dictionary(mut info) => {
                plan.scrub_dict(&mut info);
                trailer.set("Info", Object::Dictionary(info));
            }
            _ => {}
        }
    }
    trailer.set("ID", new_file_id(&reader.index.trailer, &output.sink.hash, reader.index.source_len));

    output.entries.sort_unstable_by_key(|entry| entry.number);
    let xref_offset = match form {
        RewriteForm::Table => {
            let highest = output.entries.last().map_or(0, |entry| u64::from(entry.number));
            let size = highest + 1;
            output.write_table(&mut trailer, size)?
        }
        RewriteForm::Stream => output.write_xref_stream(&mut trailer)?,
    };
    let size = match form {
        RewriteForm::Table => output.entries.last().map_or(0, |entry| u64::from(entry.number)) + 1,
        RewriteForm::Stream => u64::from(output.next_number),
    };
    output.emit(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes())?;
    output.sink.flush()?;
    let sha256: [u8; 32] = output.sink.hash.clone().finalize().into();

    let index_live = reader
        .index
        .locations
        .values()
        .filter(|location| !matches!(location, ObjectLocation64::Free { .. }))
        .count() as u64;
    let kept_from_source = plan.live.iter().filter(|number| reader.index.locations.contains_key(number)).count() as u64;
    let summary = RewriteSummary {
        previous_len: reader.index.source_len,
        new_len: output.sink.position,
        form,
        xref_offset,
        size,
        objects_written,
        objects_compressed,
        object_streams: output.object_streams,
        objects_dropped: index_live.saturating_sub(kept_from_source),
        stream_bytes_copied: output.stream_bytes,
        removal: std::mem::take(&mut plan.report),
        broken_signatures: std::mem::take(&mut plan.signatures),
        sha256,
    };
    let written = Written {
        entries: output.entries,
        dead: plan.dead,
        expected_pages: plan.expected_pages,
    };
    Ok((summary, written))
}

/// The first string of `/ID` is the file's permanent identifier and stays. The second changes with each version, so it comes from
/// what was written. An `/ID` that isn't two direct strings isn't carried: it could be anything.
fn new_file_id(old_trailer: &Dictionary, written: &Sha256, source_len: u64) -> Object {
    let mut hash = written.clone();
    hash.update(source_len.to_be_bytes());
    let parts = match old_trailer.get(b"ID") {
        Ok(Object::Array(parts)) if parts.len() == 2 && parts.iter().all(|part| matches!(part, Object::String(..))) => {
            Some(parts)
        }
        _ => None,
    };
    if let Some(Object::String(previous, _)) = parts.and_then(|parts| parts.get(1)) {
        hash.update(previous);
    }
    let digest = hash.finalize();
    let changing = Object::String(digest[..16].to_vec(), crate::StringFormat::Hexadecimal);
    let permanent = parts
        .and_then(|parts| parts.first().cloned())
        .unwrap_or_else(|| changing.clone());
    Object::Array(vec![permanent, changing])
}

impl IndexedReader {
    /// Check a finished rewrite against what was written, and return a reader of the new file: nothing should replace anything
    /// before this passes. Every byte of it is compared by hash, in small pieces. It has to open as one section with no `/Prev` and no rescan,
    /// every object has to be where it was put and read back, what's dead has to be missing, and it has to have the pages it should.
    pub(super) fn verify_rewritten(
        &self, written_source: Arc<dyn RandomAccessSource>, summary: &RewriteSummary, written: &Written,
    ) -> RewriteResult<IndexedReader> {
        let failed = |reason| Err(RewriteError::VerificationFailed { reason });
        self.whole_file_source().validate_unchanged()?;
        if written_source.len()? != summary.new_len {
            return failed("its length isn't what was written");
        }
        let mut hash = Sha256::new();
        let mut buffer = vec![0_u8; COPY_CHUNK_BYTES];
        let mut position = 0_u64;
        while position < summary.new_len {
            let step = usize::try_from(summary.new_len - position).map_or(COPY_CHUNK_BYTES, |left| left.min(COPY_CHUNK_BYTES));
            written_source.read_exact_at(position, &mut buffer[..step])?;
            hash.update(&buffer[..step]);
            position += step as u64;
        }
        drop(buffer);
        if <[u8; 32]>::from(hash.finalize()) != summary.sha256 {
            return failed("its bytes aren't what was written");
        }
        let reader = Self::open_shared(written_source, self.options.clone())?;
        let index = &reader.index;
        if index.recovered {
            return failed("it needed a rescan to open");
        }
        if index.xref_start != summary.xref_offset {
            return failed("startxref doesn't name its section");
        }
        if index.chain.sections != 1 || index.trailer.has(b"Prev") || index.trailer.has(b"XRefStm") {
            return failed("it has more than one section");
        }
        if index.source_origin != 0 {
            return failed("its header isn't at the start");
        }
        if index.declared_size != summary.size {
            return failed("/Size is wrong");
        }
        let live_entries = index
            .locations
            .values()
            .filter(|location| !matches!(location, ObjectLocation64::Free { .. }))
            .count();
        if live_entries != written.entries.len() {
            return failed("it holds a different number of objects");
        }
        let mut reuse = ContainerReuse::with_budget(RETAINED_CONTAINER_BYTES);
        for entry in &written.entries {
            let location = index.locations.get(&entry.number);
            let (matches, id) = if entry.compressed {
                (
                    location
                        == Some(&ObjectLocation64::Compressed {
                            container: u32::try_from(entry.first).unwrap_or(u32::MAX),
                            index: u32::from(entry.second),
                        }),
                    (entry.number, 0),
                )
            } else {
                (
                    location
                        == Some(&ObjectLocation64::Normal {
                            offset: entry.first,
                            generation: entry.second,
                        }),
                    (entry.number, entry.second),
                )
            };
            if !matches {
                return failed("an object isn't where it was written");
            }
            let readable = if entry.compressed {
                reader.resolve_object_reusing(id, &mut reuse).is_ok()
            } else {
                reader.written_object_parses(id)
            };
            if !readable {
                return failed("an object that was written can't be read back");
            }
        }
        drop(reuse);
        if written.dead.iter().any(|number| {
            !matches!(
                index.locations.get(&number),
                None | Some(ObjectLocation64::Free { .. })
            )
        }) {
            return failed("something that was removed is still there");
        }
        let root = index
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(|_| RewriteError::VerificationFailed {
                reason: "/Root is gone",
            })?;
        if !matches!(reader.resolve_object(root), Ok(Object::Dictionary(_))) {
            return failed("/Root no longer resolves");
        }
        if let Some(expected) = written.expected_pages
            && reader.page_map_with_container_budget(RETAINED_CONTAINER_BYTES).ok().map(|map| map.len()) != Some(expected)
        {
            return failed("it doesn't have the pages it should");
        }
        Ok(reader)
    }
}
