//! Control leases: holding the device is explicit, expiring, and taking it from someone is a
//! decision rather than a side effect of connecting.
//!
//! # The problem
//!
//! Today `promote_session_if_registered` replaces the current session unconditionally and
//! hands the caller its predecessor to tear down. Connecting *is* seizing. The consequences
//! are the ones every single-session KVM has:
//!
//! * a scripted run loses the device mid-sequence because somebody opened the web UI, and the
//!   script cannot tell that from the device failing;
//! * a human loses their BIOS screen to a cron job, with no indication of what took it;
//! * nothing anywhere records who displaced whom.
//!
//! An orchestrator cannot safely drive hardware whose control can be yanked without warning,
//! and a person cannot safely use hardware a script may grab. The fix is not to forbid
//! takeover — sometimes the holder is a wedged script and taking it is exactly right — but to
//! make takeover **refused by default, explicit when intended, and always recorded**.
//!
//! # The rules
//!
//! 1. **First claimant wins.** A second is REFUSED, and told who holds it and for how long.
//! 2. ⛔ **An expired lease is free.** Leases carry a TTL and holders renew. A holder that
//!    crashes, or whose network drops, must not hold the device forever — that failure is
//!    indistinguishable from a careful user thinking, which is why it needs a clock and not a
//!    heuristic.
//! 3. ⛔ **Takeover needs a REASON, not a flag.** `force: true` is something a client sets
//!    once and forgets in a retry loop. A non-empty reason is something a human writes, and it
//!    is what makes the audit record worth reading.
//! 4. **Renew and release are compare-and-set against the token.** You cannot renew or drop a
//!    lease that is not yours — otherwise a stale client reaps the holder that replaced it.
//! 5. **Every grant, refusal and takeover is an event**, so "who took the device at 03:14" has
//!    an answer.
//!
//! The clock is injected, so every expiry rule here is tested without sleeping.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

pub const DEFAULT_TTL_SECS: u64 = 60;
/// How much audit history to keep in memory. Small: this is a breadcrumb trail for "what just
/// happened to my session", not a log.
pub const AUDIT_CAPACITY: usize = 64;

/// ⛔ THE TOKEN IS THE AUTHORITY, SO IT MUST BE UNGUESSABLE.
///
/// It was `<unix_secs>-<counter>-<len(holder)>`. The counter restarts at 1 each boot and the
/// holder name is usually public, so a 60-second window was about 480 candidates: anyone could
/// forge the live token and then renew or release somebody else's hold. Every CAS rule below
/// rests on this value being secret, so it is now 128 bits from the OS.
///
/// On failure we panic rather than degrade. There is no weaker token worth minting, and a
/// panic here fails closed: the lease is simply not granted.
fn random_token() -> String {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("OS entropy unavailable; refusing to mint a weak lease");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub token: String,
    pub holder: String,
    pub acquired_at: u64,
    pub expires_at: u64,
}

impl Lease {
    pub fn expired_at(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

/// What happened. ⛔ Refusal is a first-class outcome, not an error: being told who holds the
/// device and for how long is useful information, and a client that treats it as a failure to
/// retry is the client this design exists to stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Acquired {
    Granted { lease: Lease },
    TookOver { lease: Lease, displaced: String, reason: String },
    Refused { held_by: String, expires_in_secs: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEntry {
    Granted { at: u64, holder: String },
    Refused { at: u64, who: String, held_by: String },
    TookOver { at: u64, who: String, displaced: String, reason: String },
    Released { at: u64, holder: String },
    Expired { at: u64, holder: String },
}

pub struct LeaseManager {
    current: Option<Lease>,
    /// The live connection keeping `current` alive, if any. See `bind`.
    bound: Option<String>,
    ttl_secs: u64,
    audit: VecDeque<AuditEntry>,
}

impl LeaseManager {
    pub fn new(ttl_secs: u64) -> Self {
        Self { current: None, bound: None, ttl_secs: ttl_secs.max(1), audit: VecDeque::new() }
    }

    fn record(&mut self, e: AuditEntry) {
        if self.audit.len() == AUDIT_CAPACITY {
            self.audit.pop_front();
        }
        self.audit.push_back(e);
    }

    /// Drop an expired lease, recording that it lapsed rather than letting it vanish silently.
    ///
    /// ⛔ A BOUND LEASE DOES NOT EXPIRE. The TTL exists to free the device from a holder that
    /// vanished. A holder with a live connection has not vanished, and renewing on a timer to
    /// say so would be the same fact told twice -- and no safer, because a timer that only
    /// stops when the connection is reported gone fails exactly when that report fails.
    fn reap(&mut self, now: u64) {
        if self.bound.is_some() {
            return;
        }
        if let Some(l) = &self.current
            && l.expired_at(now)
        {
            let holder = l.holder.clone();
            self.current = None;
            self.bound = None;
            self.record(AuditEntry::Expired { at: now, holder });
        }
    }

    fn mint(&mut self, holder: &str, now: u64) -> Lease {
        Lease {
            token: random_token(),
            holder: holder.to_string(),
            acquired_at: now,
            expires_at: now + self.ttl_secs,
        }
    }

    /// Take the device, or be told why not.
    ///
    /// `reason` is the takeover gate: empty means "do not displace anyone". A non-empty reason
    /// displaces the current holder and is recorded verbatim.
    pub fn acquire(&mut self, who: &str, now: u64, reason: &str) -> Acquired {
        self.reap(now);
        match &self.current {
            None => {
                let lease = self.mint(who, now);
                self.current = Some(lease.clone());
                self.bound = None;
                self.record(AuditEntry::Granted { at: now, holder: who.to_string() });
                Acquired::Granted { lease }
            }
            Some(held) if reason.trim().is_empty() => {
                let held_by = held.holder.clone();
                let expires_in_secs = held.expires_at.saturating_sub(now);
                self.record(AuditEntry::Refused {
                    at: now, who: who.to_string(), held_by: held_by.clone(),
                });
                Acquired::Refused { held_by, expires_in_secs }
            }
            Some(held) => {
                let displaced = held.holder.clone();
                let lease = self.mint(who, now);
                self.current = Some(lease.clone());
                self.bound = None; // the displaced holder's connection no longer protects it
                self.record(AuditEntry::TookOver {
                    at: now, who: who.to_string(), displaced: displaced.clone(),
                    reason: reason.trim().to_string(),
                });
                Acquired::TookOver { lease, displaced, reason: reason.trim().to_string() }
            }
        }
    }

    /// Extend a lease you hold. ⛔ CAS on the token: a stale client must not be able to renew
    /// the holder that replaced it.
    pub fn renew(&mut self, token: &str, now: u64) -> Option<Lease> {
        self.reap(now);
        let ttl = self.ttl_secs;
        match &mut self.current {
            Some(l) if l.token == token => {
                l.expires_at = now + ttl;
                Some(l.clone())
            }
            _ => None,
        }
    }

    /// Give it back. CAS on the token for the same reason as renew.
    pub fn release(&mut self, token: &str, now: u64) -> bool {
        self.reap(now);
        match &self.current {
            Some(l) if l.token == token => {
                let holder = l.holder.clone();
                self.current = None;
                self.bound = None;
                self.record(AuditEntry::Released { at: now, holder });
                true
            }
            _ => false,
        }
    }

    /// Is this token the live holder right now? The question every guarded action asks.
    pub fn holds(&mut self, token: &str, now: u64) -> bool {
        self.reap(now);
        self.current.as_ref().is_some_and(|l| l.token == token)
    }

    pub fn current(&mut self, now: u64) -> Option<Lease> {
        self.reap(now);
        self.current.clone()
    }

    pub fn audit(&self) -> Vec<AuditEntry> {
        self.audit.iter().cloned().collect()
    }

    /// Tie the live lease to a connection, so it lasts as long as that connection does.
    ///
    /// Only the holder of `token` can bind it: otherwise any caller could pin someone else's
    /// lease open. Returns false if that token is not the live one.
    pub fn bind(&mut self, session_id: &str, token: &str, now: u64) -> bool {
        self.reap(now);
        match &self.current {
            Some(l) if l.token == token => {
                self.bound = Some(session_id.to_string());
                true
            }
            _ => false,
        }
    }

    /// That connection ended, so give the device back now rather than at the end of a TTL
    /// nothing has been counting. A no-op for any session that was not the bound one, which
    /// is what makes it safe to call from every teardown path.
    pub fn unbind(&mut self, session_id: &str, now: u64) -> bool {
        if self.bound.as_deref() != Some(session_id) {
            return false;
        }
        self.bound = None;
        let Some(l) = self.current.take() else { return true };
        self.record(AuditEntry::Released { at: now, holder: l.holder });
        true
    }

    pub fn ttl_secs(&self) -> u64 {
        self.ttl_secs
    }
}
