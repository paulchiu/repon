//! Work runs off the caller's thread and reports back over a channel.
//!
//! No tokio: a rayon pool does the work and a crossbeam channel carries each result to
//! whoever holds the receiver, which is gitui's proven shape and matches gitoxide's own
//! preference to keep git off an async runtime. Supersession of an in-flight round is
//! not decided here; it belongs to the refresh model.

use crossbeam_channel::Sender;
use rayon::iter::{IntoParallelIterator, ParallelIterator};

/// Runs `work` over every job in parallel, sending each result the moment it lands.
///
/// Blocks until every job has been sent, so call it from a worker thread rather than
/// from a render loop. A closed receiver ends in dropped results, not a panic.
pub fn scatter<J, R, F>(jobs: Vec<J>, results: Sender<R>, work: F)
where
    J: Send,
    R: Send,
    F: Fn(J) -> R + Send + Sync,
{
    jobs.into_par_iter().for_each_with(results, |results, job| {
        let _ = results.send(work(job));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_result_reaches_the_receiver() {
        let (tx, rx) = crossbeam_channel::unbounded();

        scatter(vec![1, 2, 3], tx, |n| n * 2);

        let mut got: Vec<i32> = rx.into_iter().collect();
        got.sort_unstable();
        assert_eq!(got, vec![2, 4, 6]);
    }
}
