use std::env;
use std::path::PathBuf;

fn main() {
    // Always rerun if schemas or relevant env vars change, so a missed
    // regeneration is at least visible to contributors who opt in.
    println!("cargo:rerun-if-changed=schema");
    println!("cargo:rerun-if-env-changed=CAPNP");
    println!("cargo:rerun-if-env-changed=BROWSERCTL_REGEN_SCHEMA");

    // Generated Cap'n Proto bindings live in `src/generated/` and are committed
    // to the repo, so normal builds (including `cargo install`) don't need the
    // `capnp` compiler installed. Codegen only runs when explicitly requested.
    let regen =
        cfg!(feature = "regenerate-schema") || env::var_os("BROWSERCTL_REGEN_SCHEMA").is_some();
    if !regen {
        return;
    }

    // Allow contributors / CI to override the capnp path via env var.
    // Otherwise look beside the workspace for the xtask-managed install.
    if env::var_os("CAPNP").is_none() {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let candidate = PathBuf::from(&manifest_dir)
            .join("target")
            .join("tools")
            .join("capnp")
            .join("current")
            .join("bin")
            .join(if cfg!(windows) { "capnp.exe" } else { "capnp" });
        if candidate.exists() {
            // SAFETY: this only runs during build, single-threaded.
            unsafe { env::set_var("CAPNP", &candidate) };
        }
    }

    let result = capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/errors.capnp")
        .file("schema/daemon.capnp")
        .output_path("src/generated")
        .run();

    if let Err(e) = result {
        eprintln!();
        eprintln!("error: failed to regenerate Cap'n Proto schemas: {e}");
        eprintln!();
        eprintln!("Regeneration was requested via the `regenerate-schema` feature");
        eprintln!("or BROWSERCTL_REGEN_SCHEMA, but `capnp` could not be invoked.");
        eprintln!("Install it with:");
        eprintln!("    cargo xtask install-capnp");
        eprintln!("or set CAPNP to point at a capnp binary.");
        eprintln!();
        eprintln!("Normal builds do not require capnp — unset BROWSERCTL_REGEN_SCHEMA");
        eprintln!("(and drop the `regenerate-schema` feature) to use the committed bindings.");
        eprintln!();
        std::process::exit(1);
    }
}
