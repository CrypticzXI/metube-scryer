// Deterministic metadata-only process fixture: never contacts YouTube or writes media.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("fixture-1.0");
        return;
    }
    assert!(args.iter().any(|arg| arg == "--skip-download"));
    assert!(args.iter().any(|arg| arg == "--ignore-config"));
    assert!(
        !args
            .iter()
            .any(|arg| arg == "--no-simulate" || arg == "--output" || arg == "--exec")
    );
    let video = serde_json::json!({"id":"dQw4w9WgXcQ","title":"The Exact Episode","channel":"Official","channel_id":"UCaaaaaaaaaaaaaaaaaaaaaa",
        "duration":120,"upload_date":"20261120","height":720,"vcodec":"avc1.640028","ext":"mp4","filesize":123456});
    if args.iter().any(|arg| arg == "--format") {
        let index = args.iter().position(|arg| arg == "--format").unwrap();
        assert_eq!(args[index + 1], "bv*[height<=1080]+ba/b[height<=1080]");
        println!("{video}");
    } else {
        assert!(args.iter().any(|arg| arg == "--flat-playlist"));
        println!(
            "{}",
            serde_json::json!({"title":"Official uploads","channel":"Official","channel_id":"UCaaaaaaaaaaaaaaaaaaaaaa","entries":[video]})
        );
    }
}
