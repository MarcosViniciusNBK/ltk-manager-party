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
