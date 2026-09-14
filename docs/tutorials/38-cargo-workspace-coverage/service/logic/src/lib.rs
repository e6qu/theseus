#[inline(never)]
pub fn classify(value: u32) -> &'static str {
    if value == 7 {
        "seven"
    } else if value % 2 == 0 {
        "even"
    } else {
        "odd"
    }
}
