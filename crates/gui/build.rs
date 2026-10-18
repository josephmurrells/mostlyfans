use embed_manifest::manifest::ExecutionLevel;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PROFILE");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let level = if std::env::var("PROFILE").as_deref() == Ok("release") {
        ExecutionLevel::RequireAdministrator
    } else {
        ExecutionLevel::AsInvoker
    };
    embed_manifest(new_manifest("FanForge").requested_execution_level(level))
        .expect("the application manifest could not be embedded");
}
