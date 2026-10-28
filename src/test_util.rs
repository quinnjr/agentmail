//! Helpers shared by the unit tests.

use std::time::{Duration, Instant};

/// How long [`eventually`] keeps retrying.
const DEADLINE: Duration = Duration::from_secs(2);

/// Retry `f` every few milliseconds until it returns `Ok` or about two
/// seconds pass, then return its last result.
///
/// For checks on file locks taken with `flock`: such a lock belongs to
/// the open file description, not the descriptor. When another test
/// thread in this process spawns a child, the child holds a duplicate of
/// every descriptor we have open (close-on-exec ones included) from the
/// fork until it execs. A lock we just released, or just took and dropped
/// in a probe, can therefore still look held for a moment. Any check that
/// a lock is free must wait that window out instead of trying once.
pub(crate) fn eventually<T, E>(f: impl FnMut() -> Result<T, E>) -> Result<T, E> {
    eventually_within(DEADLINE, f)
}

/// [`eventually`] with the deadline given.
fn eventually_within<T, E>(limit: Duration, mut f: impl FnMut() -> Result<T, E>) -> Result<T, E> {
    let deadline = Instant::now() + limit;
    loop {
        let res = f();
        if res.is_ok() || Instant::now() >= deadline {
            return res;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eventually_retries_until_ok() {
        let mut calls = 0;
        let res: Result<u32, ()> = eventually(|| {
            calls += 1;
            if calls < 3 { Err(()) } else { Ok(calls) }
        });
        assert_eq!(res, Ok(3));
    }

    #[test]
    fn eventually_gives_up_with_the_last_error() {
        let started = Instant::now();
        let mut calls = 0;
        let limit = Duration::from_millis(50);
        let res: Result<(), u32> = eventually_within(limit, || {
            calls += 1;
            Err(calls)
        });
        assert!(calls > 1, "never retried");
        assert_eq!(res, Err(calls));
        assert!(started.elapsed() >= limit);
    }
}
