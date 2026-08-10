use std::collections::BTreeMap;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let code = lcov_merger::run(&argv, &env, &mut std::io::stderr());
    std::process::exit(code);
}
