use super::*;

#[test]
fn related_tools_share_an_exact_concurrent_allowance() {
    let allowance = ResearchAllowance::new(3);
    let results = std::thread::scope(|scope| {
        let threads = (0..12)
            .map(|_| {
                let allowance = allowance.clone();
                scope.spawn(move || allowance.try_acquire())
            })
            .collect::<Vec<_>>();
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|allowed| *allowed)
            .count()
    });
    assert_eq!(results, 3);
    assert_eq!(allowance.remaining(), 0);
    assert_eq!(allowance.attempts(), 12);
}

#[test]
fn refusal_counts_cannot_wrap_and_reopen_an_exhausted_allowance() {
    let counter = Arc::new(AtomicUsize::new(usize::MAX));
    let allowance = ResearchAllowance::with_counter(usize::MAX, counter.clone());
    assert!(!allowance.try_acquire());
    assert!(!allowance.try_acquire());
    assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    assert_eq!(allowance.remaining(), 0);
    assert!(!ResearchAllowance::new(0).try_acquire());
}
