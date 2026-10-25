pub fn f(c: bool, s: usize, q: usize) -> usize {
    if c {
        0
    } else {
        s.saturating_sub(q)
    }
}
