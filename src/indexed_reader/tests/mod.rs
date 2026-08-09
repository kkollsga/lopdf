//! Shared fixtures and helpers for the indexed-reader test modules.

mod cache;
mod encrypted;
mod errors;
mod framing;
mod lex;
mod open;
mod options;
mod page_map;
mod preflight;
mod resolve;
mod stats;
mod streams;

use super::*;
use crate::encryption::crypt_filters::{Aes128CryptFilter, Aes256CryptFilter, CryptFilter};
use crate::source::BytesSource;
#[cfg(any(unix, windows))]
use crate::source::FileSource;
use crate::writer::Writer;
use crate::xref::XrefEntry;
use crate::{Document, EncryptionState, EncryptionVersion, Permissions, StringFormat};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use std::io::Write;
#[cfg(any(unix, windows))]
use std::io::{Seek, SeekFrom};
use std::sync::Mutex;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

type ClassicEntry = (u64, u16, bool);
type ClassicSection = (u32, Vec<ClassicEntry>);

struct ObjectDef<'a> {
    id: u32,
    object_generation: u16,
    xref_generation: u16,
    body: &'a [u8],
}

fn push_object(pdf: &mut Vec<u8>, id: u32, body: &[u8]) -> u64 {
    push_object_with_generation(pdf, id, 0, body)
}

fn push_object_with_generation(pdf: &mut Vec<u8>, id: u32, generation: u16, body: &[u8]) -> u64 {
    let offset = u64::try_from(pdf.len()).unwrap();
    pdf.extend_from_slice(format!("{id} {generation} obj\n").as_bytes());
    pdf.extend_from_slice(body);
    pdf.extend_from_slice(b"\nendobj\n");
    offset
}

fn basic_body() -> (Vec<u8>, Vec<u64>) {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0];
    offsets.push(push_object(&mut pdf, 1, b"<< /Type /Catalog /Pages 2 0 R >>"));
    offsets.push(push_object(&mut pdf, 2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"));
    offsets.push(push_object(
        &mut pdf,
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
    ));
    offsets.push(push_object(&mut pdf, 4, b"<< /Title (base) >>"));
    (pdf, offsets)
}

fn append_classic(pdf: &mut Vec<u8>, sections: &[ClassicSection], trailer: &str) -> u64 {
    let xref = u64::try_from(pdf.len()).unwrap();
    pdf.extend_from_slice(b"xref\n");
    for (start, entries) in sections {
        pdf.extend_from_slice(format!("{start} {}\n", entries.len()).as_bytes());
        for (offset, generation, in_use) in entries {
            let state = if *in_use { 'n' } else { 'f' };
            pdf.extend_from_slice(format!("{offset:010} {generation:05} {state} \n").as_bytes());
        }
    }
    pdf.extend_from_slice(format!("trailer\n{trailer}\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    xref
}

fn classic_pdf() -> Vec<u8> {
    let (mut pdf, offsets) = basic_body();
    let entries = offsets
        .into_iter()
        .enumerate()
        .map(|(id, offset)| (offset, if id == 0 { 65535 } else { 0 }, id != 0))
        .collect();
    append_classic(&mut pdf, &[(0, entries)], "<< /Size 5 /Root 1 0 R /Info 4 0 R >>");
    pdf
}

fn malformed_startxref_pdf(value: &str) -> Vec<u8> {
    format!("%PDF-1.7\nstartxref\n{value}\n%%EOF\n").into_bytes()
}

fn object_pdf(definitions: &[ObjectDef<'_>]) -> Vec<u8> {
    object_pdf_with_root(definitions, (1, 0))
}

fn object_pdf_with_root(definitions: &[ObjectDef<'_>], root: crate::ObjectId) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let max_id = definitions.iter().map(|definition| definition.id).max().unwrap_or(0);
    let mut entries = vec![(0, 65535, false); usize::try_from(max_id).unwrap() + 1];
    for definition in definitions {
        let offset =
            push_object_with_generation(&mut pdf, definition.id, definition.object_generation, definition.body);
        entries[usize::try_from(definition.id).unwrap()] = (offset, definition.xref_generation, true);
    }
    append_classic(
        &mut pdf,
        &[(0, entries)],
        &format!("<< /Size {} /Root {} {} R >>", u64::from(max_id) + 1, root.0, root.1),
    );
    pdf
}

fn xref_number_header_mismatch_fixture() -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let offset = push_object(&mut pdf, 2, b"<< /Type /Catalog >>");
    append_classic(&mut pdf, &[(1, vec![(offset, 0, true)])], "<< /Size 3 /Root 1 0 R >>");
    pdf
}

fn malformed_normal_xref_fixture(object_id: u32, offset_delta: u64, body: &[u8]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let object_offset = push_object(&mut pdf, object_id, body);
    let malformed_offset = object_offset.checked_add(offset_delta).unwrap();
    append_classic(
        &mut pdf,
        &[(object_id, vec![(malformed_offset, 0, true)])],
        &format!("<< /Size {} >>", u64::from(object_id) + 1),
    );
    pdf
}

fn nul_header_probe_fixture() -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    push_object(&mut pdf, 1, b"(unreachable)");
    let malformed_offset = u64::try_from(pdf.len()).unwrap();
    pdf.extend(std::iter::repeat_n(
        b'\0',
        usize::try_from(INDIRECT_HEADER_LIMIT).unwrap() + 1,
    ));
    append_classic(&mut pdf, &[(1, vec![(malformed_offset, 0, true)])], "<< /Size 2 >>");
    pdf
}

fn encrypted_pdf(revision: u8, owner: &str, user: &str) -> Vec<u8> {
    encrypted_pdf_with_stream(revision, owner, user, b"encrypted stream")
}

fn encrypted_pdf_with_stream(revision: u8, owner: &str, user: &str, stream_content: &[u8]) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    document.objects.insert(
        (1, 0),
        Object::String(b"encrypted string".to_vec(), StringFormat::Literal),
    );
    document.objects.insert(
        (2, 0),
        Object::Stream(Stream::new(
            dictionary! { "Type" => "Metadata" },
            stream_content.to_vec(),
        )),
    );
    document.objects.insert(
        (3, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Sentinel" => Object::Reference((1, 0)) }),
    );
    document.max_id = 3;
    document.trailer.set("Root", Object::Reference((3, 0)));
    let id = vec![0x42; 16];
    document.trailer.set(
        "ID",
        Object::Array(vec![
            Object::String(id.clone(), StringFormat::Literal),
            Object::String(id, StringFormat::Literal),
        ]),
    );

    let aes128: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
    let aes256: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
    let file_key = [0x5a; 32];
    let state = match revision {
        2 => EncryptionState::try_from(EncryptionVersion::V1 {
            document: &document,
            owner_password: owner,
            user_password: user,
            permissions: Permissions::PRINTABLE,
        }),
        3 => EncryptionState::try_from(EncryptionVersion::V2 {
            document: &document,
            owner_password: owner,
            user_password: user,
            key_length: 128,
            permissions: Permissions::PRINTABLE,
        }),
        4 => EncryptionState::try_from(EncryptionVersion::V4 {
            document: &document,
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes128)]),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: owner,
            user_password: user,
            permissions: Permissions::PRINTABLE,
        }),
        #[allow(deprecated)]
        5 => EncryptionState::try_from(EncryptionVersion::R5 {
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes256.clone())]),
            file_encryption_key: &file_key,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: owner,
            user_password: user,
            permissions: Permissions::PRINTABLE,
        }),
        6 => EncryptionState::try_from(EncryptionVersion::V5 {
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes256)]),
            file_encryption_key: &file_key,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: owner,
            user_password: user,
            permissions: Permissions::PRINTABLE,
        }),
        _ => unreachable!(),
    }
    .unwrap();
    document.encrypt(&state).unwrap();
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

#[cfg(not(target_arch = "wasm32"))]
fn encrypted_malformed_dictionary_pdf(revision: u8, owner: &str, user: &str) -> Vec<u8> {
    encrypted_pdf_fixture(revision, owner, user, b"encrypted stream", true)
}

#[cfg(not(target_arch = "wasm32"))]
fn encrypted_pdf_fixture(
    revision: u8, owner: &str, user: &str, stream_content: &[u8], malformed_dictionary: bool,
) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    document.objects.insert(
        (1, 0),
        Object::String(b"encrypted string".to_vec(), StringFormat::Literal),
    );
    document.objects.insert(
        (2, 0),
        Object::Stream(Stream::new(
            dictionary! { "Type" => "Metadata" },
            stream_content.to_vec(),
        )),
    );
    let root = if malformed_dictionary {
        dictionary! {
            "Type" => "Catalog",
            "Length" => 5,
            "Sentinel" => Object::string_literal("malformed dictionary secret"),
        }
    } else {
        dictionary! { "Type" => "Catalog", "Sentinel" => Object::Reference((1, 0)) }
    };
    document.objects.insert((3, 0), Object::Dictionary(root));
    document.max_id = 3;
    document.trailer.set("Root", Object::Reference((3, 0)));
    let id = vec![0x42; 16];
    document.trailer.set(
        "ID",
        Object::Array(vec![
            Object::String(id.clone(), StringFormat::Literal),
            Object::String(id, StringFormat::Literal),
        ]),
    );

    let aes128: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
    let aes256: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
    let file_key = [0x5a; 32];
    let state = match revision {
        2 => EncryptionState::try_from(EncryptionVersion::V1 {
            document: &document,
            owner_password: owner,
            user_password: user,
            permissions: Permissions::PRINTABLE,
        }),
        3 => EncryptionState::try_from(EncryptionVersion::V2 {
            document: &document,
            owner_password: owner,
            user_password: user,
            key_length: 128,
            permissions: Permissions::PRINTABLE,
        }),
        4 => EncryptionState::try_from(EncryptionVersion::V4 {
            document: &document,
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes128)]),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: owner,
            user_password: user,
            permissions: Permissions::PRINTABLE,
        }),
        #[allow(deprecated)]
        5 => EncryptionState::try_from(EncryptionVersion::R5 {
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes256.clone())]),
            file_encryption_key: &file_key,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: owner,
            user_password: user,
            permissions: Permissions::PRINTABLE,
        }),
        6 => EncryptionState::try_from(EncryptionVersion::V5 {
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes256)]),
            file_encryption_key: &file_key,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: owner,
            user_password: user,
            permissions: Permissions::PRINTABLE,
        }),
        _ => unreachable!(),
    }
    .unwrap();
    document.encrypt(&state).unwrap();
    if malformed_dictionary {
        let Object::Dictionary(dictionary) = document.objects.remove(&(3, 0)).unwrap() else {
            unreachable!("the encrypted malformed fixture root stays a dictionary")
        };
        document
            .objects
            .insert((3, 0), Object::Stream(Stream::new(dictionary, b"hello".to_vec())));
    }
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

fn inline_encrypt_dictionary(mut pdf: Vec<u8>) -> Vec<u8> {
    let trailer_encrypt = rfind(&pdf, b"/Encrypt ").unwrap() + b"/Encrypt ".len();
    let mut cursor = TokenCursor::new(&pdf[trailer_encrypt..]);
    let id = u32::try_from(cursor.unsigned().unwrap()).unwrap();
    let generation = u16::try_from(cursor.unsigned().unwrap()).unwrap();
    cursor.expect(b"R").unwrap();
    let reference_len = pdf[trailer_encrypt..].len() - cursor.remaining().len();
    let header = format!("{id} {generation} obj\n");
    let start = pdf
        .windows(header.len())
        .position(|window| window == header.as_bytes())
        .unwrap()
        + header.len();
    let end = start
        + pdf[start..]
            .windows(b"\nendobj".len())
            .position(|w| w == b"\nendobj")
            .unwrap();
    let dictionary = pdf[start..end].to_vec();
    pdf.splice(trailer_encrypt..trailer_encrypt + reference_len, dictionary);
    pdf
}

fn encrypted_object_stream_pdf(revision: u8, flate: bool, member_padding: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    encrypted_object_stream_pdf_with_eol(revision, flate, member_padding, b"\n")
}

fn encrypted_object_stream_pdf_with_eol(
    revision: u8, flate: bool, member_padding: usize, stream_eol: &[u8],
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    encrypted_object_stream_pdf_with_eol_and_prefix(revision, flate, member_padding, stream_eol, 0)
}

fn encrypted_object_stream_pdf_with_eol_and_prefix(
    revision: u8, flate: bool, member_padding: usize, stream_eol: &[u8], prefix_padding: usize,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    encrypted_object_stream_pdf_with_options(revision, flate, member_padding, stream_eol, prefix_padding, None)
}

fn encrypted_object_stream_pdf_with_options(
    revision: u8, flate: bool, member_padding: usize, stream_eol: &[u8], prefix_padding: usize,
    metadata_override: Option<(i64, i64, i64)>,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    const CONTAINER_ID: u32 = 5;
    const IMAGE_ID: u32 = 20;
    const STRING_ID: u32 = 21;
    const ENCRYPT_ID: u32 = 30;
    const XREF_ID: u32 = 31;
    assert!(matches!(stream_eol, b"\n" | b"\r" | b"\r\n"));

    let mut document = Document::with_version("1.7");
    let file_id = vec![0x24; 16];
    document.trailer.set(
        "ID",
        Object::Array(vec![
            Object::String(file_id.clone(), StringFormat::Literal),
            Object::String(file_id.clone(), StringFormat::Literal),
        ]),
    );
    let aes128: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
    let aes256: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
    let file_key = [0x5a; 32];
    let state = match revision {
        2 => EncryptionState::try_from(EncryptionVersion::V1 {
            document: &document,
            owner_password: "owner",
            user_password: "user",
            permissions: Permissions::PRINTABLE,
        }),
        3 => EncryptionState::try_from(EncryptionVersion::V2 {
            document: &document,
            owner_password: "owner",
            user_password: "user",
            key_length: 128,
            permissions: Permissions::PRINTABLE,
        }),
        4 => EncryptionState::try_from(EncryptionVersion::V4 {
            document: &document,
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes128)]),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password: "user",
            permissions: Permissions::PRINTABLE,
        }),
        #[allow(deprecated)]
        5 => EncryptionState::try_from(EncryptionVersion::R5 {
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes256.clone())]),
            file_encryption_key: &file_key,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password: "user",
            permissions: Permissions::PRINTABLE,
        }),
        6 => EncryptionState::try_from(EncryptionVersion::V5 {
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes256)]),
            file_encryption_key: &file_key,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password: "user",
            permissions: Permissions::PRINTABLE,
        }),
        _ => unreachable!(),
    }
    .unwrap();

    let mut member = b"<< /Type /Catalog /Text (member secret) /Image 20 0 R /Pad (".to_vec();
    member.extend(std::iter::repeat_n(b'x', member_padding));
    member.extend_from_slice(b") >>");
    let (first, content) = object_stream_content(&[(10, member.as_slice())]);
    let (declared_n, declared_first, declared_length) = metadata_override
        .map(|(n, first, length)| (n, first, Some(length)))
        .unwrap_or((1, i64::try_from(first).unwrap(), None));
    let mut container_dictionary = Dictionary::from_iter([
        (b"Type".to_vec(), Object::Name(b"ObjStm".to_vec())),
        (b"N".to_vec(), Object::Integer(declared_n)),
        (b"First".to_vec(), Object::Integer(declared_first)),
    ]);
    if prefix_padding != 0 {
        container_dictionary.set("PrefixPad", Object::Name(vec![b'x'; prefix_padding]));
    }
    // ISO 32000-1 §7.3.8.1 only permits LF or CRLF after the `stream` keyword. The bare-CR
    // variant these fixtures can emit is deliberate — it pins the lenient recovery both the
    // eager and the indexed reader implement — but it is only unambiguous while the first
    // payload byte is not itself an EOL byte: `stream\r` followed by 0x0A reads as a legal
    // CRLF and silently swallows a payload byte. AES crypt filters prefix the ciphertext with
    // a random IV, so re-encrypt a fresh copy of the plaintext until the leading byte cannot
    // be misread. RC4 revisions are deterministic, hence the bounded attempt count.
    let plain_container = Object::Stream(Stream::new(container_dictionary, content));
    let mut attempts = 0_u32;
    let mut container = loop {
        let mut candidate = plain_container.clone();
        if flate {
            candidate.as_stream_mut().unwrap().compress().unwrap();
        }
        encryption::encrypt_object(&state, (CONTAINER_ID, 0), &mut candidate).unwrap();
        let leading = candidate.as_stream().unwrap().content.first().copied();
        if stream_eol != b"\r" || !matches!(leading, Some(b'\n' | b'\r')) {
            break candidate;
        }
        attempts += 1;
        assert!(
            attempts < 64,
            "bare-CR encrypted ObjStm fixture (revision {revision}, flate {flate}) could not \
             produce a leading ciphertext byte outside {{CR, LF}}"
        );
    };
    if let Some(declared_length) = declared_length {
        container.as_stream_mut().unwrap().dict.set("Length", declared_length);
    }

    let image_plaintext = b"shared encrypted image".to_vec();
    let mut image = Object::Stream(Stream::new(
        Dictionary::from_iter([
            (b"Type".to_vec(), Object::Name(b"XObject".to_vec())),
            (b"Subtype".to_vec(), Object::Name(b"Image".to_vec())),
        ]),
        image_plaintext.clone(),
    ));
    encryption::encrypt_object(&state, (IMAGE_ID, 0), &mut image).unwrap();
    let mut string = Object::String(b"normal secret".to_vec(), StringFormat::Literal);
    encryption::encrypt_object(&state, (STRING_ID, 0), &mut string).unwrap();
    let encrypt = Object::Dictionary(state.encode().unwrap());

    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = BTreeMap::new();
    for (id, object) in [
        (CONTAINER_ID, container),
        (IMAGE_ID, image),
        (STRING_ID, string),
        (ENCRYPT_ID, encrypt),
    ] {
        let offset = u64::try_from(pdf.len()).unwrap();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        if id == CONTAINER_ID {
            let mut rendered = Vec::new();
            Writer::write_object(&mut rendered, &object).unwrap();
            let marker = rendered
                .windows(b"stream\n".len())
                .position(|window| window == b"stream\n")
                .unwrap();
            rendered.splice(
                marker + b"stream".len()..marker + b"stream\n".len(),
                stream_eol.iter().copied(),
            );
            pdf.extend_from_slice(&rendered);
        } else {
            Writer::write_object(&mut pdf, &object).unwrap();
        }
        pdf.extend_from_slice(b"\nendobj\n");
        offsets.insert(id, offset);
    }

    let xref_offset = u64::try_from(pdf.len()).unwrap();
    let mut xref_content = Vec::new();
    for id in 0..=XREF_ID {
        if id == 10 {
            encode_field(2, 1, &mut xref_content);
            encode_field(u64::from(CONTAINER_ID), 8, &mut xref_content);
            encode_field(0, 4, &mut xref_content);
        } else if id == XREF_ID {
            encode_field(1, 1, &mut xref_content);
            encode_field(xref_offset, 8, &mut xref_content);
            encode_field(0, 4, &mut xref_content);
        } else if let Some(offset) = offsets.get(&id) {
            encode_field(1, 1, &mut xref_content);
            encode_field(*offset, 8, &mut xref_content);
            encode_field(0, 4, &mut xref_content);
        } else {
            encode_field(0, 1, &mut xref_content);
            encode_field(0, 8, &mut xref_content);
            encode_field(0, 4, &mut xref_content);
        }
    }
    let xref = Object::Stream(Stream::new(
        Dictionary::from_iter([
            (b"Type".to_vec(), Object::Name(b"XRef".to_vec())),
            (b"Size".to_vec(), Object::Integer(i64::from(XREF_ID + 1))),
            (b"Root".to_vec(), Object::Reference((10, 0))),
            (b"Encrypt".to_vec(), Object::Reference((ENCRYPT_ID, 0))),
            (
                b"ID".to_vec(),
                Object::Array(vec![
                    Object::String(file_id.clone(), StringFormat::Literal),
                    Object::String(file_id, StringFormat::Literal),
                ]),
            ),
            (
                b"W".to_vec(),
                Object::Array(vec![Object::Integer(1), Object::Integer(8), Object::Integer(4)]),
            ),
        ]),
        xref_content.clone(),
    ));
    pdf.extend_from_slice(format!("{XREF_ID} 0 obj\n").as_bytes());
    Writer::write_object(&mut pdf, &xref).unwrap();
    pdf.extend_from_slice(format!("\nendobj\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());
    (pdf, image_plaintext, xref_content)
}

fn generated_page_tree_pdf(page_count: u32, declared_count: i64) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    let kids: Vec<_> = (0..page_count)
        .map(|index| {
            let id = (index + 3, 0);
            document.objects.insert(
                id,
                Object::Dictionary(dictionary! {
                    "Type" => "Page",
                    "Parent" => Object::Reference((2, 0)),
                    "Index" => i64::from(index),
                }),
            );
            Object::Reference(id)
        })
        .collect();
    document.objects.insert(
        (2, 0),
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => declared_count,
            "Resources" => Object::Dictionary(dictionary! { "Marker" => "root" }),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }),
    );
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    document.max_id = page_count + 2;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

/// A flat `page_count`-leaf page tree whose leaves each reference their own
/// content stream, which is the shape a page walk followed by a per-page
/// `/Contents` read actually traverses.
fn page_tree_with_contents_pdf(page_count: u32) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    let kids: Vec<_> = (0..page_count)
        .map(|index| {
            let page_id = (index * 2 + 3, 0);
            let content_id = (index * 2 + 4, 0);
            document.objects.insert(
                content_id,
                Object::Stream(Stream::new(
                    Dictionary::new(),
                    format!("BT ({index}) Tj ET").into_bytes(),
                )),
            );
            document.objects.insert(
                page_id,
                Object::Dictionary(dictionary! {
                    "Type" => "Page",
                    "Parent" => Object::Reference((2, 0)),
                    "Contents" => Object::Reference(content_id),
                }),
            );
            Object::Reference(page_id)
        })
        .collect();
    document.objects.insert(
        (2, 0),
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => i64::from(page_count),
            "Resources" => Object::Dictionary(dictionary! { "Marker" => "root" }),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }),
    );
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    document.max_id = page_count * 2 + 2;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

fn encrypted_page_tree_pdf() -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    document.objects.insert(
        (2, 0),
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference((3, 0)), Object::Reference((4, 0))],
            "Count" => 99,
            "Rotate" => 90,
        }),
    );
    for id in [3, 4] {
        document.objects.insert(
            (id, 0),
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => Object::Reference((2, 0)),
                "Secret" => format!("page-{id}"),
            }),
        );
    }
    document.max_id = 4;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let file_id = vec![0x61; 16];
    document.trailer.set(
        "ID",
        Object::Array(vec![
            Object::String(file_id.clone(), StringFormat::Literal),
            Object::String(file_id, StringFormat::Literal),
        ]),
    );
    let aes128: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
    let state = EncryptionState::try_from(EncryptionVersion::V4 {
        document: &document,
        encrypt_metadata: true,
        crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), aes128)]),
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password: "owner",
        user_password: "user",
        permissions: Permissions::PRINTABLE,
    })
    .unwrap();
    document.encrypt(&state).unwrap();
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

fn generated_deep_page_tree_pdf(leaf_depth: usize) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    for depth in 0..=leaf_depth {
        let id = u32::try_from(depth).unwrap() + 2;
        let object = if depth == leaf_depth {
            Object::Dictionary(dictionary! { "Type" => "Page" })
        } else {
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference((id + 1, 0))],
                "Count" => 1,
            })
        };
        document.objects.insert((id, 0), object);
    }
    document.max_id = u32::try_from(leaf_depth).unwrap() + 2;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

fn cyclic_page_tree_pdf() -> Vec<u8> {
    object_pdf(&[
        ObjectDef {
            id: 1,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Catalog /Pages 2 0 R >>",
        },
        ObjectDef {
            id: 2,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 999999 >>",
        },
        ObjectDef {
            id: 3,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Pages /Kids [2 0 R 5 0 R] /Count -1 >>",
        },
        ObjectDef {
            id: 4,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Page >>",
        },
        ObjectDef {
            id: 5,
            object_generation: 0,
            xref_generation: 0,
            body: b"<< /Type /Page >>",
        },
    ])
}

fn assert_page_map_snapshot_matches_legacy(reader: &IndexedReader) {
    let legacy_page_map = reader.page_map().unwrap();
    let legacy_stats = reader.index_stats().unwrap();
    let (page_map, stats) = reader.page_map_with_stats().unwrap();
    assert_eq!(page_map, legacy_page_map);
    assert_eq!(stats, legacy_stats);
    assert_eq!(stats.page_count(), page_map.len());
}

fn assert_indexed_errors_equal(expected: &IndexedReaderError, actual: &IndexedReaderError) {
    assert_eq!(std::mem::discriminant(expected), std::mem::discriminant(actual));
    assert_eq!(expected.to_string(), actual.to_string());
    assert_eq!(format!("{expected:?}"), format!("{actual:?}"));
}

fn assert_page_map_snapshot_error_matches_legacy(reader: &IndexedReader) {
    let page_map_error = reader.page_map().unwrap_err();
    let stats_error = reader.index_stats().unwrap_err();
    let combined_error = reader.page_map_with_stats().unwrap_err();
    assert_indexed_errors_equal(&page_map_error, &stats_error);
    assert_indexed_errors_equal(&page_map_error, &combined_error);
}

fn repeated_page_dag_pdf(levels: u32) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    for level in 0..levels {
        let id = level + 2;
        let child = (id + 1, 0);
        document.objects.insert(
            (id, 0),
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(child), Object::Reference(child)],
                "Count" => 1_i64 << levels.min(30),
            }),
        );
    }
    let leaf = (levels + 2, 0);
    document
        .objects
        .insert(leaf, Object::Dictionary(dictionary! { "Type" => "Page" }));
    document.max_id = leaf.0;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

fn wide_page_tree_pdf(distinct_nodes: u32, non_reference_kids: usize, self_cycle: bool) -> Vec<u8> {
    assert!(distinct_nodes > 0);
    let mut document = Document::with_version("1.7");
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    for node in 0..distinct_nodes {
        let id = node + 2;
        let child = if self_cycle || node + 1 == distinct_nodes {
            (id, 0)
        } else {
            (id + 1, 0)
        };
        let mut kids = Vec::with_capacity(non_reference_kids + 1);
        kids.push(Object::Reference(child));
        kids.extend(std::iter::repeat_n(Object::Null, non_reference_kids));
        document.objects.insert(
            (id, 0),
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 0,
            }),
        );
    }
    document.max_id = distinct_nodes + 1;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

/// A finite chain of `distinct_nodes` wide `/Pages` nodes: each carries one reference kid
/// followed by `non_reference_kids` nulls, and the last carries nulls only. Unlike
/// [`wide_page_tree_pdf`] no node ever points back at itself, so the walk ends by exhausting
/// its work budget rather than by hitting the depth cap.
fn wide_page_tree_chain_pdf(distinct_nodes: u32, non_reference_kids: usize) -> Vec<u8> {
    assert!(distinct_nodes > 0);
    let mut document = Document::with_version("1.7");
    document.objects.insert(
        (1, 0),
        Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference((2, 0)) }),
    );
    for node in 0..distinct_nodes {
        let id = node + 2;
        let mut kids = Vec::with_capacity(non_reference_kids + 1);
        if node + 1 < distinct_nodes {
            kids.push(Object::Reference((id + 1, 0)));
        }
        kids.extend(std::iter::repeat_n(Object::Null, non_reference_kids));
        document.objects.insert(
            (id, 0),
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 0,
            }),
        );
    }
    document.max_id = distinct_nodes + 1;
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut pdf = Vec::new();
    document.save_to(&mut pdf).unwrap();
    pdf
}

fn open_encrypted(pdf: &[u8], password: Option<&[u8]>) -> IndexedReaderResult<IndexedReader> {
    IndexedReader::open_with_password(
        Arc::new(BytesSource::from(pdf.to_vec())),
        ResolverLimits::default(),
        password,
    )
}

fn assert_encrypted_fixture_plaintext(reader: &IndexedReader) {
    assert_eq!(
        reader.resolve_object((1, 0)).unwrap(),
        Object::String(b"encrypted string".to_vec(), StringFormat::Literal)
    );
    assert_eq!(
        reader.resolve_object((2, 0)).unwrap().as_stream().unwrap().content,
        b"encrypted stream"
    );
    assert_eq!(
        reader
            .resolve_object((3, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Sentinel")
            .unwrap(),
        &Object::Reference((1, 0))
    );
}

fn open_reader(pdf: &[u8], limits: ResolverLimits) -> IndexedReader {
    IndexedReader::open_with_limits(Arc::new(BytesSource::from(pdf.to_vec())), limits).unwrap()
}

fn encode_field(value: u64, width: usize, output: &mut Vec<u8>) {
    for shift in (0..width).rev() {
        output.push((value >> (shift * 8)) as u8);
    }
}

fn xref_stream_pdf(compressed: bool) -> Vec<u8> {
    let (mut pdf, offsets) = basic_body();
    let xref = u64::try_from(pdf.len()).unwrap();
    let mut decoded = Vec::new();
    encode_field(0, 1, &mut decoded);
    encode_field(0, 8, &mut decoded);
    encode_field(65535, 2, &mut decoded);
    for offset in offsets.iter().skip(1).copied().chain(std::iter::once(xref)) {
        encode_field(1, 1, &mut decoded);
        encode_field(offset, 8, &mut decoded);
        encode_field(0, 2, &mut decoded);
    }
    let (content, filter) = if compressed {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&decoded).unwrap();
        (encoder.finish().unwrap(), " /Filter /FlateDecode")
    } else {
        (decoded, "")
    };
    pdf.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /XRef /Size 6 /Root 1 0 R /Info 4 0 R /W [1 8 2] /Length {}{} >>\nstream\n",
            content.len(),
            filter
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&content);
    pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    pdf
}

struct ObjectStreamFixture {
    pdf: Vec<u8>,
    container_stream_start: u64,
    container_stream_length: u64,
}

fn object_stream_content(members: &[(u32, &[u8])]) -> (usize, Vec<u8>) {
    let mut header = Vec::new();
    let mut bodies = Vec::new();
    for (id, body) in members {
        header.extend_from_slice(format!("{id} {} ", bodies.len()).as_bytes());
        bodies.extend_from_slice(body);
        bodies.push(b'\n');
    }
    let first = header.len();
    header.extend_from_slice(&bodies);
    (first, header)
}

fn object_stream_fixture(dictionary: &str, content: &[u8], compressed_entries: &[(u32, u32)]) -> ObjectStreamFixture {
    object_stream_fixture_with_declared_length(
        dictionary,
        content,
        compressed_entries,
        i64::try_from(content.len()).unwrap(),
    )
}

fn object_stream_fixture_with_declared_length(
    dictionary: &str, content: &[u8], compressed_entries: &[(u32, u32)], declared_length: i64,
) -> ObjectStreamFixture {
    const CONTAINER_ID: u32 = 5;
    const XREF_ID: u32 = 6;

    let mut pdf = b"%PDF-1.7\n".to_vec();
    let container_offset = u64::try_from(pdf.len()).unwrap();
    let prefix = format!("{CONTAINER_ID} 0 obj\n<< {dictionary} /Length {declared_length} >>\nstream\n");
    pdf.extend_from_slice(prefix.as_bytes());
    let container_stream_start = u64::try_from(pdf.len()).unwrap();
    pdf.extend_from_slice(content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_offset = u64::try_from(pdf.len()).unwrap();
    let size = compressed_entries
        .iter()
        .map(|(id, _)| *id)
        .max()
        .unwrap_or(XREF_ID)
        .max(XREF_ID)
        + 1;
    let mut xref_content = Vec::new();
    for id in 0..size {
        if id == CONTAINER_ID {
            encode_field(1, 1, &mut xref_content);
            encode_field(container_offset, 8, &mut xref_content);
            encode_field(0, 4, &mut xref_content);
        } else if id == XREF_ID {
            encode_field(1, 1, &mut xref_content);
            encode_field(xref_offset, 8, &mut xref_content);
            encode_field(0, 4, &mut xref_content);
        } else if let Some((_, index)) = compressed_entries.iter().find(|(target, _)| *target == id) {
            encode_field(2, 1, &mut xref_content);
            encode_field(u64::from(CONTAINER_ID), 8, &mut xref_content);
            encode_field(u64::from(*index), 4, &mut xref_content);
        } else {
            encode_field(0, 1, &mut xref_content);
            encode_field(0, 8, &mut xref_content);
            encode_field(0, 4, &mut xref_content);
        }
    }
    let root = compressed_entries.first().map(|(id, _)| *id).unwrap_or(CONTAINER_ID);
    pdf.extend_from_slice(
        format!(
            "{XREF_ID} 0 obj\n<< /Type /XRef /Size {size} /Root {root} 0 R /W [1 8 4] /Length {} >>\nstream\n",
            xref_content.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&xref_content);
    pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes());

    ObjectStreamFixture {
        pdf,
        container_stream_start,
        container_stream_length: u64::try_from(content.len()).unwrap(),
    }
}

fn open_bytes(pdf: &[u8]) -> PdfIndex {
    PdfIndex::open(Arc::new(BytesSource::from(pdf.to_vec()))).unwrap()
}

/// Open through the cross-reference sections only, with the rescan recovery
/// suppressed, so a test can pin what the xref machinery itself accepts.
fn open_without_recovery(source: Arc<dyn RandomAccessSource>) -> IndexedReaderResult<PdfIndex> {
    let source_len = source.len()?;
    let (source_origin, version) = read_header(source.as_ref(), source_len)?;
    PdfIndex::open_from_xref(&source, source_len, source_origin, &version)
}

fn assert_eager_normal_fingerprint(pdf: &[u8], index: &PdfIndex) {
    let eager = Document::load_mem(pdf).unwrap();
    assert_eq!(index.version, eager.version);
    assert_eq!(index.xref_start, u64::try_from(eager.xref_start).unwrap());
    assert_eq!(index.declared_size, u64::from(eager.reference_table.size));
    for (&id, eager_entry) in &eager.reference_table.entries {
        // Normal entries are the shared behavioral surface. The one
        // deliberate divergence is documented on ObjectLocation64::Free.
        if let XrefEntry::Normal { offset, generation } = eager_entry {
            assert_eq!(
                index.locations.get(&id),
                Some(&ObjectLocation64::Normal {
                    offset: u64::from(*offset),
                    generation: *generation
                })
            );
        }
    }
    assert_eq!(
        index.trailer.get(b"Root").unwrap().as_reference().unwrap(),
        eager.trailer.get(b"Root").unwrap().as_reference().unwrap()
    );
}

/// Uncompressed `/Type /ObjStm` holding a single generation-zero member.
fn push_single_member_object_stream(pdf: &mut Vec<u8>, container: u32, member: u32, body: &str) -> u64 {
    let header = format!("{member} 0 ");
    let content = format!("{header}{body}");
    let offset = u64::try_from(pdf.len()).unwrap();
    pdf.extend_from_slice(
        format!(
            "{container} 0 obj\n<< /Type /ObjStm /N 1 /First {} /Length {} >>\nstream\n",
            header.len(),
            content.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(content.as_bytes());
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    offset
}

/// Cross-reference stream describing exactly one compressed member, for use
/// as a classic section's `/XRefStm` supplement.
fn push_compressed_supplement(pdf: &mut Vec<u8>, id: u32, member: u32, container: u32, size: u32) -> u64 {
    let mut encoded = Vec::new();
    encode_field(2, 1, &mut encoded);
    encode_field(u64::from(container), 8, &mut encoded);
    encode_field(0, 2, &mut encoded);
    let offset = u64::try_from(pdf.len()).unwrap();
    pdf.extend_from_slice(
        format!(
            "{id} 0 obj\n<< /Type /XRef /Size {size} /Index [{member} 1] /W [1 8 2] /Length {} >>\nstream\n",
            encoded.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&encoded);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    offset
}

/// A hybrid-reference file whose newest section carries no `/XRefStm` of its
/// own: object 6 is superseded across two object streams, and only the
/// *second* section's supplement names the newer container. Both readers
/// must walk every section's own supplement — and merge it before
/// descending to that section's `/Prev` — to land on the newest revision
/// (ISO 32000-1, 7.5.8.4).
fn hybrid_superseded_across_object_streams() -> Vec<u8> {
    let (mut pdf, offsets) = basic_body();
    let base_container = push_single_member_object_stream(&mut pdf, 5, 6, "<< /Rev (base) >>");
    let base_supplement = push_compressed_supplement(&mut pdf, 7, 6, 5, 11);
    let mut base_entries: Vec<ClassicEntry> = offsets
        .into_iter()
        .enumerate()
        .map(|(id, offset)| (offset, if id == 0 { 65535 } else { 0 }, id != 0))
        .collect();
    base_entries.push((base_container, 0, true));
    let base_xref = append_classic(
        &mut pdf,
        &[(0, base_entries)],
        &format!("<< /Size 11 /Root 1 0 R /Info 4 0 R /XRefStm {base_supplement} >>"),
    );

    let update_container = push_single_member_object_stream(&mut pdf, 8, 6, "<< /Rev (update) >>");
    let update_supplement = push_compressed_supplement(&mut pdf, 9, 6, 8, 11);
    let update_xref = append_classic(
        &mut pdf,
        &[
            (0, vec![(0, 65535, false)]),
            // The hybrid mask: a legacy reader must not see object 6.
            (6, vec![(0, 65535, false)]),
            (8, vec![(update_container, 0, true), (update_supplement, 0, true)]),
        ],
        &format!("<< /Size 11 /Root 1 0 R /Info 4 0 R /Prev {base_xref} /XRefStm {update_supplement} >>"),
    );

    let newest_info = push_object(&mut pdf, 10, b"<< /Title (newest) >>");
    append_classic(
        &mut pdf,
        &[(0, vec![(0, 65535, false)]), (10, vec![(newest_info, 0, true)])],
        &format!("<< /Size 11 /Root 1 0 R /Info 10 0 R /Prev {update_xref} >>"),
    );
    pdf
}

fn live_normal_locations(index: &PdfIndex) -> BTreeMap<u32, ObjectLocation64> {
    index
        .locations
        .iter()
        .filter(|(_, location)| matches!(location, ObjectLocation64::Normal { .. }))
        .map(|(id, location)| (*id, location.clone()))
        .collect()
}

fn padded_classic_xref(padding_over_limit: u64) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let xref = u64::try_from(pdf.len()).unwrap();
    let prefix = b"xref\n";
    let suffix = b"trailer\n<< /Size 1 /Root 1 0 R >>";
    pdf.extend_from_slice(prefix);
    let used = u64::try_from(prefix.len() + suffix.len()).unwrap();
    let padding = XREF_WINDOW_LIMIT - used + padding_over_limit;
    pdf.resize(pdf.len() + usize::try_from(padding).unwrap(), 0);
    pdf.extend_from_slice(suffix);
    pdf.extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
    pdf
}

fn compressed_limit_xref(decoded_len: usize) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&vec![0_u8; decoded_len]).unwrap();
    let content = encoder.finish().unwrap();
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let xref = u64::try_from(pdf.len()).unwrap();
    pdf.extend_from_slice(
        format!(
            "1 0 obj\n<< /Type /XRef /Size 1 /Index [0 1] /W [1 0 0] /Filter /FlateDecode /Length {} >>\nstream\n",
            content.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&content);
    pdf.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    pdf
}

fn empty_width_xref(width: u64) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let xref = u64::try_from(pdf.len()).unwrap();
    pdf.extend_from_slice(
        format!(
            "1 0 obj\n<< /Type /XRef /Size 0 /Index [0 0] /W [1 {width} 1] /Length 0 >>\nstream\n\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n"
        )
        .as_bytes(),
    );
    pdf
}

fn configure_test_caches(reader: &mut IndexedReader, total_bytes: usize, total_entries: usize) {
    reader.configure_resolution_caches(total_bytes / 2, total_entries / 2, total_bytes / 4, total_entries / 4);
}

/// A PDF with `count` tiny integer objects (`2 0 obj` upwards) behind object 1.
fn wide_object_pdf(count: u32) -> Vec<u8> {
    let bodies: Vec<Vec<u8>> = (0..count)
        .map(|index| format!("<< /Index {index} >>").into_bytes())
        .collect();
    let mut definitions: Vec<ObjectDef<'_>> = vec![ObjectDef {
        id: 1,
        object_generation: 0,
        xref_generation: 0,
        body: b"<< /Type /Catalog >>",
    }];
    for (index, body) in bodies.iter().enumerate() {
        definitions.push(ObjectDef {
            id: u32::try_from(index).unwrap() + 2,
            object_generation: 0,
            xref_generation: 0,
            body: body.as_slice(),
        });
    }
    object_pdf(&definitions)
}

fn read_all_encoded(descriptor: &IndexedStreamDescriptor, chunk_bytes: usize) -> Vec<u8> {
    let mut reader = descriptor.open_plain_encoded().unwrap();
    let mut chunk = vec![0; chunk_bytes];
    let mut bytes = Vec::new();
    loop {
        let read = reader.read_chunk(&mut chunk).unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    bytes
}

struct OverlaySource {
    len: u64,
    regions: Vec<(u64, Vec<u8>)>,
    requests: Mutex<Vec<(u64, usize)>>,
}

struct TracingBytesSource {
    bytes: Vec<u8>,
    requests: Mutex<Vec<(u64, usize)>>,
}

struct LengthTracingBytesSource {
    bytes: Vec<u8>,
    len_calls: AtomicUsize,
    requests: Mutex<Vec<(u64, usize)>>,
}

struct SwitchableFailureSource {
    bytes: Vec<u8>,
    mode: AtomicU8,
}

struct FailOnceSource {
    bytes: Vec<u8>,
    armed: AtomicBool,
    armed_reads: AtomicUsize,
}

#[cfg(not(target_arch = "wasm32"))]
struct FailingOffsetSource {
    bytes: Vec<u8>,
    fail_offset: AtomicU64,
    requests: Mutex<Vec<(u64, usize)>>,
}

impl RandomAccessSource for FailOnceSource {
    fn len(&self) -> Result<u64, SourceError> {
        Ok(u64::try_from(self.bytes.len()).unwrap())
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
        if self.armed.load(Ordering::SeqCst) {
            self.armed_reads.fetch_add(1, Ordering::SeqCst);
            if self.armed.swap(false, Ordering::SeqCst) {
                return Err(SourceError::Io(std::io::Error::other("one-shot positional failure")));
            }
        }
        let offset = usize::try_from(offset).unwrap();
        let length = output.len().min(self.bytes.len().saturating_sub(offset));
        output[..length].copy_from_slice(&self.bytes[offset..offset + length]);
        Ok(length)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl RandomAccessSource for FailingOffsetSource {
    fn len(&self) -> Result<u64, SourceError> {
        Ok(u64::try_from(self.bytes.len()).unwrap())
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
        self.requests.lock().unwrap().push((offset, output.len()));
        if offset == self.fail_offset.load(Ordering::SeqCst) {
            return Err(SourceError::Io(std::io::Error::other(
                "injected bounded tail read failure",
            )));
        }
        let offset = usize::try_from(offset).map_err(|_| SourceError::OutOfBounds {
            offset,
            length: u64::try_from(output.len()).unwrap_or(u64::MAX),
            source_len: u64::try_from(self.bytes.len()).unwrap(),
        })?;
        let length = output.len().min(self.bytes.len().saturating_sub(offset));
        output[..length].copy_from_slice(&self.bytes[offset..offset + length]);
        Ok(length)
    }
}

impl RandomAccessSource for SwitchableFailureSource {
    fn len(&self) -> Result<u64, SourceError> {
        Ok(u64::try_from(self.bytes.len()).unwrap())
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
        match self.mode.load(Ordering::SeqCst) {
            1 => {
                return Err(SourceError::Io(std::io::Error::other(
                    "injected positional read failure",
                )));
            }
            2 => return Ok(0),
            _ => {}
        }
        let offset = usize::try_from(offset).unwrap();
        let read = output.len().min(self.bytes.len().saturating_sub(offset));
        output[..read].copy_from_slice(&self.bytes[offset..offset + read]);
        Ok(read)
    }
}

impl RandomAccessSource for TracingBytesSource {
    fn len(&self) -> Result<u64, SourceError> {
        Ok(u64::try_from(self.bytes.len()).unwrap())
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
        self.requests.lock().unwrap().push((offset, output.len()));
        let offset = usize::try_from(offset).map_err(|_| SourceError::OutOfBounds {
            offset,
            length: u64::try_from(output.len()).unwrap_or(u64::MAX),
            source_len: u64::try_from(self.bytes.len()).unwrap(),
        })?;
        if offset > self.bytes.len() {
            return Ok(0);
        }
        let length = output.len().min(self.bytes.len() - offset);
        output[..length].copy_from_slice(&self.bytes[offset..offset + length]);
        Ok(length)
    }
}

impl RandomAccessSource for LengthTracingBytesSource {
    fn len(&self) -> Result<u64, SourceError> {
        self.len_calls.fetch_add(1, Ordering::SeqCst);
        Ok(u64::try_from(self.bytes.len()).unwrap())
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
        self.requests.lock().unwrap().push((offset, output.len()));
        let offset = usize::try_from(offset).map_err(|_| SourceError::OutOfBounds {
            offset,
            length: u64::try_from(output.len()).unwrap_or(u64::MAX),
            source_len: u64::try_from(self.bytes.len()).unwrap(),
        })?;
        if offset > self.bytes.len() {
            return Ok(0);
        }
        let length = output.len().min(self.bytes.len() - offset);
        output[..length].copy_from_slice(&self.bytes[offset..offset + length]);
        Ok(length)
    }
}

impl RandomAccessSource for OverlaySource {
    fn len(&self) -> Result<u64, SourceError> {
        Ok(self.len)
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, SourceError> {
        if offset > self.len {
            return Err(SourceError::OutOfBounds {
                offset,
                length: 0,
                source_len: self.len,
            });
        }
        self.requests.lock().unwrap().push((offset, output.len()));
        let available = usize::try_from(self.len - offset).unwrap_or(usize::MAX);
        let read = output.len().min(available);
        output[..read].fill(0);
        let read_end = offset + u64::try_from(read).unwrap();
        for (region_offset, bytes) in &self.regions {
            let region_end = *region_offset + u64::try_from(bytes.len()).unwrap();
            let overlap_start = offset.max(*region_offset);
            let overlap_end = read_end.min(region_end);
            if overlap_start < overlap_end {
                let destination = usize::try_from(overlap_start - offset).unwrap();
                let source = usize::try_from(overlap_start - region_offset).unwrap();
                let length = usize::try_from(overlap_end - overlap_start).unwrap();
                output[destination..destination + length].copy_from_slice(&bytes[source..source + length]);
            }
        }
        Ok(read)
    }
}

fn revision_source(count: usize) -> Arc<OverlaySource> {
    let first = 1_024_u64 * 1_024;
    let stride = 8_u64 * 1_024;
    let mut regions = vec![(0, b"%PDF-1.7\n".to_vec())];
    for revision in 0..count {
        let revision = u64::try_from(revision).unwrap();
        let offset = first + revision * stride;
        let previous = (revision > 0).then(|| first + (revision - 1) * stride);
        let prev = previous.map(|value| format!(" /Prev {value}")).unwrap_or_default();
        regions.push((
            offset,
            format!("xref\n1 1\n{revision:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R{prev} >>\n").into_bytes(),
        ));
    }
    let newest = first + u64::try_from(count - 1).unwrap() * stride;
    let len = newest + 2 * TAIL_SCAN_LIMIT;
    let eof_offset = len - 128;
    regions.push((eof_offset, format!("startxref\n{newest}\n%%EOF\n").into_bytes()));
    Arc::new(OverlaySource {
        len,
        regions,
        requests: Mutex::new(Vec::new()),
    })
}

fn corrupt_marker(mut pdf: Vec<u8>, marker: &[u8]) -> Vec<u8> {
    let position = rfind(&pdf, marker).unwrap();
    pdf[position..position + marker.len()].fill(b'x');
    pdf
}

fn assert_scalar_preflight_bounds_retained(input: &[u8]) {
    let object = crate::parser::direct_object(input)
        .unwrap_or_else(|| panic!("direct-object corpus entry did not parse: {input:?}"));
    let measured = scalar_object_retained_bytes(&object);
    let preflight = scalar_ast_preflight(input)
        .unwrap_or_else(|| panic!("preflight rejected direct-object corpus entry: {input:?}"));
    assert!(
        usize::try_from(preflight.reserved_bytes(false)).unwrap_or(usize::MAX) >= measured,
        "input={input:?}, preflight={preflight:?}, measured={measured}"
    );
}

fn flate_encode(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn lzw_encode(data: &[u8]) -> Vec<u8> {
    // `Stream::decompress_lzw` defaults `/EarlyChange` to 1, which is the
    // TIFF code-size switch.
    weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
        .encode(data)
        .unwrap()
}

/// The `/EarlyChange 0` counterpart of [`lzw_encode`]: the plain (non-TIFF)
/// code-size switch `Stream::decompress_lzw` selects when the parameter
/// reaches it.
fn lzw_encode_late_change(data: &[u8]) -> Vec<u8> {
    weezl::encode::Encoder::new(weezl::BitOrder::Msb, 8)
        .encode(data)
        .unwrap()
}

fn ascii_hex_encode(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len() * 2 + 1);
    for byte in data {
        output.extend_from_slice(format!("{byte:02X}").as_bytes());
    }
    output.push(b'>');
    output
}

fn ascii85_encode(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    for group in data.chunks(4) {
        let mut padded = [0_u8; 4];
        padded[..group.len()].copy_from_slice(group);
        let mut value = u32::from_be_bytes(padded);
        let mut digits = [0_u8; 5];
        for slot in digits.iter_mut().rev() {
            *slot = b'!' + u8::try_from(value % 85).unwrap();
            value /= 85;
        }
        output.extend_from_slice(&digits[..group.len() + 1]);
    }
    output.extend_from_slice(b"~>");
    output
}

/// Pad to a whole number of predictor rows. PNG predictors are row-framed,
/// so a trailing partial row has nothing to predict against; the padding is
/// whitespace past the last member body, which the member index never reads.
fn pad_to_rows(data: &[u8], row_bytes: usize) -> Vec<u8> {
    let mut padded = data.to_vec();
    while !padded.len().is_multiple_of(row_bytes) {
        padded.push(b' ');
    }
    padded
}

/// Apply the PNG `Up` filter (type 2) to every row, which is what a
/// `/Predictor 12` stream carries.
fn png_up_predict(data: &[u8], row_bytes: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len() / row_bytes * (row_bytes + 1));
    let mut previous = vec![0_u8; row_bytes];
    for row in data.chunks(row_bytes) {
        output.push(2);
        for (index, byte) in row.iter().enumerate() {
            output.push(byte.wrapping_sub(previous[index]));
        }
        previous.copy_from_slice(row);
    }
    output
}

/// Apply TIFF Predictor 2 (horizontal differencing) at 8 bits, 1 colour.
fn tiff_predict2(data: &[u8], row_bytes: usize) -> Vec<u8> {
    let mut output = data.to_vec();
    for row in output.chunks_mut(row_bytes) {
        for index in (1..row.len()).rev() {
            row[index] = row[index].wrapping_sub(row[index - 1]);
        }
    }
    output
}

fn assert_public_preparation_encrypted_overlap_refusal(stream_eol: &[u8], expected_span: (u64, u64)) {
    let (pdf, _, _) = encrypted_object_stream_pdf_with_eol(4, false, 512 * 1024, stream_eol);
    let container_offset = pdf
        .windows(b"5 0 obj\n".len())
        .position(|window| window == b"5 0 obj\n")
        .unwrap();
    let mut stream_marker = b"stream".to_vec();
    stream_marker.extend_from_slice(stream_eol);
    let stream = pdf[container_offset..]
        .windows(stream_marker.len())
        .position(|window| window == stream_marker)
        .unwrap();
    let encoded_start_usize = container_offset + stream + stream_marker.len();
    let encoded_length = pdf[encoded_start_usize..]
        .windows(b"\nendstream".len())
        .position(|window| window == b"\nendstream")
        .unwrap();
    let encoded_start = u64::try_from(encoded_start_usize).unwrap();
    let encoded_end = encoded_start + u64::try_from(encoded_length).unwrap();
    assert_eq!((encoded_start, encoded_end), expected_span);
    let source = Arc::new(TracingBytesSource {
        bytes: pdf,
        requests: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn RandomAccessSource> = source.clone();
    let reader = IndexedReader::open_shared(
        erased,
        IndexedReaderOptions {
            password: Some(b"user".to_vec()),
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(reader.cache_stats(), IndexedReaderCacheStats::default());
    let IndexedObjectLocation::Compressed { container, index } = reader.object_location((10, 0)).unwrap() else {
        panic!("encrypted fixture member was not declared compressed")
    };
    assert_eq!(container, (5, 0));
    assert_eq!(index, 0);

    let mut limit = 1_u64;
    let refusal_limit = loop {
        let permit = crate::ScalarResolutionPermit::new(limit);
        let error = reader
            .prepare_object_stream_with_permit(container, &permit)
            .unwrap_err();
        if let IndexedReaderError::StreamLimitExceeded { length, .. } = &error {
            assert!(*length > limit, "non-progressing stream admission");
            assert_eq!(permit.stats().current_bytes, 0);
            permit.close().unwrap();
            limit = *length;
            continue;
        }
        let IndexedReaderError::ScalarResourceLimit { requested, phase, .. } = error else {
            panic!("unexpected bounded refusal: {error:?}")
        };
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
        if phase == "object-stream-decryption-overlap" {
            break limit;
        }
        assert!(requested > limit, "non-progressing admission at {phase}");
        limit = requested;
    };

    let mut observed = None;
    for _ in 0..2 {
        assert_eq!(reader.cache_stats(), IndexedReaderCacheStats::default());
        source.requests.lock().unwrap().clear();
        let permit = crate::ScalarResolutionPermit::new(refusal_limit);
        let error = reader
            .prepare_object_stream_with_permit(container, &permit)
            .unwrap_err();
        let IndexedReaderError::ScalarResourceLimit {
            requested,
            limit,
            phase,
            ..
        } = error
        else {
            panic!("unexpected bounded refusal: {error:?}")
        };
        assert_eq!(phase, "object-stream-decryption-overlap");
        assert_eq!(
            observed.get_or_insert((requested, limit, phase)),
            &(requested, limit, phase)
        );
        let requests = source.requests.lock().unwrap();
        assert!(
            requests.iter().all(|(offset, length)| {
                let request_end = offset.saturating_add(u64::try_from(*length).unwrap_or(u64::MAX));
                request_end <= encoded_start || *offset >= encoded_end
            }),
            "payload read was issued before decryption-overlap admission for {encoded_start}..{encoded_end}: \
             {requests:?}"
        );
        assert_eq!(permit.stats().current_bytes, 0);
        permit.close().unwrap();
        assert_eq!(reader.cache_stats(), IndexedReaderCacheStats::default());
    }
}
