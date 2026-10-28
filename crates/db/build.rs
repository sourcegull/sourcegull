// `sqlx::migrate!` embeds the migration files at compile time; rebuild when one is added.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
