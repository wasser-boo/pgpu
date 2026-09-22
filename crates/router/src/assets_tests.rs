use super::*;
use crate::test_support::TestApp;

#[tokio::test]
async fn empty_manifest_is_still_sent_as_readiness_handshake() {
    let t = TestApp::new(); t.insert(11, &crate::db::now_iso());
    let mut rx = t.app.hub.register(11, 1, None, Default::default());
    let app = t.app.clone();
    let task = tokio::spawn(async move { push_assets(&app, 11).await });
    let RouterCommand::Cmd { id, command: Command::PushAssetsManifest { manifest } } = rx.recv().await.unwrap() else { panic!("manifest required"); };
    assert!(manifest.is_empty());
    t.app.hub.resolve(11, id, Ok(serde_json::json!({"needed":[],"required_missing":[]})));
    assert_eq!(task.await.unwrap().unwrap()["pushed"], 0);
}

#[tokio::test]
async fn asset_push_reports_verified_final_gate_not_initial_missing_list() {
    let t = TestApp::new(); t.insert(11, &crate::db::now_iso());
    let root = t.dir.join("assets/llm"); std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("reference.wav"), b"test voice").unwrap();
    std::fs::write(root.join("reference.wav.meta.toml"), "required = true\nmode = '0600'\n").unwrap();
    let mut rx = t.app.hub.register(11, 1, None, Default::default());
    let app = t.app.clone(); let task = tokio::spawn(async move { push_assets(&app, 11).await });
    let RouterCommand::Cmd { id, command: Command::PushAssetsManifest { manifest } } = rx.recv().await.unwrap() else { panic!("manifest required"); };
    let entry = manifest[0].clone();
    t.app.hub.resolve(11, id, Ok(serde_json::json!({"needed":[entry.id],"required_missing":[entry.id]})));
    let RouterCommand::Cmd { id, command: Command::PushAssetData { entry: pushed, data_b64 } } = rx.recv().await.unwrap() else { panic!("data required"); };
    assert_eq!(pushed.id, entry.id);
    use base64::Engine;
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(data_b64).unwrap(), b"test voice");
    t.app.hub.resolve(11, id, Ok(serde_json::json!({"target":pushed.target})));
    let RouterCommand::Cmd { id, command: Command::PushAssetsManifest { .. } } = rx.recv().await.unwrap() else { panic!("final verification required"); };
    t.app.hub.resolve(11, id, Ok(serde_json::json!({"needed":[],"required_missing":[]})));
    let result = task.await.unwrap().unwrap();
    assert_eq!(result["pushed"], 1); assert_eq!(result["required_missing"], serde_json::json!([]));
}

#[tokio::test]
async fn upload_rejects_traversal_and_does_not_follow_file_or_scope_symlinks() {
    let t = TestApp::new();
    for name in ["/tmp/escape", "../escape", "nested/name", ".hidden", "bad name", ""] {
        let response = upload(t.app.clone(), "llm".into(), name.into(), t.request("data")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
    }
    let root = t.dir.join("assets/llm"); std::fs::create_dir_all(&root).unwrap();
    let outside = t.dir.join("outside"); std::fs::write(&outside, b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("file.wav")).unwrap();
    let response = upload(t.app.clone(), "llm".into(), "file.wav".into(), t.request("new")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    assert_eq!(std::fs::read(root.join("file.wav")).unwrap(), b"new");
    std::os::unix::fs::symlink(&t.dir, t.dir.join("assets/media")).unwrap();
    let response = upload(t.app.clone(), "media".into(), "escape".into(), t.request("new")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!t.dir.join("escape").exists());
}

#[test]
fn oversized_asset_is_rejected_before_reading_its_contents_and_staging_is_hidden() {
    let t = TestApp::new(); let root = t.dir.join("assets/llm"); std::fs::create_dir_all(&root).unwrap();
    let file = root.join("large.bin");
    std::fs::File::create(&file).unwrap().set_len(MAX_PUSH_BYTES + 1).unwrap();
    assert!(read_push_asset(&file).unwrap_err().to_string().contains("32 MiB"));
    std::fs::remove_file(file).unwrap();
    std::fs::write(root.join(".upload-in-progress.tmp"), b"incomplete").unwrap();
    assert!(manifest_for_role(&t.app, "llm").assets.is_empty());
}
