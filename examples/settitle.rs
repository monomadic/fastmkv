//! `cargo run --example settitle -- FILE TITLE`: for trying a write by hand.
//! Edits FILE itself; use a copy.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut mkv = fastmkv::open(&a[1]).expect("open");
    mkv.set_title(Some(&a[2])).expect("title");
    mkv.set("TITLE", &a[2]).expect("tag");
    let plan = mkv.plan().expect("plan");
    for p in &plan.patches {
        println!("{:>12}  {} bytes", p.offset, p.bytes.len());
    }
    plan.apply().expect("apply");
}
