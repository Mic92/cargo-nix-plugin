pub fn greet() -> String {
    let mut s = bar::base();
    if cfg!(feature = "loud") {
        s.push('!');
    }
    s
}
