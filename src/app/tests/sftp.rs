use super::*;

/// Regression: connecting from the SFTP picker's search must connect to the
/// *filtered* host, not whatever sits at the same index once the filter clears.
///
/// `sftp_connect_selected` used to clear the search query (rebuilding the
/// visible list) *before* reading the selection, which remapped the selected
/// index onto an unfiltered host and connected to the wrong one. The fix reads
/// the selection first. Here we filter down to the last host and assert that is
/// exactly what we connect to (`sftp_host` records the target's name).
#[test]
pub(crate) fn sftp_picker_search_connects_to_filtered_host() {
    let mut app = test_app(vec![
        ("alpha", host("alpha")),
        ("bravo", host("bravo")),
        ("charlie", host("charlie")),
    ]);
    app.active_tab = 1; // SFTP tab

    // Open picker search and narrow to the last host only.
    app.handle_key(key_char('/')).unwrap();
    for c in "charlie".chars() {
        app.handle_key(key_char(c)).unwrap();
    }

    // Enter connects. The worker thread will fail to reach charlie.example.com
    // in the background, but `sftp_host` is set synchronously to the chosen
    // target before any event is drained.
    app.handle_key(key(KeyCode::Enter)).unwrap();

    assert_eq!(app.sftp_host.as_deref(), Some("charlie"));
}

#[test]
fn edit_key_starts_a_remote_download_for_the_right_pane() {
    use crate::sftp::model::{FileEntry, SftpState};
    use crate::sftp::SftpCommand;

    let mut app = test_app(vec![]);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut state = SftpState::new("/srv", "/tmp");
    state.remote.set_entries(vec![FileEntry {
        name: "notes.txt".into(),
        is_dir: false,
        size: 12,
        is_symlink: false,
        perm: Some(0o644),
    }]);
    state.remote.selected = 1; // row 0 is the synthetic ".." entry
    app.sftp = Some(state);
    app.sftp_tx = Some(tx);
    app.active_tab = 1;

    app.handle_key(key_char('e')).unwrap();

    assert!(matches!(
        rx.try_recv().unwrap(),
        SftpCommand::EditDownload { remote, local }
            if remote == std::path::Path::new("/srv/notes.txt")
                && local.file_name().is_some_and(|name| name == "notes.txt")
    ));
    assert!(matches!(
        app.remote_edit.as_ref().map(|edit| edit.phase),
        Some(RemoteEditPhase::Downloading)
    ));
    assert!(matches!(
        app.remote_edit.as_ref().map(|edit| edit.source),
        Some(EditSource::RightRemote)
    ));
}

#[test]
fn edit_key_starts_a_local_edit_in_place_for_the_local_pane() {
    use crate::sftp::model::{FileEntry, Focus, SftpState};

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "hello").unwrap();

    crate::config::with_test_config_dir(dir.path(), || {
        let _visual = crate::config::EnvVar::set("VISUAL", "definitely-not-an-editor-sshub-test");

        let mut app = test_app(vec![]);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut state = SftpState::new("/srv", dir.path().to_str().unwrap());
        state.focus = Focus::Local;
        state.local.set_entries(vec![FileEntry {
            name: "notes.txt".into(),
            is_dir: false,
            size: 5,
            is_symlink: false,
            perm: Some(0o644),
        }]);
        state.local.selected = 1;
        state.local.cwd = dir.path().to_path_buf();
        app.sftp = Some(state);
        app.sftp_tx = Some(tx);
        app.active_tab = 1;

        app.handle_key(key_char('e')).unwrap();

        // No worker involved: the file is edited in place.
        assert!(rx.try_recv().is_err());
        let edit = app.remote_edit.as_ref().expect("local edit started");
        assert_eq!(edit.source, EditSource::Local);
        assert_eq!(edit.local_path, file);
        assert!(edit.temp_dir.is_none());
        // The bogus $VISUAL never spawns a session: the editor start fails and
        // the edit waits for a retry instead of downloading anything.
        assert_eq!(edit.phase, RemoteEditPhase::RetryingEditor);
        assert!(app
            .sftp
            .as_ref()
            .and_then(|state| state.notice.as_deref())
            .is_some_and(|notice| notice.contains("press e to retry")));
    });
}

#[test]
fn edit_key_routes_the_download_to_the_second_worker() {
    use crate::sftp::model::{FileEntry, Focus, SftpState};
    use crate::sftp::SftpCommand;

    let mut app = test_app(vec![]);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut state = SftpState::new("/srv", "/tmp");
    state.focus = Focus::Local;
    state.left_host = Some("second".into());
    state.local.set_entries(vec![FileEntry {
        name: "notes.txt".into(),
        is_dir: false,
        size: 12,
        is_symlink: false,
        perm: Some(0o644),
    }]);
    state.local.selected = 1;
    state.local.cwd = std::path::PathBuf::from("/srv");
    app.sftp = Some(state);
    app.sftp_tx2 = Some(tx);
    app.active_tab = 1;

    app.handle_key(key_char('e')).unwrap();

    assert!(matches!(
        rx.try_recv().unwrap(),
        SftpCommand::EditDownload { remote, local }
            if remote == std::path::Path::new("/srv/notes.txt")
                && local.file_name().is_some_and(|name| name == "notes.txt")
    ));
    assert!(matches!(
        app.remote_edit
            .as_ref()
            .map(|edit| (edit.source, edit.phase)),
        Some((EditSource::LeftRemote, RemoteEditPhase::Downloading))
    ));
}

#[test]
fn switching_the_left_pane_waits_for_a_second_server_edit() {
    use crate::sftp::model::SftpState;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/tmp"));
    app.active_tab = 1;
    let temp_dir = tempfile::tempdir().unwrap();
    app.remote_edit = Some(RemoteEditState {
        source: EditSource::LeftRemote,
        remote_path: "/srv/notes.txt".into(),
        local_path: temp_dir.path().join("notes.txt"),
        temp_dir: Some(temp_dir),
        remote_mode: Some(0o644),
        stamp: None,
        phase: RemoteEditPhase::Downloading,
        editor_session: None,
    });
    let (tx, _rx) = std::sync::mpsc::channel::<crate::sftp::SftpCommand>();
    app.sftp_tx2 = Some(tx);

    app.sftp_left_pane_to_local();

    assert!(
        app.sftp_tx2.is_some(),
        "switching the left pane must wait for the edit transfer"
    );
    assert!(
        app.remote_edit.is_some(),
        "the working copy must be retained"
    );
    assert!(app
        .sftp
        .as_ref()
        .and_then(|state| state.notice.as_deref())
        .is_some_and(|notice| notice.contains("wait for the edit transfer")));
}

/// A retry-phase edit still owns a working copy: switching the left pane away
/// would kill the only worker that could ever finish it, so the pane must be
/// locked until the edit is retried or discarded.
#[test]
fn switching_the_left_pane_waits_for_a_second_server_edit_retry() {
    use crate::sftp::model::SftpState;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/tmp"));
    app.active_tab = 1;
    let temp_dir = tempfile::tempdir().unwrap();
    app.remote_edit = Some(RemoteEditState {
        source: EditSource::LeftRemote,
        remote_path: "/srv/notes.txt".into(),
        local_path: temp_dir.path().join("notes.txt"),
        temp_dir: Some(temp_dir),
        remote_mode: Some(0o644),
        stamp: None,
        phase: RemoteEditPhase::RetryingEditor,
        editor_session: None,
    });
    let (tx, _rx) = std::sync::mpsc::channel::<crate::sftp::SftpCommand>();
    app.sftp_tx2 = Some(tx);

    app.sftp_left_pane_to_local();

    assert!(app.sftp_tx2.is_some(), "retry-phase edits lock the pane");
    assert!(app.remote_edit.is_some());
    assert!(app
        .sftp
        .as_ref()
        .and_then(|state| state.notice.as_deref())
        .is_some_and(|notice| notice.contains("finish or discard the edit")));
}

/// The right-hand worker must not apply edit events that belong to the
/// second-server (left) pane. The two workers share one `remote_edit`.
#[test]
fn right_worker_ignores_edit_events_for_a_left_pane_edit() {
    use crate::sftp::model::SftpState;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/tmp"));
    let temp_dir = tempfile::tempdir().unwrap();
    app.remote_edit = Some(RemoteEditState {
        source: EditSource::LeftRemote,
        remote_path: "/srv/notes.txt".into(),
        local_path: temp_dir.path().join("notes.txt"),
        temp_dir: Some(temp_dir),
        remote_mode: Some(0o644),
        stamp: None,
        phase: RemoteEditPhase::Downloading,
        editor_session: None,
    });

    app.apply_sftp_event(crate::sftp::SftpEvent::EditError("right worker".into()));

    assert_eq!(
        app.remote_edit.as_ref().map(|edit| edit.phase),
        Some(RemoteEditPhase::Downloading)
    );
    assert!(app
        .sftp
        .as_ref()
        .and_then(|state| state.notice.as_ref())
        .is_none());
}

#[test]
fn left_worker_applies_edit_events_for_a_left_pane_edit() {
    use crate::sftp::model::SftpState;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/tmp"));
    let temp_dir = tempfile::tempdir().unwrap();
    app.remote_edit = Some(RemoteEditState {
        source: EditSource::LeftRemote,
        remote_path: "/srv/notes.txt".into(),
        local_path: temp_dir.path().join("notes.txt"),
        temp_dir: Some(temp_dir),
        remote_mode: Some(0o644),
        stamp: None,
        phase: RemoteEditPhase::Downloading,
        editor_session: None,
    });

    app.apply_sftp_event_left(crate::sftp::SftpEvent::EditError("left worker".into()));

    assert_eq!(
        app.remote_edit.as_ref().map(|edit| edit.phase),
        Some(RemoteEditPhase::RetryingDownload)
    );
    assert!(app
        .sftp
        .as_ref()
        .and_then(|state| state.notice.as_deref())
        .is_some_and(|notice| notice.contains("left worker")));
}

/// The left-hand worker must not apply edit events that belong to the
/// connected (right) server. The two workers share one `remote_edit`.
#[test]
fn left_worker_ignores_edit_events_for_a_right_pane_edit() {
    use crate::sftp::model::{Phase, SftpState};
    use crate::sftp::transport::RemoteFileStamp;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/tmp"));
    let temp_dir = tempfile::tempdir().unwrap();
    app.remote_edit = Some(RemoteEditState {
        source: EditSource::RightRemote,
        remote_path: "/srv/notes.txt".into(),
        local_path: temp_dir.path().join("notes.txt"),
        temp_dir: Some(temp_dir),
        remote_mode: Some(0o644),
        stamp: Some(RemoteFileStamp {
            size: 12,
            mtime: None,
            is_regular: true,
        }),
        phase: RemoteEditPhase::Uploading,
        editor_session: None,
    });

    app.apply_sftp_event_left(crate::sftp::SftpEvent::EditUploaded(None));

    assert!(
        app.remote_edit.is_some(),
        "wrong worker must not finish the edit"
    );
    assert_eq!(
        app.remote_edit.as_ref().map(|edit| edit.phase),
        Some(RemoteEditPhase::Uploading)
    );
    assert_eq!(
        app.sftp.as_ref().map(|state| state.phase),
        Some(Phase::Browsing)
    );
}

#[test]
fn right_worker_applies_edit_events_for_a_right_pane_edit() {
    use crate::sftp::model::SftpState;
    use crate::sftp::transport::RemoteFileStamp;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/tmp"));
    let temp_dir = tempfile::tempdir().unwrap();
    app.remote_edit = Some(RemoteEditState {
        source: EditSource::RightRemote,
        remote_path: "/srv/notes.txt".into(),
        local_path: temp_dir.path().join("notes.txt"),
        temp_dir: Some(temp_dir),
        remote_mode: Some(0o644),
        stamp: Some(RemoteFileStamp {
            size: 12,
            mtime: None,
            is_regular: true,
        }),
        phase: RemoteEditPhase::Uploading,
        editor_session: None,
    });

    app.apply_sftp_event(crate::sftp::SftpEvent::EditUploaded(None));

    assert!(app.remote_edit.is_none());
    assert!(app
        .sftp
        .as_ref()
        .and_then(|state| state.notice.as_deref())
        .is_some_and(|notice| notice.contains("remote file updated")));
}

#[test]
fn local_edit_finish_clears_state_without_an_upload() {
    use crate::sftp::model::SftpState;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/tmp"));
    app.active_tab = 1;
    app.remote_edit = Some(RemoteEditState {
        source: EditSource::Local,
        remote_path: "/tmp/notes.txt".into(),
        local_path: "/tmp/notes.txt".into(),
        temp_dir: None,
        remote_mode: None,
        stamp: None,
        phase: RemoteEditPhase::Editing,
        editor_session: None,
    });

    app.local_edit_finished();

    assert!(app.remote_edit.is_none());
    assert!(app
        .sftp
        .as_ref()
        .and_then(|state| state.notice.as_deref())
        .is_some_and(|notice| notice.contains("local file edited")));
}

#[test]
fn disconnect_waits_for_an_active_edit_transfer() {
    use crate::sftp::model::SftpState;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/tmp"));
    app.active_tab = 1;
    let temp_dir = tempfile::tempdir().unwrap();
    app.remote_edit = Some(RemoteEditState {
        source: EditSource::RightRemote,
        remote_path: "/srv/notes.txt".into(),
        local_path: temp_dir.path().join("notes.txt"),
        temp_dir: Some(temp_dir),
        remote_mode: Some(0o644),
        stamp: None,
        phase: RemoteEditPhase::Downloading,
        editor_session: None,
    });

    app.handle_key(key(KeyCode::Esc)).unwrap();

    assert!(
        app.sftp.is_some(),
        "the active transfer must not be detached"
    );
    assert!(
        app.remote_edit.is_some(),
        "the working copy must be retained"
    );
    assert!(app
        .sftp
        .as_ref()
        .and_then(|state| state.notice.as_deref())
        .is_some_and(|notice| notice.contains("wait for the edit transfer")));
}

fn app_with_retry_edit(phase: RemoteEditPhase) -> App {
    use crate::sftp::model::SftpState;
    use crate::sftp::transport::RemoteFileStamp;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/tmp"));
    app.active_tab = 1;
    let temp_dir = tempfile::tempdir().unwrap();
    let local_path = temp_dir.path().join("notes.txt");
    std::fs::write(&local_path, "already edited").unwrap();
    let stamp = matches!(
        phase,
        RemoteEditPhase::Uploading | RemoteEditPhase::RetryingUpload
    )
    .then_some(RemoteFileStamp {
        size: 14,
        mtime: Some(1),
        is_regular: true,
    });
    app.remote_edit = Some(RemoteEditState {
        source: EditSource::RightRemote,
        remote_path: "/srv/notes.txt".into(),
        local_path,
        temp_dir: Some(temp_dir),
        remote_mode: Some(0o644),
        stamp,
        phase,
        editor_session: None,
    });
    app
}

/// Esc on a failed upload used to fall through `_ => None` in
/// `sftp_disconnect`, drop the working copy, and toast "remote edit
/// discarded". The file had already been edited.
#[test]
fn disconnect_does_not_silently_discard_a_retrying_upload() {
    let mut app = app_with_retry_edit(RemoteEditPhase::RetryingUpload);

    app.handle_key(key(KeyCode::Esc)).unwrap();

    assert!(
        app.sftp.is_some(),
        "Esc must not tear the session down over an unsaved edit"
    );
    assert!(
        app.remote_edit.is_some(),
        "the edited working copy must be retained"
    );
    assert_ne!(
        app.host_notice.as_deref(),
        Some("remote edit discarded"),
        "discarding an edited file must not be a silent toast"
    );
    assert_eq!(app.mode, AppMode::ConfirmDelete);
}

/// Same hole as the upload retry: a failed download also hit `_ => None`
/// and dropped the in-progress edit so `e` could never retry.
#[test]
fn disconnect_does_not_silently_discard_a_retrying_download() {
    let mut app = app_with_retry_edit(RemoteEditPhase::RetryingDownload);

    app.handle_key(key(KeyCode::Esc)).unwrap();

    assert!(
        app.sftp.is_some(),
        "Esc must not detach a retryable download"
    );
    assert!(
        app.remote_edit.is_some(),
        "the in-progress edit must be retained"
    );
    assert_ne!(app.host_notice.as_deref(), Some("remote edit discarded"));
    assert_eq!(app.mode, AppMode::ConfirmDelete);
}

#[test]
fn confirming_discard_drops_the_retrying_upload() {
    let mut app = app_with_retry_edit(RemoteEditPhase::RetryingUpload);

    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key_char('y')).unwrap();

    assert!(app.sftp.is_none(), "confirmed discard disconnects");
    assert!(
        app.remote_edit.is_none(),
        "confirmed discard drops the copy"
    );
    assert_eq!(app.host_notice.as_deref(), Some("remote edit discarded"));
    assert_eq!(app.mode, AppMode::Normal);
}

#[test]
fn cancelling_discard_keeps_the_retrying_upload() {
    let mut app = app_with_retry_edit(RemoteEditPhase::RetryingUpload);

    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Esc)).unwrap();

    assert!(app.sftp.is_some(), "cancel keeps the session");
    assert!(
        app.remote_edit.is_some(),
        "cancel keeps the edited working copy"
    );
    assert_eq!(app.mode, AppMode::Normal);
    assert!(app.pending_delete.is_none());
}

/// The SFTP progress bar sweeps toward the worker's chunked figure (#35),
/// settles on it, and resets outright when the queue moves to the next file.
#[test]
fn sftp_progress_bar_chases_the_reported_figure() {
    let app = test_app(vec![]);
    let tick = |app: &App| {
        app.sftp_progress_at.set(Some(
            std::time::Instant::now() - std::time::Duration::from_millis(16),
        ));
    };

    // First frame adopts the figure: the bar doesn't sweep in from empty.
    assert_eq!(app.sftp_progress_advance(0.4), 0.4);
    assert!(!app.sftp_progress_moving.get());

    // A chunk lands: the bar closes on it over several frames.
    tick(&app);
    let stepped = app.sftp_progress_advance(0.9);
    assert!(app.sftp_progress_moving.get());
    assert!(
        (0.4..0.9).contains(&stepped),
        "expected a partial sweep, got {stepped}"
    );
    for _ in 0..200 {
        tick(&app);
        app.sftp_progress_advance(0.9);
    }
    assert_eq!(app.sftp_progress_advance(0.9), 0.9);
    assert!(!app.sftp_progress_moving.get());

    // The next (smaller) file reports less progress: snap back rather than
    // sweeping backwards.
    tick(&app);
    assert_eq!(app.sftp_progress_advance(0.05), 0.05);
    assert!(!app.sftp_progress_moving.get());
}

/// A pane changing directory is noticed centrally and stamped with the way it
/// went (#35), whether the change came from the local filesystem or an async
/// remote listing.
#[test]
fn sftp_directory_change_stamps_its_direction() {
    use crate::sftp::model::SftpState;

    let mut app = test_app(vec![]);
    app.sftp = Some(SftpState::new("/srv", "/home/me"));

    // The first listing of a session is not a navigation.
    app.detect_sftp_navigation();
    assert!(app.sftp_nav.iter().all(|n| n.is_none()));

    // Descending into a child is stamped as going deeper.
    app.sftp.as_mut().unwrap().local.cwd = "/home/me/work".into();
    app.detect_sftp_navigation();
    assert_eq!(app.sftp_nav[0].map(|(deeper, _)| deeper), Some(true));
    assert!(app.sftp_nav[1].is_none(), "the other pane stays put");

    // Stepping back out goes the other way.
    app.sftp.as_mut().unwrap().local.cwd = "/home".into();
    app.detect_sftp_navigation();
    assert_eq!(app.sftp_nav[0].map(|(deeper, _)| deeper), Some(false));

    // A remote listing landing later is stamped just the same.
    app.sftp.as_mut().unwrap().remote.cwd = "/srv/www".into();
    app.detect_sftp_navigation();
    assert_eq!(app.sftp_nav[1].map(|(deeper, _)| deeper), Some(true));

    // Disconnecting forgets the paths, so reconnecting isn't a navigation.
    app.sftp = None;
    app.detect_sftp_navigation();
    assert!(app.sftp_nav.iter().all(|n| n.is_none()));
    app.sftp = Some(SftpState::new("/srv", "/home/me"));
    app.detect_sftp_navigation();
    assert!(app.sftp_nav.iter().all(|n| n.is_none()));
}

/// A transfer between two servers is relayed in two legs through a local temp
/// file: the source worker pulls it down, the destination worker pushes it up,
/// and only then does the item leave the queue.
#[test]
fn server_to_server_transfer_relays_in_two_legs() {
    use crate::sftp::model::{Direction, FileEntry, SftpState, Side};
    use crate::sftp::SftpCommand;
    use std::path::PathBuf;

    let mut app = test_app(vec![]);
    let (tx_right, right) = std::sync::mpsc::channel::<SftpCommand>();
    let (tx_left, left) = std::sync::mpsc::channel::<SftpCommand>();
    app.sftp_tx = Some(tx_right);
    app.sftp_tx2 = Some(tx_left);

    let mut state = SftpState::new("/srv", "/data");
    state.left_host = Some("second-host".into());
    state.remote.set_entries(vec![FileEntry {
        name: "dump.sql".into(),
        is_dir: false,
        size: 42,
        is_symlink: false,
        perm: None,
    }]);
    state.remote.selected = state
        .remote
        .entries
        .iter()
        .position(|e| e.name == "dump.sql")
        .unwrap();
    app.sftp = Some(state);

    // Stage right-to-left and run: the first leg goes to the *source* worker
    // as a download into scratch space.
    app.sftp
        .as_mut()
        .unwrap()
        .stage_toward(Side::Local)
        .unwrap();
    app.sftp_run_queue();
    let relay = app.sftp_relay.as_ref().expect("relay armed");
    assert_eq!(relay.leg, RelayLeg::Fetching);
    let scratch = relay.tmp_dir.path().to_path_buf();
    let tmp = scratch.join("dump.sql");
    // The scratch directory is owner-only: the files passing through it are the
    // user's, and /tmp is shared.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&scratch).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "scratch dir is not owner-only");
    }
    match right
        .try_recv()
        .expect("fetch leg dispatched to the source")
    {
        SftpCommand::RunQueue(q) => {
            assert_eq!(q[0].direction, Direction::Download);
            assert_eq!(q[0].src, PathBuf::from("/srv/dump.sql"));
            assert_eq!(q[0].dst, tmp, "fetched into scratch space");
        }
        _ => panic!("expected a queue run"),
    }
    assert!(left.try_recv().is_err(), "destination waits its turn");

    // The fetch lands: the push goes to the destination worker, out of scratch
    // space and into the left pane's directory.
    app.apply_sftp_event(crate::sftp::SftpEvent::QueueDone);
    assert_eq!(app.sftp_relay.as_ref().unwrap().leg, RelayLeg::Pushing);
    match left
        .try_recv()
        .expect("push leg dispatched to the destination")
    {
        SftpCommand::RunQueue(q) => {
            assert_eq!(q[0].direction, Direction::Upload);
            assert_eq!(q[0].src, tmp);
            assert_eq!(q[0].dst, PathBuf::from("/data/dump.sql"));
        }
        _ => panic!("expected a queue run"),
    }

    // The push lands: the item is done, the relay is over and the queue empty.
    app.apply_sftp_event_left(crate::sftp::SftpEvent::QueueDone);
    assert!(app.sftp_relay.is_none(), "relay finished");
    let state = app.sftp.as_ref().unwrap();
    assert!(state.queue.is_empty(), "the relayed item left the queue");
    assert_eq!(state.phase, crate::sftp::model::Phase::Browsing);
    assert!(!tmp.exists(), "scratch copy cleaned up");
    assert!(!scratch.exists(), "scratch directory goes with the relay");
}

/// A failure part-way through a relay stops it and leaves the item queued, so
/// it can be retried rather than silently vanishing.
#[test]
fn failed_relay_leg_stops_and_keeps_the_item() {
    use crate::sftp::model::{FileEntry, SftpState, Side};
    use crate::sftp::SftpCommand;

    let mut app = test_app(vec![]);
    let (tx_right, _right) = std::sync::mpsc::channel::<SftpCommand>();
    let (tx_left, _left) = std::sync::mpsc::channel::<SftpCommand>();
    app.sftp_tx = Some(tx_right);
    app.sftp_tx2 = Some(tx_left);

    let mut state = SftpState::new("/srv", "/data");
    state.left_host = Some("second-host".into());
    state.remote.set_entries(vec![FileEntry {
        name: "dump.sql".into(),
        is_dir: false,
        size: 42,
        is_symlink: false,
        perm: None,
    }]);
    state.remote.selected = 1; // past the ".." row
    app.sftp = Some(state);
    app.sftp
        .as_mut()
        .unwrap()
        .stage_toward(Side::Local)
        .unwrap();
    app.sftp_run_queue();

    app.apply_sftp_event(crate::sftp::SftpEvent::Error("disk full".into()));
    assert!(app.sftp_relay.is_none(), "relay stopped");
    let state = app.sftp.as_ref().unwrap();
    assert_eq!(state.queue.len(), 1, "the item stays queued for a retry");
    assert_eq!(state.phase, crate::sftp::model::Phase::Browsing);
    assert!(state
        .notice
        .as_deref()
        .is_some_and(|n| n.contains("disk full")));
}

/// Regression: a worker finishes its run with a `QueueDone` even after an
/// error. Acting on that used to restart the transfer that had just failed --
/// which failed again, over and over, with the queue stuck on screen.
#[test]
fn failed_relay_is_not_restarted_by_the_trailing_completion() {
    use crate::sftp::model::{FileEntry, SftpState, Side};
    use crate::sftp::SftpCommand;

    let mut app = test_app(vec![]);
    let (tx_right, right) = std::sync::mpsc::channel::<SftpCommand>();
    let (tx_left, _left) = std::sync::mpsc::channel::<SftpCommand>();
    app.sftp_tx = Some(tx_right);
    app.sftp_tx2 = Some(tx_left);

    let mut state = SftpState::new("/srv", "/data");
    state.left_host = Some("second-host".into());
    state.remote.set_entries(vec![FileEntry {
        name: "test2.py".into(),
        is_dir: false,
        size: 7,
        is_symlink: false,
        perm: None,
    }]);
    state.remote.selected = 1; // past the ".." row
    app.sftp = Some(state);
    app.sftp
        .as_mut()
        .unwrap()
        .stage_toward(Side::Local)
        .unwrap();
    app.sftp_run_queue();
    while right.try_recv().is_ok() {}

    app.apply_sftp_event(crate::sftp::SftpEvent::Error("write error".into()));
    app.apply_sftp_event(crate::sftp::SftpEvent::QueueDone);

    assert!(app.sftp_relay.is_none(), "no fresh relay armed");
    assert_eq!(
        app.sftp.as_ref().unwrap().phase,
        crate::sftp::model::Phase::Browsing,
        "the browser settles instead of looking busy forever"
    );
    assert_eq!(
        app.sftp.as_ref().unwrap().queue.len(),
        1,
        "the item stays for a manual retry"
    );
}

/// A plain (non-relayed) run that reports an error stops there too, instead of
/// rolling whatever is left into another pass that fails the same way.
#[test]
fn failed_run_does_not_dispatch_another_pass() {
    use crate::sftp::model::{Direction, FileEntry, QueuedTransfer, SftpState, Side};
    use crate::sftp::SftpCommand;

    let mut app = test_app(vec![]);
    let (tx, rx) = std::sync::mpsc::channel::<SftpCommand>();
    app.sftp_tx = Some(tx);

    let mut state = SftpState::new("/srv", "/data");
    state.remote.set_entries(vec![FileEntry {
        name: "a.bin".into(),
        is_dir: false,
        size: 1,
        is_symlink: false,
        perm: None,
    }]);
    state.remote.selected = 1; // past the ".." row
    app.sftp = Some(state);
    app.sftp
        .as_mut()
        .unwrap()
        .stage_toward(Side::Local)
        .unwrap();
    app.sftp_run_queue();
    assert!(rx.try_recv().is_ok(), "the run went out");

    // Something is staged mid-run, then the run fails.
    app.sftp.as_mut().unwrap().queue.push(QueuedTransfer {
        direction: Direction::Download,
        src: "/srv/b.bin".into(),
        dst: "/data/b.bin".into(),
        name: "b.bin".into(),
        is_dir: false,
    });
    app.apply_sftp_event(crate::sftp::SftpEvent::Error("permission denied".into()));
    app.apply_sftp_event(crate::sftp::SftpEvent::QueueDone);
    // Re-listing after the failure is expected; a second RunQueue is not.
    let dispatched_again =
        std::iter::from_fn(|| rx.try_recv().ok()).any(|c| matches!(c, SftpCommand::RunQueue(_)));
    assert!(
        !dispatched_again,
        "a failed run waits for the user rather than dispatching another pass"
    );
}

/// The dotfile setting survives a restart: it is written to `ui_state` on
/// toggle and read back when the next session builds its browser.
#[test]
fn hidden_setting_round_trips_through_the_store() {
    use crate::sftp::model::SftpState;

    let store = test_store();
    let mut app = App::new_with_deps(
        AppConfig::default(),
        AppDeps {
            resolver: Box::new(MockResolver::new(vec![])),
            metadata: Arc::new(MetadataDb::default()),
            store: store.clone(),
            password_store: Box::new(crate::credentials::NoopPasswordStore),
        },
    );
    app.reload_hosts().unwrap();
    assert!(!app.sftp_show_hidden, "dotfiles start hidden");

    app.sftp = Some(SftpState::new("/srv", "/home/me"));
    app.active_tab = 1;
    app.handle_key(key(KeyCode::Char('.'))).unwrap();
    assert!(app.sftp_show_hidden);
    assert!(app.sftp.as_ref().unwrap().local.show_hidden);
    assert!(app.sftp.as_ref().unwrap().remote.show_hidden);

    // A fresh App over the same store comes up with dotfiles shown, and a
    // browser opened in it inherits that.
    let mut next = App::new_with_deps(
        AppConfig::default(),
        AppDeps {
            resolver: Box::new(MockResolver::new(vec![])),
            metadata: Arc::new(MetadataDb::default()),
            store,
            password_store: Box::new(crate::credentials::NoopPasswordStore),
        },
    );
    next.reload_hosts().unwrap();
    assert!(next.sftp_show_hidden, "setting did not survive the restart");
}

/// Regression: `.` is a toggle in the browser but an ordinary character while
/// filtering. The search guard sits at the top of the SFTP dispatch and is easy
/// to lose in a refactor, so pin it.
#[test]
fn dot_types_into_the_filter_instead_of_toggling() {
    use crate::sftp::model::SftpState;

    let mut app = test_app(vec![]);
    let mut state = SftpState::new("/srv", "/home/me");
    state.start_search();
    app.sftp = Some(state);
    app.active_tab = 1;

    app.handle_key(key(KeyCode::Char('.'))).unwrap();
    app.handle_key(key(KeyCode::Char('s'))).unwrap();
    app.handle_key(key(KeyCode::Char('s'))).unwrap();

    let state = app.sftp.as_ref().unwrap();
    assert_eq!(state.remote.filter, ".ss", "the dot went into the filter");
    assert!(!app.sftp_show_hidden, "and did not flip the setting");
}

/// The SFTP tab reaches its themed renderer through the real frame dispatch.
///
/// The screen's own tests drive `render_browser` directly; this one proves the
/// tab wiring — that `render` selects the SFTP body and hands it the active
/// theme — with a marker no other role carries.
#[test]
fn the_sftp_tab_renders_through_the_active_theme() {
    use crate::sftp::model::{FileEntry, Focus, SftpState};
    use crate::test_support::{fg, marker, role_marker_theme};

    const DIRECTORY: u32 = 0xb1_0001;

    let mut app = test_app(vec![]);
    app.activate_resolved_theme(std::rc::Rc::new(role_marker_theme(
        "sftp-tab",
        &[fg("components.sftp.remote", DIRECTORY)],
    )));
    app.active_tab = 1;

    let mut state = SftpState::new("/srv", "/data");
    state.focus = Focus::Remote;
    state.remote.set_entries(vec![FileEntry {
        name: "uploads".into(),
        is_dir: true,
        size: 0,
        is_symlink: false,
        perm: None,
    }]);
    // Row 0 is the synthetic ".." entry, so `uploads` is not the selected one
    // and reads its directory role rather than the selection bar.
    state.remote.selected = 0;
    app.sftp = Some(state);

    let area = ratatui::layout::Rect::new(0, 0, 100, 30);
    let buf = crate::test_support::frame_at(area, |frame| crate::tui::render(frame, &app));
    assert_eq!(
        crate::test_support::fg_at_text_from(&buf, "uploads", 1),
        marker(DIRECTORY),
        "a remote directory row wears `components.sftp.remote`"
    );
}
