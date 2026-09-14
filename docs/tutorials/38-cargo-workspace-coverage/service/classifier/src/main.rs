fn main() {
    let value = std::env::args()
        .nth(1)
        .expect("one number")
        .parse()
        .expect("a number");
    println!("class={}", logic::classify(value));
}
