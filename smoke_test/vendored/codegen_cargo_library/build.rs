extern crate prost_build;

fn main() {
  let mut config = prost_build::Config::default();
  config.out_dir(env!("OUT_DIR"));
  panic!("{}", env!("OUT_DIR"));
  config.compile_protos(&["proto/sample.proto"],
                        &["src"]).unwrap();
}
