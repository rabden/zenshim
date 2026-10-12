use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Guard rails for a gateway that may be reachable from the internet.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Largest request body accepted.
    pub max_body_bytes: usize,
    /// Requests per minute per remote client; 0 turns it off. Clients on
    /// this machine are never limited.
    pub rate_per_minute: u32,
    /// Generations running at once.
    pub max_concurrent: usize,
    /// Requests that may wait for a slot before new ones are refused.
    pub max_queue: usize,
    /// How long a request may wait for a slot.
    pub queue_timeout: Duration,
    /// Longest a whole generation may take.
    pub request_timeout: Duration,
    /// Longest upstream may stay silent mid-answer.
    pub idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_bytes: 32 * 1024 * 1024,
            rate_per_minute: 120,
            max_concurrent: 16,
            max_queue: 64,
            queue_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(600),
            idle_timeout: Duration::from_secs(180),
        }
    }
}

const WINDOW: Duration = Duration::from_secs(60);
/// Clients tracked before stale entries are swept.
const SWEEP_AT: usize = 4096;

/// Fixed-window request counter per client address.
pub struct RateLimiter {
    per_minute: u32,
    windows: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        Self { per_minute, windows: Mutex::new(HashMap::new()) }
    }

    /// Count one request; `Err` carries how long until the client may retry.
    pub fn check(&self, client: IpAddr, now: Instant) -> Result<(), Duration> {
        if self.per_minute == 0 {
            return Ok(());
        }
        let mut windows = self.windows.lock().unwrap();
        if windows.len() >= SWEEP_AT {
            windows.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
        }
        let (start, count) = windows.entry(client).or_insert((now, 0));
        if now.duration_since(*start) >= WINDOW {
            (*start, *count) = (now, 0);
        }
        if *count >= self.per_minute {
            return Err(WINDOW - now.duration_since(*start));
        }
        *count += 1;
        Ok(())
    }
}

/// Why a request did not get a generation slot.
#[derive(Debug, PartialEq, Eq)]
pub enum Busy {
    /// Too many requests are already waiting.
    QueueFull,
    /// Waited `queue_timeout` without a slot freeing up.
    TimedOut,
}

/// Bounds how many generations run at once and how many may wait.
pub struct Slots {
    semaphore: Arc<Semaphore>,
    waiting: AtomicUsize,
    max_queue: usize,
    timeout: Duration,
}

impl Slots {
    pub fn new(limits: &Limits) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(limits.max_concurrent.max(1))),
            waiting: AtomicUsize::new(0),
            max_queue: limits.max_queue,
            timeout: limits.queue_timeout,
        }
    }

    /// A slot, held until the returned permit is dropped.
    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit, Busy> {
        if let Ok(permit) = self.semaphore.clone().try_acquire_owned() {
            return Ok(permit);
        }
        if self.waiting.fetch_add(1, Ordering::SeqCst) >= self.max_queue {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            return Err(Busy::QueueFull);
        }
        // Dropped with the future, so a client that gives up while
        // queued does not keep its place.
        struct Waiting<'a>(&'a AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _waiting = Waiting(&self.waiting);
        match tokio::time::timeout(self.timeout, self.semaphore.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(Busy::TimedOut),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([203, 0, 113, last])
    }

    #[test]
    fn rate_limit_is_per_client_and_resets_each_minute() {
        let limiter = RateLimiter::new(2);
        let now = Instant::now();
        assert!(limiter.check(ip(1), now).is_ok());
        assert!(limiter.check(ip(1), now).is_ok());
        let wait = limiter.check(ip(1), now + Duration::from_secs(20)).unwrap_err();
        assert_eq!(wait, Duration::from_secs(40));
        assert!(limiter.check(ip(2), now).is_ok(), "other clients are unaffected");
        assert!(limiter.check(ip(1), now + WINDOW).is_ok());
    }

    #[test]
    fn zero_turns_rate_limiting_off() {
        let limiter = RateLimiter::new(0);
        let now = Instant::now();
        assert!((0..1000).all(|_| limiter.check(ip(1), now).is_ok()));
    }

    fn slots(max_concurrent: usize, max_queue: usize, timeout_ms: u64) -> Slots {
        Slots::new(&Limits {
            max_concurrent,
            max_queue,
            queue_timeout: Duration::from_millis(timeout_ms),
            ..Limits::default()
        })
    }

    #[tokio::test]
    async fn a_waiting_request_gets_the_slot_when_it_frees() {
        let slots = Arc::new(slots(1, 4, 5_000));
        let held = slots.acquire().await.unwrap();
        let waiter = tokio::spawn({
            let slots = slots.clone();
            async move { slots.acquire().await.is_ok() }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);
        assert!(waiter.await.unwrap());
    }

    #[tokio::test]
    async fn waiting_is_bounded_in_time_and_number() {
        let slots = Arc::new(slots(1, 1, 60));
        let _held = slots.acquire().await.unwrap();
        let queued = tokio::spawn({
            let slots = slots.clone();
            async move { slots.acquire().await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(slots.acquire().await.unwrap_err(), Busy::QueueFull);
        assert_eq!(queued.await.unwrap().unwrap_err(), Busy::TimedOut);
        // The timed-out waiter gave its queue place back.
        assert_eq!(slots.acquire().await.unwrap_err(), Busy::TimedOut);
    }
}
