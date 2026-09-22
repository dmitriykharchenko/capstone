//! Real PDFs built with catalog/page tree/content streams/xref, without Python.
use pdf_extract::{
    Document, Object, Stream,
    content::{Content, Operation},
    dictionary,
};

pub enum Page {
    Text(Vec<Vec<u8>>),
    Image,
    Blank,
    Corrupt,
}

pub fn pdf(pages: Vec<Page>) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let image = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 1, "Height" => 1,
            "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
        },
        vec![0, 127, 255],
    ));
    let resources = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font },
        "XObject" => dictionary! { "Im1" => image },
    });
    let mut kids = Vec::new();
    for page in pages {
        let content = match page {
            Page::Text(paragraphs) => {
                let mut operations = vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 12.into()]),
                ];
                for (index, paragraph) in paragraphs.into_iter().enumerate() {
                    operations.push(Operation::new(
                        "Tm",
                        vec![
                            1.into(),
                            0.into(),
                            0.into(),
                            1.into(),
                            50.into(),
                            (700 - index as i64 * 60).into(),
                        ],
                    ));
                    operations.push(Operation::new(
                        "Tj",
                        vec![Object::string_literal(paragraph)],
                    ));
                }
                operations.push(Operation::new("ET", vec![]));
                Content { operations }.encode().unwrap()
            }
            Page::Image => b"q 10 0 0 10 30 30 cm /Im1 Do Q".to_vec(),
            Page::Blank => Vec::new(),
            Page::Corrupt => b"BT 1 Tm ET".to_vec(),
        };
        let content = doc.add_object(Stream::new(dictionary! {}, content));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content,
            "Resources" => resources, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        kids.push(page.into());
    }
    doc.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages", "Count" => kids.len() as i64, "Kids" => kids,
        }
        .into(),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

// Passwords are generated in memory, never returned, printed or stored. These
// fixtures prove encrypted transfer without making a user's password available.
#[allow(dead_code)]
pub fn encrypted_pdf(aes: bool) -> Vec<u8> {
    use pdf_extract::{EncryptionState, EncryptionVersion, Permissions};
    use std::{
        collections::{BTreeMap, hash_map::RandomState},
        hash::{BuildHasher, Hasher},
        sync::Arc,
    };
    let password = format!("{:x}", RandomState::new().build_hasher().finish());
    let owner = format!("{:x}", RandomState::new().build_hasher().finish());
    let mut doc =
        Document::load_mem(&pdf(vec![Page::Text(vec![b"Encrypted preview".to_vec()])])).unwrap();
    doc.trailer.set(
        "ID",
        vec![Object::string_literal(b"fixture identifier".to_vec()); 2],
    );
    let version = if aes {
        EncryptionVersion::V4 {
            document: &doc,
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(
                b"StdCF".to_vec(),
                Arc::new(pdf_extract::encryption::crypt_filters::Aes128CryptFilter)
                    as Arc<dyn pdf_extract::encryption::crypt_filters::CryptFilter>,
            )]),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: &owner,
            user_password: &password,
            permissions: Permissions::all(),
        }
    } else {
        EncryptionVersion::V2 {
            document: &doc,
            owner_password: &owner,
            user_password: &password,
            key_length: 128,
            permissions: Permissions::all(),
        }
    };
    let state = EncryptionState::try_from(version).unwrap();
    doc.encrypt(&state).unwrap();
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let locked = Document::load_mem(&bytes).unwrap();
    assert!(locked.is_encrypted());
    assert!(locked.authenticate_password("").is_err());
    assert!(locked.authenticate_password(&password).is_ok());
    bytes
}
