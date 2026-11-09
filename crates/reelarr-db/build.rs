fn main() {
    // Also rebuild when a new migration is added (not just when existing files change).
    println!("cargo:rerun-if-changed=migrations");
}
