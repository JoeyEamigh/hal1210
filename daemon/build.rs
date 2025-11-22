use spirv_builder::{Capability, MetadataPrintout, SpirvBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let compile_result = SpirvBuilder::new("./shaders/border-colors", "spirv-unknown-vulkan1.3")
    .print_metadata(MetadataPrintout::Full)
    .capability(Capability::Int8)
    .capability(Capability::Int64)
    .capability(Capability::StorageImageReadWithoutFormat)
    .build()?;

  println!("cargo:rerun-if-changed=shaders/border-colors/src/lib.rs");
  println!("cargo:rerun-if-changed=shaders/border-colors/Cargo.toml");

  println!(
    "cargo:rustc-env=WAYLED_BORDER_COLORS_SPV={}",
    compile_result.module.unwrap_single().display()
  );

  Ok(())
}
