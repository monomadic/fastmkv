//! `cargo run --example reseat -- SRC DEST`: a re-seated copy at DEST.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mkv = fastmkv::open(&a[1]).expect("open");
    println!("before: {:?}", mkv.seating());
    mkv.reseat(&a[2], fastmkv::Padding::default())
        .expect("reseat");
    println!(
        "after:  {:?}",
        fastmkv::open(&a[2]).expect("reopen").seating()
    );
}
