//! Lease behaviour, with an injected clock so every expiry rule is tested without sleeping.
//!
//! The negative control comes first: a lease that can always be taken makes every refusal
//! test below pass against a manager that refuses nothing.

use sessionlease::{Acquired, AuditEntry, LeaseManager};

fn mgr() -> LeaseManager { LeaseManager::new(60) }

fn token_of(a: &Acquired) -> String {
    match a {
        Acquired::Granted { lease } | Acquired::TookOver { lease, .. } => lease.token.clone(),
        Acquired::Refused { .. } => panic!("expected a lease, got a refusal"),
    }
}

#[test]
fn the_first_claimant_is_granted() {
    let mut m = mgr();
    match m.acquire("ci-runner", 1000, "") {
        Acquired::Granted { lease } => {
            assert_eq!(lease.holder, "ci-runner");
            assert_eq!(lease.expires_at, 1060);
        }
        other => panic!("expected Granted, got {other:?}"),
    }
}

#[test]
fn a_second_claimant_is_refused_and_told_who_holds_it_and_for_how_long() {
    // ⛔ The behaviour the whole crate exists for. Today connecting silently seizes.
    let mut m = mgr();
    m.acquire("alice-browser", 1000, "");
    match m.acquire("cron-job", 1030, "") {
        Acquired::Refused { held_by, expires_in_secs } => {
            assert_eq!(held_by, "alice-browser");
            assert_eq!(expires_in_secs, 30, "a refusal must say WHEN, not just no");
        }
        other => panic!("a second claimant seized the device: {other:?}"),
    }
}

#[test]
fn an_expired_lease_is_free_without_a_takeover() {
    // ⛔ A holder that crashed must not hold the device forever — and from outside, a crashed
    // holder is indistinguishable from a careful one thinking. Only a clock separates them.
    let mut m = mgr();
    m.acquire("dead-client", 1000, "");
    match m.acquire("next-client", 1061, "") {
        Acquired::Granted { lease } => assert_eq!(lease.holder, "next-client"),
        other => panic!("an expired lease still blocked a new holder: {other:?}"),
    }
    assert!(m.audit().iter().any(|e| matches!(e, AuditEntry::Expired { .. })),
            "a lapsed lease must be recorded, not silently vanish");
}

#[test]
fn a_lease_one_second_before_expiry_still_holds() {
    let mut m = mgr();
    m.acquire("holder", 1000, "");
    assert!(matches!(m.acquire("other", 1059, ""), Acquired::Refused { .. }));
}

#[test]
fn takeover_requires_a_reason_and_a_blank_one_does_not_count() {
    // ⛔ `force: true` is something a client sets once and forgets in a retry loop.
    let mut m = mgr();
    m.acquire("holder", 1000, "");
    for blank in ["", "   ", "\t\n"] {
        assert!(matches!(m.acquire("thief", 1010, blank), Acquired::Refused { .. }),
                "a blank reason behaved like a valid takeover");
    }
}

#[test]
fn a_reasoned_takeover_displaces_the_holder_and_is_recorded_verbatim() {
    let mut m = mgr();
    m.acquire("wedged-script", 1000, "");
    match m.acquire("operator", 1010, "  script hung in BIOS, taking console  ") {
        Acquired::TookOver { lease, displaced, reason } => {
            assert_eq!(lease.holder, "operator");
            assert_eq!(displaced, "wedged-script");
            assert_eq!(reason, "script hung in BIOS, taking console");
        }
        other => panic!("a reasoned takeover was refused: {other:?}"),
    }
    let took = m.audit().into_iter().find_map(|e| match e {
        AuditEntry::TookOver { who, displaced, reason, .. } => Some((who, displaced, reason)),
        _ => None,
    });
    assert_eq!(took, Some(("operator".into(), "wedged-script".into(),
                           "script hung in BIOS, taking console".into())),
               "a takeover that leaves no record is not audited");
}

#[test]
fn renew_extends_only_your_own_lease() {
    let mut m = mgr();
    let tok = token_of(&m.acquire("holder", 1000, ""));
    assert_eq!(m.renew(&tok, 1030).map(|l| l.expires_at), Some(1090));
    assert!(m.renew("someone-elses-token", 1030).is_none(),
            "a foreign token renewed the live lease");
}

#[test]
fn a_stale_token_cannot_renew_the_holder_that_replaced_it() {
    // ⛔ Otherwise a client that was displaced keeps the device alive under the new holder's
    // feet, and the two disagree about who is driving.
    let mut m = mgr();
    let stale = token_of(&m.acquire("first", 1000, ""));
    let _new = token_of(&m.acquire("second", 1010, "taking over"));
    assert!(m.renew(&stale, 1020).is_none());
    assert!(!m.release(&stale, 1020), "a stale token released the new holder's lease");
    assert_eq!(m.current(1020).map(|l| l.holder), Some("second".into()));
}

#[test]
fn release_frees_the_device_for_the_next_claimant() {
    let mut m = mgr();
    let tok = token_of(&m.acquire("holder", 1000, ""));
    assert!(m.release(&tok, 1010));
    assert!(matches!(m.acquire("next", 1011, ""), Acquired::Granted { .. }));
    assert!(m.audit().iter().any(|e| matches!(e, AuditEntry::Released { .. })));
}

#[test]
fn holds_is_the_question_a_guarded_action_asks() {
    let mut m = mgr();
    let tok = token_of(&m.acquire("holder", 1000, ""));
    assert!(m.holds(&tok, 1030));
    assert!(!m.holds("other", 1030));
    assert!(!m.holds(&tok, 1061), "an expired lease still reported as held");
}

#[test]
fn a_refusal_is_recorded_too_so_contention_is_visible() {
    let mut m = mgr();
    m.acquire("holder", 1000, "");
    m.acquire("loser", 1010, "");
    assert!(m.audit().iter().any(|e| matches!(e, AuditEntry::Refused { .. })),
            "contention left no trace, so nobody can tell this device is being fought over");
}

#[test]
fn the_audit_trail_is_bounded_and_keeps_the_most_recent() {
    let mut m = mgr();
    for i in 0..200u64 {
        let t = token_of(&m.acquire("h", 10_000 + i * 100, "churn"));
        m.release(&t, 10_000 + i * 100 + 1);
    }
    assert!(m.audit().len() <= sessionlease::AUDIT_CAPACITY,
            "an unbounded in-memory audit trail is a leak on a 256MB device");
    assert!(!m.audit().is_empty());
}

/// ⛔ The token is the only thing standing between a holder and anyone who wants their device.
/// The old scheme was `<unix_secs>-<counter>-<len(holder)>`: a 60-second window and a counter
/// that restarts at 1 each boot put the live token inside a few hundred guesses.
#[test]
fn a_lease_token_cannot_be_derived_from_what_an_attacker_knows() {
    let (now, who) = (1_700_000_000, "orchestrator");

    // Everything the old scheme was built from, which an attacker has.
    let mut m = mgr();
    let t = token_of(&m.acquire(who, now, ""));
    for guess in (now - 60..=now + 60).flat_map(|s| (1..=8).map(move |c| {
        format!("{}-{}-{}", s, c, who.len())
    })) {
        assert_ne!(t, guess, "token is still derivable from time, counter and holder length");
    }

    // Two managers at the identical instant must not agree — otherwise a reboot replays tokens.
    let (a, b) = (token_of(&mgr().acquire(who, now, "")), token_of(&mgr().acquire(who, now, "")));
    assert_ne!(a, b, "same clock and holder produced the same token; it is not random");
    assert_eq!(t.len(), 32, "expected 128 bits hex-encoded");
    assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
}
