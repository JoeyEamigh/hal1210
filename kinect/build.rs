use miette::IntoDiagnostic;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn compute_patches_hash(patches_dir: &Path) -> miette::Result<Option<String>> {
  if !patches_dir.exists() {
    return Ok(None);
  }
  let mut hasher = Sha256::new();
  let mut patches: Vec<_> = WalkDir::new(patches_dir)
    .into_iter()
    .filter_map(Result::ok)
    .map(|e| e.into_path())
    .filter(|p| p.extension().map(|s| s == "patch").unwrap_or(false))
    .collect();
  patches.sort();
  for path in patches {
    if path.is_file() {
      let data = std::fs::read(&path).into_diagnostic()?;
      hasher.update(&data);
    }
  }
  Ok(Some(format!("{:x}", hasher.finalize())))
}

fn read_stored_hash(dest: &Path) -> miette::Result<Option<String>> {
  let marker = dest.join(".patches_hash");
  if marker.exists() {
    let s = std::fs::read_to_string(marker).into_diagnostic()?;
    Ok(Some(s.trim().to_string()))
  } else {
    Ok(None)
  }
}

fn write_stored_hash(dest: &Path, value: &str) -> miette::Result<()> {
  let marker = dest.join(".patches_hash");
  std::fs::create_dir_all(dest).into_diagnostic()?;
  std::fs::write(marker, value).into_diagnostic()?;
  Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> miette::Result<()> {
  if dst.exists() {
    std::fs::remove_dir_all(dst).into_diagnostic()?;
  }
  std::fs::create_dir_all(dst).into_diagnostic()?;
  for entry in WalkDir::new(src).into_iter().filter_map(Result::ok) {
    let rel = entry.path().strip_prefix(src).unwrap();
    let target = dst.join(rel);
    if entry.path().is_dir() {
      std::fs::create_dir_all(&target).into_diagnostic()?;
    } else if entry.path().is_file() {
      if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).into_diagnostic()?;
      }
      std::fs::copy(entry.path(), &target).into_diagnostic()?;
    }
  }
  Ok(())
}

fn apply_patches(patches_dir: &Path, dest: &Path) -> miette::Result<()> {
  use std::process::Command;
  // Ensure 'patch' exists
  let patch_check = Command::new("patch").arg("--version").output();
  if patch_check.is_err() || !patch_check.unwrap().status.success() {
    return Err(miette::miette!(
      "`patch` utility not found; please install the 'patch' package to apply header patches."
    ));
  }
  let mut patches: Vec<_> = std::fs::read_dir(patches_dir)
    .into_diagnostic()?
    .filter_map(|r| r.ok().map(|e| e.path()))
    .filter(|p| p.extension().map(|s| s == "patch").unwrap_or(false))
    .collect();
  patches.sort();
  for p in patches {
    let status = Command::new("patch")
      .current_dir(dest)
      .arg("-p1")
      .arg("--forward")
      .arg("--silent")
      .stdin(std::process::Stdio::from(std::fs::File::open(&p).into_diagnostic()?))
      .status()
      .into_diagnostic()?;
    if !status.success() {
      return Err(miette::miette!(format!("Failed to apply patch {}", p.display())));
    }
  }
  Ok(())
}

fn main() -> miette::Result<()> {
  let include_path = PathBuf::from("src");
  let headers_path = PathBuf::from("src/freenect/headers");

  // Copy headers and apply any patches if required.
  let system_headers = Path::new("/usr/include/libfreenect");
  let dest_headers = Path::new("src/freenect/headers");
  let patches_dir = Path::new("src/freenect/patches");

  // Check system headers exist
  if !system_headers.exists() {
    return Err(miette::miette!(
      "Freenect headers not found at /usr/include/libfreenect. Please install libfreenect-dev and try again."
    ));
  }

  // Ensure we re-run the build script if patches change
  if patches_dir.exists() {
    let mut patches: Vec<_> = std::fs::read_dir(patches_dir)
      .into_diagnostic()?
      .filter_map(|r| r.ok().map(|e| e.path()))
      .filter(|p| p.extension().map(|s| s == "patch").unwrap_or(false))
      .collect();
    patches.sort();
    for p in patches {
      println!("cargo:rerun-if-changed={}", p.display());
    }
  }

  // Compute whether to copy/apply patches
  let patches_hash = compute_patches_hash(patches_dir)?;
  let need_copy = if dest_headers.exists() {
    match (&patches_hash, read_stored_hash(dest_headers)?) {
      (Some(ph), Some(stored)) => &stored != ph,
      (Some(_), None) => true,
      (None, _) => false,
    }
  } else {
    true
  };

  if need_copy {
    copy_dir(system_headers, dest_headers)?;
    if patches_dir.exists() {
      apply_patches(patches_dir, dest_headers)?;
    }
    if let Some(ph) = &patches_hash {
      write_stored_hash(dest_headers, ph)?;
    }
  }

  let mut b = autocxx_build::Builder::new("src/freenect/mod.rs", [&include_path, &headers_path]).build()?;
  b.flag_if_supported("-std=c++14")
    .flag_if_supported("-Wno-mismatched-new-delete")
    .flag_if_supported("-Wno-unused-parameter")
    .compile("libfreenect");
  println!("cargo:rerun-if-changed=src/freenect/mod.rs");

  println!("cargo:rustc-link-search=native=/usr/lib");
  println!("cargo:rustc-link-lib=dylib=freenect");
  println!("cargo:rustc-link-arg=-Wl,-l:libfreenect.so.0");
  println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib");

  Ok(())
}
