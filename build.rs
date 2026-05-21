use std::env;
use std::path::PathBuf;

fn main() {
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
        .run();

    if let Err(e) = result {
        eprintln!();
        eprintln!("error: failed to compile Cap'n Proto schemas: {e}");
        eprintln!();
        eprintln!("The capnp compiler is required to build browser-control.");
        eprintln!("Install it with:");
        eprintln!("    cargo xtask install-capnp");
        eprintln!("or set the CAPNP environment variable to point at a capnp binary.");
        eprintln!();
        std::process::exit(1);
    }

    println!("cargo:rerun-if-changed=schema");
    println!("cargo:rerun-if-env-changed=CAPNP");
}
