//! Environment parsing tests for the io_uring feature knobs
//! (`MINISS_URING_<FEATURE>=auto|on|off`), their legacy spellings, and the
//! backend-inertness contract on the readiness backends.
//!
//! Everything lives in ONE sequential test function on purpose: the process
//! environment is global state, and these variables are read by every
//! runtime construction in every other test binary. Keeping the
//! mutate-then-restore windows inside a single test in this dedicated
//! process makes the suite deterministic (the same hygiene rule
//! `src/config.rs`'s env test follows).

use rust_miniss::io::uring_features::{resolve_uring_flags, FeatureKnob, FeatureRequest};

/// Every variable the feature-knob parser reads, so each section starts from
/// and restores a known-empty environment.
const KNOB_VARS: &[&str] = &[
    "MINISS_URING_COOP_TASKRUN",
    "MINISS_URING_DEFER_TASKRUN",
    "MINISS_URING_SINGLE_ISSUER",
    "MINISS_URING_SQPOLL",
    "MINISS_URING_MULTISHOT_RECV",
    "MINISS_URING_BUF_RING",
    "MINISS_URING_NO_COOP_TASKRUN",
];

fn clear_knob_env() {
    for var in KNOB_VARS {
        // SAFETY: tests run single-threaded per process env; remove_var is
        // safe here because no other thread reads these variables
        // concurrently (this is this binary's only env-mutating test).
        unsafe {
            std::env::remove_var(var);
        }
    }
}

fn set_knob_env(var: &str, value: &str) {
    // SAFETY: as `clear_knob_env`.
    unsafe {
        std::env::set_var(var, value);
    }
}

#[test]
fn env_knob_matrix_legacy_mapping_and_backend_inertness() {
    clear_knob_env();

    // No knobs set: every knob is auto, the legacy buffer-ring switch is
    // off. The documented defaults must be unchanged.
    let request = FeatureRequest::from_env();
    assert_eq!(request.coop_taskrun, FeatureKnob::Auto);
    assert_eq!(request.defer_taskrun, FeatureKnob::Auto);
    assert_eq!(request.single_issuer, FeatureKnob::Auto);
    assert_eq!(request.sqpoll, FeatureKnob::Auto);
    assert_eq!(request.multishot_recv, FeatureKnob::Auto);
    assert!(!request.legacy_buf_ring);
    let resolved = resolve_uring_flags(&request).expect("defaults never conflict");
    assert!(resolved.coop_taskrun);
    assert!(!resolved.defer_taskrun);
    assert!(!resolved.single_issuer);
    assert!(!resolved.sqpoll);
    assert!(!resolved.multishot_recv);
    assert!(!resolved.buf_ring);

    // New spellings.
    for (raw, expected) in [
        ("auto", FeatureKnob::Auto),
        ("on", FeatureKnob::On),
        ("off", FeatureKnob::Off),
    ] {
        set_knob_env("MINISS_URING_COOP_TASKRUN", raw);
        assert_eq!(
            FeatureRequest::from_env().coop_taskrun,
            expected,
            "MINISS_URING_COOP_TASKRUN={raw}"
        );
    }
    clear_knob_env();

    // Legacy binary values keep working on every knob: `1` = on, `0` = off.
    for var in [
        "MINISS_URING_DEFER_TASKRUN",
        "MINISS_URING_SINGLE_ISSUER",
        "MINISS_URING_SQPOLL",
        "MINISS_URING_MULTISHOT_RECV",
    ] {
        set_knob_env(var, "1");
        let request = FeatureRequest::from_env();
        assert_eq!(knob_for(&request, var), FeatureKnob::On, "{var}=1");
        clear_knob_env();
        set_knob_env(var, "0");
        let request = FeatureRequest::from_env();
        assert_eq!(knob_for(&request, var), FeatureKnob::Off, "{var}=0");
        clear_knob_env();
    }

    // Legacy COOP opt-out: presence (any value) of
    // MINISS_URING_NO_COOP_TASKRUN maps to COOP_TASKRUN=off.
    set_knob_env("MINISS_URING_NO_COOP_TASKRUN", "1");
    assert_eq!(FeatureRequest::from_env().coop_taskrun, FeatureKnob::Off);
    set_knob_env("MINISS_URING_NO_COOP_TASKRUN", "");
    assert_eq!(
        FeatureRequest::from_env().coop_taskrun,
        FeatureKnob::Off,
        "legacy spelling was presence-checked: even an empty value opted out"
    );
    clear_knob_env();

    // Precedence: an explicit on/off on the new knob overrides the legacy
    // spelling; `auto` has no opinion, so the legacy spelling applies (auto
    // behaves exactly like an unset variable).
    set_knob_env("MINISS_URING_NO_COOP_TASKRUN", "1");
    set_knob_env("MINISS_URING_COOP_TASKRUN", "on");
    assert_eq!(FeatureRequest::from_env().coop_taskrun, FeatureKnob::On);
    set_knob_env("MINISS_URING_COOP_TASKRUN", "off");
    assert_eq!(FeatureRequest::from_env().coop_taskrun, FeatureKnob::Off);
    set_knob_env("MINISS_URING_COOP_TASKRUN", "auto");
    assert_eq!(
        FeatureRequest::from_env().coop_taskrun,
        FeatureKnob::Off,
        "auto defers to the legacy opt-out"
    );
    clear_knob_env();

    // Legacy buffer-ring switch: 1/on register the ring alone (no
    // multishot), 0/off/auto do not, and the value flows into the resolved
    // flags without enabling multishot.
    for (raw, registered) in [("1", true), ("on", true), ("0", false), ("off", false)] {
        set_knob_env("MINISS_URING_BUF_RING", raw);
        let request = FeatureRequest::from_env();
        assert_eq!(request.legacy_buf_ring, registered, "BUF_RING={raw}");
        let resolved = resolve_uring_flags(&request).expect("legacy buf_ring never conflicts");
        assert_eq!(resolved.buf_ring, registered, "BUF_RING={raw}");
        assert!(
            !resolved.multishot_recv,
            "BUF_RING={raw} must not enable multishot"
        );
        clear_knob_env();
    }

    // Empty values are treated as unset (the MINISS_IO_BACKEND convention);
    // the legacy binary spelling `=1` still engages the feature.
    set_knob_env("MINISS_URING_DEFER_TASKRUN", "");
    assert_eq!(FeatureRequest::from_env().defer_taskrun, FeatureKnob::Auto);
    set_knob_env("MINISS_URING_DEFER_TASKRUN", "1");
    assert_eq!(FeatureRequest::from_env().defer_taskrun, FeatureKnob::On);
    clear_knob_env();

    // Invalid values refuse startup with the same clarity as the
    // MINISS_IO_BACKEND unknown-backend panic: the message names the
    // variable, the offending value, and the expected spellings.
    set_knob_env("MINISS_URING_SQPOLL", "banana");
    let panic_payload = std::panic::catch_unwind(|| {
        let _ = FeatureRequest::from_env();
    })
    .expect_err("an unreadable knob value must refuse startup");
    let panic_text = panic_payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic_payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .expect("panic payload is a string");
    assert!(panic_text.contains("MINISS_URING_SQPOLL"), "{panic_text}");
    assert!(panic_text.contains("banana"), "{panic_text}");
    assert!(panic_text.contains("auto|on|off"), "{panic_text}");
    clear_knob_env();
    set_knob_env("MINISS_URING_BUF_RING", "yes-please");
    let panic_payload = std::panic::catch_unwind(|| {
        let _ = FeatureRequest::from_env();
    })
    .expect_err("an unreadable legacy buffer-ring value must refuse startup");
    let panic_text = panic_payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic_payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .expect("panic payload is a string");
    assert!(panic_text.contains("MINISS_URING_BUF_RING"), "{panic_text}");
    clear_knob_env();

    // Backend inertness: on the readiness backends (epoll/kqueue) the knobs
    // are neither applied nor validated — construction succeeds even with
    // garbage values, and the resolver itself never sees the environment.
    inertness_subtest();

    clear_knob_env();
}

/// Read one of the four same-name knob fields for the legacy-value loop.
fn knob_for(request: &FeatureRequest, var: &str) -> FeatureKnob {
    match var {
        "MINISS_URING_DEFER_TASKRUN" => request.defer_taskrun,
        "MINISS_URING_SINGLE_ISSUER" => request.single_issuer,
        "MINISS_URING_SQPOLL" => request.sqpoll,
        "MINISS_URING_MULTISHOT_RECV" => request.multishot_recv,
        other => panic!("unexpected var {other}"),
    }
}

/// On the readiness backends the knobs are inert: construction succeeds and
/// ignores them (a debug note is logged), even for values that would panic
/// on the io_uring path. Only compiled where `MioBackend` exists.
#[cfg(any(io_backend = "epoll", all(unix, not(target_os = "linux"))))]
fn inertness_subtest() {
    set_knob_env("MINISS_URING_SQPOLL", "banana");
    set_knob_env("MINISS_URING_DEFER_TASKRUN", "on");
    let backend = rust_miniss::io::MioBackend::new()
        .expect("the readiness backend must construct with MINISS_URING_* knobs set");
    drop(backend);
    clear_knob_env();
}

#[cfg(not(any(io_backend = "epoll", all(unix, not(target_os = "linux")))))]
fn inertness_subtest() {
    // io_uring build: the inertness contract is structural here — the knobs
    // apply only through `UringBackend::new`, and `MioBackend` is not part
    // of this build. The resolver-side proof (knobs are pure inputs, the
    // environment is read only in `FeatureRequest::from_env`) is covered by
    // the exhaustive tests in `src/io/uring_features.rs`.
    let _ = rust_miniss::io::uring_features::UringFeature::CoopTaskrun;
}
