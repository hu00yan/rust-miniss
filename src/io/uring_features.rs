//! User-facing io_uring feature knobs: three-state resolution with
//! structured setup-time errors.
//!
//! Every io_uring setup feature gets one environment knob
//! `MINISS_URING_<FEATURE>=auto|on|off` (legacy spellings keep working — see
//! [`FeatureRequest::from_env`]):
//!
//! - `auto` (default): the runtime's documented default for the feature,
//!   with the documented fallback when the kernel does not support it
//!   (COOP_TASKRUN's plain-builder retry, buffer-ring registration
//!   degradation). Auto resolutions never fail setup, and every one of them
//!   is recorded in [`ResolvedFlags::resolutions`] and surfaced at
//!   construction, so behavior is never silently different from
//!   flags-as-asked.
//! - `on`: the feature is requested, period. On a kernel that predates the
//!   feature (readable release probe) construction fails with a structured
//!   [`FeatureError`] naming the flag, the kernel constraint, the minimum
//!   kernel version and the running kernel; if the release is unreadable,
//!   the kernel's own setup error propagates. Explicit requests never fall
//!   back silently.
//! - `off`: the feature is never requested.
//!
//! Contradictions are decided by ONE table-driven function,
//! [`resolve_uring_flags`], driven by [`CONSTRAINT_TABLE`]. A kernel-rejected
//! pair requested as explicit `on` + explicit `on` is a conflict error; when
//! one side is `auto`, the auto side yields and the resolution is recorded.
//! The pure boolean constraint layer over the five effective flags is
//! exhaustively tested (2^5) alongside the full tri-state matrix (3^5) in
//! this module's tests.
//!
//! Kernel constraint sources (verified 2026-10-31 against io_uring_setup(2)
//! and `io_uring/io_uring.c` at v6.1 and master):
//!
//! - `IORING_SETUP_SQPOLL` combined with `IORING_SETUP_COOP_TASKRUN`,
//!   `IORING_SETUP_TASKRUN_FLAG` or `IORING_SETUP_DEFER_TASKRUN` fails setup
//!   with `-EINVAL` ("IPI related flags don't make sense with SQPOLL").
//! - `IORING_SETUP_DEFER_TASKRUN` requires `IORING_SETUP_SINGLE_ISSUER`
//!   (`-EINVAL` otherwise).
//! - `IORING_SETUP_COOP_TASKRUN` + `IORING_SETUP_DEFER_TASKRUN` is NOT
//!   kernel-rejected (no check tests both flags together on v6.1 or master);
//!   io_uring_setup(2) documents them as alternative taskrun modes. The
//!   runtime keeps them mutually exclusive so the requested semantics never
//!   silently change — see the [`CONSTRAINT_TABLE`] comment.
//! - `IORING_SETUP_SINGLE_ISSUER` with SQPOLL: never rejected, nothing
//!   clears it — the polling task is inherently the sole submitter. No rule
//!   is encoded; the redundancy is recorded instead.

use std::fmt;

/// A three-state user-facing feature knob.
///
/// `auto` (the default for every knob) means "the runtime's documented
/// default, with the documented fallback"; `on`/`off` are unconditional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureKnob {
    /// Runtime default + documented fallback; every auto resolution is
    /// recorded and surfaced at construction.
    Auto,
    /// Unconditional request: loud setup failure when the kernel cannot
    /// honor it, never a silent fallback.
    On,
    /// Never requested.
    Off,
}

impl fmt::Display for FeatureKnob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FeatureKnob::Auto => "auto",
            FeatureKnob::On => "on",
            FeatureKnob::Off => "off",
        })
    }
}

/// Parse one knob value. Accepts the three-state spellings plus the legacy
/// binary values (`1` = on, `0` = off) so pre-knob environments keep
/// working. Anything else refuses startup with the same clarity as
/// `MINISS_IO_BACKEND`'s unknown-backend panic. An empty value is treated as
/// unset (the same convention as the backend override).
pub fn parse_knob_value(var: &str, raw: &str) -> FeatureKnob {
    match raw {
        "auto" => FeatureKnob::Auto,
        "on" | "1" => FeatureKnob::On,
        "off" | "0" => FeatureKnob::Off,
        other => panic!(
            "invalid {var}={other}: expected auto|on|off (legacy 1|0 accepted); \
             refusing to start with an unreadable io_uring feature knob"
        ),
    }
}

/// An io_uring feature: one of the five knobs, or the provided buffer ring
/// that [`UringFeature::MultishotRecv`] carries as its inseparable pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UringFeature {
    /// `IORING_SETUP_COOP_TASKRUN` (Linux 5.19+; default on with a
    /// plain-builder fallback).
    CoopTaskrun,
    /// `IORING_SETUP_DEFER_TASKRUN` (Linux 6.1+; requires SINGLE_ISSUER).
    DeferTaskrun,
    /// `IORING_SETUP_SINGLE_ISSUER` (Linux 6.0+).
    SingleIssuer,
    /// `IORING_SETUP_SQPOLL` (Linux 5.1+; one kernel thread per ring).
    Sqpoll,
    /// `IORING_RECV_MULTISHOT` long-lived receive arms (Linux 6.0+),
    /// inseparable from the provided buffer ring (Linux 5.19+).
    MultishotRecv,
    /// The provided buffer ring (`IORING_REGISTER_PBUF_RING`, group 1,
    /// Linux 5.19+) — not a knob of its own: requested through the legacy
    /// `MINISS_URING_BUF_RING` spelling or promoted by `MultishotRecv`.
    BufRing,
}

impl UringFeature {
    /// The five user-facing knobs, in resolution order.
    pub const KNOBS: [UringFeature; 5] = [
        UringFeature::CoopTaskrun,
        UringFeature::DeferTaskrun,
        UringFeature::SingleIssuer,
        UringFeature::Sqpoll,
        UringFeature::MultishotRecv,
    ];

    /// Short kernel-style name used in messages and resolutions.
    pub fn name(self) -> &'static str {
        match self {
            UringFeature::CoopTaskrun => "COOP_TASKRUN",
            UringFeature::DeferTaskrun => "DEFER_TASKRUN",
            UringFeature::SingleIssuer => "SINGLE_ISSUER",
            UringFeature::Sqpoll => "SQPOLL",
            UringFeature::MultishotRecv => "MULTISHOT_RECV",
            UringFeature::BufRing => "BUF_RING",
        }
    }

    /// The environment knob that requests this feature.
    pub fn env_name(self) -> &'static str {
        match self {
            UringFeature::BufRing => "MINISS_URING_BUF_RING",
            other => {
                // All five knobs follow one spelling: MINISS_URING_<NAME>.
                concat_names(other.name())
            }
        }
    }

    /// Minimum kernel release for the feature (io_uring_setup(2) and the
    /// io_uring source; multishot accept/recv versions were already encoded
    /// by the opcode gates in `src/io/uring.rs`).
    pub fn min_kernel(self) -> (u32, u32) {
        match self {
            UringFeature::CoopTaskrun => (5, 19),
            UringFeature::SingleIssuer => (6, 0),
            UringFeature::DeferTaskrun => (6, 1),
            UringFeature::Sqpoll => (5, 1),
            UringFeature::MultishotRecv => (6, 0),
            UringFeature::BufRing => (5, 19),
        }
    }

    /// The `auto` default: only COOP_TASKRUN defaults on (2026-07-12
    /// decision row), everything else defaults off. Defaults MUST NOT
    /// change here — the knobs add freedom, not new defaults.
    pub fn default_on(self) -> bool {
        matches!(self, UringFeature::CoopTaskrun)
    }

    /// Slot into the resolver's five-wide effective-flag array.
    fn knob_slot(self) -> Option<usize> {
        UringFeature::KNOBS.iter().position(|f| *f == self)
    }
}

fn concat_names(short: &'static str) -> &'static str {
    // `MINISS_URING_` + short name; const-friendly because every short name
    // is itself a literal.
    match short {
        "COOP_TASKRUN" => "MINISS_URING_COOP_TASKRUN",
        "DEFER_TASKRUN" => "MINISS_URING_DEFER_TASKRUN",
        "SINGLE_ISSUER" => "MINISS_URING_SINGLE_ISSUER",
        "SQPOLL" => "MINISS_URING_SQPOLL",
        "MULTISHOT_RECV" => "MINISS_URING_MULTISHOT_RECV",
        _ => "MINISS_URING_BUF_RING",
    }
}

impl fmt::Display for UringFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One kernel constraint between two features, encoded exactly as the
/// kernel enforces it.
///
/// Verdict priority is declaration order: the first violated rule decides
/// the conflict error. Both rules of a pair name the constraint verbatim and
/// its source so the setup-time error can quote them.
struct KernelConstraint {
    /// Left-hand feature. For [`ConstraintKind::Requires`], the feature that
    /// does the requiring (DEFER_TASKRUN requires SINGLE_ISSUER).
    a: UringFeature,
    /// Right-hand feature.
    b: UringFeature,
    kind: ConstraintKind,
    /// The constraint, stated for the failure message.
    constraint: &'static str,
    /// Where the constraint comes from (man page / kernel source).
    source: &'static str,
}

enum ConstraintKind {
    /// The kernel rejects the pair (or, for COOP⊕DEFER, the pair would
    /// silently change requested semantics — see the table comment).
    Exclusive,
    /// `a` cannot be set without `b`; an explicit `off` for `b` contradicts
    /// an explicit `on` for `a`, and `auto` promotes `b`.
    Requires,
}

/// The kernel-constraint table. ONE table, ONE resolver loop: adding a
/// feature or a constraint is a row here plus the knob plumbing, never a new
/// branch somewhere else.
///
/// Row notes (kernel sources verified 2026-10-31):
///
/// - COOP⊕DEFER: `io_uring/io_uring.c` at v6.1 and master has NO check
///   rejecting the raw pair; io_uring_setup(2) documents the two as
///   alternative taskrun modes and DEFER fully overrides COOP. The runtime
///   enforces the exclusivity (the pre-knob `resolve_taskrun_flags` already
///   did) so "flags as asked" never silently means "flags as resolved".
/// - SQPOLL⊕{COOP, DEFER}: hard `-EINVAL` from `io_uring_sanitise_params()`
///   ("IPI related flags don't make sense with SQPOLL"), v6.1 and master.
///   This pair was previously NOT encoded: the legacy construction path sent
///   COOP+SQPOLL, hit `-EINVAL` on 6.1+ kernels, and silently degraded to a
///   plain ring — `MINISS_URING_SQPOLL` with default COOP never actually
///   engaged SQPOLL. The resolver fixes that: auto COOP yields to SQPOLL,
///   explicit COOP is a conflict error.
/// - DEFER⇒SINGLE_ISSUER: hard `-EINVAL` (v6.1 and master); `auto` promotes
///   SINGLE_ISSUER, explicit `off` contradicts explicit `on`.
/// - SQPOLL×SINGLE_ISSUER: verified NON-constraint (nothing rejects or
///   clears it; the polling task is the sole submitter) — deliberately no
///   row; the redundancy is recorded instead of inventing a constraint.
const CONSTRAINT_TABLE: &[KernelConstraint] = &[
    KernelConstraint {
        a: UringFeature::DeferTaskrun,
        b: UringFeature::CoopTaskrun,
        kind: ConstraintKind::Exclusive,
        constraint: "COOP_TASKRUN and DEFER_TASKRUN are alternative taskrun modes and cannot \
                     be combined (DEFER_TASKRUN fully overrides COOP_TASKRUN)",
        source: "io_uring_setup(2); the kernel accepts the raw pair (io_uring/io_uring.c v6.1 \
                 and master), so the runtime enforces the exclusivity to keep requested \
                 semantics honest",
    },
    KernelConstraint {
        a: UringFeature::DeferTaskrun,
        b: UringFeature::SingleIssuer,
        kind: ConstraintKind::Requires,
        constraint: "IORING_SETUP_DEFER_TASKRUN requires IORING_SETUP_SINGLE_ISSUER; setup \
                     fails with -EINVAL without it",
        source: "io_uring_setup(2) and io_uring/io_uring.c (v6.1 and master)",
    },
    KernelConstraint {
        a: UringFeature::Sqpoll,
        b: UringFeature::DeferTaskrun,
        kind: ConstraintKind::Exclusive,
        constraint: "IORING_SETUP_SQPOLL cannot be combined with IORING_SETUP_DEFER_TASKRUN; \
                     setup fails with -EINVAL",
        source: "io_uring/io_uring.c io_uring_sanitise_params(): \"IPI related flags don't \
                 make sense with SQPOLL\" (v6.1 and master)",
    },
    KernelConstraint {
        a: UringFeature::Sqpoll,
        b: UringFeature::CoopTaskrun,
        kind: ConstraintKind::Exclusive,
        constraint: "IORING_SETUP_SQPOLL cannot be combined with IORING_SETUP_COOP_TASKRUN; \
                     setup fails with -EINVAL",
        source: "io_uring/io_uring.c io_uring_sanitise_params(): \"IPI related flags don't \
                 make sense with SQPOLL\" (v6.1 and master)",
    },
];

/// One recorded auto-resolution (default, conflict drop, promotion, pair or
/// redundancy note). Surfaced at construction so behavior is never silently
/// different from flags-as-asked.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The feature the resolution is about.
    pub feature: UringFeature,
    /// The value the feature resolved to.
    pub to: bool,
    /// What kind of resolution produced `to`.
    pub action: ResolutionAction,
    /// Constraint text / reason, quoted in the surfaced message.
    pub detail: String,
}

/// The kind of an auto-resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionAction {
    /// `auto` expanded to the documented default.
    Defaulted,
    /// `auto` yielded to an explicit request across a constraint row.
    Dropped,
    /// `auto` was promoted by a hard prerequisite (DEFER ⇒ SINGLE_ISSUER).
    Promoted,
    /// The multishot pair promoted the provided buffer ring.
    Paired,
    /// Requested but redundant under another resolved flag (no rule
    /// encoded — the kernel enforces nothing here).
    Redundant,
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let on_off = if self.to { "on" } else { "off" };
        match self.action {
            ResolutionAction::Defaulted => {
                write!(f, "{}: auto -> {} (default)", self.feature, on_off)
            }
            ResolutionAction::Dropped => {
                write!(
                    f,
                    "{}: auto -> off (dropped: {})",
                    self.feature, self.detail
                )
            }
            ResolutionAction::Promoted => {
                write!(
                    f,
                    "{}: auto -> on (promoted: {})",
                    self.feature, self.detail
                )
            }
            ResolutionAction::Paired => {
                write!(
                    f,
                    "{}: {} (paired with MULTISHOT_RECV: {})",
                    self.feature, on_off, self.detail
                )
            }
            ResolutionAction::Redundant => {
                write!(
                    f,
                    "{}: {} (redundant: {})",
                    self.feature, on_off, self.detail
                )
            }
        }
    }
}

/// The flags one io_uring ring will actually be constructed with, plus every
/// recorded auto-resolution.
#[derive(Debug, Clone)]
pub struct ResolvedFlags {
    pub coop_taskrun: bool,
    pub defer_taskrun: bool,
    pub single_issuer: bool,
    pub sqpoll: bool,
    pub multishot_recv: bool,
    /// The provided buffer ring: promoted by [`UringFeature::MultishotRecv`]
    /// or requested through the legacy `MINISS_URING_BUF_RING` spelling.
    pub buf_ring: bool,
    /// Every auto resolution, in the order it was decided.
    pub resolutions: Vec<Resolution>,
}

impl fmt::Display for ResolvedFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "COOP_TASKRUN={} DEFER_TASKRUN={} SINGLE_ISSUER={} SQPOLL={} MULTISHOT_RECV={} BUF_RING={}",
            on_off(self.coop_taskrun),
            on_off(self.defer_taskrun),
            on_off(self.single_issuer),
            on_off(self.sqpoll),
            on_off(self.multishot_recv),
            on_off(self.buf_ring),
        )
    }
}

fn on_off(flag: bool) -> &'static str {
    if flag {
        "on"
    } else {
        "off"
    }
}

/// Why setup refused to construct the ring.
#[derive(Debug, Clone)]
pub struct FeatureError {
    kind: FeatureErrorKind,
}

#[derive(Debug, Clone)]
enum FeatureErrorKind {
    /// Two explicitly-on features across a constraint row.
    Conflict {
        pair: [UringFeature; 2],
        constraint: &'static str,
        source: &'static str,
        requested: Vec<(UringFeature, FeatureKnob)>,
    },
    /// An explicit `on` for a feature the running kernel predates.
    Unsupported {
        feature: UringFeature,
        requested: Vec<(UringFeature, FeatureKnob)>,
        min_kernel: (u32, u32),
        current_kernel: Option<(u32, u32)>,
    },
}

impl fmt::Display for FeatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            FeatureErrorKind::Conflict {
                pair,
                constraint,
                source,
                requested,
            } => {
                let [a, b] = *pair;
                let knob_of = |feature: UringFeature| requested_feature_knob(requested, feature);
                writeln!(
                    f,
                    "io_uring feature conflict: {}={} + {}={} — {} ({})",
                    a.env_name(),
                    knob_of(a),
                    b.env_name(),
                    knob_of(b),
                    constraint,
                    source,
                )?;
                write!(f, "requested flags: {}", render_requested(requested))?;
                writeln!(
                    f,
                    ". {} requires Linux {}.{}, {} requires Linux {}.{}.",
                    a,
                    a.min_kernel().0,
                    a.min_kernel().1,
                    b,
                    b.min_kernel().0,
                    b.min_kernel().1,
                )?;
                write!(
                    f,
                    "Set one of the two to auto (or off), e.g. {}=auto; auto resolutions \
                     are recorded and surfaced, never silent.",
                    b.env_name(),
                )
            }
            FeatureErrorKind::Unsupported {
                feature,
                requested,
                min_kernel,
                current_kernel,
            } => {
                let knob = requested_feature_knob(requested, *feature);
                write!(
                    f,
                    "io_uring feature unavailable: {}={} requests {} but ",
                    feature.env_name(),
                    knob,
                    feature,
                )?;
                match current_kernel {
                    Some((major, minor)) => {
                        write!(f, "the running kernel ({major}.{minor}) predates it — ")?
                    }
                    None => write!(f, "the running kernel release is unreadable and ")?,
                }
                write!(
                    f,
                    "{} requires Linux {}.{}+. Explicit on never falls back silently; \
                     use auto (documented fallback) or upgrade the kernel.",
                    feature, min_kernel.0, min_kernel.1,
                )
            }
        }
    }
}

impl std::error::Error for FeatureError {}

impl From<FeatureError> for std::io::Error {
    fn from(error: FeatureError) -> Self {
        std::io::Error::other(error.to_string())
    }
}

fn requested_feature_knob(
    requested: &[(UringFeature, FeatureKnob)],
    feature: UringFeature,
) -> FeatureKnob {
    requested
        .iter()
        .find(|(f, _)| *f == feature)
        .map(|(_, knob)| *knob)
        .unwrap_or(FeatureKnob::Auto)
}

fn render_requested(requested: &[(UringFeature, FeatureKnob)]) -> String {
    requested
        .iter()
        .map(|(feature, knob)| format!("{}={}", feature.env_name(), knob))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A construction-time feature request: the five knobs plus the legacy
/// buffer-ring switch and the (already probed) running kernel release.
///
/// Fields are public so embedders and tests can build requests
/// programmatically instead of through the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureRequest {
    pub coop_taskrun: FeatureKnob,
    pub defer_taskrun: FeatureKnob,
    pub single_issuer: FeatureKnob,
    pub sqpoll: FeatureKnob,
    pub multishot_recv: FeatureKnob,
    /// The legacy `MINISS_URING_BUF_RING` binary switch: registers the
    /// provided buffer ring without enabling multishot receive (exact
    /// pre-knob behavior).
    pub legacy_buf_ring: bool,
    /// Running kernel release as `(major, minor)`; `None` when unreadable.
    /// Probed once at construction, never in a hot path.
    pub kernel_release: Option<(u32, u32)>,
}

impl FeatureRequest {
    /// Parse the feature knobs from the environment.
    ///
    /// New spellings: `MINISS_URING_<FEATURE>=auto|on|off`. Legacy
    /// spellings keep working:
    ///
    /// - the binary values `1`/`0` are accepted by every knob
    ///   (`1` = on, `0` = off) — `MINISS_URING_DEFER_TASKRUN=1`,
    ///   `MINISS_URING_SQPOLL=1`, `MINISS_URING_SINGLE_ISSUER=1`,
    ///   `MINISS_URING_MULTISHOT_RECV=1` behave exactly as before. One
    ///   documented change: the legacy checks were presence-only, so
    ///   `MINISS_URING_DEFER_TASKRUN=0` used to mean "on"; `0` now means
    ///   off (the documented `=1` spelling is unchanged).
    /// - `MINISS_URING_NO_COOP_TASKRUN` (presence-checked, the pre-knob
    ///   COOP opt-out) maps to `COOP_TASKRUN=off` whenever the new knob has
    ///   no explicit opinion (`auto` or unset); an explicit `on`/`off` on
    ///   the new knob always overrides the legacy spelling. `auto` behaves
    ///   exactly like an unset variable.
    /// - `MINISS_URING_BUF_RING` stays the legacy binary switch for the
    ///   provided buffer ring alone (`1`/`on` register it, `0`/`off`/`auto`
    ///   do not); the `MULTISHOT_RECV` knob covers the pair.
    ///
    /// A set-but-invalid value panics, refusing startup with the same
    /// clarity as `MINISS_IO_BACKEND`'s unknown-backend panic. An empty
    /// value is treated as unset.
    pub fn from_env() -> Self {
        let knob = |feature: UringFeature| -> FeatureKnob {
            let var = feature.env_name();
            match std::env::var(var) {
                Ok(raw) if !raw.is_empty() => parse_knob_value(var, &raw),
                _ => FeatureKnob::Auto,
            }
        };
        let coop_taskrun = knob(UringFeature::CoopTaskrun);
        let coop_taskrun = if coop_taskrun == FeatureKnob::Auto
            && std::env::var_os("MINISS_URING_NO_COOP_TASKRUN").is_some()
        {
            // Legacy COOP opt-out: presence (any value) meant off.
            FeatureKnob::Off
        } else {
            coop_taskrun
        };
        Self {
            coop_taskrun,
            defer_taskrun: knob(UringFeature::DeferTaskrun),
            single_issuer: knob(UringFeature::SingleIssuer),
            sqpoll: knob(UringFeature::Sqpoll),
            multishot_recv: knob(UringFeature::MultishotRecv),
            legacy_buf_ring: legacy_buf_ring_from_env(),
            kernel_release: probe_kernel_release(),
        }
    }

    /// All five knobs at `auto` (the runtime defaults) with the given
    /// legacy buffer-ring switch. Used by the explicit constructor path
    /// and handy in tests.
    pub fn all_auto(legacy_buf_ring: bool) -> Self {
        Self {
            coop_taskrun: FeatureKnob::Auto,
            defer_taskrun: FeatureKnob::Auto,
            single_issuer: FeatureKnob::Auto,
            sqpoll: FeatureKnob::Auto,
            multishot_recv: FeatureKnob::Auto,
            legacy_buf_ring,
            kernel_release: probe_kernel_release(),
        }
    }
}

/// The legacy `MINISS_URING_BUF_RING` binary switch. `1`/`on` register the
/// ring, `0`/`off`/`auto` do not, anything else refuses startup (the
/// pre-knob behavior silently treated unknown values as off; the knobs now
/// refuse them loudly).
pub fn legacy_buf_ring_from_env() -> bool {
    match std::env::var("MINISS_URING_BUF_RING") {
        Ok(raw) if !raw.is_empty() => match raw.as_str() {
            "1" | "on" => true,
            "0" | "off" | "auto" => false,
            other => panic!(
                "invalid MINISS_URING_BUF_RING={other}: expected 1|on or 0|off|auto; \
                 refusing to start with an unreadable io_uring feature knob"
            ),
        },
        _ => false,
    }
}

/// Resolve the requested feature knobs into the flags one ring will be
/// constructed with, or a structured setup error.
///
/// Pipeline (all steps recorded when they change anything):
///
/// 1. Version gates: explicit `on` for a feature the running kernel
///    predates fails loudly. An unreadable release probe skips the gate —
///    the kernel's own setup call becomes the arbiter and its error
///    propagates (also loud). `auto` never gates.
/// 2. `auto` expansion to the documented defaults (COOP_TASKRUN on,
///    everything else off).
/// 3. One pass over [`CONSTRAINT_TABLE`] in declaration order: explicit
///    `on` + explicit `on` across a rejected pair is a conflict error;
///    an `auto` side yields to an explicit side and the drop is recorded;
///    `auto` is promoted across a hard prerequisite.
/// 4. Non-constraint notes: SINGLE_ISSUER under SQPOLL is recorded as
///    redundant (the kernel enforces nothing there).
/// 5. The multishot pair: `MULTISHOT_RECV` resolved on promotes the
///    provided buffer ring. Implementation-internal, never a user-visible
///    conflict.
pub fn resolve_uring_flags(request: &FeatureRequest) -> Result<ResolvedFlags, FeatureError> {
    let knobs = knob_list(request);

    // Step 1: loud version gates for explicit `on`.
    if let Some(current) = request.kernel_release {
        for (feature, knob) in &knobs {
            if *knob == FeatureKnob::On && current < feature.min_kernel() {
                return Err(FeatureError {
                    kind: FeatureErrorKind::Unsupported {
                        feature: *feature,
                        requested: knobs.clone(),
                        min_kernel: feature.min_kernel(),
                        current_kernel: Some(current),
                    },
                });
            }
        }
    }

    // Step 2: auto expansion with recorded defaults.
    let mut effective = [false; 5];
    let mut resolutions: Vec<Resolution> = Vec::new();
    for (slot, feature) in UringFeature::KNOBS.iter().enumerate() {
        let knob = requested_feature_knob(&knobs, *feature);
        effective[slot] = match knob {
            FeatureKnob::On => true,
            FeatureKnob::Off => false,
            FeatureKnob::Auto => {
                let default = feature.default_on();
                resolutions.push(Resolution {
                    feature: *feature,
                    to: default,
                    action: ResolutionAction::Defaulted,
                    detail: String::new(),
                });
                default
            }
        };
    }

    // Step 3: one pass over the constraint table, declaration order.
    for rule in CONSTRAINT_TABLE {
        let (Some(a_slot), Some(b_slot)) = (rule.a.knob_slot(), rule.b.knob_slot()) else {
            continue;
        };
        match rule.kind {
            ConstraintKind::Exclusive => {
                if !effective[a_slot] || !effective[b_slot] {
                    continue;
                }
                let a_knob = requested_feature_knob(&knobs, rule.a);
                let b_knob = requested_feature_knob(&knobs, rule.b);
                if a_knob == FeatureKnob::On && b_knob == FeatureKnob::On {
                    return Err(FeatureError {
                        kind: FeatureErrorKind::Conflict {
                            pair: [rule.a, rule.b],
                            constraint: rule.constraint,
                            source: rule.source,
                            requested: knobs.clone(),
                        },
                    });
                }
                // Auto yields to the explicit side; the drop is recorded and
                // replaces the default record for that feature.
                for (slot, feature, knob) in [(a_slot, rule.a, a_knob), (b_slot, rule.b, b_knob)] {
                    if knob == FeatureKnob::Auto {
                        effective[slot] = false;
                        drop_default_record(&mut resolutions, feature);
                        resolutions.push(Resolution {
                            feature,
                            to: false,
                            action: ResolutionAction::Dropped,
                            detail: rule.constraint.to_string(),
                        });
                    }
                }
            }
            ConstraintKind::Requires => {
                // `rule.a` requires `rule.b`.
                if !effective[a_slot] || effective[b_slot] {
                    continue;
                }
                let b_knob = requested_feature_knob(&knobs, rule.b);
                if b_knob == FeatureKnob::Auto {
                    effective[b_slot] = true;
                    drop_default_record(&mut resolutions, rule.b);
                    resolutions.push(Resolution {
                        feature: rule.b,
                        to: true,
                        action: ResolutionAction::Promoted,
                        detail: rule.constraint.to_string(),
                    });
                } else {
                    // Explicit refusal of a hard prerequisite.
                    return Err(FeatureError {
                        kind: FeatureErrorKind::Conflict {
                            pair: [rule.a, rule.b],
                            constraint: rule.constraint,
                            source: rule.source,
                            requested: knobs.clone(),
                        },
                    });
                }
            }
        }
    }

    // Step 4: verified non-constraint, recorded as redundancy.
    if effective[UringFeature::Sqpoll.knob_slot().expect("SQPOLL is a knob")]
        && requested_feature_knob(&knobs, UringFeature::SingleIssuer) == FeatureKnob::On
    {
        resolutions.push(Resolution {
            feature: UringFeature::SingleIssuer,
            to: true,
            action: ResolutionAction::Redundant,
            detail: "the SQPOLL thread is inherently the sole submitter \
                     (io_uring_setup(2)); the kernel neither rejects nor clears \
                     SINGLE_ISSUER on SQPOLL rings"
                .to_string(),
        });
    }

    // Step 5: the multishot pair (implementation-internal, never a
    // user-visible conflict).
    let mut buf_ring = request.legacy_buf_ring;
    if effective[UringFeature::MultishotRecv
        .knob_slot()
        .expect("MULTISHOT_RECV is a knob")]
        && !buf_ring
    {
        buf_ring = true;
        resolutions.push(Resolution {
            feature: UringFeature::BufRing,
            to: true,
            action: ResolutionAction::Paired,
            detail: "MULTISHOT_RECV is inseparable from the provided buffer ring \
                     (kernel: IORING_RECV_MULTISHOT requires REQ_F_BUFFER_SELECT)"
                .to_string(),
        });
    }

    Ok(ResolvedFlags {
        coop_taskrun: effective[0],
        defer_taskrun: effective[1],
        single_issuer: effective[2],
        sqpoll: effective[3],
        multishot_recv: effective[4],
        buf_ring,
        resolutions,
    })
}

fn knob_list(request: &FeatureRequest) -> Vec<(UringFeature, FeatureKnob)> {
    vec![
        (UringFeature::CoopTaskrun, request.coop_taskrun),
        (UringFeature::DeferTaskrun, request.defer_taskrun),
        (UringFeature::SingleIssuer, request.single_issuer),
        (UringFeature::Sqpoll, request.sqpoll),
        (UringFeature::MultishotRecv, request.multishot_recv),
    ]
}

fn drop_default_record(resolutions: &mut Vec<Resolution>, feature: UringFeature) {
    resolutions.retain(|resolution| {
        !(resolution.action == ResolutionAction::Defaulted && resolution.feature == feature)
    });
}

/// Read the running kernel release (`/proc/sys/kernel/osrelease`, falling
/// back to `/proc/version`) as `(major, minor)`. Construction-time only,
/// never a hot path. Non-Linux targets return `None`.
pub(crate) fn probe_kernel_release() -> Option<(u32, u32)> {
    #[cfg(target_os = "linux")]
    {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .or_else(|_| std::fs::read_to_string("/proc/version"))
            .ok()?;
        parse_release_string(&release)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Parse a kernel release string such as `6.8.0-14-generic` or
/// `Linux version 7.2.12 ...` into `(major, minor)`. Same digit-run parsing
/// the opcode gates in `src/io/uring.rs` have always used.
pub fn parse_release_string(release: &str) -> Option<(u32, u32)> {
    let mut numbers = release
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok());
    let major = numbers.next()?;
    let minor = numbers.next().unwrap_or(0);
    Some((major, minor))
}

/// Note (once per construction, on non-io_uring backends) that the
/// `MINISS_URING_*` feature knobs are present but inert: the epoll/kqueue
/// backend has no io_uring setup flags, so the knobs are neither applied
/// nor validated there.
pub fn note_uring_knobs_ignored_on(backend: &'static str) {
    const KNOB_VARS: &[&str] = &[
        "MINISS_URING_COOP_TASKRUN",
        "MINISS_URING_DEFER_TASKRUN",
        "MINISS_URING_SINGLE_ISSUER",
        "MINISS_URING_SQPOLL",
        "MINISS_URING_MULTISHOT_RECV",
        "MINISS_URING_BUF_RING",
        "MINISS_URING_NO_COOP_TASKRUN",
    ];
    if KNOB_VARS.iter().any(|var| std::env::var_os(var).is_some()) {
        tracing::debug!(
            "MINISS_URING_* feature knobs are set but ignored: the {backend} backend \
             has no io_uring setup flags"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an all-explicit request from five booleans (bit order:
    /// coop=1, defer=2, single=4, sqpoll=8, multishot=16).
    fn explicit_request(bits: u32) -> FeatureRequest {
        let knob = |on: bool| {
            if on {
                FeatureKnob::On
            } else {
                FeatureKnob::Off
            }
        };
        FeatureRequest {
            coop_taskrun: knob(bits & 1 != 0),
            defer_taskrun: knob(bits & 2 != 0),
            single_issuer: knob(bits & 4 != 0),
            sqpoll: knob(bits & 8 != 0),
            multishot_recv: knob(bits & 16 != 0),
            legacy_buf_ring: false,
            kernel_release: None,
        }
    }

    fn tri_state_knob(code: u32) -> FeatureKnob {
        match code {
            0 => FeatureKnob::Auto,
            1 => FeatureKnob::On,
            _ => FeatureKnob::Off,
        }
    }

    /// All five knobs at `auto`, no legacy buffer ring, no release probe —
    /// the base for targeted tests that override individual knobs.
    fn all_auto() -> FeatureRequest {
        FeatureRequest {
            coop_taskrun: FeatureKnob::Auto,
            defer_taskrun: FeatureKnob::Auto,
            single_issuer: FeatureKnob::Auto,
            sqpoll: FeatureKnob::Auto,
            multishot_recv: FeatureKnob::Auto,
            legacy_buf_ring: false,
            kernel_release: None,
        }
    }

    /// The independent boolean-constraint oracle, written from the
    /// documented kernel rules rather than from the implementation:
    /// table order (DEFER⊕COOP, DEFER⇒SINGLE_ISSUER, SQPOLL⊕DEFER,
    /// SQPOLL⊕COOP), fail-fast first match; DEFER on implies
    /// SINGLE_ISSUER on (it was either requested or the DEFER⇒SINGLE_ISSUER
    /// check would have fired).
    fn boolean_oracle(bits: u32) -> Result<[bool; 6], [UringFeature; 2]> {
        let coop = bits & 1 != 0;
        let defer = bits & 2 != 0;
        let single = bits & 4 != 0;
        let sqpoll = bits & 8 != 0;
        let multishot = bits & 16 != 0;
        if coop && defer {
            Err([UringFeature::DeferTaskrun, UringFeature::CoopTaskrun])
        } else if defer && !single {
            Err([UringFeature::DeferTaskrun, UringFeature::SingleIssuer])
        } else if sqpoll && defer {
            Err([UringFeature::Sqpoll, UringFeature::DeferTaskrun])
        } else if sqpoll && coop {
            Err([UringFeature::Sqpoll, UringFeature::CoopTaskrun])
        } else {
            Ok([coop, defer, single || defer, sqpoll, multishot, multishot])
        }
    }

    fn conflict_pair(error: &FeatureError) -> Option<[UringFeature; 2]> {
        match &error.kind {
            FeatureErrorKind::Conflict { pair, .. } => Some(*pair),
            FeatureErrorKind::Unsupported { .. } => None,
        }
    }

    #[test]
    fn boolean_constraint_table_is_exhaustive_2n() {
        // Every combination of the five effective (explicit) flags against
        // the pure kernel-constraint table: 2^5 = 32 cells, each asserted
        // against the independent oracle.
        for bits in 0u32..32 {
            let request = explicit_request(bits);
            let resolved = resolve_uring_flags(&request);
            match (resolved, boolean_oracle(bits)) {
                (Ok(flags), Ok(expected)) => {
                    assert_eq!(
                        [
                            flags.coop_taskrun,
                            flags.defer_taskrun,
                            flags.single_issuer,
                            flags.sqpoll,
                            flags.multishot_recv,
                            flags.buf_ring,
                        ],
                        expected,
                        "bits={bits} ({:b})",
                        bits
                    );
                    // All-explicit requests never produce default/drop/
                    // promotion records; only the redundancy note and the
                    // pair promotion can appear.
                    for resolution in &flags.resolutions {
                        match resolution.action {
                            ResolutionAction::Redundant => assert!(
                                bits & 8 != 0 && bits & 4 != 0,
                                "redundancy note requires SQPOLL+SINGLE_ISSUER, bits={bits}"
                            ),
                            ResolutionAction::Paired => {
                                assert!(bits & 16 != 0, "pair requires multishot, bits={bits}")
                            }
                            other => panic!("unexpected all-explicit resolution {other:?} for bits={bits}"),
                        }
                    }
                }
                (Err(error), Err(expected_pair)) => {
                    assert_eq!(
                        conflict_pair(&error),
                        Some(expected_pair),
                        "bits={bits} ({:b})",
                        bits
                    );
                    let message = error.to_string();
                    assert!(message.contains("conflict"), "bits={bits}: {message}");
                }
                (resolved, expected) => panic!(
                    "verdict mismatch for bits={bits} ({:b}): resolved={resolved:?}, oracle={expected:?}",
                    bits
                ),
            }
        }
    }

    #[test]
    fn tri_state_matrix_is_systematic_3n() {
        // The full 3^5 = 243 tri-state matrix. The oracle mirrors the
        // documented semantics: auto expands to the defaults (COOP on,
        // others off); a kernel-rejected pair with both sides explicit is
        // an error; the auto side of a violated pair yields (recorded);
        // DEFER promotes SINGLE_ISSUER unless SINGLE_ISSUER is explicitly
        // off; explicit multishot promotes the buffer ring. The release
        // probe is None here, so version gates are out of scope for this
        // sweep (covered by the targeted gate tests below).
        for code in 0u32..243 {
            let request = FeatureRequest {
                coop_taskrun: tri_state_knob(code % 3),
                defer_taskrun: tri_state_knob((code / 3) % 3),
                single_issuer: tri_state_knob((code / 9) % 3),
                sqpoll: tri_state_knob((code / 27) % 3),
                multishot_recv: tri_state_knob((code / 81) % 3),
                legacy_buf_ring: false,
                kernel_release: None,
            };
            let defaults = [true, false, false, false, false];
            let knobs = [
                request.coop_taskrun,
                request.defer_taskrun,
                request.single_issuer,
                request.sqpoll,
                request.multishot_recv,
            ];
            let mut eff = [false; 5];
            for slot in 0..5 {
                eff[slot] = match knobs[slot] {
                    FeatureKnob::On => true,
                    FeatureKnob::Off => false,
                    FeatureKnob::Auto => defaults[slot],
                };
            }
            let mut verdict: Option<[UringFeature; 2]> = None;
            // DEFER ⊕ COOP (table order), DEFER ⇒ SINGLE_ISSUER,
            // SQPOLL ⊕ DEFER, SQPOLL ⊕ COOP.
            if eff[0] && eff[1] {
                if knobs[0] == FeatureKnob::On && knobs[1] == FeatureKnob::On {
                    verdict = Some([UringFeature::DeferTaskrun, UringFeature::CoopTaskrun]);
                }
                if knobs[0] == FeatureKnob::Auto {
                    eff[0] = false;
                }
                if knobs[1] == FeatureKnob::Auto {
                    eff[1] = false;
                }
            }
            if verdict.is_none() && eff[1] && !eff[2] {
                if knobs[2] == FeatureKnob::Auto {
                    eff[2] = true;
                } else {
                    verdict = Some([UringFeature::DeferTaskrun, UringFeature::SingleIssuer]);
                }
            }
            if verdict.is_none() && eff[3] && eff[1] {
                if knobs[3] == FeatureKnob::On && knobs[1] == FeatureKnob::On {
                    verdict = Some([UringFeature::Sqpoll, UringFeature::DeferTaskrun]);
                }
                if knobs[3] == FeatureKnob::Auto {
                    eff[3] = false;
                }
                if knobs[1] == FeatureKnob::Auto {
                    eff[1] = false;
                }
            }
            if verdict.is_none() && eff[3] && eff[0] {
                if knobs[3] == FeatureKnob::On && knobs[0] == FeatureKnob::On {
                    verdict = Some([UringFeature::Sqpoll, UringFeature::CoopTaskrun]);
                }
                if knobs[3] == FeatureKnob::Auto {
                    eff[3] = false;
                }
                if knobs[0] == FeatureKnob::Auto {
                    eff[0] = false;
                }
            }
            let buf_ring = eff[4];
            let resolved = resolve_uring_flags(&request);
            match (resolved, verdict) {
                (Ok(flags), None) => {
                    assert_eq!(
                        [
                            flags.coop_taskrun,
                            flags.defer_taskrun,
                            flags.single_issuer,
                            flags.sqpoll,
                            flags.multishot_recv,
                            flags.buf_ring,
                        ],
                        [eff[0], eff[1], eff[2], eff[3], eff[4], buf_ring],
                        "code={code} ({code:b}, knobs {:?})",
                        knobs
                    );
                }
                (Err(error), Some(pair)) => {
                    assert_eq!(
                        conflict_pair(&error),
                        Some(pair),
                        "code={code} ({code:b}, knobs {:?})",
                        knobs
                    );
                }
                (resolved, verdict) => panic!(
                    "verdict mismatch for code={code} ({code:b}, knobs {:?}): \
                     resolved={resolved:?}, oracle verdict={verdict:?}",
                    knobs
                ),
            }
        }
    }

    #[test]
    fn auto_yields_to_explicit_across_conflicts_and_records_it() {
        // The owner-mandated shapes: COOP=auto + DEFER=on drops COOP with
        // the resolution recorded; COOP=on + DEFER=on is a conflict error.
        let dropped = resolve_uring_flags(&FeatureRequest {
            coop_taskrun: FeatureKnob::Auto,
            defer_taskrun: FeatureKnob::On,
            ..all_auto()
        })
        .expect("auto COOP yields to explicit DEFER");
        assert!(!dropped.coop_taskrun);
        assert!(dropped.defer_taskrun);
        assert!(
            dropped.single_issuer,
            "DEFER promotes SINGLE_ISSUER from auto"
        );
        let drop_record = dropped
            .resolutions
            .iter()
            .find(|resolution| resolution.feature == UringFeature::CoopTaskrun)
            .expect("COOP drop is recorded");
        assert_eq!(drop_record.action, ResolutionAction::Dropped);
        assert!(drop_record.detail.contains("alternative taskrun modes"));
        assert!(
            !dropped
                .resolutions
                .iter()
                .any(
                    |resolution| resolution.action == ResolutionAction::Defaulted
                        && resolution.feature == UringFeature::CoopTaskrun
                ),
            "the drop replaces the default record"
        );
        let promotion = dropped
            .resolutions
            .iter()
            .find(|resolution| resolution.feature == UringFeature::SingleIssuer)
            .expect("SINGLE_ISSUER promotion is recorded");
        assert_eq!(promotion.action, ResolutionAction::Promoted);

        let conflict = resolve_uring_flags(&FeatureRequest {
            coop_taskrun: FeatureKnob::On,
            defer_taskrun: FeatureKnob::On,
            ..all_auto()
        })
        .expect_err("explicit COOP + explicit DEFER is a conflict, not a resolution");
        assert_eq!(
            conflict_pair(&conflict),
            Some([UringFeature::DeferTaskrun, UringFeature::CoopTaskrun])
        );
    }

    #[test]
    fn sqpoll_with_default_coop_resolves_instead_of_silently_degrading() {
        // The pre-knob path sent COOP+SQPOLL, hit the kernel's -EINVAL and
        // silently degraded to a plain ring. The resolver drops auto COOP
        // before construction so SQPOLL actually engages.
        let resolved = resolve_uring_flags(&FeatureRequest {
            coop_taskrun: FeatureKnob::Auto,
            sqpoll: FeatureKnob::On,
            ..all_auto()
        })
        .expect("auto COOP yields to explicit SQPOLL");
        assert!(!resolved.coop_taskrun);
        assert!(resolved.sqpoll);
        assert!(resolved
            .resolutions
            .iter()
            .any(|resolution| resolution.feature == UringFeature::CoopTaskrun
                && resolution.action == ResolutionAction::Dropped
                && resolution
                    .detail
                    .contains("cannot be combined with IORING_SETUP_COOP_TASKRUN")));
    }

    #[test]
    fn version_gates_are_loud_for_explicit_on_only() {
        let request_on = |feature_wanted: UringFeature, release: Option<(u32, u32)>| {
            let knob = |feature: UringFeature| {
                if feature == feature_wanted {
                    FeatureKnob::On
                } else {
                    FeatureKnob::Auto
                }
            };
            FeatureRequest {
                coop_taskrun: knob(UringFeature::CoopTaskrun),
                defer_taskrun: knob(UringFeature::DeferTaskrun),
                single_issuer: knob(UringFeature::SingleIssuer),
                sqpoll: knob(UringFeature::Sqpoll),
                multishot_recv: knob(UringFeature::MultishotRecv),
                legacy_buf_ring: false,
                kernel_release: release,
            }
        };
        // COOP=on at exactly 5.19 passes (boundary).
        assert!(resolve_uring_flags(&request_on(UringFeature::CoopTaskrun, Some((5, 19)))).is_ok());
        // COOP=on one minor earlier fails loudly.
        let error = resolve_uring_flags(&request_on(UringFeature::CoopTaskrun, Some((5, 18))))
            .expect_err("COOP=on below 5.19 is loud");
        assert!(matches!(
            error.kind,
            FeatureErrorKind::Unsupported {
                feature: UringFeature::CoopTaskrun,
                ..
            }
        ));
        // DEFER=on (6.1+), SINGLE_ISSUER=on (6.0+) and MULTISHOT=on (6.0+)
        // on a 5.19 kernel fail with Unsupported, not Conflict.
        for (feature, min_kernel) in [
            (UringFeature::DeferTaskrun, (6, 1)),
            (UringFeature::SingleIssuer, (6, 0)),
            (UringFeature::MultishotRecv, (6, 0)),
        ] {
            let error = resolve_uring_flags(&request_on(feature, Some((5, 19))))
                .expect_err("explicit on below the minimum kernel is loud");
            match &error.kind {
                FeatureErrorKind::Unsupported {
                    feature: flagged,
                    min_kernel: minimum,
                    current_kernel: Some(current),
                    ..
                } => {
                    assert_eq!(
                        (*flagged, *minimum, *current),
                        (feature, min_kernel, (5, 19))
                    );
                }
                other => panic!("expected Unsupported for {feature}, got {other:?}"),
            }
        }
        // SQPOLL=on on 5.19 passes its 5.1 gate; on 5.0 it is loud.
        assert!(resolve_uring_flags(&request_on(UringFeature::Sqpoll, Some((5, 19)))).is_ok());
        let error = resolve_uring_flags(&request_on(UringFeature::Sqpoll, Some((5, 0))))
            .expect_err("SQPOLL=on below 5.1 is loud");
        assert!(matches!(
            error.kind,
            FeatureErrorKind::Unsupported {
                feature: UringFeature::Sqpoll,
                ..
            }
        ));
        // auto on an old kernel never gates (documented fallback path).
        assert!(resolve_uring_flags(&FeatureRequest {
            legacy_buf_ring: false,
            kernel_release: Some((5, 15)),
            ..all_auto()
        })
        .is_ok());
        // Unreadable release: the gate is skipped, the kernel's own setup
        // error becomes the arbiter (also loud).
        assert!(resolve_uring_flags(&request_on(UringFeature::DeferTaskrun, None)).is_ok());
    }

    #[test]
    fn conflict_error_names_flags_constraint_and_versions() {
        let error = resolve_uring_flags(&FeatureRequest {
            sqpoll: FeatureKnob::On,
            defer_taskrun: FeatureKnob::On,
            ..all_auto()
        })
        .expect_err("explicit SQPOLL + explicit DEFER conflict");
        let message = error.to_string();
        // (a) the requested flags, (b) the kernel constraint verbatim,
        // (c) the minimum kernel versions.
        assert!(message.contains("MINISS_URING_SQPOLL=on"), "{message}");
        assert!(
            message.contains("MINISS_URING_DEFER_TASKRUN=on"),
            "{message}"
        );
        assert!(
            message.contains("IPI related flags don't make sense with SQPOLL"),
            "{message}"
        );
        assert!(message.contains("EINVAL"), "{message}");
        assert!(message.contains("SQPOLL requires Linux 5.1"), "{message}");
        assert!(
            message.contains("DEFER_TASKRUN requires Linux 6.1"),
            "{message}"
        );
        assert!(message.contains("=auto"), "{message}");
        let error = resolve_uring_flags(&FeatureRequest {
            defer_taskrun: FeatureKnob::On,
            single_issuer: FeatureKnob::Off,
            ..all_auto()
        })
        .expect_err("explicit DEFER + explicit SINGLE_ISSUER=off contradiction");
        let message = error.to_string();
        assert!(
            message.contains("requires IORING_SETUP_SINGLE_ISSUER"),
            "{message}"
        );
        assert!(
            message.contains("MINISS_URING_DEFER_TASKRUN=on"),
            "{message}"
        );
        assert!(
            message.contains("MINISS_URING_SINGLE_ISSUER=off"),
            "{message}"
        );
    }

    #[test]
    fn unsupported_error_distinguishes_version_too_old() {
        let error = resolve_uring_flags(&FeatureRequest {
            defer_taskrun: FeatureKnob::On,
            legacy_buf_ring: false,
            kernel_release: Some((5, 19)),
            ..all_auto()
        })
        .expect_err("DEFER=on on 5.19 is a version gate, not a flag conflict");
        let message = error.to_string();
        assert!(message.contains("unavailable"), "{message}");
        assert!(
            message.contains("MINISS_URING_DEFER_TASKRUN=on"),
            "{message}"
        );
        assert!(message.contains("running kernel (5.19)"), "{message}");
        assert!(message.contains("requires Linux 6.1+"), "{message}");
        assert!(message.contains("auto"), "{message}");
        // Gate precedence: the version gate wins even when the request also
        // carries a flag-level contradiction (DEFER=on + SINGLE_ISSUER=off).
        let error = resolve_uring_flags(&FeatureRequest {
            defer_taskrun: FeatureKnob::On,
            single_issuer: FeatureKnob::Off,
            legacy_buf_ring: false,
            kernel_release: Some((5, 19)),
            ..all_auto()
        })
        .expect_err("the version gate fires before conflict resolution");
        assert!(matches!(error.kind, FeatureErrorKind::Unsupported { .. }));
    }

    #[test]
    fn explicit_multishot_promotes_the_buffer_ring_as_a_pair() {
        let resolved = resolve_uring_flags(&FeatureRequest {
            multishot_recv: FeatureKnob::On,
            ..all_auto()
        })
        .expect("multishot never conflicts");
        assert!(resolved.multishot_recv);
        assert!(resolved.buf_ring, "the inseparable pair is promoted");
        let paired = resolved
            .resolutions
            .iter()
            .find(|resolution| resolution.feature == UringFeature::BufRing)
            .expect("the pair promotion is recorded");
        assert_eq!(paired.action, ResolutionAction::Paired);
        assert!(paired.detail.contains("REQ_F_BUFFER_SELECT"));
        // Legacy buf_ring alone stays alone: no multishot, no pair record.
        let resolved = resolve_uring_flags(&FeatureRequest {
            legacy_buf_ring: true,
            ..all_auto()
        })
        .expect("legacy buf_ring is honored");
        assert!(resolved.buf_ring);
        assert!(!resolved.multishot_recv);
        assert!(
            resolved
                .resolutions
                .iter()
                .all(|resolution| resolution.action == ResolutionAction::Defaulted),
            "legacy buf_ring alone records only auto defaults, got {:?}",
            resolved.resolutions
        );
    }

    #[test]
    fn single_issuer_under_sqpoll_is_redundant_not_conflicting() {
        let resolved = resolve_uring_flags(&FeatureRequest {
            single_issuer: FeatureKnob::On,
            sqpoll: FeatureKnob::On,
            ..all_auto()
        })
        .expect("the kernel neither rejects nor clears SINGLE_ISSUER with SQPOLL");
        assert!(resolved.sqpoll);
        assert!(resolved.single_issuer, "no invented constraint");
        let note = resolved
            .resolutions
            .iter()
            .find(|resolution| resolution.feature == UringFeature::SingleIssuer)
            .expect("the redundancy is recorded");
        assert_eq!(note.action, ResolutionAction::Redundant);
        assert!(note.detail.contains("sole submitter"));
    }

    #[test]
    fn parse_knob_value_accepts_tri_state_and_legacy_binaries() {
        assert_eq!(parse_knob_value("V", "auto"), FeatureKnob::Auto);
        assert_eq!(parse_knob_value("V", "on"), FeatureKnob::On);
        assert_eq!(parse_knob_value("V", "off"), FeatureKnob::Off);
        assert_eq!(parse_knob_value("V", "1"), FeatureKnob::On);
        assert_eq!(parse_knob_value("V", "0"), FeatureKnob::Off);
    }

    #[test]
    #[should_panic(expected = "invalid V=banana")]
    fn parse_knob_value_refuses_unknown_values_loudly() {
        let _ = parse_knob_value("V", "banana");
    }

    #[test]
    fn parse_release_string_matches_the_opcode_gate_parsing() {
        assert_eq!(parse_release_string("6.8.0-14-generic"), Some((6, 8)));
        assert_eq!(parse_release_string("5.19.0"), Some((5, 19)));
        assert_eq!(
            parse_release_string("Linux version 7.2.12 (x)"),
            Some((7, 2))
        );
        assert_eq!(parse_release_string("6"), Some((6, 0)));
        assert_eq!(parse_release_string("no digits here"), None);
        assert_eq!(parse_release_string(""), None);
    }

    #[test]
    fn defaults_are_unchanged_by_the_knobs() {
        // No knobs set: COOP auto -> on (the 2026-07-12 default), everything
        // else auto -> off. The knobs add freedom, not new defaults.
        let resolved =
            resolve_uring_flags(&FeatureRequest::all_auto(false)).expect("defaults never conflict");
        assert!(resolved.coop_taskrun);
        assert!(!resolved.defer_taskrun);
        assert!(!resolved.single_issuer);
        assert!(!resolved.sqpoll);
        assert!(!resolved.multishot_recv);
        assert!(!resolved.buf_ring);
        let defaulted: Vec<_> = resolved
            .resolutions
            .iter()
            .filter(|resolution| resolution.action == ResolutionAction::Defaulted)
            .collect();
        assert_eq!(defaulted.len(), 5, "every auto default is recorded");
        assert!(defaulted
            .iter()
            .all(|resolution| resolution.to == resolution.feature.default_on()));
    }
}
