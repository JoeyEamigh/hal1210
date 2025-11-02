use spirv_builder::{Capability, MetadataPrintout, SpirvBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
  SpirvBuilder::new("./shaders/color", "spirv-unknown-vulkan1.3")
    .print_metadata(MetadataPrintout::Full)
    .capability(Capability::Int8)
    .build()?;

  Ok(())
}
