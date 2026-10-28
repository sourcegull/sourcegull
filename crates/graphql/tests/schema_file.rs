//! `ui/schema.graphql` is the server's SDL, so the UI's operations can be validated against it
//! (ui/src/lib/schema.test.ts). Regenerate with `SOURCEGULL_UPDATE_SCHEMA=1 cargo test -p
//! sourcegull-graphql --test integration schema_file::`.

#[test]
fn ui_schema_file_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/schema.graphql");
    let sdl = sourcegull::features::graphql_sdl();
    if std::env::var_os("SOURCEGULL_UPDATE_SCHEMA").is_some() {
        std::fs::write(path, &sdl).expect("write ui/schema.graphql");
        return;
    }
    let on_disk = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        on_disk == sdl,
        "ui/schema.graphql is stale: run SOURCEGULL_UPDATE_SCHEMA=1 cargo test -p sourcegull-graphql --test integration schema_file::"
    );
}
