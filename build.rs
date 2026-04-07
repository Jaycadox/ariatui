use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let build_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| format!("build-{}-{}", duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or_else(|_| "build-unknown".into());
    println!("cargo:rustc-env=ARIATUI_BUILD_ID={build_id}");
}
