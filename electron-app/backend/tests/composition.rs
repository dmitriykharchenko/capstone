use research_backend::{composition::browsing_registry, config::PaperBrowsingConfig};
use std::{collections::HashMap, fs, path::Path};

fn available(env: &HashMap<&str, String>) -> Vec<String> {
    let registry = browsing_registry(|key| {
        assert!(
            [
                "CAPSTONE_APP_DATA_ROOT",
                "CAPSTONE_RESOURCES_DIR",
                "CAPSTONE_PAPERS_DIR"
            ]
            .contains(&key)
        );
        env.get(key).cloned()
    });
    let health = registry.health();
    assert_eq!(health["search_enabled"], false);
    assert_eq!(health["chat_mode"], "unavailable");
    assert_eq!(health["paid_operations_authorized"], false);
    assert!(!health.to_string().contains("private"));
    health["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["available"] == true)
        .map(|c| c["command"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn only_browsing_is_installed_without_reading_provider_or_store_settings() {
    let temp = tempfile::tempdir().unwrap();
    let resources = temp.path().join("private-resources");
    let papers = temp.path().join("private-papers");
    let data = temp.path().join("private-data");
    fs::create_dir(&resources).unwrap();
    fs::create_dir(&papers).unwrap();
    fs::write(resources.join(".env"), "CAPSTONE_PAPERS_DIR=wrong").unwrap();
    let env = HashMap::from([
        ("CAPSTONE_APP_DATA_ROOT", data.to_str().unwrap().to_owned()),
        (
            "CAPSTONE_RESOURCES_DIR",
            resources.to_str().unwrap().to_owned(),
        ),
        ("CAPSTONE_PAPERS_DIR", papers.to_str().unwrap().to_owned()),
    ]);
    assert_eq!(
        available(&env),
        ["get_paper", "health", "list_papers", "read_pdf"]
    );
    assert!(!data.exists());
    assert!(fs::read_dir(&papers).unwrap().next().is_none());
    for key in env.keys() {
        for invalid in ["", "relative", "/missing-private-pdf-root"] {
            if *key == "CAPSTONE_APP_DATA_ROOT" && invalid.starts_with('/') {
                continue;
            }
            let mut env = env.clone();
            env.insert(key, invalid.into());
            assert_eq!(available(&env), ["health"]);
        }
        let mut env = env.clone();
        env.remove(key);
        assert_eq!(available(&env), ["health"]);
    }
    assert_eq!(available(&HashMap::new()), ["health"]);
}

#[test]
fn browsing_retains_root_containment_and_requires_existing_read_only_directories() {
    let temp = tempfile::tempdir().unwrap();
    let resources = temp.path().join("resources");
    let papers = resources.join("ResearchPapers");
    let data = temp.path().join("data");
    fs::create_dir_all(&papers).unwrap();
    assert!(PaperBrowsingConfig::new(&data, &resources, &papers).is_ok());
    for invalid in [&data, temp.path(), &resources, &papers] {
        assert!(PaperBrowsingConfig::new(invalid, &resources, invalid).is_err());
    }
    for invalid in [
        Path::new("relative"),
        &papers.join(".."),
        &papers.join("missing"),
    ] {
        assert!(PaperBrowsingConfig::new(&data, &resources, invalid).is_err());
        assert!(PaperBrowsingConfig::new(&data, invalid, &papers).is_err());
    }
    let file = temp.path().join("file");
    fs::write(&file, b"not a directory").unwrap();
    assert!(PaperBrowsingConfig::new(&data, &resources, &file).is_err());
    assert!(!data.exists());
}

#[cfg(unix)]
#[test]
fn canonical_aliases_cannot_overlap_data_and_read_only_roots() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let resources = temp.path().join("resources");
    fs::create_dir(&data).unwrap();
    fs::create_dir(&resources).unwrap();
    let alias = temp.path().join("papers");
    std::os::unix::fs::symlink(&data, &alias).unwrap();
    assert!(PaperBrowsingConfig::new(&data, &resources, &alias).is_err());
}
