use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = env::args().collect();
    println!("Hello from a Rust application running in pagh ring 3!");
    println!("argv = {:?}", args);
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(t) => println!("unix time = {}", t.as_secs()),
        Err(_) => println!("clock is before Unix epoch"),
    }
}
