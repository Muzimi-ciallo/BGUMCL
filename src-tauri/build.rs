use dotenvy::{dotenv_override, from_filename};
use std::path::{Path, PathBuf};
use std::{env, fs};

/// Read `KEY = "value"` style lines from a .env file and set BGUMCL_* env
/// vars (overriding any existing value). This is more robust than dotenvy for
/// values containing special characters like `$` (e.g. the CurseForge API key),
/// which can get mangled when passed through the shell / process environment.
fn load_env_file(path: &Path) {
  if let Ok(content) = fs::read_to_string(path) {
    for line in content.lines() {
      let line = line.trim();
      if line.is_empty() || line.starts_with('#') {
        continue;
      }
      let Some(eq) = line.find('=') else { continue };
      let key = line[..eq].trim().to_string();
      let mut value = line[eq + 1..].trim().to_string();
      if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value = value[1..value.len() - 1].to_string();
      }
      if key.starts_with("BGUMCL_") && !value.is_empty() {
        // std::env::set_var is `unsafe` on edition 2024 / newer Rust.
        unsafe {
          env::set_var(&key, &value);
        }
      }
    }
  }
}

fn main() {
  if std::env::var("GITHUB_ACTIONS").is_err() {
    // Load env variables from ".env" files. Cargo runs build scripts with cwd
    // set to the crate directory (src-tauri), but the files may also live at
    // the repository root, so try both locations.
    from_filename(".env.template").ok();
    dotenv_override().ok();
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let candidates = [
      Path::new(&manifest_dir).join(".env"),
      Path::new(&manifest_dir).join(".env.template"),
      PathBuf::from(".env"),
      PathBuf::from(".env.template"),
    ];
    for candidate in candidates {
      load_env_file(&candidate);
    }
  }

  // A formal release must contain the CurseForge key so that the MCIM
  // metadata mirror has a reliable official fallback. Local development
  // builds remain possible without the key because they do not set the
  // release build marker.
  if env::var("BGUMCL_BUILD_TYPE").as_deref() == Ok("release")
    && env::var("BGUMCL_CURSEFORGE_API_KEY")
      .map(|value| value.trim().is_empty())
      .unwrap_or(true)
  {
    panic!("BGUMCL_CURSEFORGE_API_KEY is required for release builds");
  }

  let out_dir = env::var("OUT_DIR").unwrap_or_else(|_| "".to_string());
  let dest_path = Path::new(&out_dir).join("secrets.rs");
  let _ = fs::remove_file(&dest_path);

  // Iterate over all env variables and print those starting with "BGUMCL_" for compilation (env variables can not be accessed directly in compile time)
  // ref: https://users.rust-lang.org/t/std-set-var-in-build-rs-not-setting-environment-variable/34924/6
  // original naive impl, see: https://github.com/UNIkeEN/BGUMCL/pull/412/files
  for (key, value) in env::vars() {
    if key.starts_with("BGUMCL_") {
      println!("cargo:rerun-if-env-changed={}", key);
      println!("cargo:rustc-env={}={}", key, value);
    }
  }

  // Notify Cargo to auto re-run the build script if .env changes
  println!("cargo:rerun-if-changed=.env");
  println!("cargo:rerun-if-changed=.env.template");

  tauri_build::build()
}
