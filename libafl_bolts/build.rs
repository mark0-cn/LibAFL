#[rustversion::nightly]
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-cfg=nightly");
}

#[rustversion::not(nightly)]
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
