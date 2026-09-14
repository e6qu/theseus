fn main() {
    let value = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    std::process::exit(if value == 7 { 7 } else { value as i32 & 1 });
}
