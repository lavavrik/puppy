//! Splits the global download speed limit across download engines.

/// Splits a global download limit across engines in proportion to their active download counts.
/// Engines with 0 active still get a small share so a newly started download isn't unlimited before the next rebalance.
pub fn divide(global: Option<u64>, active_per_engine: &[usize]) -> Vec<Option<u64>> {
    let Some(global) = global else {
        return vec![None; active_per_engine.len()];
    };
    let total: u128 = active_per_engine.iter().map(|&a| a as u128).sum();
    if total == 0 {
        // Nothing is active: each engine may start one download before the next rebalance.
        return vec![Some(global); active_per_engine.len()];
    }
    let global = global as u128;
    active_per_engine
        .iter()
        .map(|&active| {
            let share = if active > 0 {
                global * active as u128 / total
            } else {
                global / (total + 1)
            };
            // The share never exceeds `global`, so narrowing back to u64 is lossless.
            Some(share.max(1) as u64)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::divide;

    #[test]
    fn unlimited() {
        assert_eq!(divide(None, &[2, 1]), vec![None, None]);
        assert_eq!(divide(None, &[]), Vec::<Option<u64>>::new());
    }

    #[test]
    fn single_engine_gets_all() {
        assert_eq!(divide(Some(1000), &[3]), vec![Some(1000)]);
    }

    #[test]
    fn proportional_split() {
        assert_eq!(divide(Some(900), &[2, 1]), vec![Some(600), Some(300)]);
    }

    #[test]
    fn idle_engine_gets_small_share() {
        // t = 3, global / (t + 1) = 900 / 4 = 225 for the idle engine.
        assert_eq!(divide(Some(900), &[3, 0]), vec![Some(900), Some(225)]);
    }

    #[test]
    fn no_actives_each_gets_global() {
        assert_eq!(divide(Some(500), &[0, 0]), vec![Some(500), Some(500)]);
    }

    #[test]
    fn never_zero() {
        let shares = divide(Some(1), &[3, 3]);
        assert_eq!(shares.len(), 2);
        for share in shares {
            assert!(share.expect("limited") >= 1);
        }
        // Idle engine with a tiny global limit still gets at least 1.
        assert_eq!(divide(Some(1), &[5, 0]), vec![Some(1), Some(1)]);
    }
}
