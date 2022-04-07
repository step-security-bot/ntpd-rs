// An implementation of the NTP clock filter algorithm, as described by
//
//      https://datatracker.ietf.org/doc/html/rfc5905#page-37
//
// Specifically this is a rust implementation of the `clock_filter()` routine,
// described in the appendix
//
//      https://datatracker.ietf.org/doc/html/rfc5905#appendix-A.5.2

use crate::{packet::NtpLeapIndicator, NtpDuration, NtpTimestamp};

/// frequency tolerance (15 ppm)
const ONE_OVER_PHI: i64 = 15_000_000;

/// spike gate (clock filter)
const SGATE: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct FilterTuple {
    offset: NtpDuration,
    delay: NtpDuration,
    dispersion: NtpDuration,
    time: NtpTimestamp,
}

impl FilterTuple {
    const DUMMY: Self = Self {
        offset: NtpDuration::ZERO,
        delay: NtpDuration::MAX_DISPERSION,
        dispersion: NtpDuration::MAX_DISPERSION,
        time: NtpTimestamp::ZERO,
    };

    fn is_dummy(self) -> bool {
        self == Self::DUMMY
    }
}

#[derive(Debug, Clone, Copy)]
struct ClockFilterContents {
    register: [FilterTuple; 8],
}

impl ClockFilterContents {
    #[allow(dead_code)]
    const fn new() -> Self {
        Self {
            register: [FilterTuple::DUMMY; 8],
        }
    }

    /// Insert the new tuple at index 0, move all other tuples one to the right.
    /// The final (oldest) tuple is discarded
    fn shift_and_insert(&mut self, mut current: FilterTuple, dispersion_correction: NtpDuration) {
        for tuple in self.register.iter_mut() {
            tuple.dispersion += dispersion_correction;

            std::mem::swap(&mut current, tuple);
        }
    }

    fn sort_by_delay(&mut self) {
        self.register.sort_by(|t1, t2| {
            t1.delay
                .partial_cmp(&t2.delay)
                .unwrap_or(std::cmp::Ordering::Less)
        });
    }

    fn valid_tuples(&self) -> &[FilterTuple] {
        let num_invalid_tuples = self
            .register
            .iter()
            .rev()
            .take_while(|t| t.is_dummy())
            .count();

        let num_valid_tuples = self.register.len() - num_invalid_tuples;

        &self.register[..num_valid_tuples]
    }

    /// #[no_run]
    ///                     i=n-1
    ///                     ---     epsilon_i
    ///      epsilon =       \     ----------
    ///                      /        (i+1)
    ///                     ---     2
    ///                     i=0
    /// Invariant: the register is sorted wrt delay
    fn dispersion(self) -> NtpDuration {
        self.register
            .iter()
            .enumerate()
            .map(|(i, t)| t.dispersion / 2i64.pow(i as u32 + 1))
            .fold(NtpDuration::default(), |a, b| a + b)
    }

    /// #[no_run]
    ///                          +-----                 -----+^1/2
    ///                          |  n-1                      |
    ///                          |  ---                      |
    ///                  1       |  \                     2  |
    ///      psi   =  -------- * |  /    (theta_0-theta_j)   |
    ///                (n-1)     |  ---                      |
    ///                          |  j=1                      |
    ///                          +-----                 -----+
    ///
    /// Invariant: the register is sorted wrt delay
    fn jitter(self, s: &System, smallest_delay: FilterTuple) -> f64 {
        let register = self.valid_tuples();

        let root_mean_square = register
            .iter()
            .map(|t| (t.offset - smallest_delay.offset).to_seconds().powi(2))
            .sum::<f64>()
            .sqrt();

        // root mean square average (RMS average). - 1 to exclude the smallest_delay
        let jitter = root_mean_square / (register.len() - 1) as f64;

        // In order to ensure consistency and avoid divide exceptions in other
        // computations, the psi is bounded from below by the system precision
        // s.rho expressed in seconds.
        jitter.max(s.precision)
    }
}

pub struct System {
    precision: f64,
    poll: NtpDuration,
    leap_indicator: NtpLeapIndicator,
    reference_id: u8,
}

impl System {
    #[cfg(test)]
    fn dummy() -> Self {
        Self {
            precision: 0.0,
            poll: NtpDuration::default(),
            leap_indicator: NtpLeapIndicator::NoWarning,
            reference_id: 0,
        }
    }
}

pub struct Peer {
    clock_filter: ClockFilterContents,
    t: NtpTimestamp,

    jitter: f64,
    offset: NtpDuration,
    delay: NtpDuration,
    dispersion: NtpDuration,

    root_delay: NtpDuration,
    root_dispersion: NtpDuration,

    burst_counter: u32,
    leap_indicator: NtpLeapIndicator,
    stratum: u8,
    reach: i32,

    reference_id: u8,
    destination_address: u8,
}

pub struct LocalClock {
    t: NtpTimestamp,
}

#[allow(dead_code)]
pub fn clock_filter(
    peer: &mut Peer,
    s: &System,
    c: &LocalClock,
    clock_offset: NtpDuration,
    roundtrip_delay: NtpDuration,
    dispersion: NtpDuration,
) {
    let new_tuple = FilterTuple {
        offset: clock_offset,
        delay: roundtrip_delay,
        dispersion,
        time: c.t,
    };

    let dispersion_correction = (c.t - peer.t) / ONE_OVER_PHI;
    peer.clock_filter
        .shift_and_insert(new_tuple, dispersion_correction);

    let mut temporary_list = peer.clock_filter;

    temporary_list.sort_by_delay();

    let smallest_delay = temporary_list.register[0];

    let dtemp = peer.offset;
    peer.offset = smallest_delay.offset;
    peer.delay = smallest_delay.delay;

    // TODO (not in the skeleton as far as I can see)
    // If the first tuple epoch t_0 is not
    // later than the last valid sample epoch tp, the routine exits without
    // affecting the current peer variables.

    peer.dispersion = temporary_list.dispersion();
    peer.jitter = temporary_list.jitter(s, smallest_delay);

    // Prime directive: use a sample only once and never a sample
    // older than the latest one, but anything goes before first
    // synchronized.
    if smallest_delay.time - peer.t <= NtpDuration::default() && s.leap_indicator.is_synchronized()
    {
        return;
    }

    // Popcorn spike suppressor.  Compare the difference between the
    // last and current offsets to the current jitter.  If greater
    // than SGATE (3) and if the interval since the last offset is
    // less than twice the system poll interval, dump the spike.
    // Otherwise, and if not in a burst, shake out the truechimers.
    let too_soon = (smallest_delay.time - peer.t) < (s.poll * 2i64);
    if (peer.offset - dtemp).to_seconds().abs() > SGATE * peer.jitter && too_soon {
        return;
    }

    peer.t = smallest_delay.time;
    if peer.burst_counter == 0 {
        todo!()
        // clock_select();
    }
}

enum FitError {
    StratumNotSynchronized,
    ServerStratumInvalid,
    Distance,
    Loop,
    Unreachable,
}

/// Distance threshold
const MAXDIST: f64 = 1.0;

/// Maximum Stratum Number
const MAXSTRAT: u8 = 16;

fn fit(p: &Peer, s: &System, c: &LocalClock) -> Result<(), FitError> {
    if !p.leap_indicator.is_synchronized() {
        Err(FitError::StratumNotSynchronized)
    } else if p.stratum >= MAXSTRAT {
        Err(FitError::ServerStratumInvalid)
    } else if root_distance(p, c)
        > NtpDuration::from_seconds(MAXDIST + (s.poll / ONE_OVER_PHI).to_seconds())
    {
        // A distance error occurs if the root distance exceeds the
        // distance threshold plus an increment equal to one poll
        // interval.
        Err(FitError::Distance)
    } else if p.reference_id == p.destination_address || p.reference_id == s.reference_id {
        // A loop error occurs if the remote peer is synchronized to the
        // local peer or the remote peer is synchronized to the current
        // system peer.  Note this is the behavior for IPv4; for IPv6
        // the MD5 hash is used instead.
        Err(FitError::Loop)
    } else if p.reach == 0 {
        Err(FitError::Unreachable)
    } else {
        Ok(())
    }
}

/// The root synchronization distance is the maximum error due to
/// all causes of the local clock relative to the primary server.
/// It is defined as half the total delay plus total dispersion
/// plus peer jitter.
fn root_distance(p: &Peer, c: &LocalClock) -> NtpDuration {
    NtpDuration::MIN_DISPERSION.max(p.root_delay + p.delay) / 2i64
        + p.root_dispersion
        + p.dispersion
        + ((c.t - p.t) / ONE_OVER_PHI)
        + NtpDuration::from_seconds(p.jitter)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn dispersion_of_dummys() {
        // The observer should note (a) if all stages contain the dummy tuple
        // with dispersion MAXDISP, the computed dispersion is a little less than 16 s

        let register = ClockFilterContents::new();
        let value = register.dispersion().to_seconds();

        assert!((16.0 - value) < 0.1)
    }

    #[test]
    fn dummys_are_not_valid() {
        assert!(ClockFilterContents::new().valid_tuples().is_empty())
    }

    #[test]
    fn jitter_of_single() {
        let mut register = ClockFilterContents::new();
        register.register[0].offset = NtpDuration::from_seconds(42.0);
        let first = register.register[0];
        let system = System::dummy();
        let value = register.jitter(&system, first);

        assert_eq!(value, 0.0)
    }

    #[test]
    fn jitter_of_pair() {
        let mut register = ClockFilterContents::new();
        register.register[0].offset = NtpDuration::from_seconds(20.0);
        register.register[1].offset = NtpDuration::from_seconds(30.0);
        let first = register.register[0];
        let system = System::dummy();
        let value = register.jitter(&system, first);

        // jitter is calculated relative to the first tuple
        assert!((value - 10.0).abs() < 1e-6)
    }

    #[test]
    fn jitter_of_triple() {
        let mut register = ClockFilterContents::new();
        register.register[0].offset = NtpDuration::from_seconds(20.0);
        register.register[1].offset = NtpDuration::from_seconds(20.0);
        register.register[2].offset = NtpDuration::from_seconds(30.0);
        let first = register.register[0];
        let system = System::dummy();
        let value = register.jitter(&system, first);

        // jitter is calculated relative to the first tuple
        assert!((value - 5.0).abs() < 1e-6)
    }
}
