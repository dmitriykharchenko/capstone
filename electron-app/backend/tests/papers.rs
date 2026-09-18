mod papers_fixtures;

use papers_fixtures::{Page, pdf};
use research_backend::{papers::chunking::*, papers::*, runtime::Cancellation};
use std::fs;

#[test]
fn paragraph_boundaries_preserve_content_and_newline_styles() {
    for separator in ["\n", "\r", "\r\n"] {
        let text = format!(
            "  First{separator}continued.  {separator}\t{separator}\u{a0} Second{separator}  indented. \t"
        );
        assert_eq!(
            split_into_paragraphs(&text),
            [
                format!("First{separator}continued."),
                format!("Second{separator}  indented.")
            ]
        );
    }
    assert_eq!(split_into_paragraphs("one\r\ntwo"), ["one\r\ntwo"]);
    assert_eq!(
        split_into_paragraphs("one\r\n\rthree\n\nfour"),
        ["one", "three", "four"]
    );
    assert!(split_into_paragraphs(" \t\r\n\n\u{1c}\u{a0}").is_empty());
}

#[test]
fn sentence_splitting_is_greedy_and_retains_unicode() {
    assert_eq!(
        split_oversized_chunk("Alpha one. Beta two. Gamma three.", 22).unwrap(),
        ["Alpha one. Beta two.", "Gamma three."]
    );
    assert_eq!(
        split_oversized_chunk("One?\" Two!’ Three.)] Tail", 17).unwrap(),
        ["One?\" Two!’", "Three.)]", "Tail"]
    );
    assert_eq!(
        split_oversized_chunk("abcdefghij", 4).unwrap(),
        ["abcd", "efgh", "ij"]
    );
    assert_eq!(split_oversized_chunk("ééé", 4).unwrap(), ["éé", "é"]);
    assert_eq!(
        split_oversized_chunk("  small  ", 20).unwrap(),
        ["  small  "]
    );
    assert_eq!(split_oversized_chunk("", 0), Err(ChunkingError::ZeroBudget));
    assert_eq!(
        split_oversized_chunk("中", 2),
        Err(ChunkingError::CharacterExceedsBudget)
    );
    let text = "é中🦀".repeat(4000);
    let chunks = split_oversized_chunk(&text, DEFAULT_MAX_CHUNK_BYTES).unwrap();
    assert!(chunks.iter().all(|c| !c.is_empty() && c.len() <= 8000));
    assert_eq!(chunks.concat(), text);
}

#[test]
fn ids_titles_listing_and_missing_directory_match_legacy() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "z.pdf",
        "DBLP_conf_Example-Title.pdf",
        "A.v1.pdf",
        "a.PDF",
        "bad name.pdf",
        ".hidden.pdf",
    ] {
        fs::write(root.path().join(name), b"pdf").unwrap();
    }
    fs::create_dir(root.path().join("directory.pdf")).unwrap();
    let repository = NativePaperRepository::new(root.path()).unwrap();
    let papers = repository.list_papers().unwrap();
    assert_eq!(
        papers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["A.v1", "DBLP_conf_Example-Title", "z"]
    );
    assert_eq!(papers[1].title, "Example Title");
    assert_eq!(papers[1].filename, "DBLP_conf_Example-Title.pdf");
    assert_eq!(papers[1].size_bytes, 3);
    assert_eq!(derive_title("dBlP-JoUrNaLs_Test"), "Test");
    assert_eq!(derive_title("DBLPconf__"), "DBLPconf__");
    for id in [
        "", "..", "../z", "z/../A", "z\\file", "/tmp/z", "é", "z\0", " z", "z%2f",
    ] {
        assert!(!is_safe_id(id));
        assert!(matches!(
            repository.get_paper(id),
            Err(PaperError::InvalidId)
        ));
    }
    let absent = NativePaperRepository::new(root.path().join("absent")).unwrap();
    assert!(absent.list_papers().unwrap().is_empty());
    assert!(matches!(absent.get_paper("z"), Err(PaperError::NotFound)));
}

#[tokio::test]
async fn real_multipage_pdf_preserves_provenance_and_utf8_bounds() {
    let root = tempfile::tempdir().unwrap();
    let long = vec![0xe9; 9001];
    let bytes = pdf(vec![
        Page::Text(vec![
            b"First paragraph.".to_vec(),
            b"Second paragraph.".to_vec(),
        ]),
        Page::Blank,
        Page::Image,
        Page::Text(vec![long]),
    ]);
    fs::write(root.path().join("DBLP_conf_Test.pdf"), &bytes).unwrap();
    let repository = NativePaperRepository::new(root.path()).unwrap();
    let cancellation = Cancellation::default();
    assert_eq!(
        repository
            .read_pdf_bytes("DBLP_conf_Test", &cancellation)
            .unwrap(),
        bytes
    );
    let pages = repository
        .extract_pages("DBLP_conf_Test", &cancellation)
        .unwrap();
    assert_eq!(
        pages.iter().map(|p| p.page_number).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert!(pages[1].text.is_empty() && pages[2].text.is_empty());
    assert!(pages[3].text.trim() == "é".repeat(9001));
    let chunks = repository
        .extract_chunks("DBLP_conf_Test".into(), cancellation)
        .await
        .unwrap();
    assert_eq!(chunks.len(), 5);
    assert_eq!(chunks[0].text, "First paragraph.");
    assert_eq!(chunks[1].text, "Second paragraph.");
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.source.chunk_index, index as u32);
        assert_eq!(chunk.source.page, if index < 2 { 1 } else { 4 });
        assert_eq!(chunk.source.paper_id, "DBLP_conf_Test");
        assert_eq!(chunk.source.filename, "DBLP_conf_Test.pdf");
        assert_eq!(chunk.source.title, "Test");
        assert!(!chunk.text.is_empty() && chunk.text.len() <= 8000);
    }
    assert_eq!(
        chunks[2..]
            .iter()
            .map(|c| c.text.as_str())
            .collect::<String>(),
        "é".repeat(9001)
    );
    assert_eq!(
        serde_json::to_value(&chunks).unwrap(),
        serde_json::to_value(
            repository
                .chunks("DBLP_conf_Test", &Cancellation::default())
                .unwrap()
        )
        .unwrap()
    );
    assert_eq!(
        fs::read(root.path().join("DBLP_conf_Test.pdf")).unwrap(),
        bytes
    );
}

#[test]
fn corrupt_empty_and_image_only_pdfs_are_explicit() {
    let cancellation = Cancellation::default();
    assert_eq!(
        extract_pdf_pages(&[], &cancellation),
        Err(PaperError::EmptyFile)
    );
    assert_eq!(
        extract_pdf_pages(b"not a PDF", &cancellation),
        Err(PaperError::CorruptPdf)
    );
    assert_eq!(
        extract_pdf_pages(
            &pdf(vec![
                Page::Text(vec![b"Valid page".to_vec()]),
                Page::Corrupt
            ]),
            &cancellation
        ),
        Err(PaperError::CorruptPdf)
    );
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("image.pdf"),
        pdf(vec![Page::Image, Page::Blank]),
    )
    .unwrap();
    fs::write(root.path().join("empty.pdf"), []).unwrap();
    let oversized = fs::File::create(root.path().join("oversized.pdf")).unwrap();
    oversized.set_len(MAX_PDF_BYTES + 1).unwrap();
    let repository = NativePaperRepository::new(root.path()).unwrap();
    assert!(
        repository
            .chunks("image", &cancellation)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        repository.read_pdf_bytes("empty", &cancellation),
        Err(PaperError::EmptyFile)
    );
    assert_eq!(
        repository.read_pdf_bytes("oversized", &cancellation),
        Err(PaperError::TooLarge)
    );
}

#[test]
fn missing_pages_fail_instead_of_silently_becoming_an_empty_document() {
    let cancellation = Cancellation::default();
    let bytes = pdf(vec![]);
    assert!(extract_pdf_pages(&bytes, &cancellation).unwrap().is_empty());
    let mut doc = pdf_extract::Document::load_mem(&bytes).unwrap();
    let root = doc
        .catalog()
        .unwrap()
        .get(b"Pages")
        .unwrap()
        .as_reference()
        .unwrap();
    doc.get_dictionary_mut(root).unwrap().set("Count", 1);
    let mut missing = Vec::new();
    doc.save_to(&mut missing).unwrap();
    assert_eq!(
        extract_pdf_pages(&missing, &cancellation),
        Err(PaperError::CorruptPdf)
    );
    doc.get_dictionary_mut(root)
        .unwrap()
        .set("Count", MAX_PDF_PAGES as i64 + 1);
    let mut oversized = Vec::new();
    doc.save_to(&mut oversized).unwrap();
    assert_eq!(
        extract_pdf_pages(&oversized, &cancellation),
        Err(PaperError::ExtractionLimit)
    );
}

#[cfg(unix)]
#[test]
fn symlinks_special_files_and_replaced_root_cannot_escape() {
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::symlink},
    };
    let root = tempfile::tempdir().unwrap();
    let inside = root.path().join("papers");
    let outside = root.path().join("outside");
    fs::create_dir(&inside).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(inside.join("safe.pdf"), b"approved").unwrap();
    fs::write(outside.join("safe.pdf"), b"outside").unwrap();
    symlink(outside.join("safe.pdf"), inside.join("escape.pdf")).unwrap();
    symlink(inside.join("safe.pdf"), inside.join("alias.pdf")).unwrap();
    let fifo = CString::new(inside.join("fifo.pdf").as_os_str().as_bytes()).unwrap();
    // SAFETY: NUL-terminated temporary path; creates only this fixture FIFO.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let repository = NativePaperRepository::new(&inside).unwrap();
    for id in ["escape", "alias", "fifo"] {
        assert!(matches!(
            repository.get_paper(id),
            Err(PaperError::UnsafePath)
        ));
    }
    fs::rename(&inside, root.path().join("pinned")).unwrap();
    symlink(&outside, &inside).unwrap();
    assert_eq!(
        repository
            .read_pdf_bytes("safe", &Cancellation::default())
            .unwrap(),
        b"approved"
    );
    for _ in 0..2 {
        let papers = repository.list_papers().unwrap();
        assert_eq!(papers.len(), 1);
        assert_eq!(papers[0].id, "safe");
        assert_eq!(papers[0].size_bytes, 8);
    }
}

#[cfg(unix)]
#[test]
fn atomic_file_symlink_replacement_never_reads_outside_bytes() {
    use std::{
        os::unix::fs::symlink,
        sync::{Arc, Barrier},
    };
    let root = tempfile::tempdir().unwrap();
    let inside = root.path().join("papers");
    fs::create_dir(&inside).unwrap();
    let external = root.path().join("external.pdf");
    fs::write(&external, b"outside").unwrap();
    fs::write(inside.join("safe.pdf"), b"approved").unwrap();
    let repository = NativePaperRepository::new(&inside).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let other = barrier.clone();
    let writer = std::thread::spawn(move || {
        other.wait();
        for _ in 0..250 {
            fs::rename(inside.join("safe.pdf"), inside.join("parked")).unwrap();
            symlink(&external, inside.join("safe.pdf")).unwrap();
            fs::remove_file(inside.join("safe.pdf")).unwrap();
            fs::rename(inside.join("parked"), inside.join("safe.pdf")).unwrap();
        }
    });
    barrier.wait();
    for _ in 0..1000 {
        match repository.read_pdf_bytes("safe", &Cancellation::default()) {
            Ok(bytes) => assert_eq!(bytes, b"approved"),
            Err(PaperError::NotFound | PaperError::UnsafePath | PaperError::ChangedDuringRead) => {}
            Err(error) => panic!("unexpected error: {error:?}"),
        }
    }
    writer.join().unwrap();
}
