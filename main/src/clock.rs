use std::fmt;
use std::sync::{Arc, LazyLock, Mutex};

pub type Instant = std::time::Instant;

pub struct DisplayInstant(pub Instant);

impl fmt::Display for DisplayInstant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0.elapsed())
    }
}

pub type Utc = time::OffsetDateTime;
pub type Duration = time::Duration;

#[derive(Clone, Debug)]
pub struct Clock(ClockInner);

impl Clock {
    /// Current time according to the monotone clock.
    pub fn now(&self) -> Instant {
        match &self.0 {
            ClockInner::Real => Instant::now(),
            ClockInner::Fake(fake) => fake.now(),
        }
    }
}

#[derive(Clone, Debug)]
enum ClockInner {
    Real,
    Fake(FakeClock),
}

#[derive(Clone, Debug)]
pub struct FakeClock(Arc<Mutex<FakeClockInner>>);

impl FakeClock {
    /// Constructor of a fake clock. Use it in tests.
    /// It supports manually moving time forward (via advance()).
    /// You can also arbitrarily set the UTC time in runtime.
    /// Use FakeClock::clock() when calling prod code from tests.
    pub fn new(utc: Utc) -> Self {
        Self(Arc::new(Mutex::new(FakeClockInner::new(utc))))
    }
    pub fn now(&self) -> Instant {
        self.0.lock().unwrap().now()
    }

    pub fn clock(&self) -> Clock {
        Clock(ClockInner::Fake(self.clone()))
    }
}

#[derive(Debug)]
struct FakeClockInner {
    utc: Utc,
    instant: Instant,
}

// Instant doesn't have a deterministic constructor,
// however since Instant is not convertible to an unix timestamp,
// we can snapshot Instant::now() once and treat it as a constant.
// All observable effects will be then deterministic.
static FAKE_CLOCK_MONO_START: LazyLock<Instant> = LazyLock::new(Instant::now);

impl FakeClockInner {
    pub fn new(utc: Utc) -> Self {
        Self {
            utc,
            instant: *FAKE_CLOCK_MONO_START,
        }
    }

    pub fn now(&mut self) -> Instant {
        self.instant
    }
}

#[cfg(test)]
mod tests {
    use crate::clock::DisplayInstant;
    use std::time::Instant;

    #[test]
    fn test_now() {
        println!("NOW:{}", DisplayInstant(Instant::now()));
    }
}
