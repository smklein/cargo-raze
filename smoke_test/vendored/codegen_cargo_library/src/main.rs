pub mod items {
  include!(concat!(env!("OUT_DIR"), "/sample.rs"));
}

extern crate prost;
// extern crate sample;

use prost::Message;
use sample::Person;

fn main() {
  println!("hello world");
}
