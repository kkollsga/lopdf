//! Signature dictionaries in encrypted files, on the write path and on the indexed read route.
//!
//! ISO 32000-1, 7.6.2 leaves the `/Contents` of a signature dictionary in clear while every other
//! string of the object is encrypted. Expected values come from the constants these fixtures are
//! built from, never from the eager loader, and each route is asserted on its own.

use std::collections::BTreeMap;
use std::sync::Arc;

use lopdf::encryption::DecryptionError;
use lopdf::encryption::crypt_filters::{Aes128CryptFilter, Aes256CryptFilter, CryptFilter};
use lopdf::{
    BytesSource, Dictionary, Document, EncryptionState, EncryptionVersion, IncrementalDocument, IndexedReader,
    IndexedReaderError, IndexedReaderOptions, LoadOptions, Object, ObjectId, Permissions, SaveOptions, StringFormat,
    dictionary,
};

/// 37 bytes: never a whole number of AES blocks, so decrypting it can only fail.
const CONTENTS: [u8; 37] = [0x30; 37];
const CONTENTS_ON_DISK: &[u8] = b"<30303030303030303030303030303030303030303030303030303030303030303030303030>";
const REASON: &[u8] = b"Testing";

#[derive(Clone, Copy, Debug)]
enum Cipher {
    Aes128,
    Aes256,
}

const CIPHERS: [Cipher; 2] = [Cipher::Aes128, Cipher::Aes256];

fn contents() -> Object {
    Object::String(CONTENTS.to_vec(), StringFormat::Hexadecimal)
}

fn typed_signature() -> Dictionary {
    dictionary! {
        "Type" => "Sig",
        "ByteRange" => vec![0.into(), 0.into(), 0.into(), 0.into()],
        "Contents" => contents(),
        "Reason" => Object::string_literal(REASON),
    }
}

/// `/Type` is optional in a signature dictionary; `/ByteRange` and `/Contents` identify it.
fn untyped_signature() -> Dictionary {
    dictionary! {
        "ByteRange" => vec![0.into(), 0.into(), 0.into(), 0.into()],
        "Contents" => contents(),
        "Reason" => Object::string_literal(REASON),
    }
}

/// A one-page document holding `signature`, with its AES state not yet applied.
fn document_with(signature: Dictionary) -> (Document, ObjectId) {
    let mut doc = Document::with_version("1.7");
    doc.trailer.set(
        "ID",
        Object::Array(vec![
            Object::string_literal(b"0123456789abcdef"),
            Object::string_literal(b"fedcba9876543210"),
        ]),
    );
    let page =
        doc.add_object(dictionary! { "Type" => "Page", "MediaBox" => vec![0.into(), 0.into(), 9.into(), 9.into()] });
    let pages = doc.add_object(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 });
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    doc.trailer.set("Root", catalog);
    let signature = doc.add_object(signature);
    (doc, signature)
}

fn encrypt(doc: &mut Document, cipher: Cipher, user_password: &str) {
    let filters = |filter: Arc<dyn CryptFilter>| BTreeMap::from([(b"StdCF".to_vec(), filter)]);
    let version = match cipher {
        Cipher::Aes128 => EncryptionVersion::V4 {
            document: doc,
            encrypt_metadata: true,
            crypt_filters: filters(Arc::new(Aes128CryptFilter)),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password,
            permissions: Permissions::all(),
        },
        Cipher::Aes256 => EncryptionVersion::V5 {
            encrypt_metadata: true,
            crypt_filters: filters(Arc::new(Aes256CryptFilter)),
            file_encryption_key: &[0x42; 32],
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password,
            permissions: Permissions::all(),
        },
    };
    let state = EncryptionState::try_from(version).unwrap();
    doc.encrypt(&state).unwrap();
}

fn save(doc: &mut Document, use_object_streams: bool) -> Vec<u8> {
    let mut pdf = Vec::new();
    doc.save_with_options(
        &mut pdf,
        SaveOptions::builder()
            .use_object_streams(use_object_streams)
            .use_xref_streams(use_object_streams)
            .build(),
    )
    .unwrap();
    pdf
}

fn indexed(pdf: &[u8]) -> IndexedReader {
    IndexedReader::open_with_options(
        BytesSource::from(pdf.to_vec()),
        IndexedReaderOptions {
            password: Some(b"user".to_vec()),
            ..IndexedReaderOptions::default()
        },
    )
    .unwrap()
}

fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn assert_signature_values(object: &Object, label: &str) {
    let dict = object.as_dict().unwrap();
    assert_eq!(
        dict.get(b"Contents").unwrap().as_str().unwrap(),
        CONTENTS,
        "{label}: /Contents"
    );
    assert_eq!(
        dict.get(b"Reason").unwrap().as_str().unwrap(),
        REASON,
        "{label}: /Reason"
    );
}

/// Reported fix 1: `/Contents` stays in clear on write, and the indexed route reads the rest.
#[test]
fn signature_contents_stays_clear_on_write_and_reads_back_on_the_indexed_route() {
    for cipher in CIPHERS {
        for (signature, shape) in [(typed_signature(), "typed"), (untyped_signature(), "untyped")] {
            let label = format!("{cipher:?} {shape}");
            let (mut doc, id) = document_with(signature);
            encrypt(&mut doc, cipher, "user");
            let pdf = save(&mut doc, false);

            assert!(
                holds(&pdf, CONTENTS_ON_DISK),
                "{label}: /Contents is not in clear on disk"
            );
            assert!(!holds(&pdf, REASON), "{label}: /Reason was left in clear on disk");

            assert_signature_values(&indexed(&pdf).resolve_object(id).unwrap(), &label);
        }
    }
}

/// Asking for object streams changes nothing here: an encrypted document is written without them,
/// so a signature dictionary is never packed where its container's encryption would cover it.
#[test]
fn saving_with_object_streams_still_writes_the_signature_contents_in_clear() {
    for cipher in CIPHERS {
        let (mut doc, id) = document_with(typed_signature());
        encrypt(&mut doc, cipher, "user");
        let pdf = save(&mut doc, true);

        assert!(
            holds(&pdf, CONTENTS_ON_DISK),
            "{cipher:?}: /Contents is not in clear on disk"
        );
        assert!(!holds(&pdf, REASON), "{cipher:?}: /Reason was left in clear on disk");
        assert_signature_values(&indexed(&pdf).resolve_object(id).unwrap(), &format!("{cipher:?}"));
    }
}

/// The appended revision of an encrypted file goes through the same exemption, which is the route
/// a signature takes in practice.
#[test]
fn an_incremental_save_writes_the_signature_contents_in_clear() {
    for cipher in CIPHERS {
        let (mut doc, _) = document_with(dictionary! { "Marker" => 1 });
        encrypt(&mut doc, cipher, "");
        let base = save(&mut doc, false);

        let mut update: IncrementalDocument = base.as_slice().try_into().unwrap();
        let id = update.new_document.add_object(typed_signature());
        let mut pdf = Vec::new();
        update.save_to(&mut pdf).unwrap();

        let appended = &pdf[base.len()..];
        assert!(
            holds(appended, CONTENTS_ON_DISK),
            "{cipher:?}: /Contents is not in clear on disk"
        );
        assert!(
            !holds(appended, REASON),
            "{cipher:?}: /Reason was left in clear on disk"
        );
        assert_signature_values(&indexed(&pdf).resolve_object(id).unwrap(), &format!("{cipher:?}"));
    }
}

/// Reported fix 2 holds only as far as the exemption reaches: one string that cannot be decrypted
/// still costs the indexed route the whole object, while the eager loader keeps what it could
/// decrypt. This is a pinned divergence (docs/indexed-reader-contract.md).
#[test]
fn an_undecryptable_string_still_refuses_the_whole_signature_object_on_the_indexed_route() {
    for cipher in CIPHERS {
        // Neither `/Type` nor `/ByteRange`: nothing identifies a signature dictionary, so the clear
        // `/Contents` is decrypted like any other string. A clear `/Reason` beside an exempt
        // `/Contents` is the same failure.
        let no_marker = dictionary! { "Contents" => contents(), "Reason" => Object::string_literal(REASON) };
        let clear_neighbour = dictionary! {
            "Type" => "Sig",
            "Contents" => contents(),
            "Reason" => Object::String(CONTENTS.to_vec(), StringFormat::Literal),
        };
        for (signature, shape) in [(no_marker, "no marker"), (clear_neighbour, "clear neighbour")] {
            let label = format!("{cipher:?} {shape}");
            let (mut doc, id) = document_with(signature);
            encrypt(&mut doc, cipher, "user");
            // What a producer that wrote these strings in clear leaves on disk.
            let dict = doc.get_object_mut(id).unwrap().as_dict_mut().unwrap();
            if shape == "no marker" {
                dict.set("Contents", contents());
            } else {
                dict.set("Reason", Object::String(CONTENTS.to_vec(), StringFormat::Literal));
            }
            let pdf = save(&mut doc, false);

            assert!(
                matches!(
                    indexed(&pdf).resolve_object(id),
                    Err(IndexedReaderError::ObjectDecryption {
                        id: refused,
                        source: DecryptionError::InvalidCipherTextLength | DecryptionError::Padding,
                    }) if refused == id
                ),
                "{label}: the indexed route no longer refuses the object"
            );

            let eager = Document::load_mem_with_options(&pdf, LoadOptions::with_password("user")).unwrap();
            assert!(
                eager.get_object(id).unwrap().as_dict().is_ok(),
                "{label}: eager lost the object"
            );
        }
    }
}

/// `/Contents` held in its own indirect object cannot be recognised as exempt: the dictionary
/// reads fine and the refusal moves to the string.
#[test]
fn an_indirect_signature_contents_is_refused_on_the_indexed_route() {
    for cipher in CIPHERS {
        let (mut doc, id) = document_with(typed_signature());
        let value = doc.add_object(contents());
        doc.get_object_mut(id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", Object::Reference(value));
        encrypt(&mut doc, cipher, "user");
        // A producer that follows 7.6.2 leaves that string in clear.
        *doc.get_object_mut(value).unwrap() = contents();
        let pdf = save(&mut doc, false);

        let reader = indexed(&pdf);
        let dict = reader.resolve_object(id).unwrap();
        assert_eq!(
            dict.as_dict().unwrap().get(b"Reason").unwrap().as_str().unwrap(),
            REASON
        );
        assert!(
            matches!(
                reader.resolve_object(value),
                Err(IndexedReaderError::ObjectDecryption { .. })
            ),
            "{cipher:?}"
        );
    }
}
