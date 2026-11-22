fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&youtube_library_indexer::descriptor()).unwrap()
    );
}
