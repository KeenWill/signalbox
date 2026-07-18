use std::fs;

fn main() {
    // Rebuild when a migration is added or removed (directory mtime) and when an
    // existing migration file's contents change (per-file), so the embedded
    // `sqlx::migrate!` set never goes stale.
    println!("cargo:rerun-if-changed=migrations");

    if let Ok(entries) = fs::read_dir("migrations") {
        for entry in entries.flatten() {
            if let Some(path) = entry.path().to_str() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
}
