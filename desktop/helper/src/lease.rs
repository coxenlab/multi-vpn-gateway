//! 新版助手的非持久租约与重启退避；旧身份不使用这套策略。
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode { Forward, Reject, Off }
impl Mode {
    pub fn name(self) -> &'static str { match self { Self::Forward => "forward", Self::Reject => "reject", Self::Off => "off" } }
}

#[derive(Default)]
pub struct Lease { deadline: Option<Instant>, pub upstream_ok: bool }
impl Lease {
    pub fn ensure(&mut self, now: Instant, secs: u64, upstream_ok: bool) {
        self.deadline = Some(now + Duration::from_secs(secs.clamp(1, 90)));
        self.upstream_ok = upstream_ok;
    }
    pub fn renew(&mut self, now: Instant, current: u64, requested: u64, running: bool, secs: u64, upstream_ok: bool) -> Result<(), &'static str> {
        if !running || self.deadline.is_none() { return Err("ensure required"); }
        if current == 0 || current != requested { return Err("stale generation"); }
        self.ensure(now, secs, upstream_ok);
        Ok(())
    }
    pub fn clear(&mut self) { self.deadline = None; self.upstream_ok = false; }
    pub fn remaining(&self, now: Instant) -> u64 {
        self.deadline.map(|d| d.saturating_duration_since(now).as_secs()).unwrap_or(0)
    }
    pub fn mode(&self, running: bool, now: Instant) -> Mode {
        if !running || self.deadline.is_none_or(|deadline| deadline <= now) { Mode::Off }
        else if self.upstream_ok { Mode::Forward } else { Mode::Reject }
    }
}

#[derive(Default)]
pub struct Backoff { failures: usize, next: Option<Instant>, started: Option<Instant> }
impl Backoff {
    pub fn ready(&self, now: Instant) -> bool { self.next.is_none_or(|next| now >= next) }
    pub fn failed(&mut self, now: Instant) -> Duration {
        let delay = Duration::from_secs([2, 4, 8, 30][self.failures.min(3)]);
        self.failures = self.failures.saturating_add(1);
        self.next = Some(now + delay); self.started = None;
        delay
    }
    pub fn spawned(&mut self, now: Instant) { self.next = None; self.started = Some(now); }
    pub fn healthy(&mut self, now: Instant) {
        if self.started.is_some_and(|start| now.duration_since(start) >= Duration::from_secs(30)) { self.failures = 0; }
    }
    pub fn reset(&mut self) { *self = Self::default(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lease_requires_ensure_and_exact_generation_then_expires_without_persisted_resurrection() {
        let now = Instant::now(); let mut lease = Lease::default();
        assert_eq!(lease.mode(true, now), Mode::Off);
        assert!(lease.renew(now, 7, 7, true, 90, true).is_err());
        lease.ensure(now, 90, true);
        assert_eq!(lease.mode(true, now), Mode::Forward);
        assert!(lease.renew(now, 7, 6, true, 90, false).is_err());
        assert_eq!(lease.mode(true, now), Mode::Forward);
        lease.renew(now + Duration::from_secs(20), 7, 7, true, 90, false).unwrap();
        assert_eq!(lease.mode(true, now + Duration::from_secs(90)), Mode::Reject);
        assert_eq!(lease.mode(true, now + Duration::from_secs(110)), Mode::Off);
        lease.renew(now + Duration::from_secs(111), 7, 7, true, 90, true).unwrap();
        assert_eq!(lease.mode(true, now + Duration::from_secs(112)), Mode::Forward);
        lease.clear();
        assert_eq!(lease.mode(true, now), Mode::Off);
        assert!(lease.renew(now, 7, 7, false, 90, true).is_err());
    }
    #[test]
    fn repeated_crashes_back_off_and_stable_engine_resets_sequence() {
        let mut b = Backoff::default(); let mut now = Instant::now();
        for secs in [2,4,8,30,30] {
            assert_eq!(b.failed(now), Duration::from_secs(secs));
            assert!(!b.ready(now)); now += Duration::from_secs(secs);
            assert!(b.ready(now)); b.spawned(now);
        }
        b.healthy(now + Duration::from_secs(31));
        assert_eq!(b.failed(now + Duration::from_secs(31)), Duration::from_secs(2));
    }
}
