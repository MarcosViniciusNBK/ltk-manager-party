use super::*;
use ltk_manager_core::room_sync::{
    CanonicalRoomArtifact, ContentHash, RoomMod, RoomModFormat, TransferDirection,
    ROOM_MANIFEST_SCHEMA_VERSION,
};
use parking_lot::Mutex;
use std::io::Cursor;

#[derive(Default)]
struct RecordingEvents(Mutex<Vec<BackendEvent>>);

impl EventSink for RecordingEvents {
    fn emit(&self, event: BackendEvent) {
        self.0.lock().push(event);
    }
}

fn setup() -> (RoomSyncState, Arc<RecordingEvents>) {
    let root = tempfile::tempdir().unwrap().keep();
    let library = root.join("library").join("mods");
    fs::create_dir_all(&library).unwrap();
    let events = Arc::new(RecordingEvents::default());
    let state = RoomSyncState::open(&root, &[library], events.clone()).unwrap();
    (state, events)
}

fn manifest(bytes: &[u8]) -> (RoomManifest, CanonicalRoomArtifact) {
    let artifact = CanonicalRoomArtifact {
        content_hash: ContentHash::from_reader(Cursor::new(bytes)).unwrap(),
        size_bytes: bytes.len() as u64,
        format: RoomModFormat::Modpkg,
    };
    (
        RoomManifest {
            schema_version: ROOM_MANIFEST_SCHEMA_VERSION,
            room_id: "room_a".to_string(),
            revision: 1,
            game_build: None,
            mods: vec![RoomMod {
                content_hash: artifact.content_hash.clone(),
                size_bytes: artifact.size_bytes,
                format: artifact.format,
                display_name: "Shared mod".to_string(),
                version: String::new(),
                enabled: true,
                suggested_layers: Vec::new(),
            }],
        },
        artifact,
    )
}

#[test]
fn draft_membership_and_cached_manifest_stay_outside_game_actions() {
    let (state, events) = setup();
    let joined = state.create_draft("room_a").unwrap();
    assert!(joined.member_id.starts_with("local-"));
    assert_eq!(
        state.local_status("room_a").unwrap().prepared_revision,
        None,
        "creating a draft cannot prepare or apply a revision"
    );
    assert!(state.local_status("room_a").unwrap().profile.is_none());

    let bytes = b"shared archive";
    let (manifest, artifact) = manifest(bytes);
    let partial = state.cache.prepare_partial(&artifact.content_hash).unwrap();
    fs::write(partial, bytes).unwrap();
    state.cache.commit_partial(&artifact).unwrap();

    let snapshot = state.synchronize_manifest(manifest).unwrap();
    assert_eq!(snapshot.phase, RoomSyncPhase::Synchronized);
    assert_eq!(state.cache_status().unwrap().referenced_blobs, 1);
    assert_eq!(
        state
            .local_status("room_a")
            .unwrap()
            .cached_content_hashes
            .len(),
        1
    );
    assert!(state.leave("room_a").unwrap());

    let recorded = events.0.lock();
    assert!(recorded.iter().any(|event| matches!(
        event,
        BackendEvent::RoomPresenceChanged(RoomPresenceChanged {
            state: RoomPresenceState::Joined,
            ..
        })
    )));
    assert!(recorded.iter().any(|event| matches!(
        event,
        BackendEvent::RoomSyncProgress(RoomSyncSnapshot {
            phase: RoomSyncPhase::Synchronized,
            ..
        })
    )));
    assert!(recorded.iter().any(|event| matches!(
        event,
        BackendEvent::RoomPresenceChanged(RoomPresenceChanged {
            state: RoomPresenceState::Left,
            ..
        })
    )));
}

#[test]
fn transfer_and_duplicate_presence_events_are_throttled() {
    let (state, events) = setup();
    let progress = TransferProgress {
        room_id: "room_a".to_string(),
        content_hash: ContentHash::from_reader(Cursor::new(b"blob")).unwrap(),
        display_name: Some("Shared mod".to_string()),
        direction: TransferDirection::Download,
        transferred_bytes: 1,
        total_bytes: 4,
        attempt: 1,
    };
    let transfer = state.transfer_progress_callback();
    transfer(progress.clone());
    transfer(progress);
    transfer(TransferProgress {
        room_id: "room_a".to_string(),
        content_hash: ContentHash::from_reader(Cursor::new(b"blob")).unwrap(),
        display_name: Some("Shared mod".to_string()),
        direction: TransferDirection::Download,
        transferred_bytes: 4,
        total_bytes: 4,
        attempt: 1,
    });
    state.events.emit_presence(RoomPresenceChanged {
        room_id: "room_a".to_string(),
        member_id: "member_a".to_string(),
        state: RoomPresenceState::Joined,
    });
    state.events.emit_presence(RoomPresenceChanged {
        room_id: "room_a".to_string(),
        member_id: "member_a".to_string(),
        state: RoomPresenceState::Joined,
    });

    let recorded = events.0.lock();
    assert_eq!(
        recorded
            .iter()
            .filter(|event| matches!(event, BackendEvent::RoomTransferProgress(_)))
            .count(),
        2
    );
    assert_eq!(
        recorded
            .iter()
            .filter(|event| matches!(event, BackendEvent::RoomPresenceChanged(_)))
            .count(),
        1
    );
}

#[test]
fn staged_manifest_is_visible_while_its_mod_is_downloading() {
    let (state, _) = setup();
    state.create_draft("room_a").unwrap();
    let (manifest, _) = manifest(b"not downloaded yet");

    let snapshot = state.synchronize_manifest(manifest.clone()).unwrap();

    assert_eq!(snapshot.phase, RoomSyncPhase::Transferring);
    assert_eq!(state.accepted_manifest("room_a").unwrap(), Some(manifest));
}

#[test]
fn realtime_event_parser_only_accepts_named_events() {
    assert_eq!(
        room_event_name(r#"{"event":"manifest_published","data":{"revision":2}}"#).as_deref(),
        Some("manifest_published")
    );
    assert_eq!(room_event_name(r#"{"data":{}}"#), None);
    assert_eq!(room_event_name("not-json"), None);
}

#[test]
fn remote_members_accept_server_snake_case_and_emit_ipc_camel_case() {
    let wire: RemoteMemberInfoWire = serde_json::from_value(serde_json::json!({
        "member_id": "member_a",
        "display_name": "DESKTOP-ALPHA",
        "role": "member",
        "last_acknowledged_revision": 4,
        "ack_status": "synchronized",
        "is_online": true,
        "is_stale": false,
        "joined_at": "ignored server field"
    }))
    .unwrap();
    let member = RemoteMemberInfo::from(wire);

    assert_eq!(member.member_id, "member_a");
    assert_eq!(member.display_name, "DESKTOP-ALPHA");
    assert_eq!(member.last_acknowledged_revision, 4);

    let ipc = serde_json::to_value(member).unwrap();
    assert_eq!(ipc["memberId"], "member_a");
    assert_eq!(ipc["displayName"], "DESKTOP-ALPHA");
    assert_eq!(ipc["lastAcknowledgedRevision"], 4);
    assert!(ipc.get("member_id").is_none());
}

#[test]
fn computer_names_are_sanitized_for_visible_room_identity() {
    assert_eq!(
        normalize_computer_display_name(Some("  DESKTOP-ALPHA\n".to_string())),
        "DESKTOP-ALPHA"
    );
    assert_eq!(normalize_computer_display_name(None), "This computer");
    assert_eq!(
        normalize_computer_display_name(Some("x".repeat(100))),
        "x".repeat(64)
    );
}

#[test]
fn room_activity_distinguishes_idle_checking_and_real_work() {
    let (state, events) = setup();
    assert_eq!(
        state.local_status("room_a").unwrap().activity.stage,
        RoomActivityStage::Idle
    );

    state
        .events
        .emit_activity("room_a", RoomActivityStage::Checking);
    assert_eq!(
        state.local_status("room_a").unwrap().activity.stage,
        RoomActivityStage::Checking
    );

    state
        .events
        .emit_activity("room_a", RoomActivityStage::Uploading);
    state
        .events
        .finish_check("room_a", RoomActivityStage::Complete);
    assert_eq!(
        state.local_status("room_a").unwrap().activity.stage,
        RoomActivityStage::Uploading,
        "a completed read-only check must not hide a real transfer"
    );
    assert!(events
        .0
        .lock()
        .iter()
        .any(|event| matches!(event, BackendEvent::RoomActivityChanged(_))));
}

#[test]
fn library_change_notification_advances_the_realtime_generation() {
    let (state, _) = setup();
    let before = state.library_change_generation.load(Ordering::Acquire);
    state.notify_library_changed();
    assert_eq!(
        state.library_change_generation.load(Ordering::Acquire),
        before + 1
    );
}
