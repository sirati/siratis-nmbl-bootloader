#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::field_reassign_with_default,
    reason = "tests assert on contract failures"
)]

use nonmax::NonMaxU32;
use serde::{Deserialize, Serialize};
use tempfile::tempdir;

use crate::state::{
    FILE_SIZE, KNOWN_VERSION, State, init_or_validate, mark_boot_succeeded, read, write_padded,
};

fn file_size() -> usize {
    FILE_SIZE
}

fn known_version() -> u32 {
    KNOWN_VERSION
}

#[test]
fn roundtrip_with_all_known_good_slots_filled() {
    let mut state = State::default();
    for (i, slot) in state.known_good_generations.iter_mut().enumerate() {
        // Skip 0 → start at 1 so each slot has a distinct non-zero
        // value; NonMaxU32::new(u32::MAX) returns None.
        *slot = NonMaxU32::new((i as u32) + 1);
    }
    state.last_attempted_generation = NonMaxU32::new(42);
    state.last_boot_succeeded = true;
    state.recovery_attempt = 3;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("state.bin");
    write_padded(&path, &state).expect("write_padded");
    let reread = read(&path).expect("read").expect("Some(State)");
    assert_eq!(reread, state);
}

#[test]
fn forward_compat_v1_decoded_by_hypothetical_v2_fills_default() {
    // Pretend a future "v2" added a `future_field`. Encode v1,
    // decode through the v2 struct — `future_field` must come back
    // as 0 (the serde default), proving newer binaries can read
    // older files.
    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct V2 {
        state_format_version: u32,
        #[serde(default)]
        last_attempted_generation: Option<NonMaxU32>,
        #[serde(default)]
        last_boot_succeeded: bool,
        #[serde(default)]
        recovery_attempt: u32,
        #[serde(default = "crate::state::default_known_good")]
        known_good_generations: [Option<NonMaxU32>; 20],
        #[serde(default)]
        future_field: u64,
    }

    let v1 = State::default();
    let mut buf: Vec<u8> = Vec::new();
    ciborium::into_writer(&v1, &mut buf).expect("encode v1");
    let decoded: V2 = ciborium::from_reader(&buf[..]).expect("decode v2");
    assert_eq!(decoded.future_field, 0);
    assert_eq!(decoded.state_format_version, v1.state_format_version);
}

#[test]
fn forward_compat_v2_decoded_by_v1_skips_unknown_field() {
    // The OPPOSITE direction: a v2 writer emits an extra field,
    // and a v1 reader (= production `State`) silently ignores it.
    // This is the property that's only true because `State` does
    // NOT carry `#[serde(deny_unknown_fields)]`.
    #[derive(Debug, Serialize, Deserialize)]
    struct V2 {
        state_format_version: u32,
        last_attempted_generation: Option<NonMaxU32>,
        last_boot_succeeded: bool,
        recovery_attempt: u32,
        known_good_generations: [Option<NonMaxU32>; 20],
        future_field: u64,
    }

    let v2 = V2 {
        state_format_version: 1,
        last_attempted_generation: NonMaxU32::new(7),
        last_boot_succeeded: true,
        recovery_attempt: 1,
        known_good_generations: [None; 20],
        future_field: 0xdead_beef,
    };
    let mut buf: Vec<u8> = Vec::new();
    ciborium::into_writer(&v2, &mut buf).expect("encode v2");
    let decoded: State = ciborium::from_reader(&buf[..]).expect("decode v2 into v1");
    assert_eq!(decoded.state_format_version, 1);
    assert_eq!(decoded.recovery_attempt, 1);
    assert!(decoded.last_boot_succeeded);
}

#[test]
fn write_padded_produces_exactly_16k_for_empty_state() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("state.bin");
    write_padded(&path, &State::default()).expect("write_padded");
    let meta = std::fs::metadata(&path).expect("metadata");
    assert_eq!(meta.len() as usize, file_size());
}

#[test]
fn write_padded_produces_exactly_16k_for_full_state() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("state.bin");
    let mut state = State::default();
    for (i, slot) in state.known_good_generations.iter_mut().enumerate() {
        *slot = NonMaxU32::new((i as u32) + 100);
    }
    state.last_attempted_generation = NonMaxU32::new(1_000);
    state.last_boot_succeeded = true;
    state.recovery_attempt = u32::MAX - 1;
    write_padded(&path, &state).expect("write_padded");
    let meta = std::fs::metadata(&path).expect("metadata");
    assert_eq!(meta.len() as usize, file_size());
}

#[test]
fn read_too_new_version_returns_none() {
    // A future v999 file. The current binary must log a warning
    // (verified end-to-end by Phase 6 VM tests; here we just pin
    // the `Ok(None)` graceful-fallback contract) and refuse to
    // touch it.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("state.bin");
    let future = State {
        state_format_version: known_version() + 1000,
        ..State::default()
    };
    let mut buf: Vec<u8> = Vec::new();
    ciborium::into_writer(&future, &mut buf).expect("encode future");
    buf.resize(file_size(), 0);
    std::fs::write(&path, &buf).expect("write");

    assert!(read(&path).expect("read").is_none());
}

#[test]
fn init_or_validate_on_missing_dir_creates_state() {
    let dir = tempdir().expect("tempdir");
    let nested = dir.path().join("nested").join("state-dir");
    assert!(!nested.exists());
    let state = init_or_validate(&nested).expect("init_or_validate");
    assert_eq!(state, State::default());
    assert!(nested.join("state.bin").exists());
    let meta = std::fs::metadata(nested.join("state.bin")).expect("metadata");
    assert_eq!(meta.len() as usize, file_size());
}

#[test]
fn init_or_validate_on_existing_canonical_state_succeeds() {
    // Round-trip case: a previously-written canonical state must
    // re-validate without error.
    let dir = tempdir().expect("tempdir");
    let _first = init_or_validate(dir.path()).expect("first init");
    let second = init_or_validate(dir.path()).expect("second init");
    assert_eq!(second, State::default());
}

#[test]
fn mark_boot_succeeded_sets_flag_and_resets_recovery() {
    let dir = tempdir().expect("tempdir");
    // Seed with a state that has the "boot failed" shape.
    let seed = State {
        last_boot_succeeded: false,
        recovery_attempt: 4,
        last_attempted_generation: NonMaxU32::new(17),
        ..State::default()
    };
    write_padded(&dir.path().join("state.bin"), &seed).expect("seed write");

    mark_boot_succeeded(dir.path()).expect("mark_boot_succeeded");

    let after = read(&dir.path().join("state.bin"))
        .expect("read")
        .expect("Some");
    assert!(after.last_boot_succeeded);
    assert_eq!(after.recovery_attempt, 0);
    // Version preserved — must NEVER auto-bump.
    assert_eq!(after.state_format_version, known_version());
    // Other fields untouched.
    assert_eq!(after.last_attempted_generation, NonMaxU32::new(17));
}

#[test]
fn mark_boot_succeeded_on_missing_file_is_noop() {
    let dir = tempdir().expect("tempdir");
    // No state.bin written. Must return Ok(()) without panicking.
    mark_boot_succeeded(dir.path()).expect("noop on missing");
    assert!(!dir.path().join("state.bin").exists());
}

#[test]
fn mark_boot_succeeded_on_too_new_version_is_noop() {
    // A too-new file must not be rewritten.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("state.bin");
    let future = State {
        state_format_version: known_version() + 1,
        ..State::default()
    };
    let mut buf: Vec<u8> = Vec::new();
    ciborium::into_writer(&future, &mut buf).expect("encode");
    buf.resize(file_size(), 0);
    std::fs::write(&path, &buf).expect("write seed");
    let before = std::fs::read(&path).expect("read seed");

    mark_boot_succeeded(dir.path()).expect("noop on too-new");

    let after = std::fs::read(&path).expect("read after");
    assert_eq!(after, before);
}

#[test]
fn init_state_preserves_actual_prior_ns1_cbor_without_new_rescue_fields() {
    let bytes = include_bytes!("fixtures/ns1-state-v1-before-rescue-ready.bin");
    assert_eq!(bytes.len(), FILE_SIZE);
    let dir = tempdir().expect("old state directory");
    let path = dir.path().join("state.bin");
    std::fs::write(&path, bytes).expect("install exact prior NS1 state");
    let before = read(&path)
        .expect("read prior state")
        .expect("supported prior state");
    assert_eq!(before.rescue_booted_generation, None);
    assert!(!before.rescue_exit_retry_in_progress);
    let validated = init_or_validate(dir.path()).expect("validate prior CBOR with serde defaults");
    assert_eq!(validated, before);
    assert_eq!(std::fs::read(&path).expect("original bytes"), bytes);
    // A later ordinary state update may serialize the added fields, while
    // preserving existing failure/recovery history and remaining readable.
    write_padded(&path, &validated).expect("ordinary current-format rewrite");
    assert_eq!(read(&path).expect("current read"), Some(before));
}

#[test]
fn compatible_unknown_fields_are_validated_without_rewriting_or_discarding_them() {
    let mut encoded = Vec::new();
    ciborium::into_writer(&State::default(), &mut encoded).expect("encode state");
    let mut value: ciborium::Value =
        ciborium::from_reader(encoded.as_slice()).expect("state value");
    let ciborium::Value::Map(fields) = &mut value else {
        panic!("state map");
    };
    fields.push((
        ciborium::Value::Text("compatible_future_field".into()),
        ciborium::Value::Integer(17.into()),
    ));
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes).expect("encode compatible value");
    bytes.resize(FILE_SIZE, 0);
    let dir = tempdir().expect("directory");
    let path = dir.path().join("state.bin");
    std::fs::write(&path, &bytes).expect("write");
    assert_eq!(
        init_or_validate(dir.path()).expect("compatible unknown field"),
        State::default()
    );
    assert_eq!(std::fs::read(path).expect("bytes preserved"), bytes);
}

#[test]
fn init_state_refuses_nonzero_padding_or_truncated_prior_state_without_rewriting() {
    let original = include_bytes!("fixtures/ns1-state-v1-before-rescue-ready.bin");
    for bytes in [
        {
            let mut damaged = original.to_vec();
            *damaged.last_mut().expect("last padding byte") = 1;
            damaged
        },
        original[..original.len() - 1].to_vec(),
    ] {
        let dir = tempdir().expect("directory");
        let path = dir.path().join("state.bin");
        std::fs::write(&path, &bytes).expect("write damaged prior state");
        assert!(init_or_validate(dir.path()).is_err());
        assert_eq!(std::fs::read(path).expect("unchanged damaged bytes"), bytes);
    }
}
