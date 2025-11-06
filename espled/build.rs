fn main() {
  println!("cargo:rerun-if-changed=.cargo/config.toml");
  println!("cargo:rerun-if-changed=sdkconfig.defaults");

  embuild::espidf::sysenv::output();
}
