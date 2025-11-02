use spirv_builder::{Capability, MetadataPrintout, SpirvBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let compile_result = SpirvBuilder::new("./shaders/color", "spirv-unknown-vulkan1.3")
    .print_metadata(MetadataPrintout::Full)
    .capability(Capability::Int8)
    .capability(Capability::Int64)
    .build()?;

  println!("cargo:rerun-if-changed=shaders/color/src/lib.rs");
  println!("cargo:rerun-if-changed=shaders/color/Cargo.toml");

  println!(
    "cargo:rustc-env=WAYLED_COLOR_SPV={}",
    compile_result.module.unwrap_single().display()
  );

  Ok(())
}
