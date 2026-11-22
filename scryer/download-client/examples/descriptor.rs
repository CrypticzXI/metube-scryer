fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&metube_download_client::build_descriptor()).unwrap()
    );
}
