// Re-embed the migrations when a file under `migrations/` changes:
// `sqlx::migrate!` (crate::ledger::MIGRATOR) and the committed baseline
// fingerprint are read at compile time.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
