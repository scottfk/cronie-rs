//! Link libpam, which upstream's crond and crontab use (USE=pam).

fn main() {
    println!("cargo:rustc-link-lib=pam");
    println!("cargo:rerun-if-changed=build.rs");
}
