#[path = "../../build_version.rs"]
mod version;

fn main() {
    version::emit();
}
