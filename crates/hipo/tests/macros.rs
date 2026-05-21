//! Smoke tests for the `or_continue!` / `or_break!` macros.

#[test]
fn or_continue_skips_none() {
    let inputs = [Some(1_i32), None, Some(2), None, Some(3)];
    let mut kept = Vec::new();
    for opt in inputs {
        let v = hipo::or_continue!(opt);
        kept.push(v);
    }
    assert_eq!(kept, vec![1, 2, 3]);
}

#[test]
fn or_break_exits_on_none() {
    let inputs = [Some(1_i32), Some(2), None, Some(3)];
    let mut kept = Vec::new();
    for opt in inputs {
        let v = hipo::or_break!(opt);
        kept.push(v);
    }
    assert_eq!(kept, vec![1, 2]);
}

#[test]
fn or_continue_in_nested_loops_targets_inner() {
    let mut visited = Vec::new();
    for outer in 0..3 {
        for inner in [Some(outer), None, Some(outer + 100)] {
            let v = hipo::or_continue!(inner);
            visited.push(v);
        }
    }
    // outer=0: skip None → 0, 100; outer=1: 1, 101; outer=2: 2, 102.
    assert_eq!(visited, vec![0, 100, 1, 101, 2, 102]);
}
