use std::{ops::Range, panic, sync::OnceLock, thread};

pub fn available_threads() -> usize {
    // Looking it up reads files on Linux, so only once
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| thread::available_parallelism().map_or(1, |threads| threads.get()))
}

// `work` on consecutive ranges covering 0..count, one per thread, with the
// results in order. This thread does the last range rather than wait.
pub fn split_across<T: Send>(
    count: usize,
    threads: usize,
    work: impl Fn(Range<usize>) -> T + Sync,
) -> Vec<T> {
    let per_thread = count.div_ceil(threads.max(1)).max(1);
    let ranges: Vec<_> = (0..count)
        .step_by(per_thread)
        .map(|start| start..count.min(start + per_thread))
        .collect();
    let Some((last, others)) = ranges.split_last() else {
        return Vec::new();
    };
    if others.is_empty() {
        return vec![work(last.clone())];
    }
    thread::scope(|scope| {
        let work = &work;
        let others: Vec<_> = others
            .iter()
            .map(|range| scope.spawn(move || work(range.clone())))
            .collect();
        let last = work(last.clone());
        let mut results: Vec<T> = others
            .into_iter()
            .map(|thread| {
                thread
                    .join()
                    .unwrap_or_else(|error| panic::resume_unwind(error))
            })
            .collect();
        results.push(last);
        results
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_everything_in_order() {
        for (count, threads) in [
            (0, 4),
            (1, 4),
            (10, 1),
            (10, 3),
            (10, 10),
            (10, 16),
            (1000, 7),
        ] {
            let ranges = split_across(count, threads, |range| range);
            assert!(
                ranges.len() <= threads.max(1),
                "{count} {threads}: {ranges:?}"
            );
            let covered: Vec<usize> = ranges.into_iter().flatten().collect();
            assert_eq!(covered, (0..count).collect::<Vec<_>>(), "{count} {threads}");
        }
    }
}
