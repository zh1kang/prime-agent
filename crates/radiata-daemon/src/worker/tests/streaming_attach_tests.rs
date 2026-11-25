use super::*;

#[tokio::test]
async fn mid_turn_attach_includes_streaming_message() {
    let worker = created_dispatch_worker().await;
    let streaming = json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": "partial reply" }],
        "timestamp": 42,
    });
    {
        let mut core = worker.core.lock().unwrap();
        core.busy = true;
        // Non-assistant message frames must never leak into streamingMessage.
        core.track_streaming_message(&json!({
            "type": "message_start",
            "message": { "role": "user", "content": "hi" }
        }));
        assert!(core.streaming_message.is_none());
        core.track_streaming_message(&json!({
            "type": "message_update",
            "message": streaming,
        }));
        core.created = true;
    }
    let response = worker
        .dispatch(
            "attach",
            &json!({
                "activeSessionId": "suspension-session",
                "clientId": "stream-client",
                "capabilities": ["attach_snapshot", "event_sequence"],
            }),
        )
        .await;
    assert!(response.success, "attach failed: {response:?}");
    let data = response.data.expect("attach data");
    let snapshot = &data["snapshot"];
    assert_eq!(snapshot["streamingMessage"], streaming);

    worker.core.lock().unwrap().clear_streaming_message();
    let response = worker
        .dispatch(
            "attach",
            &json!({
                "activeSessionId": "suspension-session",
                "clientId": "stream-client-2",
                "capabilities": ["attach_snapshot", "event_sequence"],
            }),
        )
        .await;
    assert!(response.success, "second attach failed: {response:?}");
    let snapshot = &response.data.expect("attach data")["snapshot"];
    assert!(snapshot.get("streamingMessage").is_none());
}