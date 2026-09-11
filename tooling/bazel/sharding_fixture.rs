#[test]
fn ordinary() {}

#[test]
#[ignore]
fn selected() {}

#[test]
#[ignore]
fn selected_suffix() {}

#[test]
#[ignore]
fn skipped() {}

#[test]
fn cleanup_fixture() {
    let directory = std::env::var("SIGNALBOX_TEST_POSTGRES_CONTAINERS").unwrap();
    std::fs::write(
        std::path::Path::new(&directory).join("fixture-run"),
        "fixture-container\n",
    )
    .unwrap();
    let status = std::env::var("SIGNALBOX_FIXTURE_EXIT").unwrap();
    std::process::exit(status.parse().unwrap());
}
